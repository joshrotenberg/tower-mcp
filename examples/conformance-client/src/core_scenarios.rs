//! Core conformance scenarios (non-auth).

use anyhow::Result;
use tower_mcp::client::{McpClient, McpClientBuilder};
use tower_mcp::{HttpClientTransport, ProtocolSupport};

use crate::handlers;

fn requested_protocol_version() -> String {
    std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION")
        .unwrap_or_else(|_| tower_mcp::protocol::LATEST_PROTOCOL_VERSION.to_string())
}

pub(crate) fn uses_final_protocol() -> bool {
    requested_protocol_version() == tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28
}

pub(crate) fn client_builder() -> Result<McpClientBuilder> {
    let version = requested_protocol_version();
    let mut builder = McpClient::builder();
    if uses_final_protocol() {
        builder = builder.protocol_support(ProtocolSupport::try_new([version])?);
    }
    Ok(builder)
}

/// Run a handshake, retrying a bounded number of times on transport-level
/// failures.
///
/// The suite runs against a loopback server, where a POST occasionally fails
/// to send outright. Since #1174 an undelivered `notifications/initialized`
/// is an `initialize()` error rather than a silent no-op, which is correct
/// but turns that rare transient into a red conformance check attributed to
/// whichever PR happened to be running (#1196).
///
/// Only [`tower_mcp::Error::Transport`] is retried. A protocol error is a
/// real conformance result and must surface on the first attempt; retrying
/// one would mask the failures this suite exists to catch. Each retry is
/// logged so a systematic problem still looks systematic.
///
/// Retrying `initialize` on the same client is safe: the server creates a
/// fresh session for any `initialize`, and the transport adopts the new
/// session id from the response.
pub(crate) async fn with_handshake_retry<F, Fut, T>(what: &str, mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, tower_mcp::Error>>,
{
    const ATTEMPTS: u32 = 3;
    let mut delay = std::time::Duration::from_millis(50);
    let mut last_error = None;

    for attempt_number in 1..=ATTEMPTS {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !matches!(error, tower_mcp::Error::Transport(_)) {
                    return Err(error.into());
                }
                tracing::warn!(
                    %error,
                    attempt = attempt_number,
                    attempts = ATTEMPTS,
                    "transport failure during {what}, retrying"
                );
                last_error = Some(error);
                if attempt_number < ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    }

    Err(last_error
        .expect("a failed retry loop records its last error")
        .into())
}

pub(crate) async fn activate(client: &McpClient) -> Result<()> {
    if uses_final_protocol() {
        with_handshake_retry("discover", || {
            client.discover("conformance-client", "0.1.0")
        })
        .await?;
    } else {
        with_handshake_retry("initialize", || {
            client.initialize("conformance-client", "0.1.0")
        })
        .await?;
    }
    Ok(())
}

/// `initialize` -- Connect, list tools, disconnect.
pub async fn initialize(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;

    activate(&client).await?;
    let tools = client.list_tools().await?;
    tracing::info!("Listed {} tools", tools.tools.len());

    client.shutdown().await?;
    Ok(())
}

/// `tools_call` -- Connect with full handler, list and call all tools.
pub async fn tools_call(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .with_sampling()
        .with_elicitation()
        .connect(transport, handlers::FullHandler)
        .await?;

    activate(&client).await?;
    let tools = client.list_tools().await?;
    tracing::info!("Listed {} tools", tools.tools.len());

    for tool in &tools.tools {
        let args = build_tool_arguments(&tool.input_schema);
        tracing::info!(tool = %tool.name, "Calling tool");
        match client.call_tool(&tool.name, args).await {
            Ok(result) => {
                if result.is_error {
                    tracing::warn!(tool = %tool.name, "Tool returned error");
                }
            }
            Err(e) => {
                tracing::warn!(tool = %tool.name, error = %e, "Tool call failed");
            }
        }
    }

    client.shutdown().await?;
    Ok(())
}

/// Exercise all named and unnamed request headers required by SEP-2243.
pub async fn http_standard_headers(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;
    activate(&client).await?;

    let tools = client.list_tools().await?;
    if let Some(tool) = tools.tools.first() {
        client
            .call_tool(&tool.name, build_tool_arguments(&tool.input_schema))
            .await?;
    }
    let resources = client.list_resources().await?;
    if let Some(resource) = resources.resources.first() {
        client.read_resource(&resource.uri).await?;
    }
    let prompts = client.list_prompts().await?;
    if let Some(prompt) = prompts.prompts.first() {
        client.get_prompt(&prompt.name, None).await?;
    }

    client.shutdown().await?;
    Ok(())
}

/// Mirror every schema-designated tool argument into its MCP parameter header.
pub async fn http_custom_headers(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;
    activate(&client).await?;
    client.list_tools().await?;

    let calls = context
        .as_ref()
        .and_then(|value| value.get("toolCalls"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("conformance context did not include toolCalls"))?;
    for call in calls {
        let name = call
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("tool call is missing name"))?;
        let arguments = call
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        client.call_tool(name, arguments).await?;
    }

    client.shutdown().await?;
    Ok(())
}

/// Confirm invalid header annotations are excluded without hiding valid tools.
pub async fn http_invalid_tool_headers(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;
    activate(&client).await?;
    let tools = client.list_tools().await?;
    let valid = tools
        .tools
        .iter()
        .find(|tool| tool.name == "valid_tool")
        .ok_or_else(|| anyhow::anyhow!("valid_tool was filtered from tools/list"))?;
    client
        .call_tool(&valid.name, serde_json::json!({ "region": "us-west1" }))
        .await?;

    client.shutdown().await?;
    Ok(())
}

/// Exercise request-state echo, omission, request-id freshness, and isolation.
pub async fn mrtr_request_state(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = client_builder()?
        .with_elicitation()
        .connect(transport, handlers::FullHandler)
        .await?;
    activate(&client).await?;
    let tools = client.list_tools().await?;

    for name in [
        "test_mrtr_unrelated",
        "test_mrtr_echo_state",
        "test_mrtr_no_state",
        "test_mrtr_no_result_type",
    ] {
        anyhow::ensure!(
            tools.tools.iter().any(|tool| tool.name == name),
            "MRTR fixture tool {name} was not listed"
        );
        client.call_tool(name, serde_json::json!({})).await?;
    }

    client.shutdown().await?;
    Ok(())
}

/// `sse-retry` -- Connect and call the test_reconnection tool.
/// The SSE reconnection logic is handled by HttpClientTransport internally.
pub async fn sse_retry(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    with_handshake_retry("initialize", || {
        client.initialize("conformance-client", "0.1.0")
    })
    .await?;
    let tools = client.list_tools().await?;

    if let Some(tool) = tools.tools.iter().find(|t| t.name == "test_reconnection") {
        tracing::info!("Calling test_reconnection tool");
        let _ = client.call_tool(&tool.name, serde_json::json!({})).await;
    } else {
        tracing::warn!("test_reconnection tool not found");
    }

    client.shutdown().await?;
    Ok(())
}

/// `elicitation-defaults` -- Connect with elicitation handler that applies defaults.
pub async fn elicitation_defaults(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .with_elicitation()
        .connect(transport, handlers::ElicitationDefaultsHandler)
        .await?;

    with_handshake_retry("initialize", || {
        client.initialize("conformance-client", "0.1.0")
    })
    .await?;
    let tools = client.list_tools().await?;

    // Look for the elicitation defaults test tool
    let test_tool = tools.tools.iter().find(|t| {
        t.name == "test_client_elicitation_defaults"
            || t.name == "test_elicitation_sep1034_defaults"
    });

    if let Some(tool) = test_tool {
        tracing::info!(tool = %tool.name, "Calling elicitation defaults test tool");
        let _ = client.call_tool(&tool.name, serde_json::json!({})).await?;
    } else {
        // If the specific tool isn't found, call all tools
        for tool in &tools.tools {
            let args = build_tool_arguments(&tool.input_schema);
            let _ = client.call_tool(&tool.name, args).await;
        }
    }

    client.shutdown().await?;
    Ok(())
}

/// `ttl-list` -- Connect and verify tools/list returns a ttlMs hint.
///
/// Asserts that the server includes `ttlMs: 60000` in the `tools/list`
/// response, exercising the SEP-2549 list TTL field end-to-end.
pub async fn ttl_list(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    with_handshake_retry("initialize", || {
        client.initialize("conformance-client", "0.1.0")
    })
    .await?;
    let tools = client.list_tools().await?;

    anyhow::ensure!(
        tools.ttl_ms == Some(60_000),
        "expected ttlMs=60000 in tools/list response, got {:?}",
        tools.ttl_ms
    );
    tracing::info!("tools/list ttlMs verified: {:?}", tools.ttl_ms);

    client.shutdown().await?;
    Ok(())
}

/// `deprecated-capability` -- Connect and verify the logging capability carries
/// SEP-2577 deprecation metadata in the initialize result.
pub async fn deprecated_capability(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    let result = with_handshake_retry("initialize", || {
        client.initialize("conformance-client", "0.1.0")
    })
    .await?;

    let logging = result
        .capabilities
        .logging
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("server must advertise logging capability"))?;

    let dep = logging
        .deprecated
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("logging capability must carry deprecation info"))?;

    anyhow::ensure!(
        dep.since.as_deref() == Some("2026-07-28"),
        "expected since=2026-07-28, got {:?}",
        dep.since
    );
    tracing::info!("logging.deprecated.since verified: {:?}", dep.since);

    client.shutdown().await?;
    Ok(())
}

