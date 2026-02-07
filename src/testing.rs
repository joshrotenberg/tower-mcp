//! Test utilities for MCP servers.
//!
//! This module provides [`TestClient`], an ergonomic wrapper around [`McpRouter`]
//! for writing concise MCP server tests without manual JSON-RPC construction.
//!
//! # Quick Start
//!
//! ```rust
//! use tower_mcp::{CallToolResult, McpRouter, ToolBuilder, TestClient};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//! use serde_json::json;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct EchoInput {
//!     message: String,
//! }
//!
//! # #[tokio::main]
//! # async fn main() {
//! let echo = ToolBuilder::new("echo")
//!     .description("Echo a message")
//!     .handler(|input: EchoInput| async move {
//!         Ok(CallToolResult::text(input.message))
//!     })
//!     .build();
//!
//! let router = McpRouter::new()
//!     .server_info("test-server", "1.0.0")
//!     .tool(echo);
//!
//! let mut client = TestClient::from_router(router);
//! client.initialize().await;
//!
//! let result = client.call_tool("echo", json!({"message": "hello"})).await;
//! assert_eq!(result.all_text(), "hello");
//! # }
//! ```
//!
//! # Full Example
//!
//! The following shows a complete test setup with tools, resources, and prompts:
//!
//! ```rust
//! use std::collections::HashMap;
//! use tower_mcp::{
//!     CallToolResult, GetPromptResult, McpRouter, ReadResourceResult,
//!     PromptBuilder, ResourceBuilder, TestClient, ToolBuilder,
//! };
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//! use serde_json::json;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct AddInput {
//!     a: i64,
//!     b: i64,
//! }
//!
//! # #[tokio::main]
//! # async fn main() {
//! // -- Build the router ------------------------------------------------
//! let add = ToolBuilder::new("add")
//!     .description("Add two numbers")
//!     .handler(|input: AddInput| async move {
//!         Ok(CallToolResult::text(format!("{}", input.a + input.b)))
//!     })
//!     .build();
//!
//! let readme = ResourceBuilder::new("file:///README.md")
//!     .name("README")
//!     .description("Project readme")
//!     .text("# My Project");
//!
//! let greet = PromptBuilder::new("greet")
//!     .description("Greet someone")
//!     .required_arg("name", "Name to greet")
//!     .handler(|args: HashMap<String, String>| async move {
//!         let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
//!         Ok(GetPromptResult::user_message(
//!             format!("Please greet {} warmly.", name),
//!         ))
//!     })
//!     .build();
//!
//! let router = McpRouter::new()
//!     .server_info("test-server", "1.0.0")
//!     .tool(add)
//!     .resource(readme)
//!     .prompt(greet);
//!
//! // -- Create client and initialize ------------------------------------
//! let mut client = TestClient::from_router(router);
//! let init = client.initialize().await;
//! assert!(init.get("protocolVersion").is_some());
//!
//! // -- Tools -----------------------------------------------------------
//! let tools = client.list_tools().await;
//! assert_eq!(tools.len(), 1);
//!
//! let result = client.call_tool("add", json!({"a": 2, "b": 3})).await;
//! assert_eq!(result.all_text(), "5");
//! assert_eq!(result.first_text(), Some("5"));
//! assert!(!result.is_error);
//!
//! // -- Resources -------------------------------------------------------
//! let resources = client.list_resources().await;
//! assert_eq!(resources.len(), 1);
//!
//! let readme = client.read_resource("file:///README.md").await;
//! assert_eq!(readme.first_text(), Some("# My Project"));
//! assert_eq!(readme.first_uri(), Some("file:///README.md"));
//!
//! // -- Prompts ---------------------------------------------------------
//! let prompts = client.list_prompts().await;
//! assert_eq!(prompts.len(), 1);
//!
//! let mut args = HashMap::new();
//! args.insert("name".to_string(), "Alice".to_string());
//! let prompt = client.get_prompt("greet", args).await;
//! assert!(prompt.first_message_text().unwrap().contains("Alice"));
//!
//! // -- Error handling --------------------------------------------------
//! // Expect a JSON-RPC error for a non-existent tool:
//! let error = client
//!     .call_tool_expect_error("nonexistent", json!({}))
//!     .await;
//! assert!(error.get("code").is_some());
//!
//! // Expect a JSON-RPC error for an unknown method:
//! let error = client
//!     .send_request_expect_error("unknown/method", None)
//!     .await;
//! assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32601));
//!
//! // -- Raw escape hatch ------------------------------------------------
//! // Use send_request for methods without typed helpers:
//! let pong = client.send_request("ping", None).await;
//! assert_eq!(pong, json!({}));
//! # }
//! ```

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::context::{NotificationReceiver, ServerNotification, notification_channel};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{
    CallToolResult, GetPromptResult, JsonRpcRequest, JsonRpcResponse, McpNotification,
    ReadResourceResult,
};
use crate::router::McpRouter;

