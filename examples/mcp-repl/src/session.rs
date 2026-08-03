//! Session resurrection: keeping the REPL usable when the server loses the
//! session underneath it.
//!
//! A remote MCP server that restarts, OOMs, or sits behind an edge returning
//! 502/503 leaves the client holding a session id the server no longer knows.
//! Every subsequent request fails, and without recovery the prompt is dead
//! until the user quits and reconnects by hand.
//!
//! [`Session`] holds the live [`McpClient`] behind a swappable slot plus the
//! recipe for building a fresh one. When a command fails with a session-loss
//! error, the REPL rebuilds the connection from scratch (new transport, new
//! session id, fresh handshake) and retries the command once.
//!
//! Rebuilding rather than reusing the transport is deliberate: the existing
//! HTTP transport still carries the dead `Mcp-Session-Id`, so re-initializing
//! on it can fail the same way. A new transport starts from no session at all.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tower_mcp::client::McpClient;

/// Builds a fully connected and initialized client. Called once per
/// reconnect, so it must construct a new transport each time.
pub type Connector = Box<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<McpClient, tower_mcp::Error>> + Send>>
        + Send
        + Sync,
>;

/// The live client plus, when the transport supports it, the means to
/// re-establish it.
pub struct Session {
    client: RwLock<Arc<McpClient>>,
    connector: Option<Connector>,
    /// Serializes reconnects so two failing commands do not both rebuild.
    reconnecting: tokio::sync::Mutex<()>,
    /// Bumped on every successful reconnect. A caller that saw generation N
    /// and finds N+1 after taking the lock knows someone else already
    /// reconnected and skips its own attempt.
    generation: AtomicU64,
    /// Wakes long-lived work tied to the current client so it can move to the
    /// replacement immediately. Polling the atomic would leave an old final
    /// subscription alive until its transport happened to close.
    generation_tx: tokio::sync::watch::Sender<u64>,
}

impl Session {
    /// A `None` connector means no recovery path, which is the right answer
    /// for stdio children and the in-process demo router: there, a dropped
    /// session means the server itself is gone.
    pub fn new(client: McpClient, connector: Option<Connector>) -> Self {
        let (generation_tx, _) = tokio::sync::watch::channel(0);
        Self {
            client: RwLock::new(Arc::new(client)),
            connector,
            reconnecting: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            generation_tx,
        }
    }

    /// The client to issue the next request on. Cloned out rather than
    /// borrowed so a reconnect can swap the slot without waiting on callers.
    pub fn client(&self) -> Arc<McpClient> {
        self.client.read().unwrap().clone()
    }