/// `tasks-extension` -- Connect with the stable lifecycle and verify the
/// server advertises its partial task implementation.
///
/// The final 2026-07-28 lifecycle intentionally withholds the extension until
/// the remaining SEP-2663 behavior tracked in #951 is complete.
pub async fn tasks_extension(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    let result = with_handshake_retry("initialize", || {
        client.initialize("conformance-client", "0.1.0")
    })
    .await?;

    let extensions = result
        .capabilities
        .extensions
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("server must advertise extensions in capabilities"))?;

    anyhow::ensure!(
        extensions.contains_key(tower_mcp::protocol::TASKS_EXTENSION_ID),
        "expected io.modelcontextprotocol/tasks in capabilities.extensions, got keys: {:?}",
        extensions.keys().collect::<Vec<_>>()
    );
    tracing::info!("tasks extension advertised in capabilities.extensions");

    // Also verify the task-capable tool is callable
    let _ = client
        .call_tool("test_create_task", serde_json::json!({}))
        .await?;
    tracing::info!("test_create_task tool called successfully");

    client.shutdown().await?;
    Ok(())
}

/// Generate dummy arguments from a tool's input schema.
pub fn build_tool_arguments(schema: &serde_json::Value) -> serde_json::Value {
    let mut args = serde_json::Map::new();

    let properties = schema.get("properties").and_then(|p| p.as_object());
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if let Some(props) = properties {
        for (name, def) in props {
            // Only fill required fields to keep it minimal
            if !required.contains(&name.as_str()) {
                continue;
            }
            let value = match def.get("type").and_then(|t| t.as_str()) {
                Some("string") => serde_json::Value::String("test".to_string()),
                Some("integer") => serde_json::Value::Number(1.into()),
                Some("number") => serde_json::json!(1.0),
                Some("boolean") => serde_json::Value::Bool(true),
                Some("array") => serde_json::json!([]),
                Some("object") => serde_json::json!({}),
                _ => serde_json::Value::String("test".to_string()),
            };
            args.insert(name.clone(), value);
        }
    }

    serde_json::Value::Object(args)
}

