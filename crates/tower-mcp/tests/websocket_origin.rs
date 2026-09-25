//! `Origin` validation on the WebSocket upgrade (#1464).
//!
//! Browsers do not apply CORS to WebSocket connections, so without this check
//! any web page could connect to a server bound to localhost. These tests dial
//! a real [`WebSocketTransport`] with a raw `tokio-tungstenite` client so the
//! handshake carries exactly the `Origin` a browser would send.

#![cfg(feature = "websocket")]

use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tower_mcp::{McpRouter, WebSocketTransport};

/// Serve `transport` on an ephemeral port and return its `ws://` URL.
async fn serve(transport: WebSocketTransport) -> String {
    let app = transport.into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{addr}/")
}

fn transport() -> WebSocketTransport {
    WebSocketTransport::new(McpRouter::new().server_info("origin-test", "1.0.0"))
}

/// Open a WebSocket to `url` with the given `Origin` (or none), returning the
/// handshake status: 101 on upgrade, otherwise the rejection status.
async fn handshake(url: &str, origin: Option<&str>) -> StatusCode {
    let mut request = url.into_client_request().unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((_, response)) => response.status(),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
        Err(error) => panic!("unexpected handshake error: {error}"),
    }
}

#[tokio::test]
async fn cross_origin_upgrade_is_rejected_without_an_allowlist() {
    let url = serve(transport()).await;
    assert_eq!(
        handshake(&url, Some("https://attacker.example")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn localhost_origins_are_upgraded() {
    let url = serve(transport()).await;
    for origin in [
        "http://localhost:3000",
        "http://127.0.0.1:8080",
        "http://[::1]:3000",
    ] {
        assert_eq!(
            handshake(&url, Some(origin)).await,
            StatusCode::SWITCHING_PROTOCOLS,
            "{origin}"
        );
    }
}

#[tokio::test]
async fn upgrade_without_an_origin_header_is_allowed() {
    // Non-browser clients send no Origin; they are not the threat this guards.
    let url = serve(transport()).await;
    assert_eq!(handshake(&url, None).await, StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn allowlisted_origin_is_upgraded_and_others_are_rejected() {
    let url = serve(transport().allowed_origins(vec!["https://app.example".to_string()])).await;
    assert_eq!(
        handshake(&url, Some("https://app.example")).await,
        StatusCode::SWITCHING_PROTOCOLS
    );
    assert_eq!(
        handshake(&url, Some("https://attacker.example")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn disabled_validation_upgrades_any_origin() {
    let url = serve(transport().disable_origin_validation()).await;
    assert_eq!(
        handshake(&url, Some("https://attacker.example")).await,
        StatusCode::SWITCHING_PROTOCOLS
    );
}
