//! Newline-delimited frame reading over bytes.
//!
//! Both ends of a stdio connection read the same wire format, and both have
//! to survive a peer that puts bytes on it that will not decode. Framing
//! happens over bytes rather than over decoded text so that a bad byte costs
//! one frame instead of the connection; see [`InputFrame`] for why.
//!
//! What happens to a frame that does not decode is the caller's decision,
//! because the two ends have different answers available to them. A server
//! answers with a JSON-RPC parse error (`-32700`) and keeps serving. A client
//! has nobody to answer, so it logs the discard and reads the next frame.

use std::io::BufRead;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{Error, Result};

/// Default maximum size of one newline-delimited frame, in bytes (4 MiB).
///
/// Matches `DEFAULT_MAX_BODY_SIZE` in `transport::http`, the equivalent cap
/// on one JSON-RPC message received over HTTP: both bound a single message
/// rather than the whole connection, and there is no reason a stdio or
/// child-process peer should be allowed a larger single frame than an HTTP
/// client's POST body.
pub(crate) const DEFAULT_MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

/// One newline-delimited frame read from an input stream.
///
/// Framing happens over bytes, not over decoded text. `0x0A` cannot appear
/// inside a multi-byte UTF-8 sequence, so the newline that ends a frame is
/// unambiguous even when the bytes around it are not decodable, and input
/// the decoder rejects costs exactly the frame it landed in.
pub(crate) enum InputFrame {
    /// A frame that decoded as UTF-8, taking the ordinary path from here.
    Line(String),
    /// A frame that is not valid UTF-8.
    ///
    /// The frame is discarded rather than repaired. A lossy decode would
    /// hand the JSON parser text the peer never sent, so a stray byte inside
    /// a string argument would be served as a request with silently altered
    /// content. Discarding gives the peer the answer malformed JSON already
    /// gets: the frame is lost and the loop keeps running (#797, #1271,
    /// #1296).
    Undecodable,
}

/// Strip the delimiter from one raw frame and decode it.
pub(crate) fn decode_input_frame(mut raw: Vec<u8>) -> InputFrame {
    if raw.last() == Some(&b'\n') {
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
    }
    match String::from_utf8(raw) {
        Ok(line) => InputFrame::Line(line),
        Err(_) => InputFrame::Undecodable,
    }
}

/// Newline-delimited frame reader over an async byte stream.
///
/// This exists instead of [`tokio::io::Lines`] because `Lines` decodes before
/// it frames: one byte that is not valid UTF-8 surfaces as `InvalidData`, and
/// the read loops turn that into a transport error that ends the session for
/// every other request on the connection (#1271, #1296).
///
/// Cancellation behaves the way `Lines::next_line` does, which the `select!`
/// loops on both ends depend on: bytes read before a lost race stay in `buf`,
/// and the next call continues the same frame rather than starting a new one.
pub(crate) struct FrameReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
    /// Bound on the bytes buffered for one frame; see [`Self::next_frame`].
    max_frame_len: usize,
}

