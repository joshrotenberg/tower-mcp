//! Property tests for JSON-RPC envelope classification (#1028).
//!
//! The untagged `JsonRpcMessage` classifier sees untrusted wire input, so
//! parsing arbitrary JSON or text must never panic, and well-formed messages
//! must classify and round-trip correctly.

use proptest::prelude::*;
use tower_mcp_types::protocol::JsonRpcMessage;

/// A bounded arbitrary JSON value.
fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        ".*".prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
            prop::collection::hash_map("[a-zA-Z0-9_]{0,8}", inner, 0..6)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    /// Classifying arbitrary JSON never panics.
    #[test]
    fn classify_arbitrary_json_never_panics(v in arb_json()) {
        let _ = serde_json::from_value::<JsonRpcMessage>(v);
    }

    /// Classifying arbitrary text never panics.
    #[test]
    fn classify_arbitrary_text_never_panics(s in ".*") {
        let _ = serde_json::from_str::<JsonRpcMessage>(&s);
    }

    /// A well-formed request classifies as a single (non-batch) message and its
    /// method survives a round-trip.
    #[test]
    fn well_formed_request_round_trips(method in "[a-z/_]{1,20}", id in any::<i64>()) {
        let json = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method });
        let msg: JsonRpcMessage = serde_json::from_value(json).unwrap();
        prop_assert!(!msg.is_batch());
        prop_assert_eq!(msg.len(), 1);
        let reencoded = serde_json::to_value(&msg).unwrap();
        let seen = reencoded
            .as_array()
            .and_then(|a| a.first())
            .unwrap_or(&reencoded)
            .get("method")
            .and_then(|m| m.as_str());
        prop_assert_eq!(seen, Some(method.as_str()));
    }

    /// A JSON array of requests classifies as a batch of the right length.
    #[test]
    fn batch_classifies_and_counts(n in 1usize..8) {
        let reqs: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({ "jsonrpc": "2.0", "id": i, "method": "ping" }))
            .collect();
        let msg: JsonRpcMessage = serde_json::from_value(serde_json::Value::Array(reqs)).unwrap();
        prop_assert!(msg.is_batch());
        prop_assert_eq!(msg.len(), n);
    }
}
