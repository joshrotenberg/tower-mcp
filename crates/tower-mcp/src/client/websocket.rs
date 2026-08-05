//! WebSocket client transport.
//!
//! Connects an [`McpClient`](crate::client::McpClient) to a
//! [`WebSocketTransport`](crate::transport::WebSocketTransport) server, or to
//! any MCP server speaking the same binding.
//!
//! # How this differs from HTTP
//!
//! The streamable HTTP client carries a lot of machinery that exists to make
//! a request/response protocol behave like a duplex one: an SSE stream for
//! server-to-client traffic, session ids, `Last-Event-ID` resumption, and
//! reconnect. A WebSocket is already duplex, so none of that applies. One
//! connection carries requests, responses, notifications, and
//! server-initiated requests, and the transport is correspondingly small.
//!
//! There is no session recovery: if the socket drops, the connection is over.
//! [`supports_session_recovery`](ClientTransport::supports_session_recovery)
//! reports `false`, so the client does not attempt a reconnect that cannot
//! work.
//!
//! # Negotiation and auth
//!
//! Both ride on `Sec-WebSocket-Protocol`, matching the server:
//!
//! - `mcp.version.<version>` selects the protocol revision
//! - `mcp.auth.<token>` carries a bearer token
//!
//! The subprotocol carries auth because a browser cannot set request headers
//! on a WebSocket. A native client can, so [`WebSocketClientConfig::bearer`]
//! sends both, and [`headers`](WebSocketClientConfig::headers) adds arbitrary
//! ones for servers that want them.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, WebSocketClientTransport};
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let transport = WebSocketClientTransport::connect("ws://127.0.0.1:3000/ws").await?;
//! let client = McpClient::connect(transport).await?;
//! client.initialize("my-client", "1.0.0").await?;
//! # Ok(())
//! # }
//! ```

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::error::{Error, Result};

use super::transport::ClientTransport;

/// Connection settings for [`WebSocketClientTransport`].
#[derive(Debug, Clone, Default)]
pub struct WebSocketClientConfig {
    /// Protocol revision to request, sent as `mcp.version.<version>`.
    ///
    /// `None` sends no version subprotocol, leaving the server to apply its
    /// own default.
    pub protocol_version: Option<String>,
    /// Bearer token. Sent both as an `Authorization` header and as an
    /// `mcp.auth.<token>` subprotocol, since servers accept either.
    pub bearer: Option<String>,
    /// Additional headers for the opening handshake.
    pub headers: Vec<(String, String)>,
}

/// A [`ClientTransport`] over a WebSocket connection.
pub struct WebSocketClientTransport {
    /// Outbound frames to the socket task.
    outgoing_tx: mpsc::Sender<String>,
    /// Inbound frames from the socket task.
    incoming_rx: mpsc::Receiver<String>,
    connected: bool,
}

impl WebSocketClientTransport {
    /// Connect to `url` with default settings.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_config(url, WebSocketClientConfig::default()).await
    }

    /// Connect to `url`, negotiating a protocol version and authenticating.
    pub async fn connect_with_config(url: &str, config: WebSocketClientConfig) -> Result<Self> {
        let mut request = url
            .into_client_request()
            .map_err(|e| Error::Transport(format!("invalid WebSocket URL: {e}")))?;

        // Subprotocols are how this binding negotiates, so build the header
        // even when only one of the two parts is present.
        let mut subprotocols = Vec::new();
        if let Some(version) = &config.protocol_version {
            subprotocols.push(format!("mcp.version.{version}"));
        }
        if let Some(token) = &config.bearer {
            subprotocols.push(format!("mcp.auth.{token}"));
        }
        if !subprotocols.is_empty() {
            let value = subprotocols.join(", ");
            let header = HeaderValue::from_str(&value).map_err(|e| {
                Error::Transport(format!("subprotocol is not a valid header value: {e}"))
            })?;
            request
                .headers_mut()
                .insert("sec-websocket-protocol", header);
        }

        if let Some(token) = &config.bearer {
            let header = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| Error::Transport(format!("bearer token is not a header: {e}")))?;
            request.headers_mut().insert("authorization", header);
        }

        for (name, value) in &config.headers {
            let name: tokio_tungstenite::tungstenite::http::HeaderName = name
                .parse()
                .map_err(|e| Error::Transport(format!("invalid header name '{name}': {e}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| Error::Transport(format!("invalid header value: {e}")))?;
            request.headers_mut().insert(name, value);
        }

        let (stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| Error::Transport(format!("WebSocket connect failed: {e}")))?;

        let (mut sink, mut source) = stream.split();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<String>(64);
        let (incoming_tx, incoming_rx) = mpsc::channel::<String>(64);

        // Writer: one task owns the sink, so `send` never contends.
        tokio::spawn(async move {
            while let Some(message) = outgoing_rx.recv().await {
                if sink.send(Message::Text(message.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Reader: text frames are MCP messages. Ping/pong is handled by the
        // library; binary frames are not part of this binding and are
        // ignored rather than treated as a protocol error, since a peer
        // sending one has not necessarily broken the conversation.
        tokio::spawn(async move {
            while let Some(message) = source.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if incoming_tx.send(text.to_string()).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::debug!(%error, "WebSocket receive error");
                        break;
                    }
                }
            }
            // Dropping `incoming_tx` closes the channel, which `recv` reports
            // as end-of-stream so the client's message loop can shut down.
        });

        Ok(Self {
            outgoing_tx,
            incoming_rx,
            connected: true,
        })
    }
}

#[async_trait]
impl ClientTransport for WebSocketClientTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        self.outgoing_tx
            .send(message.to_string())
            .await
            .map_err(|_| Error::Transport("WebSocket connection closed".to_string()))
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        match self.incoming_rx.recv().await {
            Some(message) => Ok(Some(message)),
            None => {
                self.connected = false;
                Ok(None)
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected && !self.outgoing_tx.is_closed()
    }

    async fn close(&mut self) -> Result<()> {
        self.connected = false;
        // Dropping the sender ends the writer task, which closes the sink.
        self.incoming_rx.close();
        Ok(())
    }

    /// A dropped WebSocket cannot be resumed: there is no session id and no
    /// replay, so a reconnect would start a new conversation rather than
    /// continue this one.
    fn supports_session_recovery(&self) -> bool {
        false
    }
}
