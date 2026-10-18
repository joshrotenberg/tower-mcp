//! Transport diagnostics omit secrets without weakening error signalling.
//!
//! A single current-thread test owns the tracing dispatcher so background HTTP
//! tasks and callsite-interest caches cannot interfere with another capture.
#![cfg(all(feature = "http", feature = "http-client"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use tower_mcp::client::{HttpClientConfig, HttpClientTransport, HttpStatusDetails, McpClient};
use tower_mcp::{HttpTransport, McpRouter};

const SECRETS: &[&str] = &[
    "sentinel-user",
    "sentinel-password",
    "sentinel-path",
    "sentinel-query",
    "sentinel-fragment",
    "sentinel-redirect",
    "sentinel-body",
    "sentinel-header",
];

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        self.1.notify_one();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}

fn secret_url(base: &str) -> String {
    format!(
        "http://sentinel-user:sentinel-password@{}/sentinel-path?key=sentinel-query#sentinel-fragment",
        base.strip_prefix("http://").unwrap()
    )
}

fn config() -> HttpClientConfig {
    HttpClientConfig {
        auto_sse: false,
        request_timeout: Duration::from_secs(1),
        sse_reconnect: false,
        ..Default::default()
    }
}

fn assert_private(text: &str) {
    for (index, secret) in SECRETS.iter().enumerate() {
        assert!(
            !text.contains(secret),
            "credential sentinel {index} appeared in transport diagnostics"
        );
    }
}

async fn wait_for_log(logs: &Logs, message: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let changed = logs.1.notified();
            if String::from_utf8_lossy(&logs.0.lock().unwrap()).contains(message) {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("expected transport diagnostic was not logged");
}

async fn failed_initialization(url: &str, expected: &[&str]) {
    let client = McpClient::connect(HttpClientTransport::with_config(url, config()))
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.initialize("diagnostics", "1"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_private(&error.to_string());
    assert!(
        expected
            .iter()
            .any(|category| error.to_string().to_lowercase().contains(category)),
        "the transport must report a useful error category"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn credential_bearing_transport_failures_are_redacted() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Connection failure with credentials in every URL component.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    // Windows may reach the request deadline before its TCP stack reports
    // refusal. Both outcomes must remain useful and credential-free.
    failed_initialization(&secret_url(&unavailable), &["connect", "timeout"]).await;
    failed_initialization("http://[sentinel-query", &["configuration"]).await;

    let stalled = serve(
        axum::Router::new().fallback(|| async { std::future::pending::<&'static str>().await }),
    )
    .await;
    failed_initialization(&secret_url(&stalled.url), &["timeout"]).await;

    // A redirect error's source chain must not reveal its untrusted target.
    // Reserve the target port so another fixture cannot reuse it and turn a
    // cross-origin redirect into a same-origin redirect during this test.
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_origin = format!("http://{}", target_listener.local_addr().unwrap());
    let target = format!("{target_origin}/sentinel-redirect?key=sentinel-query");
    let redirect = serve(axum::Router::new().fallback(move || {
        let target = target.clone();
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", target)]) }
    }))
    .await;
    failed_initialization(&secret_url(&redirect.url), &["redirect"]).await;

    // Initial 404/500 failures must not echo URLs or raw HTML/text bodies.
    for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
        let server = serve(
            axum::Router::new()
                .fallback(move || async move { (status, "sentinel-body: reflected credentials") }),
        )
        .await;
        failed_initialization(&secret_url(&server.url), &[status.as_str()]).await;
    }

    // Authentication descriptions/metadata URLs are untrusted too. They must
    // remain available as structured protocol data, not diagnostic text.
    #[cfg(feature = "oauth-client")]
    {
        let server = serve(axum::Router::new().fallback(|| async {
            (StatusCode::FORBIDDEN, [("www-authenticate",
                "Bearer error=\"insufficient_scope\", scope=\"sentinel-header\", resource_metadata=\"https://example.test/sentinel-query\"")], "sentinel-body")
        })).await;
        failed_initialization(&secret_url(&server.url), &["insufficient_scope"]).await;
    }

    // Established requests use the background POST path. A transient HTTP
    // failure must preserve status details and allow a subsequent request.
    let reject = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rejecting = reject.clone();
    let router =
        HttpTransport::new(McpRouter::new())
            .into_router()
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let reject = rejecting.clone();
                    async move {
                        if reject.load(std::sync::atomic::Ordering::SeqCst) {
                            return axum::response::IntoResponse::into_response((
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("retry-after", "7")],
                                "sentinel-body: reflected credentials",
                            ));
                        }
                        next.run(req).await
                    }
                },
            ));
    let server = serve(router).await;
    // This server's MCP route is the root, so query credentials are sufficient
    // to exercise established-session errors without changing its routing.
    let endpoint = format!("{}/?key=sentinel-query", server.url);
    let client = McpClient::connect(HttpClientTransport::with_config(&endpoint, config()))
        .await
        .unwrap();
    client.initialize("diagnostics", "1").await.unwrap();
    reject.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = tokio::time::timeout(Duration::from_secs(5), client.list_tools())
        .await
        .unwrap()
        .unwrap_err();
    assert_private(&error.to_string());
    let details = HttpStatusDetails::from_error(&error).unwrap();
    assert_eq!(details.status, 503);
    assert_eq!(details.retry_after.as_deref(), Some("7"));
    reject.store(false, std::sync::atomic::Ordering::SeqCst);
    client.list_tools().await.unwrap();
    client.shutdown().await.unwrap();

    // The standalone notification GET uses another background task. A
    // rejected redirect there must also omit its URL from the SSE error log.
    let target = format!("{target_origin}/sentinel-redirect");
    let router =
        HttpTransport::new(McpRouter::new())
            .into_router()
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let target = target.clone();
                    async move {
                        if req.method() == axum::http::Method::GET {
                            return axum::response::IntoResponse::into_response((
                                StatusCode::TEMPORARY_REDIRECT,
                                [("location", target)],
                            ));
                        }
                        next.run(req).await
                    }
                },
            ));
    let notifications = serve(router).await;
    let client = McpClient::connect(HttpClientTransport::with_config(
        format!("{}/?key=sentinel-query", notifications.url),
        HttpClientConfig {
            auto_sse: true,
            ..config()
        },
    ))
    .await
    .unwrap();
    client.initialize("sse-diagnostics", "1").await.unwrap();
    wait_for_log(&logs, "SSE connection failed").await;
    client.shutdown().await.unwrap();

    let captured = logs.0.lock().unwrap();
    let captured = String::from_utf8_lossy(&captured);
    assert!(
        captured.contains("Transport send error"),
        "must capture failing requests"
    );
    assert!(
        captured.contains("HTTP error from server"),
        "must capture background failures"
    );
    assert_private(&captured);
}