impl<R> FrameReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    /// Create a reader bounded by [`DEFAULT_MAX_FRAME_LEN`].
    pub(crate) fn new(reader: R) -> Self {
        Self::with_max_len(reader, DEFAULT_MAX_FRAME_LEN)
    }

    /// Create a reader that rejects a frame buffering more than
    /// `max_frame_len` bytes without a delimiter.
    pub(crate) fn with_max_len(reader: R, max_frame_len: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            buf: Vec::new(),
            max_frame_len,
        }
    }

    /// Change the frame-length bound after construction.
    ///
    /// Lets a caller that already holds a `FrameReader` (built from an
    /// already-open stream, e.g. a spawned child's stdout) apply a builder
    /// method for the limit without reconstructing the reader.
    pub(crate) fn set_max_len(&mut self, max_frame_len: usize) {
        self.max_frame_len = max_frame_len;
    }

    /// Read the next frame, or `None` once the input is exhausted.
    ///
    /// Reads in whatever chunks the underlying reader fills its buffer with,
    /// rather than calling `read_until` directly, so a peer that never sends
    /// a delimiter cannot grow `buf` past `max_frame_len`: each chunk is
    /// checked against the bound before it is appended, and a frame that
    /// would cross it fails with [`Error::FrameTooLarge`] instead of
    /// buffering further. `buf` is cleared on that error, so a caller that
    /// keeps reading (this type's callers do not; the error ends the
    /// connection for that peer) starts the next frame clean rather than
    /// resuming mid-oversized-frame.
    ///
    /// Cancel-safe in the sense the type documents: this is the same
    /// fill-then-consume loop `read_until` runs internally, so bytes read
    /// before a lost race stay in `buf` exactly as they did before.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<InputFrame>> {
        loop {
            let filled = self
                .reader
                .fill_buf()
                .await
                .map_err(|e| Error::Transport(format!("Failed to read input frame: {}", e)))?;
            // Nothing read and nothing held back: end of input. Bytes still
            // held are a final frame that arrived without its delimiter.
            if filled.is_empty() {
                break;
            }
            let (take, found_newline) = match filled.iter().position(|&b| b == b'\n') {
                Some(pos) => (pos + 1, true),
                None => (filled.len(), false),
            };
            if self.buf.len() + take > self.max_frame_len {
                let size = self.buf.len() + take;
                self.reader.consume(take);
                self.buf.clear();
                return Err(Error::FrameTooLarge {
                    size,
                    limit: self.max_frame_len,
                });
            }
            self.buf.extend_from_slice(&filled[..take]);
            self.reader.consume(take);
            if found_newline {
                break;
            }
        }
        if self.buf.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_input_frame(std::mem::take(&mut self.buf))))
    }
}

/// Blocking counterpart of [`FrameReader::next_frame`], for the sync transport.
///
/// Bounded the same way: chunks are checked against `max_frame_len` before
/// they are appended, so a peer that never sends a delimiter cannot grow the
/// frame buffer past the limit.
pub(crate) fn read_frame_blocking<R: BufRead>(
    reader: &mut R,
    max_frame_len: usize,
) -> Result<Option<InputFrame>> {
    let mut raw = Vec::new();
    loop {
        let filled = reader
            .fill_buf()
            .map_err(|e| Error::Transport(format!("Failed to read input frame: {}", e)))?;
        if filled.is_empty() {
            break;
        }
        let (take, found_newline) = match filled.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (filled.len(), false),
        };
        if raw.len() + take > max_frame_len {
            let size = raw.len() + take;
            reader.consume(take);
            return Err(Error::FrameTooLarge {
                size,
                limit: max_frame_len,
            });
        }
        raw.extend_from_slice(&filled[..take]);
        reader.consume(take);
        if found_newline {
            break;
        }
    }
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode_input_frame(raw)))
}

/// Strip an optional UTF-8 BOM, then trim whitespace.
///
/// Windows tools sometimes prefix the first stdout line with a UTF-8 BOM
/// (`\u{feff}`). Without stripping it, the JSON parser sees an unexpected
/// character at offset 0 and rejects the whole message.
///
/// `trim` alone will not do: U+FEFF has not carried the Unicode
/// `White_Space` property since 4.0.1. Both ends of a connection read frames
/// a peer wrote, so both call this rather than keeping a copy each (#1303).
pub(crate) fn clean_input_line(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line).trim()
}

/// Which of the three JSON-RPC frame shapes a decoded value is.
///
/// A batch (JSON array) is always [`FrameClass::Request`] -- neither a
/// notification nor a response can be a top-level array, so an array skips
/// straight to "otherwise".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameClass {
    /// No `id` field: nothing to answer, so nothing is sent back.
    Notification,
    /// An `id`, no `method`, and a `result` or `error`: a reply arriving on
    /// a channel that never sent the matching request.
    Response,
    /// Everything else, single or batched. Includes the malformed case of an
    /// `id` with no `method` and neither `result` nor `error` -- that is not
    /// a valid request, but classifying it as one is what gets it a `-32700`
    /// reply instead of being silently dropped as if it were a response.
    Request,
}

