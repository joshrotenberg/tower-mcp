//! Circuit breaker middleware with observable state handle.
//!
//! Provides [`CircuitBreakerLayer`], a Tower middleware that monitors error rates
//! and trips open when a configurable failure threshold is exceeded. A
//! [`CircuitBreakerHandle`] allows external code (e.g., admin APIs) to query the
//! current state without access to the service itself.
//!
//! # States
//!
//! The circuit breaker has three states:
//!
//! - **Closed**: Requests flow normally. Failures are counted.
//! - **Open**: All requests are immediately rejected with a JSON-RPC error.
//!   After a configurable recovery timeout, transitions to HalfOpen.
//! - **HalfOpen**: A single probe request is allowed through. If it succeeds,
//!   the breaker resets to Closed. If it fails, it returns to Open.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::middleware::CircuitBreakerLayer;
//! use std::time::Duration;
//!
//! let (layer, handle) = CircuitBreakerLayer::builder()
//!     .failure_threshold(5)
//!     .recovery_timeout(Duration::from_secs(30))
//!     .build_with_handle();
//!
//! let transport = HttpTransport::new(router).layer(layer);
//!
//! // Later, in an admin endpoint:
//! let state = handle.state(); // CircuitState::Closed
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tower::Layer;
use tower_service::Service;

use crate::error::JsonRpcError;
use crate::router::{RouterRequest, RouterResponse};

/// The current state of a circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally. Failures are counted.
    Closed,
    /// All requests are rejected. Waiting for recovery timeout.
    Open,
    /// A single probe request is allowed to test recovery.
    HalfOpen,
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CircuitState::Closed => write!(f, "closed"),
            CircuitState::Open => write!(f, "open"),
            CircuitState::HalfOpen => write!(f, "half-open"),
        }
    }
}

/// Shared circuit breaker state.
struct SharedState {
    state: Mutex<CircuitState>,
    /// Consecutive failure count (reset on success).
    consecutive_failures: AtomicU64,
    /// When the circuit was last opened.
    opened_at: Mutex<Option<Instant>>,
    /// Number of failures before opening the circuit.
    failure_threshold: u64,
    /// How long to wait before transitioning from Open to HalfOpen.
    recovery_timeout: Duration,
}

impl SharedState {
    fn state(&self) -> CircuitState {
        *self.state.lock().unwrap()
    }

    fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if *state == CircuitState::HalfOpen {
            *state = CircuitState::Closed;
            tracing::info!("Circuit breaker closed after successful probe");
        }
    }

    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let mut state = self.state.lock().unwrap();

        match *state {
            CircuitState::Closed if failures >= self.failure_threshold => {
                *state = CircuitState::Open;
                *self.opened_at.lock().unwrap() = Some(Instant::now());
                tracing::warn!(
                    failures = failures,
                    threshold = self.failure_threshold,
                    "Circuit breaker opened"
                );
            }
            CircuitState::HalfOpen => {
                *state = CircuitState::Open;
                *self.opened_at.lock().unwrap() = Some(Instant::now());
                tracing::warn!("Circuit breaker re-opened after failed probe");
            }
            _ => {}
        }
    }

    /// Check if we should transition from Open to HalfOpen.
    fn try_half_open(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if *state != CircuitState::Open {
            return false;
        }
        let opened_at = self.opened_at.lock().unwrap();
        if let Some(opened) = *opened_at
            && opened.elapsed() >= self.recovery_timeout
        {
            *state = CircuitState::HalfOpen;
            tracing::info!("Circuit breaker half-open, allowing probe request");
            return true;
        }
        false
    }
}

/// A read-only handle for observing circuit breaker state.
///
/// Cheap to clone and safe to share across threads.
///
/// # Example
///
/// ```rust,ignore
/// let state = handle.state(); // CircuitState::Closed
/// let health = handle.health_status(); // "healthy"
/// let failures = handle.consecutive_failures(); // 0
/// ```
#[derive(Clone)]
pub struct CircuitBreakerHandle {
    shared: Arc<SharedState>,
}

impl CircuitBreakerHandle {
    /// Returns the current circuit breaker state.
    pub fn state(&self) -> CircuitState {
        self.shared.state()
    }

