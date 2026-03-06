//! Client handler example -- bidirectional MCP communication
//!
//! Demonstrates the full client handler API by connecting to a server that
//! sends sampling requests, elicitation requests, and progress notifications
//! back to the client.
//!
//! This example shows three approaches:
//! 1. `NotificationHandler` -- callback-based, for notifications only
//! 2. `ClientHandler` trait -- full control over sampling and elicitation
//! 3. Builder pattern -- configuring capabilities and roots
//!
//! Run with: cargo run --example client_handler
//!
//! This spawns the `sampling_server` example as a subprocess.

use std::collections::HashMap;

use async_trait::async_trait;
use tower_mcp::client::{ClientHandler, McpClient, NotificationHandler, StdioClientTransport};
use tower_mcp::protocol::{
    ContentRole, CreateMessageParams, CreateMessageResult, ElicitAction, ElicitFieldValue,
    ElicitRequestParams, ElicitResult, ListRootsResult, Root, SamplingContent,
    SamplingContentOrArray,
};
use tower_mcp_types::JsonRpcError;

// =============================================================================
// Approach 1: NotificationHandler (callbacks for notifications only)
// =============================================================================

/// The simplest approach -- register callbacks for specific notification types.
/// Server-initiated requests (sampling, elicitation) are rejected automatically.
fn notification_only_handler() -> NotificationHandler {
    NotificationHandler::new()
        .on_progress(|p| {
            let pct = if let Some(total) = p.total {
                format!(" ({:.0}%)", (p.progress / total) * 100.0)
            } else {
                String::new()
            };
            println!(
                "  [progress] {}{}",
                p.message.as_deref().unwrap_or("working..."),
                pct
            );
        })
        .on_log_message(|msg| {
            println!("  [log:{}] {}", msg.level, msg.data);
        })
        .on_tools_changed(|| {
            println!("  [notification] Server tools changed");
        })
}

// =============================================================================
// Approach 2: Full ClientHandler trait implementation
// =============================================================================

/// Full handler that responds to sampling requests, elicitation, and
/// roots listing in addition to notifications.
struct FullHandler;

#[async_trait]
impl ClientHandler for FullHandler {
    /// Handle sampling requests from the server.
    ///
    /// In a real application, this would forward to an LLM provider.
    /// Here we return a mock response based on the prompt.
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        // Extract the user's message text
        let prompt: String = params
            .messages
            .iter()
            .flat_map(|m| m.content.items())
            .filter_map(|c| c.as_text())
            .collect::<Vec<_>>()
            .join("\n");

        println!("  [sampling] Server requested LLM inference:");
        println!("    Prompt: {}...", &prompt[..prompt.len().min(80)]);
        if let Some(ref system) = params.system_prompt {
            println!("    System: {}", system);
        }

        // Mock LLM response -- in production, forward to OpenAI/Anthropic/etc.
        let summary = format!(
            "This is a mock summary of the {} character input.",
            prompt.len()
        );
        println!("    Response: {}", summary);

        Ok(CreateMessageResult {
            role: ContentRole::Assistant,
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: summary,
                annotations: None,
                meta: None,
            }),
            model: "mock-model-1.0".to_string(),
            stop_reason: Some("endTurn".to_string()),
            meta: None,
        })
    }

    /// Handle elicitation requests from the server.
    ///
    /// In a real application, this would show a UI to the user.
    /// Here we auto-confirm for demonstration.
    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        match params {
            ElicitRequestParams::Form(form) => {
                println!("  [elicitation] Server requests user input:");
                println!("    Message: {}", form.message);
                println!("    Required fields: {:?}", form.requested_schema.required);
                println!("    Auto-confirming for demo...");

                let mut content = HashMap::new();
                content.insert("confirmed".to_string(), ElicitFieldValue::Boolean(true));
                content.insert(
                    "reason".to_string(),
                    ElicitFieldValue::String("Approved via client handler example".to_string()),
                );

                Ok(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(content),
                    meta: None,
                })
            }
            _ => Err(JsonRpcError::invalid_params(
                "Only form elicitation is supported",
            )),
        }
    }

    /// Handle roots/list requests from the server.
    async fn handle_list_roots(&self) -> Result<ListRootsResult, JsonRpcError> {
        println!("  [roots] Server requested root listing");
        Ok(ListRootsResult {
            roots: vec![Root {
                uri: "file:///home/user/projects".to_string(),
                name: Some("Projects".to_string()),
                meta: None,
            }],
            meta: None,
        })
    }

    /// Handle all server notifications.
    async fn on_notification(&self, notification: tower_mcp::client::ServerNotification) {
        use tower_mcp::client::ServerNotification;
        match notification {
            ServerNotification::Progress(p) => {
                let pct = if let Some(total) = p.total {
                    format!(" ({:.0}%)", (p.progress / total) * 100.0)
                } else {
                    String::new()
                };
                println!(
                    "  [progress] {}{}",
                    p.message.as_deref().unwrap_or("working..."),
                    pct
                );
            }
            ServerNotification::LogMessage(msg) => {
                println!("  [log:{}] {}", msg.level, msg.data);
            }
            ServerNotification::ToolsListChanged => {
                println!("  [notification] Tools changed");
            }
            other => {
                println!("  [notification] {:?}", other);
            }
        }
    }
}