/// Classify a decoded JSON-RPC value by shape alone, before any schema
/// validation runs.
///
/// Order is pinned and matters: notification, then response, then request.
/// Classifying before validating means a malformed notification cannot come
/// back as an error the client has no id to correlate (#1272). Checking
/// response before falling through to request means a reply frame is
/// ignored rather than answered with a parse error naming an internal type.
///
/// This is the extraction of the test `process_line` in `transport::stdio`
/// used to hand-write, now shared by every receive path that needs the full
/// three-way split. A path that only needs the response test in isolation
/// (for example one that establishes "has an id" some other way) should
/// call [`is_response_frame`] directly instead -- `classify_frame` folds the
/// id check into the ordering, so its [`FrameClass::Response`] arm is only
/// reachable once an id is already known to be present.
pub(crate) fn classify_frame(value: &serde_json::Value) -> FrameClass {
    if !value.is_array() && value.get("id").is_none() {
        return FrameClass::Notification;
    }
    if is_response_frame(value) {
        return FrameClass::Response;
    }
    FrameClass::Request
}

/// The response half of [`classify_frame`]'s test, callable on its own.
///
/// A response carries no `method` and one of `result` or `error`. All three
/// conditions here matter together: dropping the `result`/`error` check
/// would misclassify any method-less frame as a response and silently
/// discard it, when a method-less frame with neither is actually an invalid
/// request that must still be refused with an error, not dropped.
///
/// This test alone says nothing about `id` -- callers that need "has an id
/// AND looks like a response" (as opposed to "would be classified `Response`
/// by [`classify_frame`]'s pinned ordering") check `id` themselves alongside
/// this.
pub(crate) fn is_response_frame(value: &serde_json::Value) -> bool {
    !value.is_array()
        && value.get("method").is_none()
        && (value.get("result").is_some() || value.get("error").is_some())
}

/// The id an error response should carry back, read straight off the frame.
///
/// A receive loop that rejects a frame before typing it still has the id
/// sitting in front of it, and JSON-RPC 2.0 section 5 requires an error
/// response to echo the request's id. Answering `null` anyway leaves a client
/// with more than one request in flight unable to tell which one failed
/// (#1372).
///
/// `None` for the cases where there genuinely is no single id to answer with:
/// a batch, a notification, and an `id` that is neither of the two JSON-RPC
/// id types. A batch is deliberately not resolved to its first element's id,
/// because answering one member's id for a whole-batch rejection would be a
/// worse lie than answering none.
pub(crate) fn correlating_id(value: &serde_json::Value) -> Option<crate::protocol::RequestId> {
    match value.get("id")? {
        serde_json::Value::String(id) => Some(crate::protocol::RequestId::String(id.clone())),
        serde_json::Value::Number(id) => id.as_i64().map(crate::protocol::RequestId::Number),
        _ => None,
    }
}

/// The id to answer `error` with, for a frame rejected before it was typed.
///
/// Section 5 permits a null id when "there was an error in detecting the id
/// in the Request object (e.g. Parse error/Invalid Request)". The two halves
/// of that sentence pull in different directions here, and the split falls on
/// whether the envelope itself is well formed:
///
/// * `-32600 Invalid Request` says the envelope is not a request at all. It is
///   named in the specification's own parenthetical, and this crate answers
///   every malformed envelope shape uniformly with a null id whether or not an
///   `id` happens to be readable out of it. Nothing is correlated to a frame
///   that was never a valid request.
/// * Anything else means the envelope parsed as a request and then failed
///   semantic validation, `-32602` invalid params and `-32022` unsupported
///   protocol version among them. The id was detected perfectly well, so
///   section 5's first sentence applies and it has to be echoed (#1372).
pub(crate) fn error_response_id(
    value: &serde_json::Value,
    error: &crate::error::JsonRpcError,
) -> Option<crate::protocol::RequestId> {
    const INVALID_REQUEST: i32 = -32600;
    if error.code == INVALID_REQUEST {
        return None;
    }
    correlating_id(value)
}

#[cfg(test)]
mod correlating_id_tests {
    use super::*;
    use crate::protocol::RequestId;

