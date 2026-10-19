//! Standard streams for `run()`: tokio's blocking-pool handles, or the process's
//! own pipes driven by the I/O reactor.
//!
//! `tokio::io::stdin()` and `tokio::io::stdout()` run every read and write on
//! the blocking thread pool. That costs a thread hand-off per message, and a
//! read that is pending when the transport stops cannot be cancelled: its
//! blocking thread stays parked in `read(2)`, and dropping the runtime waits
//! for it until the peer writes or closes the pipe.
//!
//! When fd 0 and fd 1 are pipes (how MCP clients launch stdio servers), they
//! can be registered with the reactor instead, which needs `O_NONBLOCK`. That
//! flag lives on the open file description, which the process may share with
//! others (a child that inherited stdin, stderr redirected onto stdout), so
//! flipping it on the inherited descriptor is not free. [`StdioPipes`] draws
//! the line:
//!
//! - On Linux a pipe can be reopened through `/proc/self/fd/N`, which creates
//!   a new open file description. `O_NONBLOCK` set on it is invisible to
//!   everyone else, so this is the default.
//! - Anywhere else, and for sockets (what Node's `child_process` hands a
//!   server), the only way is to set the flag on the shared description. That
//!   is opt-in, and the original flags are restored when the transport ends.
//!
//! TTYs and regular files never take this path, and neither does a stream pair
//! where only one half qualifies: the halves can share a description, so
//! converting one and not the other could hand `O_NONBLOCK` to a blocking
//! writer.

/// How [`StdioTransport::run`](super::StdioTransport::run) and the other stdio
/// transports open the process's standard streams.
///
/// The choice only affects performance and how promptly `run()` releases
/// stdin. Framing, limits, and shutdown behaviour are the same in every mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StdioPipes {
    /// Use the reactor for stdin and stdout when that cannot affect any other
    /// holder of the descriptors, and tokio's blocking handles otherwise.
    ///
    /// In practice: both streams are pipes on Linux. This is the default.
    #[default]
    Auto,
    /// Like [`Auto`](Self::Auto), and also use the reactor when that requires
    /// setting `O_NONBLOCK` on a descriptor shared with other processes or
    /// descriptors: pipes on macOS and the BSDs, and sockets everywhere on
    /// Unix.
    ///
    /// The flag is restored when the transport finishes. While it runs, a
    /// process sharing the open file description (a child that inherited
    /// stdin, or a stderr redirected onto stdout) sees non-blocking I/O and
    /// can get `EAGAIN`. Choose this when the server owns its standard
    /// streams.
    Shared,
    /// Always use `tokio::io::stdin()` and `tokio::io::stdout()`.
    Blocking,
}

#[cfg(not(unix))]
pub(super) use blocking_only::open;
#[cfg(unix)]
pub(super) use fd::open;

#[cfg(not(unix))]
mod blocking_only {
    use super::StdioPipes;

    pub(in super::super) fn open(_mode: StdioPipes) -> (tokio::io::Stdin, tokio::io::Stdout) {
        (tokio::io::stdin(), tokio::io::stdout())
    }
}