    pub fn can_reconnect(&self) -> bool {
        self.connector.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Observe successful reconnects. Long-lived requests should reopen on
    /// the client corresponding to each new generation.
    pub fn subscribe_generation(&self) -> tokio::sync::watch::Receiver<u64> {
        self.generation_tx.subscribe()
    }

    /// Rebuild the connection, unless another caller already did so since
    /// `seen` was read. Returns `Ok(())` either way; the caller should
    /// re-read [`Session::client`] afterwards.
    pub async fn reconnect(&self, seen: u64) -> Result<(), tower_mcp::Error> {
        let Some(connector) = &self.connector else {
            return Err(tower_mcp::Error::Transport(
                "this transport cannot be reconnected".to_string(),
            ));
        };
        let _guard = self.reconnecting.lock().await;
        if self.generation() != seen {
            return Ok(());
        }
        // A restarting server is usually a second or two from ready; a short
        // pause makes the retry land after the bind rather than during it.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let fresh = connector().await?;
        *self.client.write().unwrap() = Arc::new(fresh);
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.generation_tx.send_replace(generation);
        Ok(())
    }
}

/// True when the server rejected a request because the session is not yet
/// initialized (JSON-RPC `-32600` naming `notifications/initialized`). This
/// is retryable at startup: against a multi-instance server without a shared
/// session store, the initialize handshake and a follow-up request can land
/// on different instances, so a brief retry often lands on a consistent one.
/// Mid-session it means the opposite thing, that the session the handshake
/// established is gone, which is what [`is_session_lost`] keys on.
pub fn is_not_initialized(e: &tower_mcp::Error) -> bool {
    matches!(
        e,
        tower_mcp::Error::JsonRpc(j)
            if j.code == -32600 && j.message.contains("notifications/initialized")
    )
}

/// True when an error means the session the handshake established no longer
/// exists on the server, so a fresh handshake would plausibly succeed.
///
/// The cases:
///
/// - [`tower_mcp::Error::SessionExpired`], the client's own name for HTTP 404
///   against a live session id. The library retries this internally when
///   session recovery is on, so it only reaches here once that has failed.
/// - not-initialized, which mid-session means the server forgot the session
///   (restart, OOM, redeploy, or a request scattered to another instance).
/// - a closed transport, from the client's message loop shutting down.
/// - HTTP 410 Gone, and 502/503 from an edge in front of a restarting server.
///   These arrive as `Transport` strings, since the transport only maps 404
///   to a typed variant.
///
/// Everything else, including 4xx auth failures and tool errors, is a real
/// error and must surface unchanged: reconnecting would hide it behind a
/// second identical failure.
pub fn is_session_lost(e: &tower_mcp::Error) -> bool {
    if matches!(e, tower_mcp::Error::SessionExpired) || is_not_initialized(e) {
        return true;
    }
    match e {
        tower_mcp::Error::Transport(msg) => {
            msg.contains("Transport closed")
                || msg.contains("Connection closed")
                || msg.contains("HTTP 410")
                || msg.contains("HTTP 502")
                || msg.contains("HTTP 503")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonrpc(code: i32, message: &str) -> tower_mcp::Error {
        tower_mcp::Error::JsonRpc(tower_mcp::error::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        })
    }

    #[test]
    fn session_loss_covers_the_documented_conditions() {
        assert!(is_session_lost(&tower_mcp::Error::SessionExpired));
        assert!(is_session_lost(&jsonrpc(
            -32600,
            "Client must send notifications/initialized before making requests"
        )));
        assert!(is_session_lost(&tower_mcp::Error::Transport(
            "Transport closed".into()
        )));
        assert!(is_session_lost(&tower_mcp::Error::Transport(
            "Connection closed".into()
        )));
        for status in ["HTTP 410 Gone", "HTTP 502 Bad Gateway", "HTTP 503"] {
            assert!(
                is_session_lost(&tower_mcp::Error::Transport(format!(
                    "{status} from server: "
                ))),
                "{status} should count as session loss"
            );
        }
    }

    #[test]
    fn session_loss_does_not_swallow_real_errors() {
        // Auth and not-found are the server answering, not the session dying.
        assert!(!is_session_lost(&tower_mcp::Error::Transport(
            "HTTP 401 Unauthorized from server: bad token".into()
        )));
        assert!(!is_session_lost(&tower_mcp::Error::Transport(
            "HTTP 404 from http://x/mcp: MCP endpoint not found".into()
        )));
        assert!(!is_session_lost(&jsonrpc(-32602, "Invalid params")));
        assert!(!is_session_lost(&tower_mcp::Error::tool("boom")));
    }

    #[test]
    fn detects_not_initialized_startup_error() {
        assert!(is_not_initialized(&jsonrpc(
            -32600,
            "Client must send notifications/initialized before making requests"
        )));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        // Same code, different message.
        assert!(!is_not_initialized(&jsonrpc(
            -32600,
            "some other invalid request"
        )));
        // Right message text, different code.
        assert!(!is_not_initialized(&jsonrpc(
            -32602,
            "notifications/initialized"
        )));
        // A transport error is never the not-initialized case.
        assert!(!is_not_initialized(&tower_mcp::Error::Transport(
            "boom".into()
        )));
    }
}