/// An ergonomic test client for MCP servers.
///
/// Wraps an [`McpRouter`] and [`JsonRpcService`] to provide typed, concise
/// methods for testing MCP server behavior. All methods that expect successful
/// responses will panic on JSON-RPC errors, which is appropriate for test code.
///
/// # Construction
///
/// Use [`TestClient::from_router`] to create a client from an existing router:
///
/// ```rust
/// use tower_mcp::{McpRouter, TestClient};
///
/// let router = McpRouter::new().server_info("test", "1.0.0");
/// let mut client = TestClient::from_router(router);
/// ```
pub struct TestClient {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
    notification_rx: NotificationReceiver,
    next_id: i64,
}

impl TestClient {
    /// Create a new test client from an [`McpRouter`].
    ///
    /// Sets up a notification channel and wraps the router in a
    /// [`JsonRpcService`] for JSON-RPC framing.
    pub fn from_router(router: McpRouter) -> Self {
        let (tx, rx) = notification_channel(256);
        let router = router.with_notification_sender(tx);
        let service = JsonRpcService::new(router.clone());
        Self {
            service,
            router,
            notification_rx: rx,
            next_id: 1,
        }
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send an initialize request and the initialized notification.
    ///
    /// Returns the raw JSON result from the initialize response.
    /// Panics if initialization fails.
    pub async fn initialize(&mut self) -> Value {
        let id = self.next_id();
        let req = JsonRpcRequest::new(id, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }));

