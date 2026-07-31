//! Property tests for JSON-RPC envelope classification (#1028).
//!
//! The untagged `JsonRpcMessage` classifier sees untrusted wire input, so
//! parsing arbitrary JSON or text must never panic, and well-formed messages
//! must classify and round-trip correctly.

use proptest::prelude::*;
use tower_mcp_types::protocol::{ClientCapabilities, InitializeParams, JsonRpcMessage};

/// Strings that include Unicode/control characters and occasionally force a
/// genuinely long allocation instead of proptest's usual small values.
fn arb_text() -> BoxedStrategy<String> {
    prop_oneof![
        8 => prop::collection::vec(any::<char>(), 0..512)
            .prop_map(|chars| chars.into_iter().collect()),
        1 => Just("\0\r\n\t\u{001b}\u{007f}".repeat(64)),
        1 => Just("x".repeat(16 * 1024)),
    ]
    .boxed()
}

/// A bounded arbitrary JSON value.
fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    let text = arb_text();
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        text.prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(8, 256, 12, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..12).prop_map(serde_json::Value::Array),
            prop::collection::hash_map("[a-zA-Z0-9_]{0,24}", inner, 0..12)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Classifying arbitrary JSON never panics.
    #[test]
    fn classify_arbitrary_json_never_panics(v in arb_json()) {
        let _ = serde_json::from_value::<JsonRpcMessage>(v);
    }

    /// Classifying arbitrary text never panics.
    #[test]
    fn classify_arbitrary_text_never_panics(s in arb_text()) {
        let _ = serde_json::from_str::<JsonRpcMessage>(&s);
    }

    /// Raw transports see bytes before UTF-8 or JSON validation. Every byte
    /// sequence must fail cleanly or classify without panicking.
    #[test]
    fn classify_arbitrary_bytes_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = serde_json::from_slice::<JsonRpcMessage>(&bytes);
    }

    /// Capability objects are attacker-controlled on initialize and on every
    /// final-protocol request. Parsing arbitrary shapes must be total; any
    /// accepted value must also serialize back through the validated model.
    #[test]
    fn client_capabilities_parse_never_panics(value in arb_json()) {
        if let Ok(capabilities) = serde_json::from_value::<ClientCapabilities>(value) {
            prop_assert!(serde_json::to_value(capabilities).is_ok());
        }
    }

    /// Exercise capabilities through the initialize-handshake envelope as
    /// well as through the standalone capability type.
    #[test]
    fn initialize_capabilities_parse_never_panics(capabilities in arb_json()) {
        let params = serde_json::json!({
            "protocolVersion": "2026-07-28",
            "capabilities": capabilities,
            "clientInfo": { "name": "property-client", "version": "0" }
        });
        let _ = serde_json::from_value::<InitializeParams>(params);
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
