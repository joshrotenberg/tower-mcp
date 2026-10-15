//! `Host` validation on the WebSocket upgrade.
//!
//! `HttpTransport` checks `Host` as defense-in-depth alongside `Origin`
//! against DNS rebinding; the WebSocket upgrade applies the same policy and
//! the same `allowed_hosts` / `disable_host_validation` configuration. These
//! tests dial a real [`WebSocketTransport`] with a raw `tokio-tungstenite`
//! client so the handshake carries exactly the `Host` under test.

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
    WebSocketTransport::new(McpRouter::new().server_info("host-test", "1.0.0"))
}

/// Open a WebSocket to `url` with the given `Host` header, returning the
/// handshake status: 101 on upgrade, otherwise the rejection status.
async fn handshake(url: &str, host: &str) -> StatusCode {
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("host", host.parse().unwrap());
    match tokio_tungstenite::connect_async(request).await {
        Ok((_, response)) => response.status(),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
        Err(error) => panic!("unexpected handshake error: {error}"),
    }
}

#[tokio::test]
async fn disallowed_host_is_rejected() {
    let url = serve(transport().allowed_hosts(vec!["mcp.example.com".to_string()])).await;
    assert_eq!(
        handshake(&url, "attacker.example").await,
        StatusCode::BAD_REQUEST
    );
    // The port is part of the entry's match, as on `HttpTransport`.
    assert_eq!(
        handshake(&url, "mcp.example.com:9999").await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn allowed_host_is_upgraded() {
    let url = serve(transport().allowed_hosts(vec![
        "mcp.example.com".to_string(),
        "mcp.example.com:8443".to_string(),
    ]))
    .await;
    for host in ["mcp.example.com", "mcp.example.com:8443"] {
        assert_eq!(
            handshake(&url, host).await,
            StatusCode::SWITCHING_PROTOCOLS,
            "{host}"
        );
    }
}

#[tokio::test]
async fn localhost_hosts_are_upgraded_with_an_allowlist() {
    // Localhost is always accepted; the list adds hosts on top of it.
    let url = serve(transport().allowed_hosts(vec!["mcp.example.com".to_string()])).await;
    for host in ["localhost:3000", "127.0.0.1:8080", "[::1]:3000"] {
        assert_eq!(
            handshake(&url, host).await,
            StatusCode::SWITCHING_PROTOCOLS,
            "{host}"
        );
    }
}

#[tokio::test]
async fn default_policy_matches_http_transport() {
    // With no allowlist, non-localhost hosts are accepted: Host is only
    // enforced once `allowed_hosts` is configured, as on `HttpTransport`.
    let url = serve(transport()).await;
    for host in ["localhost:3000", "mcp.example.com"] {
        assert_eq!(
            handshake(&url, host).await,
            StatusCode::SWITCHING_PROTOCOLS,
            "{host}"
        );
    }
}

#[tokio::test]
async fn disabled_validation_upgrades_any_host() {
    let url = serve(
        transport()
            .allowed_hosts(vec!["mcp.example.com".to_string()])
            .disable_host_validation(),
    )
    .await;
    assert_eq!(
        handshake(&url, "attacker.example").await,
        StatusCode::SWITCHING_PROTOCOLS
    );
}