    #[test]
    fn both_json_rpc_id_types_are_read_off_the_frame() {
        assert_eq!(
            correlating_id(&serde_json::json!({"id": 42, "method": "tools/list"})),
            Some(RequestId::Number(42))
        );
        assert_eq!(
            correlating_id(&serde_json::json!({"id": "req-1", "method": "tools/list"})),
            Some(RequestId::String("req-1".into()))
        );
    }

    /// A batch rejection covers every member, so borrowing the first one's id
    /// would answer a request that may well have been fine.
    #[test]
    fn a_batch_has_no_single_id_to_answer_with() {
        assert_eq!(
            correlating_id(&serde_json::json!([{"id": 1, "method": "tools/list"}])),
            None
        );
    }

    #[test]
    fn a_notification_has_no_id() {
        assert_eq!(
            correlating_id(&serde_json::json!({"method": "notifications/initialized"})),
            None
        );
    }

    /// An `id` the JSON-RPC grammar does not allow is not an id. Answering
    /// with `null` is then correct rather than a lost correlation.
    #[test]
    fn an_id_of_the_wrong_type_is_not_an_id() {
        for id in [
            serde_json::json!(null),
            serde_json::json!({"nested": 1}),
            serde_json::json!([1]),
            serde_json::json!(true),
        ] {
            assert_eq!(
                correlating_id(&serde_json::json!({"id": id, "method": "tools/list"})),
                None,
                "{id} is not a JSON-RPC id"
            );
        }
    }

    /// Fractional numbers are not JSON-RPC ids either, and must not silently
    /// truncate to a different id than the client sent.
    #[test]
    fn a_fractional_id_does_not_truncate() {
        assert_eq!(
            correlating_id(&serde_json::json!({"id": 1.5, "method": "tools/list"})),
            None
        );
    }

    /// The split the specification's section 5 parenthetical draws. An
    /// envelope that was never a valid request correlates to nothing, even
    /// with a perfectly readable id on it.
    #[test]
    fn an_invalid_request_answers_with_a_null_id() {
        let frame = serde_json::json!({"jsonrpc": "2.0", "id": 6});
        assert_eq!(
            error_response_id(
                &frame,
                &crate::error::JsonRpcError::invalid_request("no method")
            ),
            None
        );
    }

