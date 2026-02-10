//! MCP Client implementation
//!
//! Provides client functionality for connecting to MCP servers.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::BoxError;
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     // Connect to an MCP server via stdio
//!     let transport = StdioClientTransport::spawn("my-mcp-server", &["--flag"]).await?;
//!     let mut client = McpClient::new(transport);
//!
//!     // Initialize the connection
//!     let server_info = client.initialize("my-client", "1.0.0").await?;
//!     println!("Connected to: {}", server_info.server_info.name);
//!
//!     // List available tools
//!     let tools = client.list_tools().await?;
//!     for tool in &tools.tools {
//!         println!("Tool: {}", tool.name);
//!     }
//!
//!     // Call a tool
//!     let result = client.call_tool("my-tool", serde_json::json!({"arg": "value"})).await?;
//!     println!("Result: {:?}", result);
//!
//!     Ok(())
//! }
//! ```

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, CompleteParams, CompleteResult,
    CompletionArgument, CompletionReference, GetPromptParams, GetPromptResult, Implementation,
    InitializeParams, InitializeResult, JsonRpcRequest, JsonRpcResponse, ListPromptsParams,
    ListPromptsResult, ListResourcesParams, ListResourcesResult, ListRootsResult, ListToolsParams,
    ListToolsResult, ReadResourceParams, ReadResourceResult, Root, RootsCapability, notifications,
};

/// Trait for MCP client transports
#[async_trait]
pub trait ClientTransport: Send {
    /// Send a request and receive a response
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value>;

    /// Send a notification (no response expected)
    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()>;

    /// Check if the transport is still connected
    fn is_connected(&self) -> bool;

    /// Close the transport
    async fn close(self: Box<Self>) -> Result<()>;
}

/// MCP Client for connecting to MCP servers
pub struct McpClient<T: ClientTransport> {
    transport: T,
    initialized: bool,
    server_info: Option<InitializeResult>,
    /// Client capabilities to declare during initialization
    capabilities: ClientCapabilities,
    /// Roots available to the server
    roots: Vec<Root>,
}

