//! Opaque cursors and the page slice behind every `*/list` response.
//!
//! The cursor is deliberately opaque to a client: it encodes an offset, and
//! nothing in the protocol promises that it always will.

use super::*;

/// Decode a pagination cursor into an offset.
///
/// Returns `Err` if the cursor is malformed.
pub(super) fn decode_cursor(cursor: &str) -> Result<usize> {
    let bytes = BASE64
        .decode(cursor)
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))?;
    let s = String::from_utf8(bytes)
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))?;
    s.parse::<usize>()
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))
}

/// Encode an offset into an opaque pagination cursor.
pub(super) fn encode_cursor(offset: usize) -> String {
    BASE64.encode(offset.to_string())
}
/// Apply pagination to a collected list of items.
///
/// Returns the page of items and an optional `next_cursor`.
pub(super) fn paginate<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    page_size: Option<usize>,
) -> Result<(Vec<T>, Option<String>)> {
    let Some(page_size) = page_size else {
        return Ok((items, None));
    };

    let offset = match cursor {
        Some(c) => decode_cursor(c)?,
        None => 0,
    };

    if offset >= items.len() {
        return Ok((Vec::new(), None));
    }

    let end = (offset + page_size).min(items.len());
    let next_cursor = if end < items.len() {
        Some(encode_cursor(end))
    } else {
        None
    };

    let mut items = items;
    let page = items.drain(offset..end).collect();
    Ok((page, next_cursor))
}