    /// A valid envelope that failed semantic validation is the other side of
    /// it: the id was detected, so it has to come back.
    #[test]
    fn a_semantic_failure_answers_with_the_id() {
        let frame = serde_json::json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"});
        for error in [
            crate::error::JsonRpcError::invalid_params("bad _meta"),
            crate::error::JsonRpcError::internal_error("boom"),
        ] {
            assert_eq!(
                error_response_id(&frame, &error),
                Some(RequestId::Number(6)),
                "code {} should correlate",
                error.code
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert one frame decoded to exactly `expected`.
    fn assert_line(frame: Option<InputFrame>, expected: &str) {
        match frame {
            Some(InputFrame::Line(line)) => assert_eq!(line, expected),
            Some(InputFrame::Undecodable) => panic!("{expected:?} must decode"),
            None => panic!("expected a frame, got end of input"),
        }
    }

    /// Extract the `(size, limit)` pair from a [`Error::FrameTooLarge`]
    /// result, panicking otherwise.
    ///
    /// `InputFrame` deliberately has no `Debug` impl (it can hold a full
    /// frame's contents), so the bounded-length tests use this instead of
    /// `Result::unwrap_err`, which requires one.
    fn expect_frame_too_large(result: Result<Option<InputFrame>>) -> (usize, usize) {
        match result {
            Err(Error::FrameTooLarge { size, limit }) => (size, limit),
            Err(other) => panic!("expected FrameTooLarge, got a different error: {other}"),
            Ok(Some(InputFrame::Line(_))) => panic!("expected FrameTooLarge, got a line"),
            Ok(Some(InputFrame::Undecodable)) => {
                panic!("expected FrameTooLarge, got an undecodable frame")
            }
            Ok(None) => panic!("expected FrameTooLarge, got end of input"),
        }
    }

    /// Assert one frame was rejected by the decoder.
    fn assert_undecodable(frame: Option<InputFrame>) {
        assert!(
            matches!(frame, Some(InputFrame::Undecodable)),
            "expected an undecodable frame"
        );
    }

    #[test]
    fn decoding_strips_the_delimiter_in_both_line_endings() {
        assert_line(Some(decode_input_frame(b"{}\n".to_vec())), "{}");
        assert_line(Some(decode_input_frame(b"{}\r\n".to_vec())), "{}");
        // A frame that arrived without its delimiter, at end of input.
        assert_line(Some(decode_input_frame(b"{}".to_vec())), "{}");
    }

    #[test]
    fn decoding_rejects_bytes_rather_than_repairing_them() {
        // A lossy decode would turn this into a frame the peer never sent.
        assert_undecodable(Some(decode_input_frame(vec![0xff, 0xfe, b'\n'])));
    }

    #[tokio::test]
    async fn a_bad_frame_costs_only_itself() {
        let input: &[u8] = b"\xff\xfe\n{\"id\":1}\n";
        let mut frames = FrameReader::new(input);

        assert_undecodable(frames.next_frame().await.unwrap());
        assert_line(frames.next_frame().await.unwrap(), "{\"id\":1}");
        assert!(
            frames.next_frame().await.unwrap().is_none(),
            "end of input must be reported once the frames are consumed"
        );
    }

    #[test]
    fn the_blocking_reader_treats_a_bad_frame_the_same_way() {
        let mut input: &[u8] = b"\xff\xfe\n{\"id\":1}\n";

        assert_undecodable(read_frame_blocking(&mut input, DEFAULT_MAX_FRAME_LEN).unwrap());
        assert_line(
            read_frame_blocking(&mut input, DEFAULT_MAX_FRAME_LEN).unwrap(),
            "{\"id\":1}",
        );
        assert!(
            read_frame_blocking(&mut input, DEFAULT_MAX_FRAME_LEN)
                .unwrap()
                .is_none()
        );
    }

    /// The blocking reader is bounded exactly like the async one: a frame at
    /// the limit parses, one byte over fails with [`Error::FrameTooLarge`].
    #[test]
    fn the_blocking_reader_is_bounded_the_same_way() {
        let limit = 16;

        let mut at_limit: &[u8] = b"123456789012345\n"; // 15 bytes + \n = 16
        assert_line(
            read_frame_blocking(&mut at_limit, limit).unwrap(),
            "123456789012345",
        );

        let mut over_limit: &[u8] = b"1234567890123456\n"; // 16 bytes + \n = 17
        assert_eq!(
            expect_frame_too_large(read_frame_blocking(&mut over_limit, limit)),
            (17, 16)
        );
    }

    /// The `select!` loops on both ends poll `next_frame` against other
    /// branches, so a frame that loses the race has to survive to the next
    /// call rather than being split in two.
    #[tokio::test]
    async fn a_partial_frame_survives_a_cancelled_read() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let mut frames = FrameReader::new(reader);
        let frame = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;

        tokio::io::AsyncWriteExt::write_all(&mut writer, &frame.as_bytes()[..10])
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), frames.next_frame())
                .await
                .is_err(),
            "a partial frame must remain pending until its newline arrives"
        );

