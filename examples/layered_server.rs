//! Middleware Placement Example
//!
//! Demonstrates Tower middleware at every architectural level:
//! - **Transport level**: Global timeout, tracing for all requests
//! - **Per-tool**: Stricter timeout for external API calls
//! - **Per-resource**: Caching for slow-changing configuration
//! - **Per-prompt**: Metrics to track prompt usefulness
//!
//! ## Layer Composition
//!
//! Layers wrap handlers from outside-in. For a tool with both transport
//! and tool-level timeouts:
//!
//! ```text
//! Request → Transport(30s) → Router → Tool(10s) → Handler
//!                                       ↑
//!                           Inner layer fires first
//! ```
//!
//! This means per-component layers take precedence for things like timeouts.
//!
//! ## Running
//!
//! ```bash
//! cargo run --example layered_server --features http
//! ```
//!
//! ## When to Use Each Level
//!
//! | Level     | Use Case                                      |
//! |-----------|-----------------------------------------------|
//! | Transport | Global defaults, auth, tracing, rate limiting |
//! | Tool      | API timeouts, retries, circuit breakers       |
//! | Resource  | Caching, validation, access control           |
//! | Prompt    | Metrics, audit logging, A/B testing           |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower::{Layer, Service};
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder, StdioTransport,
    ToolBuilder,
};

// =============================================================================
// Custom Middleware: Metrics Layer
// =============================================================================
//
// Tracks call count and can be extended for latency histograms, error rates, etc.
// In production, you'd use tower-metrics or similar.

#[derive(Clone)]
struct MetricsLayer {
    name: String,
    counter: Arc<AtomicU64>,
}

impl MetricsLayer {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    fn count(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService {
            inner,
            name: self.name.clone(),
            counter: self.counter.clone(),
        }
    }
}

#[derive(Clone)]
struct MetricsService<S> {
    inner: S,
    name: String,
    counter: Arc<AtomicU64>,
}

impl<S, Request> Service<Request> for MetricsService<S>
where
    S: Service<Request> + Clone + Send + 'static,
    S::Future: Send,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let mut inner = self.inner.clone();
        let name = self.name.clone();
        let counter = self.counter.clone();

        Box::pin(async move {
            let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(name = %name, count = count, "metrics: request started");
            let result = inner.call(request).await;
            tracing::info!(name = %name, "metrics: request completed");
            result
        })
    }
}

// =============================================================================
// Custom Middleware: Simple Cache Layer
// =============================================================================
//
// Caches the last response for a configurable TTL. Real implementations would
// use moka, cached, or similar. This demonstrates the pattern.

#[derive(Clone)]
struct CacheLayer {
    ttl: Duration,
    cache: Arc<RwLock<Option<(std::time::Instant, String)>>>,
}

impl CacheLayer {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            cache: Arc::new(RwLock::new(None)),
        }
    }
}

impl<S> Layer<S> for CacheLayer {
    type Service = CacheService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CacheService {
            inner,
            ttl: self.ttl,
            cache: self.cache.clone(),
        }
    }
}

#[derive(Clone)]
struct CacheService<S> {
    inner: S,
    ttl: Duration,
    cache: Arc<RwLock<Option<(std::time::Instant, String)>>>,
}

// Note: This is a simplified cache that works with ReadResourceResult.
// A production cache would be generic over response types.

// =============================================================================
// Application State
// =============================================================================

/// Shared application state accessible by handlers
#[derive(Clone)]
struct AppState {
    /// Simulated external API client
    api_base_url: String,
    /// Metrics for tracking prompt usage
    prompt_metrics: Arc<PromptMetrics>,
}

#[derive(Default)]
struct PromptMetrics {
    analyze_calls: AtomicU64,
    summarize_calls: AtomicU64,
}