impl<T: ClientTransport> McpClient<T> {
    /// Create a new MCP client with the given transport
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            initialized: false,
            server_info: None,
            capabilities: ClientCapabilities::default(),
            roots: Vec::new(),
        }
    }

    /// Configure roots for this client.
    ///
    /// The client will declare roots support during initialization and
    /// provide these roots when requested by the server.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::{McpClient, StdioClientTransport};
    /// use tower_mcp::protocol::Root;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let transport = StdioClientTransport::spawn("server", &[]).await?;
    /// let client = McpClient::new(transport)
    ///     .with_roots(vec![Root { uri: "file:///project".into(), name: Some("Project".into()) }]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_roots(mut self, roots: Vec<Root>) -> Self {
        self.roots = roots;
        self.capabilities.roots = Some(RootsCapability { list_changed: true });
        self
    }

    /// Configure custom capabilities for this client.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::{McpClient, StdioClientTransport};
    /// use tower_mcp::protocol::ClientCapabilities;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let transport = StdioClientTransport::spawn("server", &[]).await?;
    /// let client = McpClient::new(transport)
    ///     .with_capabilities(ClientCapabilities {
    ///         sampling: Some(Default::default()),
    ///         ..Default::default()
    ///     });
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_capabilities(mut self, capabilities: ClientCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Get the server info (available after initialization)
    pub fn server_info(&self) -> Option<&InitializeResult> {
        self.server_info.as_ref()
    }

    /// Check if the client is initialized
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Get the current roots
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Set roots and notify the server if initialized
    ///
    /// If the client is already initialized, sends a roots list changed notification.
    pub async fn set_roots(&mut self, roots: Vec<Root>) -> Result<()> {
        self.roots = roots;
        if self.initialized {
            self.notify_roots_changed().await?;
        }
        Ok(())
    }

    /// Add a root and notify the server if initialized
    pub async fn add_root(&mut self, root: Root) -> Result<()> {
        self.roots.push(root);
        if self.initialized {
            self.notify_roots_changed().await?;
        }
        Ok(())
    }

    /// Remove a root by URI and notify the server if initialized
    pub async fn remove_root(&mut self, uri: &str) -> Result<bool> {
        let initial_len = self.roots.len();
        self.roots.retain(|r| r.uri != uri);
        let removed = self.roots.len() < initial_len;
        if removed && self.initialized {
            self.notify_roots_changed().await?;
        }
        Ok(removed)
    }

    /// Send roots list changed notification to the server
    async fn notify_roots_changed(&mut self) -> Result<()> {
        self.transport
            .notify(notifications::ROOTS_LIST_CHANGED, serde_json::json!({}))
            .await
    }

    /// Get the roots list result (for responding to server's roots/list request)
    ///
    /// Returns a result suitable for responding to a roots/list request from the server.
    pub fn list_roots(&self) -> ListRootsResult {
        ListRootsResult {
            roots: self.roots.clone(),
        }
    }

    /// Initialize the MCP connection
    pub async fn initialize(
        &mut self,
        client_name: &str,
        client_version: &str,
    ) -> Result<&InitializeResult> {
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info: Implementation {
                name: client_name.to_string(),
                version: client_version.to_string(),
                ..Default::default()
            },
        };

        let result: InitializeResult = self.request("initialize", &params).await?;
        self.server_info = Some(result);

        // Send initialized notification
        self.transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;

        self.initialized = true;

        Ok(self.server_info.as_ref().unwrap())
    }

    /// List available tools
    pub async fn list_tools(&mut self) -> Result<ListToolsResult> {
        self.ensure_initialized()?;
        self.request("tools/list", &ListToolsParams { cursor: None })
            .await
    }

    /// Call a tool
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        self.ensure_initialized()?;
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            meta: None,
        };
        self.request("tools/call", &params).await
    }

    /// List available resources
    pub async fn list_resources(&mut self) -> Result<ListResourcesResult> {
        self.ensure_initialized()?;
        self.request("resources/list", &ListResourcesParams { cursor: None })
            .await
    }

    /// Read a resource
    pub async fn read_resource(&mut self, uri: &str) -> Result<ReadResourceResult> {
        self.ensure_initialized()?;
        let params = ReadResourceParams {
            uri: uri.to_string(),
        };
        self.request("resources/read", &params).await
    }

    /// List available prompts
    pub async fn list_prompts(&mut self) -> Result<ListPromptsResult> {
        self.ensure_initialized()?;
        self.request("prompts/list", &ListPromptsParams { cursor: None })
            .await
    }

    /// Get a prompt
    pub async fn get_prompt(
        &mut self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult> {
        self.ensure_initialized()?;
        let params = GetPromptParams {
            name: name.to_string(),
            arguments: arguments.unwrap_or_default(),
        };
        self.request("prompts/get", &params).await
    }

    /// Ping the server
    pub async fn ping(&mut self) -> Result<()> {
        let _: serde_json::Value = self.request("ping", &serde_json::json!({})).await?;
        Ok(())
    }

    /// Request completion suggestions from the server
    ///
    /// This is used to get autocomplete suggestions for prompt arguments or resource URIs.
    pub async fn complete(
        &mut self,
        reference: CompletionReference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.ensure_initialized()?;
        let params = CompleteParams {
            reference,
            argument: CompletionArgument::new(argument_name, argument_value),
            context: None,
        };
        self.request("completion/complete", &params).await
    }

    /// Request completion for a prompt argument
    pub async fn complete_prompt_arg(
        &mut self,
        prompt_name: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::prompt(prompt_name),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Request completion for a resource URI
    pub async fn complete_resource_uri(
        &mut self,
        resource_uri: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::resource(resource_uri),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Send a raw request
    pub async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        let result = self.transport.request(method, params_value).await?;

        serde_json::from_value(result)
            .map_err(|e| Error::Transport(format!("Failed to deserialize response: {}", e)))
    }

    /// Send a notification
    pub async fn notify<P: serde::Serialize>(&mut self, method: &str, params: &P) -> Result<()> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        self.transport.notify(method, params_value).await
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.initialized {
            return Err(Error::Transport("Client not initialized".to_string()));
        }
        Ok(())
    }
}

