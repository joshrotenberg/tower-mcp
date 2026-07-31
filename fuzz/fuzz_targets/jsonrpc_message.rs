#![no_main]

use libfuzzer_sys::fuzz_target;
use tower_mcp_types::protocol::JsonRpcMessage;

fuzz_target!(|input: &[u8]| {
    let _ = serde_json::from_slice::<JsonRpcMessage>(input);
});