        tokio::io::AsyncWriteExt::write_all(&mut writer, &frame.as_bytes()[10..])
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"\n")
            .await
            .unwrap();
        assert_line(frames.next_frame().await.unwrap(), frame);
    }

    // =========================================================================
    // Bounded frame length tests (#1470)
    // =========================================================================

    /// A frame that never sends a delimiter and exceeds the configured limit
    /// fails instead of buffering forever. This is the read-loop counterpart
    /// of [`crate::client::http`]'s `SseEventTooLarge` handling.
    #[tokio::test]
    async fn an_oversized_frame_without_a_delimiter_is_rejected() {
        let limit = 16;
        let input: &[u8] = b"this line is much longer than the sixteen byte limit\n";
        let mut frames = FrameReader::with_max_len(input, limit);

        let (size, reported) = expect_frame_too_large(frames.next_frame().await);
        assert_eq!(reported, limit);
        assert!(size > limit, "the error must report the overshoot");
    }

    /// One byte over the limit is enough to fail; the delimiter that would
    /// have completed the frame never gets a chance to arrive.
    #[tokio::test]
    async fn a_frame_one_byte_over_the_limit_is_rejected() {
        let limit = 16;
        let input: &[u8] = b"1234567890123456\n"; // 16 bytes of payload + \n = 17
        let mut frames = FrameReader::with_max_len(input, limit);

        assert_eq!(expect_frame_too_large(frames.next_frame().await), (17, 16));
    }

    /// The boundary case: a frame whose bytes (payload plus delimiter) total
    /// exactly the configured limit must still parse.
    #[tokio::test]
    async fn a_frame_exactly_at_the_limit_still_parses() {
        let limit = 16;
        let input: &[u8] = b"123456789012345\n"; // 15 bytes of payload + \n = 16
        let mut frames = FrameReader::with_max_len(input, limit);

        assert_line(frames.next_frame().await.unwrap(), "123456789012345");
    }

    /// A final frame within the limit that arrives without its delimiter at
    /// EOF must still parse, exactly as it did before this type bounded its
    /// buffer: the limit only rejects a frame that grows past it, not one
    /// that simply never saw a trailing newline.
    #[tokio::test]
    async fn a_final_frame_without_a_delimiter_within_the_limit_still_parses_at_eof() {
        let limit = 16;
        let input: &[u8] = b"{}";
        let mut frames = FrameReader::with_max_len(input, limit);

        assert_line(frames.next_frame().await.unwrap(), "{}");
        assert!(frames.next_frame().await.unwrap().is_none());
    }

    /// The bound is enforced per chunk as bytes arrive, not only once the
    /// whole oversized frame has already been buffered. A duplex stream with
    /// a smaller capacity than the payload forces the writer to pace itself,
    /// so `next_frame` sees the delimiter-less payload as several chunks; the
    /// buffer must never hold more than `limit` bytes before the read fails.
    #[tokio::test]
    async fn the_limit_is_enforced_incrementally_and_the_buffer_never_exceeds_it() {
        let limit = 8;
        let (mut writer, reader) = tokio::io::duplex(4);
        let mut frames = FrameReader::with_max_len(reader, limit);

        let write = tokio::spawn(async move {
            // 9 bytes, no delimiter: one more than `limit`.
            let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, b"123456789").await;
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), frames.next_frame())
            .await
            .expect("an oversized frame must fail rather than hang");
        let (size, reported) = expect_frame_too_large(result);
        assert_eq!(reported, limit);
        // The buffer is checked against the limit before each chunk is
        // appended, so it can overshoot by at most one chunk (bounded by
        // the duplex's 4-byte capacity here), never by the whole 9-byte
        // payload.
        assert!(
            size <= limit + 4,
            "buffer grew past one chunk beyond the limit: {size}"
        );

        write.abort();
    }

    /// A per-instance limit configured via [`FrameReader::with_max_len`]
    /// applies independently of the default: a frame that fits comfortably
    /// under [`DEFAULT_MAX_FRAME_LEN`] can still be rejected under a smaller
    /// configured one, and vice versa.
    #[tokio::test]
    async fn the_limit_is_configurable_per_instance() {
        let payload = b"{\"jsonrpc\":\"2.0\"}\n";

        let mut generous = FrameReader::with_max_len(&payload[..], DEFAULT_MAX_FRAME_LEN);
        assert_line(generous.next_frame().await.unwrap(), r#"{"jsonrpc":"2.0"}"#);

        let mut strict = FrameReader::with_max_len(&payload[..], 4);
        let (_, reported) = expect_frame_too_large(strict.next_frame().await);
        assert_eq!(reported, 4);
    }

    // =========================================================================
    // clean_input_line tests
    // =========================================================================

    #[test]
    fn test_clean_input_line_no_bom() {
        assert_eq!(
            clean_input_line(r#"{"jsonrpc":"2.0"}"#),
            r#"{"jsonrpc":"2.0"}"#
        );
    }

    #[test]
    fn test_clean_input_line_strips_leading_bom() {
        let with_bom = "\u{feff}{\"jsonrpc\":\"2.0\"}";
        assert_eq!(clean_input_line(with_bom), r#"{"jsonrpc":"2.0"}"#);
    }

    #[test]
    fn test_clean_input_line_strips_bom_then_trims() {
        // BOM, then whitespace, then content, then trailing newline.
        let input = "\u{feff}   {\"id\":1}\n";
        assert_eq!(clean_input_line(input), r#"{"id":1}"#);
    }

    #[test]
    fn test_clean_input_line_does_not_strip_internal_bom() {
        // Only a *leading* BOM is stripped; one inside the payload stays.
        let input = "{\"text\":\"hi\u{feff}there\"}";
        assert_eq!(clean_input_line(input), input);
    }

    #[test]
    fn test_clean_input_line_empty() {
        assert_eq!(clean_input_line(""), "");
        assert_eq!(clean_input_line("\u{feff}"), "");
        assert_eq!(clean_input_line("   \n\t"), "");
    }

    // =========================================================================
    // classify_frame / is_response_frame tests
    // =========================================================================

    #[test]
    fn classify_frame_notification_has_no_id() {
        let value = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(classify_frame(&value), FrameClass::Notification);
    }

    #[test]
    fn classify_frame_response_has_id_no_method_and_result() {
        let value = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert_eq!(classify_frame(&value), FrameClass::Response);
    }

    #[test]
    fn classify_frame_response_has_id_no_method_and_error() {
        let value =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "x"}});
        assert_eq!(classify_frame(&value), FrameClass::Response);
    }

    #[test]
    fn classify_frame_request_has_id_and_method() {
        let value = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        assert_eq!(classify_frame(&value), FrameClass::Request);
    }

    /// A batch is always `Request`, whatever shape its members are -- a
    /// top-level array can never be a notification or a response.
    #[test]
    fn classify_frame_batch_array_is_always_request() {
        let value = serde_json::json!([
            {"jsonrpc": "2.0", "id": 1, "method": "a"},
            {"jsonrpc": "2.0", "method": "b"},
        ]);
        assert_eq!(classify_frame(&value), FrameClass::Request);
    }

    /// The key must be *absent* to count as no id. `"id": null` still has
    /// the key, so `get("id")` returns `Some(Null)`, not `None`.
    #[test]
    fn classify_frame_id_present_but_null_is_not_a_notification() {
        let value = serde_json::json!({"jsonrpc": "2.0", "id": null, "method": "tools/list"});
        assert_eq!(classify_frame(&value), FrameClass::Request);
    }

    /// An id-bearing, null-id response is still a response: the id key is
    /// present, there is no method, and `result` is present.
    #[test]
    fn classify_frame_id_present_but_null_can_still_be_a_response() {
        let value = serde_json::json!({"jsonrpc": "2.0", "id": null, "result": {}});
        assert_eq!(classify_frame(&value), FrameClass::Response);
    }

    /// An id with no method and neither `result` nor `error` is not a valid
    /// request, but it must still classify as `Request` so it gets refused
    /// with a `-32700` reply rather than being silently dropped as if it
    /// were a response.
    #[test]
    fn classify_frame_id_no_method_no_result_or_error_is_a_request_not_a_response() {
        let value = serde_json::json!({"jsonrpc": "2.0", "id": 1});
        assert_eq!(classify_frame(&value), FrameClass::Request);
    }

    #[test]
    fn is_response_frame_matches_the_response_shape() {
        assert!(is_response_frame(
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}})
        ));
        assert!(is_response_frame(
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1}})
        ));
    }

    #[test]
    fn is_response_frame_rejects_a_request_shape() {
        assert!(!is_response_frame(
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        ));
        assert!(!is_response_frame(
            &serde_json::json!({"jsonrpc": "2.0", "id": 1})
        ));
    }

    /// `is_response_frame` alone does not consider `id`: it is meant to be
    /// combined with whatever id check a caller already has, or called
    /// after a caller has already established (as `classify_frame` does)
    /// that the frame is not a notification.
    #[test]
    fn is_response_frame_does_not_itself_require_an_id() {
        assert!(is_response_frame(&serde_json::json!({"result": {}})));
    }

    #[test]
    fn is_response_frame_rejects_a_batch_array() {
        assert!(!is_response_frame(&serde_json::json!([
            {"result": {}},
        ])));
    }
}