// ============================================================================
// Stdio Client Transport
// ============================================================================

/// Client transport that communicates with a subprocess via stdio
pub struct StdioClientTransport {
    child: Option<Child>,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    request_id: AtomicI64,
}

impl StdioClientTransport {
    /// Spawn a new subprocess and connect to it
    pub async fn spawn(program: &str, args: &[&str]) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("Failed to spawn {}: {}", program, e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        tracing::info!(program = %program, "Spawned MCP server process");

        Ok(Self {
            child: Some(child),
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicI64::new(1),
        })
    }

    /// Create from an existing child process
    pub fn from_child(mut child: Child) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        Ok(Self {
            child: Some(child),
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicI64::new(1),
        })
    }

    async fn send_line(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write: {}", e)))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush: {}", e)))?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Transport(format!("Failed to read: {}", e)))?;

        if line.is_empty() {
            return Err(Error::Transport("Connection closed".to_string()));
        }

        Ok(line)
    }
}

#[async_trait]
impl ClientTransport for StdioClientTransport {
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method).with_params(params);

        let request_json = serde_json::to_string(&request)
            .map_err(|e| Error::Transport(format!("Failed to serialize: {}", e)))?;

        tracing::debug!(method = %method, id = %id, "Sending request");
        self.send_line(&request_json).await?;

        let response_line = self.read_line().await?;
        tracing::debug!(response = %response_line.trim(), "Received response");

        let response: JsonRpcResponse = serde_json::from_str(response_line.trim())
            .map_err(|e| Error::Transport(format!("Failed to parse response: {}", e)))?;

        match response {
            JsonRpcResponse::Result(r) => Ok(r.result),
            JsonRpcResponse::Error(e) => Err(Error::JsonRpc(e.error)),
        }
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let json = serde_json::to_string(&notification)
            .map_err(|e| Error::Transport(format!("Failed to serialize: {}", e)))?;

        tracing::debug!(method = %method, "Sending notification");
        self.send_line(&json).await
    }

    fn is_connected(&self) -> bool {
        // Assume connected if we have a child process handle
        self.child.is_some()
    }

    async fn close(mut self: Box<Self>) -> Result<()> {
        // Close stdin to signal EOF
        drop(self.stdin);

        if let Some(mut child) = self.child.take() {
            // Wait for process with timeout
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

            match result {
                Ok(Ok(status)) => {
                    tracing::info!(status = ?status, "Child process exited");
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Error waiting for child");
                }
                Err(_) => {
                    tracing::warn!("Timeout waiting for child, killing");
                    let _ = child.kill().await;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Mock transport that returns preconfigured responses
    struct MockTransport {
        responses: Arc<Mutex<VecDeque<serde_json::Value>>>,
        requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        notifications: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        connected: bool,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
                notifications: Arc::new(Mutex::new(Vec::new())),
                connected: true,
            }
        }

        fn with_responses(responses: Vec<serde_json::Value>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
                notifications: Arc::new(Mutex::new(Vec::new())),
                connected: true,
            }
        }

        #[allow(dead_code)]
        fn get_requests(&self) -> Vec<(String, serde_json::Value)> {
            self.requests.lock().unwrap().clone()
        }

        #[allow(dead_code)]
        fn get_notifications(&self) -> Vec<(String, serde_json::Value)> {
            self.notifications.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ClientTransport for MockTransport {
        async fn request(
            &mut self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value> {
            self.requests
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Transport("No more mock responses".to_string()))
        }

        async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
            self.notifications
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        async fn close(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    fn mock_initialize_response() -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "serverInfo": {
                "name": "test-server",
                "version": "1.0.0"
            },
            "capabilities": {
                "tools": {}
            }
        })
    }

    #[tokio::test]
    async fn test_client_not_initialized() {
        let mut client = McpClient::new(MockTransport::new());

        // Should fail because not initialized
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_client_initialize() {
        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let mut client = McpClient::new(transport);

        assert!(!client.is_initialized());

        let result = client.initialize("test-client", "1.0.0").await;
        assert!(result.is_ok());
        assert!(client.is_initialized());

        let server_info = client.server_info().unwrap();
        assert_eq!(server_info.server_info.name, "test-server");
    }

    #[tokio::test]
    async fn test_list_tools() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "tools": [
                    {
                        "name": "test_tool",
                        "description": "A test tool",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "test_tool");
    }

    #[tokio::test]
    async fn test_call_tool() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    {
                        "type": "text",
                        "text": "Tool result"
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client
            .call_tool("test_tool", serde_json::json!({"arg": "value"}))
            .await
            .unwrap();

        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn test_list_resources() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "resources": [
                    {
                        "uri": "file://test.txt",
                        "name": "Test File"
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let resources = client.list_resources().await.unwrap();

        assert_eq!(resources.resources.len(), 1);
        assert_eq!(resources.resources[0].uri, "file://test.txt");
    }

    #[tokio::test]
    async fn test_read_resource() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "contents": [
                    {
                        "uri": "file://test.txt",
                        "text": "File contents"
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.read_resource("file://test.txt").await.unwrap();

        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].text.as_deref(), Some("File contents"));
    }

    #[tokio::test]
    async fn test_list_prompts() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "prompts": [
                    {
                        "name": "test_prompt",
                        "description": "A test prompt"
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let prompts = client.list_prompts().await.unwrap();

        assert_eq!(prompts.prompts.len(), 1);
        assert_eq!(prompts.prompts[0].name, "test_prompt");
    }

    #[tokio::test]
    async fn test_get_prompt() {
        let transport = MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Prompt message"
                        }
                    }
                ]
            }),
        ]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.get_prompt("test_prompt", None).await.unwrap();

        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_ping() {
        let transport =
            MockTransport::with_responses(vec![mock_initialize_response(), serde_json::json!({})]);
        let mut client = McpClient::new(transport);

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.ping().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_roots_management() {
        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let notifications = transport.notifications.clone();
        let mut client = McpClient::new(transport);

        // Initially no roots
        assert!(client.roots().is_empty());

        // Add a root before initialization (no notification)
        client.add_root(Root::new("file:///project")).await.unwrap();
        assert_eq!(client.roots().len(), 1);
        assert!(notifications.lock().unwrap().is_empty());

        // Initialize
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Add another root after initialization (should notify)
        client.add_root(Root::new("file:///other")).await.unwrap();
        assert_eq!(client.roots().len(), 2);
        assert_eq!(notifications.lock().unwrap().len(), 2); // initialized + roots changed

        // Remove a root
        let removed = client.remove_root("file:///project").await.unwrap();
        assert!(removed);
        assert_eq!(client.roots().len(), 1);

        // Try to remove non-existent root
        let not_removed = client.remove_root("file:///nonexistent").await.unwrap();
        assert!(!not_removed);
    }

    #[tokio::test]
    async fn test_with_roots() {
        let roots = vec![Root::new("file:///test")];
        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let client = McpClient::new(transport).with_roots(roots);

        assert_eq!(client.roots().len(), 1);
        assert!(client.capabilities.roots.is_some());
    }

    #[tokio::test]
    async fn test_with_capabilities() {
        let capabilities = ClientCapabilities {
            sampling: Some(Default::default()),
            ..Default::default()
        };

        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let client = McpClient::new(transport).with_capabilities(capabilities);

        assert!(client.capabilities.sampling.is_some());
    }

    #[tokio::test]
    async fn test_list_roots() {
        let roots = vec![
            Root::new("file:///project1"),
            Root::with_name("file:///project2", "Project 2"),
        ];
        let transport = MockTransport::new();
        let client = McpClient::new(transport).with_roots(roots);

        let result = client.list_roots();
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots[1].name, Some("Project 2".to_string()));
    }
}