    /// Returns a human-readable health status string.
    ///
    /// - `"healthy"` when Closed
    /// - `"degraded"` when HalfOpen
    /// - `"open"` when Open
    pub fn health_status(&self) -> &'static str {
        match self.shared.state() {
            CircuitState::Closed => "healthy",
            CircuitState::HalfOpen => "degraded",
            CircuitState::Open => "open",
        }
    }

    /// Returns the current consecutive failure count.
    pub fn consecutive_failures(&self) -> u64 {
        self.shared.consecutive_failures()
    }
}

/// Builder for [`CircuitBreakerLayer`].
pub struct CircuitBreakerBuilder {
    failure_threshold: u64,
    recovery_timeout: Duration,
}

impl Default for CircuitBreakerBuilder {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(30),
        }
    }
}

impl CircuitBreakerBuilder {
    /// Set the number of consecutive failures before the circuit opens.
    ///
    /// Default: 5
    pub fn failure_threshold(mut self, threshold: u64) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// Set how long to wait in the Open state before allowing a probe request.
    ///
    /// Default: 30 seconds
    pub fn recovery_timeout(mut self, timeout: Duration) -> Self {
        self.recovery_timeout = timeout;
        self
    }

    /// Build the layer and return a handle for observing state.
    pub fn build_with_handle(self) -> (CircuitBreakerLayer, CircuitBreakerHandle) {
        let shared = Arc::new(SharedState {
            state: Mutex::new(CircuitState::Closed),
            consecutive_failures: AtomicU64::new(0),
            opened_at: Mutex::new(None),
            failure_threshold: self.failure_threshold,
            recovery_timeout: self.recovery_timeout,
        });
        let layer = CircuitBreakerLayer {
            shared: shared.clone(),
        };
        let handle = CircuitBreakerHandle { shared };
        (layer, handle)
    }

    /// Build the layer without a handle.
    pub fn build(self) -> CircuitBreakerLayer {
        self.build_with_handle().0
    }
}

/// Tower layer that wraps services with circuit breaker logic.
///
/// Use [`CircuitBreakerLayer::builder()`] to configure and build.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::middleware::CircuitBreakerLayer;
/// use std::time::Duration;
///
/// let (layer, handle) = CircuitBreakerLayer::builder()
///     .failure_threshold(5)
///     .recovery_timeout(Duration::from_secs(30))
///     .build_with_handle();
/// ```
#[derive(Clone)]
pub struct CircuitBreakerLayer {
    shared: Arc<SharedState>,
}

impl CircuitBreakerLayer {
    /// Create a new builder for configuring the circuit breaker.
    pub fn builder() -> CircuitBreakerBuilder {
        CircuitBreakerBuilder::default()
    }
}

impl<S> Layer<S> for CircuitBreakerLayer {
    type Service = CircuitBreakerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CircuitBreakerService {
            inner,
            shared: self.shared.clone(),
        }
    }
}

/// Tower service that applies circuit breaker logic.
///
/// Created by [`CircuitBreakerLayer`].
#[derive(Clone)]
pub struct CircuitBreakerService<S> {
    inner: S,
    shared: Arc<SharedState>,
}