// =============================================================================
// Main -- demonstrates both approaches
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=warn,client_handler=debug")
        .init();

    println!("=== MCP Client Handler Example ===\n");

    // --- Demo 1: NotificationHandler (callbacks only) ---
    println!("--- Demo 1: NotificationHandler (progress callbacks) ---\n");
    {
        let transport =
            StdioClientTransport::spawn("cargo", &["run", "--example", "sampling_server"]).await?;

        let handler = notification_only_handler();
        let client = McpClient::connect_with_handler(transport, handler).await?;

        let info = client.initialize("handler-demo", "1.0.0").await?;
        println!(
            "Connected to: {} v{}\n",
            info.server_info.name, info.server_info.version
        );

        // Call slow_task -- progress callbacks will fire
        println!("Calling slow_task (3 steps)...");
        let result = client
            .call_tool("slow_task", serde_json::json!({"steps": 3}))
            .await?;
        // Small delay for progress notifications to arrive
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        println!("Result: {}\n", result.all_text());

        // Calling summarize will fail because NotificationHandler rejects sampling
        println!("Calling summarize (will fail -- NotificationHandler rejects sampling)...");
        let result = client
            .call_tool(
                "summarize",
                serde_json::json!({"text": "Tower is a library of modular components for building networking clients and servers."}),
            )
            .await?;
        println!("Result: {}\n", result.all_text());
    }

    // --- Demo 2: Full ClientHandler trait ---
    println!("--- Demo 2: Full ClientHandler (sampling + elicitation) ---\n");
    {
        let transport =
            StdioClientTransport::spawn("cargo", &["run", "--example", "sampling_server"]).await?;

        // Use builder to declare sampling + elicitation capabilities
        let client = McpClient::builder()
            .with_sampling()
            .with_elicitation()
            .with_roots(vec![Root {
                uri: "file:///home/user/projects".to_string(),
                name: Some("Projects".to_string()),
                meta: None,
            }])
            .connect(transport, FullHandler)
            .await?;

        let info = client.initialize("handler-demo", "1.0.0").await?;
        println!(
            "Connected to: {} v{}\n",
            info.server_info.name, info.server_info.version
        );

        // Call summarize -- triggers sampling request back to client
        println!("Calling summarize (triggers sampling request to client)...");
        let result = client
            .call_tool(
                "summarize",
                serde_json::json!({
                    "text": "The Model Context Protocol (MCP) is an open protocol that standardizes how applications provide context to LLMs. MCP enables a standard way for AI models to access tools, data sources, and computational capabilities through a unified interface."
                }),
            )
            .await?;
        println!("Result: {}\n", result.all_text());

        // Call confirm_delete -- triggers elicitation request back to client
        println!("Calling confirm_delete (triggers elicitation request to client)...");
        let result = client
            .call_tool(
                "confirm_delete",
                serde_json::json!({"item": "old-backup.tar.gz"}),
            )
            .await?;
        println!("Result: {}\n", result.all_text());

        // Call slow_task with progress
        println!("Calling slow_task (3 steps with progress)...");
        let result = client
            .call_tool("slow_task", serde_json::json!({"steps": 3}))
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        println!("Result: {}\n", result.all_text());
    }

    println!("=== Done ===");
    Ok(())
}