#[cfg(test)]
mod handshake_retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn a_transient_transport_failure_is_retried() {
        let attempts = AtomicU32::new(0);
        let result = with_handshake_retry("test", || {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed);
            async move {
                if attempt == 0 {
                    Err(tower_mcp::Error::Transport("connection reset".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await
        .expect("the second attempt succeeds");

        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    /// The property that keeps the suite meaningful: a protocol error is a
    /// real conformance result, so it must fail on the first attempt rather
    /// than being retried into a pass or a slower failure.
    #[tokio::test]
    async fn a_protocol_error_is_not_retried() {
        let attempts = AtomicU32::new(0);
        let error = with_handshake_retry("test", || {
            attempts.fetch_add(1, Ordering::Relaxed);
            async move {
                Err::<(), _>(tower_mcp::Error::JsonRpc(
                    tower_mcp::error::JsonRpcError::invalid_request("bad handshake"),
                ))
            }
        })
        .await
        .expect_err("a protocol error must surface");

        assert!(error.to_string().contains("bad handshake"));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a protocol error must not be retried"
        );
    }

    #[tokio::test]
    async fn a_persistent_transport_failure_gives_up() {
        let attempts = AtomicU32::new(0);
        let error = with_handshake_retry("test", || {
            attempts.fetch_add(1, Ordering::Relaxed);
            async move { Err::<(), _>(tower_mcp::Error::Transport("server is down".into())) }
        })
        .await
        .expect_err("a persistent failure must still fail");

        assert!(error.to_string().contains("server is down"));
        assert_eq!(attempts.load(Ordering::Relaxed), 3, "bounded, not infinite");
    }
}
