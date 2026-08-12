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
}
