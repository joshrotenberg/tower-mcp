//! Client transport trait for raw JSON message I/O.
//!
//! This module defines the [`ClientTransport`] trait, the low-level abstraction
//! for sending and receiving JSON-RPC messages over a transport mechanism.
//!
//! Unlike the previous `ClientTransport` which bundled request/response
//! correlation, this trait provides raw message I/O. The [`McpClient`](super::McpClient)
//! handles correlation, multiplexing, and dispatch in its background task.

use async_trait::async_trait;

use crate::error::Result;

/// Low-level transport for sending and receiving raw JSON-RPC messages.
///
/// Implementations handle the physical I/O (stdio, HTTP, WebSocket) while
/// the [`McpClient`](super::McpClient) handles JSON-RPC framing, request/response
/// correlation, and server-initiated request dispatch.
///
/// # Implementing a Custom Transport
///
/// ```rust,ignore
/// use async_trait::async_trait;
/// use tower_mcp::client::ClientTransport;
/// use tower_mcp::error::Result;
///
/// struct MyTransport { /* ... */ }
///
/// #[async_trait]
/// impl ClientTransport for MyTransport {
///     async fn send(&mut self, message: &str) -> Result<()> {
///         // Write message to the transport
///         Ok(())
///     }
///
///     async fn recv(&mut self) -> Result<Option<String>> {
///         // Read next message, None on EOF
///         Ok(None)
///     }
///
///     fn is_connected(&self) -> bool { true }
///
///     async fn close(&mut self) -> Result<()> { Ok(()) }
/// }
/// ```
#[async_trait]
pub trait ClientTransport: Send + 'static {
    /// Send a raw JSON message to the server.
    ///
    /// The message is a complete JSON-RPC request, response, or notification
    /// serialized as a string. The transport adds any necessary framing
    /// (e.g., newline for stdio).
    async fn send(&mut self, message: &str) -> Result<()>;

    /// Receive the next raw JSON message from the server.
    ///
    /// Returns `Ok(Some(json))` for a message, `Ok(None)` for clean EOF/close,
    /// or `Err(...)` for transport errors.
    async fn recv(&mut self) -> Result<Option<String>>;

    /// Check if the transport is still connected.
    fn is_connected(&self) -> bool;

    /// Close the transport gracefully.
    ///
    /// After calling this, `recv()` should return `Ok(None)`.
    async fn close(&mut self) -> Result<()>;

    /// Reset the transport's session state for re-initialization.
    ///
    /// Called when the server indicates the session has expired. The
    /// transport should clear its session ID, stop any SSE streams,
    /// and prepare for a new `initialize` handshake.
    ///
    /// The default implementation is a no-op (for transports like stdio
    /// that don't have sessions).
    async fn reset_session(&mut self) {}

    /// Whether this transport supports automatic session recovery.
    ///
    /// When true, the client will attempt to re-initialize and retry
    /// failed operations when the server returns a session expired error.
    ///
    /// Default: `false`.
    fn supports_session_recovery(&self) -> bool {
        false
    }
}
