//! Integration tests for crates-mcp using the tower-mcp client
//!
//! These tests spawn the crates-mcp server as a subprocess and test it
//! using the MCP client protocol.

use std::path::PathBuf;

use tower_mcp::client::{McpClient, StdioClientTransport};

/// Helper to get the path to the crates-mcp binary
fn binary_path() -> String {
    // The workspace root's target directory
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let binary = workspace_root.join("target/debug/crates-mcp");
    binary.to_string_lossy().to_string()
}

#[tokio::test]
async fn test_initialize() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);

    let result = client.initialize("test-client", "1.0.0").await;
    assert!(result.is_ok(), "Initialize failed: {:?}", result.err());

    let info = result.unwrap();
    assert_eq!(info.server_info.name, "crates-mcp");
}

#[tokio::test]
async fn test_list_tools() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let tools = client.list_tools().await.expect("Failed to list tools");

    // Should have 7 tools
    assert_eq!(tools.tools.len(), 7);

    // Check for expected tool names
    let tool_names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"search_crates"));
    assert!(tool_names.contains(&"get_crate_info"));
    assert!(tool_names.contains(&"get_crate_versions"));
    assert!(tool_names.contains(&"get_dependencies"));
    assert!(tool_names.contains(&"get_reverse_dependencies"));
    assert!(tool_names.contains(&"get_downloads"));
    assert!(tool_names.contains(&"get_owners"));
}

#[tokio::test]
async fn test_list_resources() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let resources = client
        .list_resources()
        .await
        .expect("Failed to list resources");

    // Should have 1 resource
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].uri, "crates://recent-searches");
}

#[tokio::test]
async fn test_list_prompts() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let prompts = client.list_prompts().await.expect("Failed to list prompts");

    // Should have 2 prompts
    assert_eq!(prompts.prompts.len(), 2);

    let prompt_names: Vec<&str> = prompts.prompts.iter().map(|p| p.name.as_str()).collect();
    assert!(prompt_names.contains(&"analyze_crate"));
    assert!(prompt_names.contains(&"compare_crates"));
}

#[tokio::test]
#[ignore = "requires network access to crates.io"]
async fn test_search_crates() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let result = client
        .call_tool(
            "search_crates",
            serde_json::json!({
                "query": "serde"
            }),
        )
        .await
        .expect("Failed to call search_crates");

    // Check that we got content back
    assert!(!result.content.is_empty());

    // The result should contain text mentioning "serde"
    if let tower_mcp::protocol::Content::Text { text, .. } = &result.content[0] {
        assert!(text.contains("serde"), "Result should mention serde");
    } else {
        panic!("Expected text content");
    }
}

#[tokio::test]
#[ignore = "requires network access to crates.io"]
async fn test_get_crate_info() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let result = client
        .call_tool(
            "get_crate_info",
            serde_json::json!({
                "name": "serde"
            }),
        )
        .await
        .expect("Failed to call get_crate_info");

    assert!(!result.content.is_empty());

    if let tower_mcp::protocol::Content::Text { text, .. } = &result.content[0] {
        assert!(text.contains("serde"), "Result should contain crate name");
        assert!(text.contains("Downloads"), "Result should contain stats");
    } else {
        panic!("Expected text content");
    }
}

#[tokio::test]
async fn test_tool_error_handling() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    // Call with missing required argument
    let result = client
        .call_tool("search_crates", serde_json::json!({}))
        .await;

    // Should return an error for missing argument
    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_recent_searches_empty() {
    let transport = StdioClientTransport::spawn(&binary_path(), &[])
        .await
        .expect("Failed to spawn server");

    let mut client = McpClient::new(transport);
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("Failed to initialize");

    let result = client
        .read_resource("crates://recent-searches")
        .await
        .expect("Failed to read resource");

    assert_eq!(result.contents.len(), 1);
    assert_eq!(result.contents[0].uri, "crates://recent-searches");

    // Should be empty JSON array initially
    if let Some(text) = &result.contents[0].text {
        assert!(text.contains("[]") || text.contains("[\n]"));
    } else {
        panic!("Expected text content");
    }
}
