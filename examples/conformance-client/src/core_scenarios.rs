//! Core conformance scenarios (non-auth).

use anyhow::Result;
use tower_mcp::HttpClientTransport;
use tower_mcp::client::McpClient;

use crate::handlers;

/// `initialize` -- Connect, list tools, disconnect.
pub async fn initialize(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    client.initialize("conformance-client", "0.1.0").await?;
    let tools = client.list_tools().await?;
    tracing::info!("Listed {} tools", tools.tools.len());

    client.shutdown().await?;
    Ok(())
}

/// `tools_call` -- Connect with full handler, list and call all tools.
pub async fn tools_call(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .with_sampling()
        .with_elicitation()
        .connect(transport, handlers::FullHandler)
        .await?;

    client.initialize("conformance-client", "0.1.0").await?;
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

/// `sse-retry` -- Connect and call the test_reconnection tool.
/// The SSE reconnection logic is handled by HttpClientTransport internally.
pub async fn sse_retry(server_url: &str) -> Result<()> {
    let transport = HttpClientTransport::new(server_url);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    client.initialize("conformance-client", "0.1.0").await?;
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

    client.initialize("conformance-client", "0.1.0").await?;
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