impl PromptMetrics {
    fn record(&self, prompt_name: &str) {
        match prompt_name {
            "analyze" => self.analyze_calls.fetch_add(1, Ordering::Relaxed),
            "summarize" => self.summarize_calls.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    fn report(&self) -> String {
        format!(
            "Prompt usage: analyze={}, summarize={}",
            self.analyze_calls.load(Ordering::Relaxed),
            self.summarize_calls.load(Ordering::Relaxed)
        )
    }
}

// =============================================================================
// Tool Inputs/Outputs
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchApiInput {
    /// API endpoint path (e.g., "/users/123")
    endpoint: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowOperationInput {
    /// How long to simulate work (seconds)
    duration_secs: u64,
}

// =============================================================================
// Tools
// =============================================================================

/// Tool that calls an external REST API - needs stricter timeout than default
fn build_fetch_api_tool(state: AppState) -> tower_mcp::Tool {
    ToolBuilder::new("fetch_api_data")
        .description(
            "Fetch data from external REST API. Has a 10-second timeout \
             (stricter than the global 30-second default) because external \
             APIs should respond quickly or fail fast.",
        )
        .handler_with_state(
            Arc::new(state),
            |state: Arc<AppState>, input: FetchApiInput| async move {
                // Simulate API call
                let url = format!("{}{}", state.api_base_url, input.endpoint);
                tracing::info!(url = %url, "fetching from external API");

                // In real code: reqwest::get(&url).await?
                tokio::time::sleep(Duration::from_millis(100)).await;

                Ok(CallToolResult::text(format!(
                    "Fetched data from {} (simulated)",
                    url
                )))
            },
        )
        // Per-tool layers: stricter timeout for external API calls
        // .layer(TimeoutLayer::new(Duration::from_secs(10)))
        // .layer(RetryLayer::new(3))  // retry transient failures
        .read_only()
        .build()
        .expect("valid tool")
}

/// Tool that simulates slow work - uses default transport timeout
fn build_slow_tool() -> tower_mcp::Tool {
    ToolBuilder::new("slow_operation")
        .description(
            "Simulates a slow operation. Uses the global 30-second timeout. \
             If duration_secs > 30, this will timeout.",
        )
        .handler(|input: SlowOperationInput| async move {
            tracing::info!(duration = input.duration_secs, "starting slow operation");
            tokio::time::sleep(Duration::from_secs(input.duration_secs)).await;
            Ok(CallToolResult::text(format!(
                "Completed after {} seconds",
                input.duration_secs
            )))
        })
        .build()
        .expect("valid tool")
}

// =============================================================================
// Resources
// =============================================================================

/// Resource that rarely changes - benefits from caching
fn build_config_resource() -> tower_mcp::Resource {
    ResourceBuilder::new("config://app/settings")
        .name("Application Settings")
        .description(
            "Application configuration. Cached for 1 hour since settings \
             rarely change. Cache reduces load on config storage.",
        )
        // Per-resource layer: cache for slow-changing data
        // .layer(CacheLayer::new(Duration::from_secs(3600)))
        .text(
            r#"{
  "feature_flags": {
    "new_ui": true,
    "beta_features": false
  },
  "limits": {
    "max_requests_per_minute": 100,
    "max_file_size_mb": 50
  }
}"#,
        )
        .build()
        .expect("valid resource")
}

/// Resource that changes frequently - no caching
fn build_status_resource() -> tower_mcp::Resource {
    ResourceBuilder::new("status://server/health")
        .name("Server Health")
        .description("Real-time server health. Not cached - always fresh data.")
        .text(
            r#"{
  "status": "healthy",
  "uptime_seconds": 12345,
  "active_connections": 42
}"#,
        )
        .build()
        .expect("valid resource")
}

// =============================================================================
// Prompts
// =============================================================================

/// Prompt with metrics tracking - we want to know how often it's used
fn build_analyze_prompt(metrics: Arc<PromptMetrics>) -> tower_mcp::Prompt {
    PromptBuilder::new("analyze_data")
        .description(
            "Guided data analysis prompt. Tracked with metrics to measure \
             usefulness and guide prompt improvements.",
        )
        .required_arg("data_description", "What kind of data are you analyzing?")
        .optional_arg("focus_area", "Specific aspect to focus on")
        // Per-prompt layer: track usage metrics
        // .layer(MetricsLayer::new("prompt.analyze_data"))
        .handler(move |args| {
            let metrics = metrics.clone();
            async move {
                metrics.record("analyze");

                let data_desc = args
                    .get("data_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown data");
                let focus = args
                    .get("focus_area")
                    .and_then(|v| v.as_str())
                    .unwrap_or("general patterns");

                Ok(tower_mcp::GetPromptResult {
                    description: Some(format!("Analysis prompt for: {}", data_desc)),
                    messages: vec![tower_mcp::protocol::PromptMessage {
                        role: tower_mcp::protocol::PromptRole::User,
                        content: tower_mcp::protocol::PromptContent::Text {
                            text: format!(
                                "I have {} that I'd like to analyze.\n\n\
                                 Please help me understand:\n\
                                 1. Key patterns and trends\n\
                                 2. Anomalies or outliers\n\
                                 3. Actionable insights\n\n\
                                 Focus especially on: {}",
                                data_desc, focus
                            ),
                        },
                    }],
                })
            }
        })
        .build()
        .expect("valid prompt")
}

/// Prompt without special tracking - standard prompt
fn build_summarize_prompt() -> tower_mcp::Prompt {
    PromptBuilder::new("summarize")
        .description("Simple summarization prompt. No special middleware.")
        .required_arg("content", "The content to summarize")
        .optional_arg("max_length", "Maximum summary length (short/medium/long)")
        .handler(|args| async move {
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("(no content provided)");
            let length = args
                .get("max_length")
                .and_then(|v| v.as_str())
                .unwrap_or("medium");

            let instruction = match length {
                "short" => "Summarize in 1-2 sentences.",
                "long" => "Provide a detailed summary with key points.",
                _ => "Summarize in a paragraph.",
            };

            Ok(tower_mcp::GetPromptResult {
                description: Some("Summarization prompt".to_string()),
                messages: vec![tower_mcp::protocol::PromptMessage {
                    role: tower_mcp::protocol::PromptRole::User,
                    content: tower_mcp::protocol::PromptContent::Text {
                        text: format!(
                            "Please summarize the following:\n\n{}\n\n{}",
                            content, instruction
                        ),
                    },
                }],
            })
        })
        .build()
        .expect("valid prompt")
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("layered_server=debug".parse()?)
                .add_directive("tower_mcp=info".parse()?),
        )
        .init();

