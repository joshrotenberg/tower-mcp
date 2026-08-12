//! ASCII case-insensitive matching for wire tokens.
//!
//! HTTP defines a number of tokens as case-insensitive: authentication scheme
//! names (RFC 9110 section 11.1) and cache directive names (RFC 9111 section
//! 5.2) among them. A peer is free to send `BEARER` or `Max-Age`, and a server
//! that compares by exact bytes silently fails to recognize either.
//!
//! This module exists because that comparison was written by hand three
//! separate times before anyone shared it: `auth.rs` (#1276, fixed in #1289),
//! `oauth/middleware.rs` (#1337, fixed in #1342), and `oauth/token.rs`
//! (#1358). Each was found on its own, after the previous one had been fixed.

/// `s` with `prefix` removed, comparing ASCII case-insensitively.
///
/// `None` when `s` does not start with `prefix`. Only ASCII case is folded,
/// which is what the specifications above call for; a token is ASCII by
/// definition, and folding Unicode case here would accept input the grammar
/// does not.
///
/// This is a prefix match and nothing more. A caller that needs the token to
/// end at a delimiter, the way an auth scheme has to be followed by a space so
/// that `Bearerish` does not match `Bearer`, has to check the remainder
/// itself; [`crate::auth::strip_scheme`] is that check on top of this.
pub(crate) fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` rather than slicing: `prefix.len()` can land inside a multi-byte
    // character, and a header is arbitrary peer input.
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_matches_in_any_ascii_case() {
        for prefix in ["max-age=", "MAX-AGE=", "Max-Age=", "mAx-AgE="] {
            assert_eq!(
                strip_prefix_ignore_ascii_case(&format!("{prefix}300"), "max-age="),
                Some("300"),
                "{prefix} should match"
            );
        }
    }

    #[test]
    fn a_different_prefix_does_not_match() {
        assert_eq!(strip_prefix_ignore_ascii_case("no-cache", "max-age="), None);
        assert_eq!(strip_prefix_ignore_ascii_case("", "Bearer"), None);
    }

    /// The match is a prefix match, so the caller is the one that decides
    /// whether the token ended where it should have.
    #[test]
    fn the_remainder_is_returned_unexamined() {
        assert_eq!(
            strip_prefix_ignore_ascii_case("Bearerish", "Bearer"),
            Some("ish")
        );
        assert_eq!(strip_prefix_ignore_ascii_case("Bearer", "Bearer"), Some(""));
    }

    /// A header is arbitrary peer input, so the prefix length can fall inside
    /// a multi-byte character. That is a non-match, not a panic.
    #[test]
    fn a_prefix_ending_inside_a_character_does_not_match() {
        assert_eq!(strip_prefix_ignore_ascii_case("é", "ma"), None);
        assert_eq!(strip_prefix_ignore_ascii_case("mé", "max"), None);
    }

    /// Only ASCII case folds. `İ` lowercases to `i` under Unicode rules, and
    /// accepting it would take input the token grammar does not allow.
    #[test]
    fn non_ascii_case_does_not_fold() {
        assert_eq!(strip_prefix_ignore_ascii_case("İd=1", "id="), None);
    }
}
