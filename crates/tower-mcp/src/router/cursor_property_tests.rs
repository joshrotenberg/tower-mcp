//! Property tests for the opaque pagination cursor encoding.

use super::{decode_cursor, encode_cursor};
use proptest::prelude::*;

fn arb_cursor_text() -> BoxedStrategy<String> {
    prop_oneof![
        8 => prop::collection::vec(any::<char>(), 0..512)
            .prop_map(|chars| chars.into_iter().collect()),
        1 => Just("\0\r\n\t\u{001b}\u{007f}".repeat(64)),
        1 => Just("A".repeat(16 * 1024)),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A cursor round-trips: decode(encode(n)) == n.
    #[test]
    fn cursor_round_trips(offset in any::<usize>()) {
        prop_assert_eq!(decode_cursor(&encode_cursor(offset)).unwrap(), offset);
    }

    /// Decoding arbitrary client input never panics; it is Ok or a clean Err.
    #[test]
    fn decode_cursor_never_panics(s in arb_cursor_text()) {
        let _ = decode_cursor(&s);
    }
}
