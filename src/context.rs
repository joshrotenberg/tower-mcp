//! Request context for MCP handlers
//!
//! Provides progress reporting and cancellation support for long-running operations.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::context::RequestContext;
//!
//! async fn long_running_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
//!     for i in 0..100 {
//!         // Check if cancelled
//!         if ctx.is_cancelled() {
//!             return Err(Error::tool("Operation cancelled"));
//!         }
//!
//!         // Report progress
//!         ctx.report_progress(i as f64, Some(100.0), Some("Processing...")).await;
//!
//!         do_work(i).await;
//!     }
//!     Ok(CallToolResult::text("Done!"))
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use crate::protocol::{ProgressParams, ProgressToken, RequestId};

/// A notification to be sent to the client
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// Progress update for a request
    Progress(ProgressParams),
    /// A subscribed resource has been updated
    ResourceUpdated {
        /// The URI of the updated resource
        uri: String,
    },
    /// The list of available resources has changed
    ResourcesListChanged,
}

/// Sender for server notifications
pub type NotificationSender = mpsc::Sender<ServerNotification>;

/// Receiver for server notifications
pub type NotificationReceiver = mpsc::Receiver<ServerNotification>;

/// Create a new notification channel
pub fn notification_channel(buffer: usize) -> (NotificationSender, NotificationReceiver) {
    mpsc::channel(buffer)
}

/// Context for a request, providing progress and cancellation support
#[derive(Clone)]
pub struct RequestContext {
    /// The request ID
    request_id: RequestId,
    /// Progress token (if provided by client)
    progress_token: Option<ProgressToken>,
    /// Cancellation flag
    cancelled: Arc<AtomicBool>,
    /// Channel for sending notifications
    notification_tx: Option<NotificationSender>,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("request_id", &self.request_id)
            .field("progress_token", &self.progress_token)
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .finish()
    }
}

impl RequestContext {
    /// Create a new request context
    pub fn new(request_id: RequestId) -> Self {
        Self {
            request_id,
            progress_token: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            notification_tx: None,
        }
    }

    /// Set the progress token
    pub fn with_progress_token(mut self, token: ProgressToken) -> Self {
        self.progress_token = Some(token);
        self
    }

    /// Set the notification sender
    pub fn with_notification_sender(mut self, tx: NotificationSender) -> Self {
        self.notification_tx = Some(tx);
        self
    }

    /// Get the request ID
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Get the progress token (if any)
    pub fn progress_token(&self) -> Option<&ProgressToken> {
        self.progress_token.as_ref()
    }

    /// Check if the request has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Mark the request as cancelled
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Get a cancellation token that can be shared
    pub fn cancellation_token(&self) -> CancellationToken {
        CancellationToken {
            cancelled: self.cancelled.clone(),
        }
    }

    /// Report progress to the client
    ///
    /// This is a no-op if no progress token was provided or no notification sender is configured.
    pub async fn report_progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.progress_token else {
            return;
        };
        let Some(tx) = &self.notification_tx else {
            return;
        };

        let params = ProgressParams {
            progress_token: token.clone(),
            progress,
            total,
            message: message.map(|s| s.to_string()),
        };

        // Best effort - don't block if channel is full
        let _ = tx.try_send(ServerNotification::Progress(params));
    }

    /// Report progress synchronously (non-async version)
    ///
    /// This is a no-op if no progress token was provided or no notification sender is configured.
    pub fn report_progress_sync(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.progress_token else {
            return;
        };
        let Some(tx) = &self.notification_tx else {
            return;
        };

        let params = ProgressParams {
            progress_token: token.clone(),
            progress,
            total,
            message: message.map(|s| s.to_string()),
        };

        let _ = tx.try_send(ServerNotification::Progress(params));
    }
}

/// A token that can be used to check for cancellation
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Request cancellation
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Builder for creating request contexts
#[derive(Default)]
pub struct RequestContextBuilder {
    request_id: Option<RequestId>,
    progress_token: Option<ProgressToken>,
    notification_tx: Option<NotificationSender>,
}

impl RequestContextBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the request ID
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Set the progress token
    pub fn progress_token(mut self, token: ProgressToken) -> Self {
        self.progress_token = Some(token);
        self
    }

    /// Set the notification sender
    pub fn notification_sender(mut self, tx: NotificationSender) -> Self {
        self.notification_tx = Some(tx);
        self
    }

    /// Build the request context
    ///
    /// Panics if request_id is not set.
    pub fn build(self) -> RequestContext {
        let mut ctx = RequestContext::new(self.request_id.expect("request_id is required"));
        if let Some(token) = self.progress_token {
            ctx = ctx.with_progress_token(token);
        }
        if let Some(tx) = self.notification_tx {
            ctx = ctx.with_notification_sender(tx);
        }
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancellation() {
        let ctx = RequestContext::new(RequestId::Number(1));
        assert!(!ctx.is_cancelled());

        let token = ctx.cancellation_token();
        assert!(!token.is_cancelled());

        ctx.cancel();
        assert!(ctx.is_cancelled());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_progress_reporting() {
        let (tx, mut rx) = notification_channel(10);

        let ctx = RequestContext::new(RequestId::Number(1))
            .with_progress_token(ProgressToken::Number(42))
            .with_notification_sender(tx);

        ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
            .await;

        let notification = rx.recv().await.unwrap();
        match notification {
            ServerNotification::Progress(params) => {
                assert_eq!(params.progress, 50.0);
                assert_eq!(params.total, Some(100.0));
                assert_eq!(params.message.as_deref(), Some("Halfway"));
            }
            _ => panic!("Expected Progress notification"),
        }
    }

    #[tokio::test]
    async fn test_progress_no_token() {
        let (tx, mut rx) = notification_channel(10);

        // No progress token - should be a no-op
        let ctx = RequestContext::new(RequestId::Number(1)).with_notification_sender(tx);

        ctx.report_progress(50.0, Some(100.0), None).await;

        // Channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_builder() {
        let (tx, _rx) = notification_channel(10);

        let ctx = RequestContextBuilder::new()
            .request_id(RequestId::String("req-1".to_string()))
            .progress_token(ProgressToken::String("prog-1".to_string()))
            .notification_sender(tx)
            .build();

        assert_eq!(ctx.request_id(), &RequestId::String("req-1".to_string()));
        assert!(ctx.progress_token().is_some());
    }
}