        let result = self.send_request_inner(req).await;
        self.router
            .handle_notification(McpNotification::Initialized);
        result
    }

    /// List all tools registered on the server.
    ///
    /// Returns the tools array from the response. Panics on error.
    pub async fn list_tools(&mut self) -> Vec<Value> {
        let result = self.send_request("tools/list", None).await;
        result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Call a tool by name with the given arguments.
    ///
    /// Returns a typed [`CallToolResult`]. Panics on JSON-RPC errors.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use tower_mcp::TestClient;
    /// # use serde_json::json;
    /// # async fn example(client: &mut TestClient) {
    /// let result = client.call_tool("echo", json!({"message": "hi"})).await;
    /// assert_eq!(result.first_text(), Some("hi"));
    /// # }
    /// ```
    pub async fn call_tool(&mut self, name: &str, args: Value) -> CallToolResult {
        let raw = self.call_tool_raw(name, args).await;
        serde_json::from_value(raw).expect("failed to deserialize CallToolResult")
    }

    /// Call a tool and return the raw JSON response.
    ///
    /// Useful when you need to inspect fields not covered by [`CallToolResult`].
    pub async fn call_tool_raw(&mut self, name: &str, args: Value) -> Value {
        self.send_request(
            "tools/call",
            Some(serde_json::json!({
                "name": name,
                "arguments": args,
            })),
        )
        .await
    }

    /// Call a tool and parse the result as a JSON [`Value`].
    ///
    /// Panics if the tool call fails, returns an error, or has no parseable content.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use tower_mcp::TestClient;
    /// # use serde_json::json;
    /// # async fn example(client: &mut TestClient) {
    /// let value = client.call_tool_json("search", json!({"q": "rust"})).await;
    /// assert!(value["results"].is_array());
    /// # }
    /// ```
    pub async fn call_tool_json(&mut self, name: &str, args: Value) -> Value {
        let result = self.call_tool(name, args).await;
        assert!(
            !result.is_error,
            "tool '{}' returned an error: {}",
            name,
            result.all_text()
        );
        result
            .as_json()
            .expect("no parseable content in tool result")
            .expect("failed to parse tool result as JSON")
    }

    /// Call a tool and deserialize the result into a typed value.
    ///
    /// Panics if the tool call fails, returns an error, or deserialization fails.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use tower_mcp::TestClient;
    /// # use serde::Deserialize;
    /// # use serde_json::json;
    /// # #[derive(Deserialize)]
    /// # struct SearchResult { count: usize }
    /// # async fn example(client: &mut TestClient) {
    /// let result: SearchResult = client.call_tool_typed("search", json!({"q": "rust"})).await;
    /// assert!(result.count > 0);
    /// # }
    /// ```
    pub async fn call_tool_typed<T: DeserializeOwned>(&mut self, name: &str, args: Value) -> T {
        let result = self.call_tool(name, args).await;
        assert!(
            !result.is_error,
            "tool '{}' returned an error: {}",
            name,
            result.all_text()
        );
        result
            .deserialize()
            .expect("no parseable content in tool result")
            .expect("failed to deserialize tool result")
    }

    /// Call a tool and assert that it returns an error.
    ///
    /// Panics if the tool call succeeds without an error. Returns the raw
    /// JSON response body (which may be a `CallToolResult` with `isError: true`
    /// or a JSON-RPC error).
    pub async fn call_tool_expect_error(&mut self, name: &str, args: Value) -> Value {
        let id = self.next_id();
        let req = JsonRpcRequest::new(id, "tools/call").with_params(serde_json::json!({
            "name": name,
            "arguments": args,
        }));

        let resp = self
            .service
            .call_single(req)
            .await
            .expect("transport error");

        match resp {
            JsonRpcResponse::Error(e) => {
                serde_json::to_value(&e.error).expect("failed to serialize error")
            }
            JsonRpcResponse::Result(r) => {
                // Check for isError flag in CallToolResult
                if r.result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                    r.result
                } else {
                    panic!(
                        "expected tool call to '{}' to fail, but it succeeded: {:?}",
                        name, r.result
                    );
                }
            }
        }
    }

    /// List all resources registered on the server.
    ///
    /// Returns the resources array from the response. Panics on error.
    pub async fn list_resources(&mut self) -> Vec<Value> {
        let result = self.send_request("resources/list", None).await;
        result
            .get("resources")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Read a resource by URI.
    ///
    /// Returns a typed [`ReadResourceResult`]. Panics on JSON-RPC errors.
    pub async fn read_resource(&mut self, uri: &str) -> ReadResourceResult {
        let raw = self
            .send_request("resources/read", Some(serde_json::json!({ "uri": uri })))
            .await;
        serde_json::from_value(raw).expect("failed to deserialize ReadResourceResult")
    }

    /// List all prompts registered on the server.
    ///
    /// Returns the prompts array from the response. Panics on error.
    pub async fn list_prompts(&mut self) -> Vec<Value> {
        let result = self.send_request("prompts/list", None).await;
        result
            .get("prompts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Get a prompt by name with the given arguments.
    ///
    /// Returns a typed [`GetPromptResult`]. Panics on JSON-RPC errors.
    pub async fn get_prompt(
        &mut self,
        name: &str,
        args: HashMap<String, String>,
    ) -> GetPromptResult {
        let raw = self
            .send_request(
                "prompts/get",
                Some(serde_json::json!({
                    "name": name,
                    "arguments": args,
                })),
            )
            .await;
        serde_json::from_value(raw).expect("failed to deserialize GetPromptResult")
    }

    /// Send a completion request with raw parameters.
    ///
    /// Returns the raw JSON result. Panics on error.
    pub async fn complete(&mut self, params: Value) -> Value {
        self.send_request("completion/complete", Some(params)).await
    }

    /// Send an arbitrary request and expect success.
    ///
    /// This is an escape hatch for methods not covered by the typed helpers.
    /// Panics on JSON-RPC errors.
    pub async fn send_request(&mut self, method: &str, params: Option<Value>) -> Value {
        let id = self.next_id();
        let mut req = JsonRpcRequest::new(id, method);
        if let Some(p) = params {
            req = req.with_params(p);
        }
        self.send_request_inner(req).await
    }

    /// Send an arbitrary request and expect a JSON-RPC error.
    ///
    /// Panics if the response is a success. Returns the error object as JSON.
    pub async fn send_request_expect_error(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Value {
        let id = self.next_id();
        let mut req = JsonRpcRequest::new(id, method);
        if let Some(p) = params {
            req = req.with_params(p);
        }

        let resp = self
            .service
            .call_single(req)
            .await
            .expect("transport error");

        match resp {
            JsonRpcResponse::Error(e) => {
                serde_json::to_value(&e.error).expect("failed to serialize error")
            }
            JsonRpcResponse::Result(r) => {
                panic!(
                    "expected request '{}' to fail, but it succeeded: {:?}",
                    method, r.result
                );
            }
        }
    }

    /// Try to receive a notification without blocking.
    ///
    /// Returns `None` if no notification is available.
    pub fn try_recv_notification(&mut self) -> Option<ServerNotification> {
        self.notification_rx.try_recv().ok()
    }

    /// Drain all pending notifications.
    ///
    /// Returns all notifications that have been sent since the last drain.
    pub fn drain_notifications(&mut self) -> Vec<ServerNotification> {
        let mut notifications = Vec::new();
        while let Ok(n) = self.notification_rx.try_recv() {
            notifications.push(n);
        }
        notifications
    }

    async fn send_request_inner(&mut self, req: JsonRpcRequest) -> Value {
        let method = req.method.clone();
        let resp = self
            .service
            .call_single(req)
            .await
            .expect("transport error");

        match resp {
            JsonRpcResponse::Result(r) => r.result,
            JsonRpcResponse::Error(e) => {
                panic!(
                    "expected request '{}' to succeed, but got error: {} (code {})",
                    method, e.error.message, e.error.code,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallToolResult, GetPromptResult, PromptBuilder, ResourceBuilder, ToolBuilder};
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoInput {
        message: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct AddInput {
        a: i64,
        b: i64,
    }

    #[derive(Debug, Clone, Deserialize, serde::Serialize, JsonSchema, PartialEq)]
    struct AddResult {
        sum: i64,
    }

    fn create_test_router() -> McpRouter {
        let echo = ToolBuilder::new("echo")
            .description("Echo a message")
            .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
            .build();

        let add = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let add_json = ToolBuilder::new("add_json")
            .description("Add two numbers and return JSON")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::from_serialize(&AddResult {
                    sum: input.a + input.b,
                })
                .unwrap())
            })
            .build();

        let readme = ResourceBuilder::new("file:///README.md")
            .name("README")
            .description("Project readme")
            .text("# My Project");

        let greet = PromptBuilder::new("greet")
            .description("Greet someone")
            .required_arg("name", "Name to greet")
            .handler(|args: HashMap<String, String>| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult::user_message(format!(
                    "Please greet {} warmly.",
                    name
                )))
            })
            .build();

        McpRouter::new()
            .server_info("test-server", "1.0.0")
            .tool(echo)
            .tool(add)
            .tool(add_json)
            .resource(readme)
            .prompt(greet)
    }

    #[tokio::test]
    async fn test_client_initialize() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);

        let init = client.initialize().await;

        assert!(init.get("protocolVersion").is_some());
        assert!(init.get("serverInfo").is_some());
        assert_eq!(
            init.get("serverInfo")
                .and_then(|s| s.get("name"))
                .and_then(|n| n.as_str()),
            Some("test-server")
        );
    }

    #[tokio::test]
    async fn test_client_list_tools() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let tools = client.list_tools().await;

        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"add"));
        assert!(names.contains(&"add_json"));
    }

    #[tokio::test]
    async fn test_client_call_tool() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client.call_tool("echo", json!({"message": "hello"})).await;

        assert_eq!(result.all_text(), "hello");
        assert_eq!(result.first_text(), Some("hello"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_client_call_tool_with_computation() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client.call_tool("add", json!({"a": 40, "b": 2})).await;

        assert_eq!(result.all_text(), "42");
    }

    #[tokio::test]
    async fn test_client_call_tool_expect_error() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let error = client
            .call_tool_expect_error("nonexistent", json!({}))
            .await;

        assert!(error.get("code").is_some());
    }

    #[tokio::test]
    async fn test_client_list_resources() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let resources = client.list_resources().await;

        assert_eq!(resources.len(), 1);
        assert_eq!(
            resources[0].get("uri").and_then(|u| u.as_str()),
            Some("file:///README.md")
        );
    }

    #[tokio::test]
    async fn test_client_read_resource() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client.read_resource("file:///README.md").await;

        assert_eq!(result.first_text(), Some("# My Project"));
        assert_eq!(result.first_uri(), Some("file:///README.md"));
    }

    #[tokio::test]
    async fn test_client_list_prompts() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let prompts = client.list_prompts().await;

        assert_eq!(prompts.len(), 1);
        assert_eq!(
            prompts[0].get("name").and_then(|n| n.as_str()),
            Some("greet")
        );
    }

    #[tokio::test]
    async fn test_client_get_prompt() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = client.get_prompt("greet", args).await;

        assert!(result.first_message_text().unwrap().contains("Alice"));
    }

    #[tokio::test]
    async fn test_client_send_request_expect_error() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let error = client
            .send_request_expect_error("unknown/method", None)
            .await;

        // -32601 is "Method not found"
        assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_client_ping() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let pong = client.send_request("ping", None).await;

        assert_eq!(pong, json!({}));
    }

    #[tokio::test]
    async fn test_client_id_increments() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);

        // Each call should increment the ID
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
    }

    #[tokio::test]
    async fn test_client_call_tool_raw() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let raw = client
            .call_tool_raw("echo", json!({"message": "test"}))
            .await;

        // Raw response should have content array
        assert!(raw.get("content").is_some());
        assert!(raw.get("content").unwrap().is_array());
    }

    #[tokio::test]
    async fn test_client_call_tool_json() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let value = client
            .call_tool_json("add_json", json!({"a": 10, "b": 20}))
            .await;
        assert_eq!(value["sum"], 30);
    }

    #[tokio::test]
    async fn test_client_call_tool_typed() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result: AddResult = client
            .call_tool_typed("add_json", json!({"a": 10, "b": 20}))
            .await;
        assert_eq!(result, AddResult { sum: 30 });
    }
}
