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
}

impl<R> FrameReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buf: Vec::new(),
        }
    }

    /// Read the next frame, or `None` once the input is exhausted.
    ///
    /// Cancel-safe in the sense the type documents.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<InputFrame>> {
        let read = self
            .reader
            .read_until(b'\n', &mut self.buf)
            .await
            .map_err(|e| Error::Transport(format!("Failed to read input frame: {}", e)))?;
        // Nothing read and nothing held back: end of input. Bytes still held
        // are a final frame that arrived without its delimiter.
        if read == 0 && self.buf.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_input_frame(std::mem::take(&mut self.buf))))
    }
}

/// Blocking counterpart of [`FrameReader::next_frame`], for the sync transport.
pub(crate) fn read_frame_blocking<R: BufRead>(reader: &mut R) -> Result<Option<InputFrame>> {
    let mut raw = Vec::new();
    let read = reader
        .read_until(b'\n', &mut raw)
        .map_err(|e| Error::Transport(format!("Failed to read input frame: {}", e)))?;
    if read == 0 {
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

        assert_undecodable(read_frame_blocking(&mut input).unwrap());
        assert_line(read_frame_blocking(&mut input).unwrap(), "{\"id\":1}");
        assert!(read_frame_blocking(&mut input).unwrap().is_none());
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
