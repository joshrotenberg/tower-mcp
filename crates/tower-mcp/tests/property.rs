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

proptest! {
    /// Matching arbitrary input against a template never panics.
    #[test]
    fn match_uri_never_panics(uri in ".*") {
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
}
