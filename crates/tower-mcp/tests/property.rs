//! Property tests for URI-template matching (#1028).
//!
//! `match_uri` sees untrusted URIs, so matching arbitrary input must never
//! panic. Expanding a template with a value and matching it back must extract
//! that value; an extra path segment must not match a single-variable template.

use proptest::prelude::*;
use std::collections::HashMap;
use tower_mcp::protocol::ReadResourceResult;
use tower_mcp::resource::{ResourceTemplate, ResourceTemplateBuilder};

/// A `db://users/{id}` template with a trivial handler (unused by matching).
fn users_template() -> ResourceTemplate {
    ResourceTemplateBuilder::new("db://users/{id}").handler(
        |_uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult::default())
        },
    )
}

fn empty_template_handler(template: String) -> Result<ResourceTemplate, tower_mcp::Error> {
    ResourceTemplateBuilder::new(template).try_handler(
        |_uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult::default())
        },
    )
}

fn arb_template_text() -> BoxedStrategy<String> {
    prop_oneof![
        8 => prop::collection::vec(any::<char>(), 0..512)
            .prop_map(|chars| chars.into_iter().collect()),
        1 => Just("{".repeat(4096)),
        1 => Just("a".repeat(16 * 1024)),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Matching arbitrary input against a template never panics.
    #[test]
    fn match_uri_never_panics(uri in arb_template_text()) {
        let template = users_template();
        let _ = template.match_uri(&uri);
    }

    /// Expanding the template with a slash-free value and matching it back
    /// extracts that value.
    #[test]
    fn expand_then_match_round_trips(id in "[^/ ]{1,20}") {
        let template = users_template();
        let caps = template
            .match_uri(&format!("db://users/{id}"))
            .expect("a slash-free segment should match");
        prop_assert_eq!(caps.get("id").map(String::as_str), Some(id.as_str()));
    }

    /// An extra path segment does not match a single-variable template.
    #[test]
    fn extra_path_segment_does_not_match(a in "[^/ ]{1,10}", b in "[^/ ]{1,10}") {
        let template = users_template();
        let uri = format!("db://users/{a}/{b}");
        prop_assert!(template.match_uri(&uri).is_none());
    }

    /// Dynamic template compilation is a fallible boundary and must never
    /// panic, even with unmatched braces, control characters, or long input.
    #[test]
    fn compile_arbitrary_template_never_panics(template in arb_template_text()) {
        if let Ok(compiled) = empty_template_handler(template) {
            let _ = compiled.match_uri("scheme://arbitrary/input");
        }
    }
}

/// The regex crate guarantees linear-time matching. Keep a large adversarial
/// non-match in the suite so a future switch to a backtracking engine or a
/// less-safe generated expression is immediately visible as a hang/timeout.
#[test]
fn template_regex_handles_large_nonmatch() {
    let template = empty_template_handler("scheme://{value}/suffix".to_string()).unwrap();
    let input = format!("scheme://{}!", "a".repeat(1024 * 1024));
    assert!(template.match_uri(&input).is_none());
}