impl<S> Service<RouterRequest> for CircuitBreakerService<S>
where
    S: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send,
{
    type Response = RouterResponse;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let shared = self.shared.clone();
        let id = req.id.clone();

        let current_state = shared.state();

        match current_state {
            CircuitState::Open => {
                // Check if we should transition to HalfOpen
                if !shared.try_half_open() {
                    // Still open -- reject immediately
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id,
                            inner: Err(JsonRpcError::internal_error(
                                "Circuit breaker is open".to_string(),
                            )),
                        })
                    });
                }
                // Fell through to HalfOpen -- allow the probe request below
            }
            CircuitState::Closed | CircuitState::HalfOpen => {
                // Allow request
            }
        }

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.call(req).await?;
            if response.is_error() {
                shared.record_failure();
            } else {
                shared.record_success();
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tower::ServiceExt;

    use crate::protocol::{EmptyResult, McpResponse, RequestId};

    /// A test service that returns success or error based on a flag.
    #[derive(Clone)]
    struct MockService {
        should_fail: Arc<Mutex<bool>>,
    }

    impl Service<RouterRequest> for MockService {
        type Response = RouterResponse;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RouterRequest) -> Self::Future {
            let should_fail = *self.should_fail.lock().unwrap();
            Box::pin(async move {
                if should_fail {
                    Ok(RouterResponse {
                        id: req.id,
                        inner: Err(JsonRpcError::internal_error("test error")),
                    })
                } else {
                    Ok(RouterResponse {
                        id: req.id,
                        inner: Ok(McpResponse::Empty(EmptyResult {})),
                    })
                }
            })
        }
    }

    fn test_request(id: i64) -> RouterRequest {
        RouterRequest::new(RequestId::Number(id), crate::protocol::McpRequest::Ping)
    }

    #[tokio::test]
    async fn test_circuit_breaker_stays_closed_on_success() {
        let (layer, handle) = CircuitBreakerLayer::builder()
            .failure_threshold(3)
            .build_with_handle();

        let svc = layer.layer(MockService {
            should_fail: Arc::new(Mutex::new(false)),
        });

        let response = svc.oneshot(test_request(1)).await.unwrap();
        assert!(!response.is_error());
        assert_eq!(handle.state(), CircuitState::Closed);
        assert_eq!(handle.health_status(), "healthy");
        assert_eq!(handle.consecutive_failures(), 0);
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_threshold() {
        let (layer, handle) = CircuitBreakerLayer::builder()
            .failure_threshold(3)
            .build_with_handle();

        let should_fail = Arc::new(Mutex::new(true));
        let mock = MockService {
            should_fail: should_fail.clone(),
        };

        // Send 3 failing requests (threshold = 3)
        let mut svc = layer.layer(mock);
        for i in 0..3 {
            let response = svc.call(test_request(i)).await.unwrap();
            assert!(response.is_error());
        }

        assert_eq!(handle.state(), CircuitState::Open);
        assert_eq!(handle.health_status(), "open");

        // Next request should be rejected immediately (circuit is open)
        let response = svc.call(test_request(99)).await.unwrap();
        assert!(response.is_error());
    }

    #[tokio::test]
    async fn test_circuit_breaker_recovery() {
        let (layer, handle) = CircuitBreakerLayer::builder()
            .failure_threshold(2)
            .recovery_timeout(Duration::from_millis(10))
            .build_with_handle();

        let should_fail = Arc::new(Mutex::new(true));
        let mock = MockService {
            should_fail: should_fail.clone(),
        };
        let mut svc = layer.layer(mock);

        // Trip the breaker
        for i in 0..2 {
            let _ = svc.call(test_request(i)).await.unwrap();
        }
        assert_eq!(handle.state(), CircuitState::Open);

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Switch to success
        *should_fail.lock().unwrap() = false;

        // Next request should go through (half-open probe)
        let response = svc.call(test_request(10)).await.unwrap();
        assert!(!response.is_error());
        assert_eq!(handle.state(), CircuitState::Closed);
        assert_eq!(handle.health_status(), "healthy");
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_failure_reopens() {
        let (layer, handle) = CircuitBreakerLayer::builder()
            .failure_threshold(2)
            .recovery_timeout(Duration::from_millis(10))
            .build_with_handle();

        let should_fail = Arc::new(Mutex::new(true));
        let mock = MockService {
            should_fail: should_fail.clone(),
        };
        let mut svc = layer.layer(mock);

        // Trip the breaker
        for i in 0..2 {
            let _ = svc.call(test_request(i)).await.unwrap();
        }
        assert_eq!(handle.state(), CircuitState::Open);

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Probe request fails (still failing)
        let response = svc.call(test_request(10)).await.unwrap();
        assert!(response.is_error());
        assert_eq!(handle.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn test_build_without_handle() {
        let layer = CircuitBreakerLayer::builder()
            .failure_threshold(5)
            .recovery_timeout(Duration::from_secs(60))
            .build();

        let mock = MockService {
            should_fail: Arc::new(Mutex::new(false)),
        };
        let svc = layer.layer(mock);
        let response = svc.oneshot(test_request(1)).await.unwrap();
        assert!(!response.is_error());
    }
}
