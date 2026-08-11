//! Graceful shutdown for the transports that own their listener (#1285).
//!
//! `HttpTransport::serve`, `WebSocketTransport::serve`, and
//! `UnixSocketTransport::serve` each own the `axum::serve` call, which is why
//! none of them could be stopped: the caller never holds the server. Each one
//! now delegates here, generic over the listener so the TCP and Unix-domain
//! paths cannot drift apart.

use std::future::{Future, IntoFuture};
use std::time::Duration;

use crate::error::Error;

/// Serve `router` on `listener`, stop accepting once `signal` resolves, and
/// return when the connections still open have finished.
///
/// `drain_timeout` bounds that last step. `None` waits for every open
/// connection, which is what `axum::serve(..).with_graceful_shutdown(..)`
/// does on its own. That wait has no natural end for a transport that holds
/// connections open by design: an SSE notification stream is an in-flight
/// response until its client hangs up.
///
/// `Some(limit)` stops waiting after `limit` and returns. It does not close
/// the connections that are still open: axum serves each one on its own
/// task, so nothing here can reach them. What it returns is control, which
/// is what the caller was blocked on. The listener is already gone either
/// way, because axum drops it as soon as the signal fires.
pub(crate) async fn serve_with_shutdown<L>(
    listener: L,
    router: axum::Router,
    signal: impl Future<Output = ()> + Send + 'static,
    drain_timeout: Option<Duration>,
) -> crate::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    // Fires when the caller's signal does, so the deadline below covers only
    // the drain rather than the whole life of the server.
    let (draining_tx, draining_rx) = tokio::sync::oneshot::channel::<()>();

    let serve = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            signal.await;
            let _ = draining_tx.send(());
        })
        .into_future();

    let result = match drain_timeout {
        None => serve.await,
        Some(limit) => {
            let deadline = async move {
                if draining_rx.await.is_err() {
                    // The shutdown future was dropped along with the server,
                    // so there is no drain left to bound.
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(limit).await;
            };

            tokio::select! {
                result = serve => result,
                () = deadline => {
                    tracing::warn!(
                        timeout_ms = limit.as_millis() as u64,
                        "graceful shutdown timed out; returning with connections still open"
                    );
                    Ok(())
                }
            }
        }
    };

    result.map_err(|e| Error::Transport(format!("Server error: {}", e)))
}