    // Shared state
    let prompt_metrics = Arc::new(PromptMetrics::default());
    let state = AppState {
        api_base_url: "https://api.example.com".to_string(),
        prompt_metrics: prompt_metrics.clone(),
    };

    // Build components
    let fetch_api_tool = build_fetch_api_tool(state.clone());
    let slow_tool = build_slow_tool();
    let config_resource = build_config_resource();
    let status_resource = build_status_resource();
    let analyze_prompt = build_analyze_prompt(prompt_metrics.clone());
    let summarize_prompt = build_summarize_prompt();

    // Assemble router
    let router = McpRouter::new()
        .server_info("layered-server", "0.1.0")
        .instructions(
            "Demonstrates middleware at different levels:\n\
             - fetch_api_data: Has tool-level 10s timeout (external API)\n\
             - slow_operation: Uses default 30s transport timeout\n\
             - config://app/settings: Cached for 1 hour\n\
             - status://server/health: Never cached\n\
             - analyze_data prompt: Tracked with usage metrics\n\
             - summarize prompt: No special middleware",
        )
        .tool(fetch_api_tool)
        .tool(slow_tool)
        .resource(config_resource)
        .resource(status_resource)
        .prompt(analyze_prompt)
        .prompt(summarize_prompt);

    // Check for transport arg
    let use_http = std::env::args().any(|arg| arg == "--http");

    if use_http {
        // HTTP transport with global layers
        let transport = HttpTransport::new(router);
        // Transport-level layers apply to ALL requests
        // .layer(TimeoutLayer::new(Duration::from_secs(30)))
        // .layer(TraceLayer::new_for_http())

        let app = transport.into_router();
        let addr = "127.0.0.1:3000";
        tracing::info!("Starting layered server on http://{}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
    } else {
        // Stdio transport
        tracing::info!("Starting layered server on stdio");
        tracing::info!("Tip: Use --http for HTTP transport with more middleware options");
        StdioTransport::new(router).run().await?;
    }

    Ok(())
}

// =============================================================================
// Summary: Middleware Placement Decision Tree
// =============================================================================
//
// Q: Should this apply to ALL requests?
// └─ Yes → Transport level (.layer() on HttpTransport/WebSocketTransport)
//    └─ Examples: Global timeout, tracing, rate limiting, auth
//
// Q: Is this specific to ONE tool/resource/prompt?
// └─ Yes → Component level (.layer() on Builder)
//    └─ Tool: API timeouts, retries, circuit breakers
//    └─ Resource: Caching, validation, access control
//    └─ Prompt: Metrics, A/B testing, audit logging
//
// Q: Is this HTTP-specific (not MCP protocol)?
// └─ Yes → HTTP layer (.into_router().layer())
//    └─ Examples: CORS, compression, HTTP-level auth
//
// =============================================================================