#[cfg(unix)]
mod fd {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};

    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};

    use super::StdioPipes;

    /// Reader half returned by [`open`].
    pub(in super::super) enum StdioReader {
        Blocking(tokio::io::Stdin),
        Fd(FdIo),
    }

    /// Writer half returned by [`open`].
    pub(in super::super) enum StdioWriter {
        Blocking(tokio::io::Stdout),
        Fd(FdIo),
    }

    /// The kinds of descriptor that work with the reactor and that fd 0 / fd 1
    /// are commonly handed as.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Fifo,
        Socket,
    }

    /// Open stdin and stdout according to `mode`, falling back to tokio's
    /// blocking handles whenever the reactor path does not apply.
    pub(in super::super) fn open(mode: StdioPipes) -> (StdioReader, StdioWriter) {
        if mode != StdioPipes::Blocking
            && let Some((reader, writer)) = open_fds(mode)
        {
            tracing::debug!(?mode, "stdio: using non-blocking pipes");
            return (StdioReader::Fd(reader), StdioWriter::Fd(writer));
        }
        (
            StdioReader::Blocking(tokio::io::stdin()),
            StdioWriter::Blocking(tokio::io::stdout()),
        )
    }

    fn open_fds(mode: StdioPipes) -> Option<(FdIo, FdIo)> {
        // Registering needs a reactor. A runtime built without I/O reports
        // that as an error from `AsyncFd`, which sends us to the fallback; a
        // missing runtime cannot happen because `run()` is async.
        let read_kind = kind(libc::STDIN_FILENO)?;
        let write_kind = kind(libc::STDOUT_FILENO)?;

        // Both halves or neither: see the module docs. Every step below is
        // undone on failure by dropping what was created so far.
        let reader = FdIo::open(libc::STDIN_FILENO, read_kind, Interest::READABLE, mode)?;
        let writer = FdIo::open(libc::STDOUT_FILENO, write_kind, Interest::WRITABLE, mode)?;
        Some((reader, writer))
    }

    /// `Some` for pipes, FIFOs and sockets, `None` for everything else
    /// (TTYs, regular files, `/dev/null`, a closed descriptor).
    fn kind(fd: RawFd) -> Option<Kind> {
        // SAFETY: `stat` is plain old data, and `fstat` only writes to it.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return None;
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFIFO => Some(Kind::Fifo),
            libc::S_IFSOCK => Some(Kind::Socket),
            _ => None,
        }
    }

    /// A non-blocking descriptor registered with the reactor.
    pub(in super::super) struct FdIo {
        inner: AsyncFd<NonBlockingFd>,
    }

    /// Owns the descriptor and puts back the file status flags it changed.
    struct NonBlockingFd {
        fd: OwnedFd,
        /// The flags to restore, set only when this changed a shared
        /// description (a private one dies with the descriptor).
        restore: Option<libc::c_int>,
    }

    impl AsRawFd for NonBlockingFd {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    impl Drop for NonBlockingFd {
        fn drop(&mut self) {
            if let Some(flags) = self.restore {
                // SAFETY: `fd` is open until this struct finishes dropping.
                unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, flags) };
            }
        }
    }

    impl FdIo {
        fn open(fd: RawFd, kind: Kind, interest: Interest, mode: StdioPipes) -> Option<Self> {
            let nonblocking = match private_reopen(fd, kind, interest) {
                Some(fd) => NonBlockingFd { fd, restore: None },
                None if mode == StdioPipes::Shared => share(fd)?,
                None => return None,
            };
            let inner = AsyncFd::with_interest(nonblocking, interest).ok()?;
            Some(Self { inner })
        }
    }

    /// Reopen a FIFO as a new open file description with `O_NONBLOCK`.
    ///
    /// Linux exposes the description behind a descriptor as
    /// `/proc/self/fd/N`, and opening that path creates a new one. Sockets
    /// cannot be reopened, nor can pipes on other platforms.
    #[cfg(target_os = "linux")]
    fn private_reopen(fd: RawFd, kind: Kind, interest: Interest) -> Option<OwnedFd> {
        use std::os::unix::fs::OpenOptionsExt;

        if kind != Kind::Fifo {
            return None;
        }
        let file = std::fs::OpenOptions::new()
            .read(interest.is_readable())
            .write(interest.is_writable())
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{fd}"))
            .ok()?;
        // The path could name something else if `/proc` is not what it seems.
        let fd = OwnedFd::from(file);
        (self::kind(fd.as_raw_fd()) == Some(Kind::Fifo)).then_some(fd)
    }

    #[cfg(not(target_os = "linux"))]
    fn private_reopen(_fd: RawFd, _kind: Kind, _interest: Interest) -> Option<OwnedFd> {
        None
    }

    /// Duplicate `fd` and set `O_NONBLOCK` on the description both share.
    fn share(fd: RawFd) -> Option<NonBlockingFd> {
        // SAFETY: `fcntl` with these commands only inspects descriptors.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup < 0 {
            return None;
        }
        // SAFETY: `dup` is a descriptor this call just created and owns.
        let dup = unsafe { OwnedFd::from_raw_fd(dup) };
        let flags = unsafe { libc::fcntl(dup.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return None;
        }
        if flags & libc::O_NONBLOCK != 0 {
            // Someone already made it non-blocking; leave it as found.
            return Some(NonBlockingFd {
                fd: dup,
                restore: None,
            });
        }
        if unsafe { libc::fcntl(dup.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
            return None;
        }
        Some(NonBlockingFd {
            fd: dup,
            restore: Some(flags),
        })
    }

    /// Retry `f` until it stops reporting `EINTR`.
    fn retry_interrupted(mut f: impl FnMut() -> isize) -> io::Result<usize> {
        loop {
            let n = f();
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    impl AsyncRead for StdioReader {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                StdioReader::Blocking(stdin) => Pin::new(stdin).poll_read(cx, buf),
                StdioReader::Fd(io) => loop {
                    let mut guard = ready!(io.inner.poll_read_ready(cx))?;
                    let unfilled = buf.initialize_unfilled();
                    let result = guard.try_io(|inner| {
                        let fd = inner.get_ref().as_raw_fd();
                        // SAFETY: `unfilled` is a valid, writable slice.
                        retry_interrupted(|| unsafe {
                            libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len())
                        })
                    });
                    match result {
                        Ok(Ok(n)) => {
                            buf.advance(n);
                            return Poll::Ready(Ok(()));
                        }
                        Ok(Err(e)) => return Poll::Ready(Err(e)),
                        // Readiness was stale; `try_io` cleared it, so poll again.
                        Err(_would_block) => continue,
                    }
                },
            }
        }
    }

    impl AsyncWrite for StdioWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                StdioWriter::Blocking(stdout) => Pin::new(stdout).poll_write(cx, buf),
                StdioWriter::Fd(io) => loop {
                    let mut guard = ready!(io.inner.poll_write_ready(cx))?;
                    let result = guard.try_io(|inner| {
                        let fd = inner.get_ref().as_raw_fd();
                        // SAFETY: `buf` is a valid, readable slice.
                        retry_interrupted(|| unsafe {
                            libc::write(fd, buf.as_ptr().cast(), buf.len())
                        })
                    });
                    match result {
                        Ok(result) => return Poll::Ready(result),
                        Err(_would_block) => continue,
                    }
                },
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                StdioWriter::Blocking(stdout) => Pin::new(stdout).poll_flush(cx),
                // Writes go straight to the descriptor; nothing is buffered.
                StdioWriter::Fd(_) => Poll::Ready(Ok(())),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                StdioWriter::Blocking(stdout) => Pin::new(stdout).poll_shutdown(cx),
                // Closing is left to drop, as it is for stdout.
                StdioWriter::Fd(_) => Poll::Ready(Ok(())),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn kind_classifies_pipes_and_sockets_but_not_other_files() {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let pipe = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            assert!(kind(pipe[0].as_raw_fd()) == Some(Kind::Fifo));

            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
                0
            );
            let socket = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            assert!(kind(socket[0].as_raw_fd()) == Some(Kind::Socket));

            let null = std::fs::File::open("/dev/null").unwrap();
            assert!(kind(null.as_raw_fd()).is_none());
            assert!(kind(-1).is_none());
        }

        fn flags(fd: RawFd) -> libc::c_int {
            unsafe { libc::fcntl(fd, libc::F_GETFL) }
        }

        #[test]
        fn shared_sets_nonblocking_and_restores_it_on_drop() {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let [read, write] = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            assert_eq!(flags(read.as_raw_fd()) & libc::O_NONBLOCK, 0);

            let shared = share(read.as_raw_fd()).unwrap();
            assert_ne!(flags(read.as_raw_fd()) & libc::O_NONBLOCK, 0);
            drop(shared);
            assert_eq!(flags(read.as_raw_fd()) & libc::O_NONBLOCK, 0);
            drop(write);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn private_reopen_leaves_the_original_description_alone() {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let [read, write] = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });

            let reopened =
                private_reopen(read.as_raw_fd(), Kind::Fifo, Interest::READABLE).unwrap();
            assert_ne!(flags(reopened.as_raw_fd()) & libc::O_NONBLOCK, 0);
            assert_eq!(flags(read.as_raw_fd()) & libc::O_NONBLOCK, 0);
            drop(write);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn private_reopen_refuses_sockets() {
            let mut fds = [0; 2];
            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
                0
            );
            let [a, _b] = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            assert!(private_reopen(a.as_raw_fd(), Kind::Socket, Interest::READABLE).is_none());
        }
    }
}
