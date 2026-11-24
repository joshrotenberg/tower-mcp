//! Legacy listener session loss (#1684). The peer hands each exchange to the
//! test, so request ordering is controlled without sleeping or polling.

use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
const SSE: &str = "Content-Type: text/event-stream\r\n";

struct Exchange {
    request: String,
    stream: TcpStream,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.request
            .lines()
            .take_while(|line| !line.is_empty())
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name).then(|| value.trim())
            })
    }

    async fn answer(mut self, response: String) {
        self.stream.write_all(response.as_bytes()).await.unwrap();
        self.stream.shutdown().await.unwrap();
    }

    async fn expire(self) {
        self.answer(raw_response("404 Not Found", "", "")).await;
    }

    async fn open_sse(&mut self) {
        self.stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    }
}

struct Peer {
    url: String,
    requests: mpsc::UnboundedReceiver<Exchange>,
}

impl Peer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let send = send.clone();
                tokio::spawn(async move {
                    if let Some(request) = read_http_request(&mut stream).await {
                        let _ = send.send(Exchange { request, stream });
                    }
                });
            }
        });
        Self { url, requests }
    }

    async fn next(&mut self) -> Exchange {
        timeout(WAIT, self.requests.recv())
            .await
            .expect("expected HTTP request")
            .unwrap()
    }

    async fn handshake(&mut self, session: &str) -> Exchange {
        let init = self.next().await;
        self.finish_handshake(init, session).await
    }

    async fn finish_handshake(&mut self, init: Exchange, session: &str) -> Exchange {
        assert!(raw_is(&init.request, "initialize"), "{}", init.request);
        assert_eq!(init.header("mcp-session-id"), None);
        assert_eq!(init.header("last-event-id"), None);
        let response = raw_initialize_ok(&init.request).replace("raw-session", session);
        init.answer(response).await;

        // The listener task can open its GET before or after the notification.
        let first = self.next().await;
        let second = self.next().await;
        let (get, ready) = if first.request.starts_with("GET ") {
            (first, second)
        } else {
            (second, first)
        };
        assert!(get.request.starts_with("GET "), "{}", get.request);
        assert_eq!(get.header("mcp-session-id"), Some(session));
        assert_eq!(get.header("last-event-id"), None);
        assert!(
            raw_is(&ready.request, "notifications/initialized"),
            "{}",
            ready.request
        );
        assert_eq!(ready.header("mcp-session-id"), Some(session));
        ready.answer(raw_response("202 Accepted", "", "")).await;
        get
    }

    async fn client(&self, notified: Arc<Notify>, recover: bool) -> McpClient {
        let handler = NotificationHandler::new().on_tools_changed(move || notified.notify_one());
        McpClient::connect_with_handler(
            HttpClientTransport::with_config(
                &self.url,
                HttpClientConfig {
                    sse_reconnect_delay: Duration::from_millis(5),
                    session_recovery: recover,
                    ..Default::default()
                },
            ),
            handler,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn initial_listener_404_restores_passive_notifications() {
    let mut peer = Peer::start().await;
    let notified = Arc::new(Notify::new());
    let client = peer.client(notified.clone(), true).await;
    let (info, get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.expire().await;

    let get = peer.handshake("second").await;
    get.answer(raw_response(
        "200 OK",
        SSE,
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
    ))
    .await;
    timeout(WAIT, notified.notified())
        .await
        .expect("replacement listener delivered a notification");
}

#[tokio::test]
async fn initial_listener_404_wakes_a_streamed_initialize() {
    let mut peer = Peer::start().await;
    let client = peer.client(Arc::new(Notify::new()), true).await;
    let server = async {
        let mut init = peer.next().await;
        assert!(raw_is(&init.request, "initialize"));
        init.stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nmcp-session-id: first\r\nConnection: close\r\n\r\n").await.unwrap();
        let get = peer.next().await;
        assert_eq!(get.header("mcp-session-id"), Some("first"));
        get.expire().await;
        let _replacement = peer.handshake("second").await;
    };
    let (result, ()) = timeout(WAIT, async {
        tokio::join!(client.initialize("client", "1"), server)
    })
    .await
    .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn resumed_listener_and_post_404_share_one_recovery_without_replaying_a_tool() {
    let mut peer = Peer::start().await;
    let client = Arc::new(peer.client(Arc::new(Notify::new()), true).await);
    let (info, mut get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.open_sse().await;

    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("charge", serde_json::json!({})).await }
    });
    let post = peer.next().await;
    assert!(raw_is(&post.request, "tools/call"));
    // The operation already executed. Its POST sends only a retry hint,
    // asking the client to collect its terminal response on the resumed GET.
    post.answer(raw_response("200 OK", SSE, "id: receipt\nretry: 5\n\n"))
        .await;
    let resume = peer.next().await;
    assert!(resume.request.starts_with("GET "));
    assert_eq!(resume.header("last-event-id"), Some("receipt"));
    assert_eq!(resume.header("mcp-session-id"), Some("first"));

    let mut ping = tokio::spawn({
        let client = client.clone();
        async move { client.ping().await }
    });
    let rejected = peer.next().await;
    assert!(raw_is(&rejected.request, "ping"));
    tokio::join!(resume.expire(), rejected.expire());
    let error = timeout(WAIT, call)
        .await
        .expect("pending tool woke promptly")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("outcome is unknown"), "{error}");
    let mut replacement = peer.handshake("second").await;
    replacement.open_sse().await;

    // The ping can be retried only if its own 404 was observed before the
    // listener expired. No other request may be resubmitted.
    tokio::select! {
        result = &mut ping => {
            assert!(result.unwrap().unwrap_err().to_string().contains("outcome is unknown"));
        }
        retry = peer.next() => {
            assert!(raw_is(&retry.request, "ping"), "unexpected replay: {}", retry.request);
            assert_eq!(retry.header("mcp-session-id"), Some("second"));
            let response = raw_response("200 OK", RAW_JSON, &format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, raw_request_id(&retry.request)));
            retry.answer(response).await;
            timeout(WAIT, ping).await.unwrap().unwrap().unwrap();
        }
    }
    // The peer has acknowledged initialized, but the independent recovery
    // task may still be reading that acknowledgment.
    timeout(WAIT, async {
        while !client.is_initialized() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement handshake committed");
    let check = async {
        let request = peer.next().await;
        assert!(
            raw_is(&request.request, "ping"),
            "unexpected replay: {}",
            request.request
        );
        assert_eq!(request.header("mcp-session-id"), Some("second"));
        let response = raw_response(
            "200 OK",
            RAW_JSON,
            &format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
                raw_request_id(&request.request)
            ),
        );
        request.answer(response).await;
    };
    let (result, ()) = timeout(WAIT, async { tokio::join!(client.ping(), check) })
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn expired_listener_without_recovery_fails_pending_request() {
    let mut peer = Peer::start().await;
    let client = peer.client(Arc::new(Notify::new()), false).await;
    let (info, get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();

    let server = async {
        let post = peer.next().await;
        assert!(raw_is(&post.request, "ping"));
        get.expire().await;
        let delete = peer.next().await;
        assert!(delete.request.starts_with("DELETE "), "{}", delete.request);
        delete.answer(raw_response("204 No Content", "", "")).await;
    };
    let (result, ()) = timeout(WAIT, async { tokio::join!(client.ping(), server) })
        .await
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("Session expired"));
}

#[tokio::test]
async fn immediate_expiry_of_replacement_listener_stops_recovery() {
    let mut peer = Peer::start().await;
    let client = peer.client(Arc::new(Notify::new()), true).await;
    let (info, get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.expire().await;
    peer.handshake("second").await.expire().await;
    let delete = peer.next().await;
    assert!(
        delete.request.starts_with("DELETE "),
        "unexpected extra handshake: {}",
        delete.request
    );
    delete.answer(raw_response("204 No Content", "", "")).await;
    assert!(!client.is_connected());
}

#[tokio::test]
async fn absent_optional_listener_keeps_the_original_session() {
    let mut peer = Peer::start().await;
    let client = peer.client(Arc::new(Notify::new()), true).await;
    let (info, get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.answer(raw_response("405 Method Not Allowed", "", ""))
        .await;
    let serve_ping = async {
        let ping = peer.next().await;
        assert!(raw_is(&ping.request, "ping"));
        assert_eq!(ping.header("mcp-session-id"), Some("first"));
        let response = raw_response(
            "200 OK",
            RAW_JSON,
            &format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
                raw_request_id(&ping.request)
            ),
        );
        ping.answer(response).await;
    };
    let (result, ()) = timeout(WAIT, async { tokio::join!(client.ping(), serve_ping) })
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn listener_recovery_validates_replacement_protocol_before_acknowledging_it() {
    let mut peer = Peer::start().await;
    let client = peer.client(Arc::new(Notify::new()), true).await;
    let (info, get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.expire().await;
    let init = peer.next().await;
    assert!(raw_is(&init.request, "initialize"));
    assert_eq!(init.header("mcp-session-id"), None);
    let reply = raw_initialize_ok(&init.request)
        .replace("2025-11-25", "unsupported")
        .replace("raw-session", "second");
    init.answer(reply).await;
    // The listener can race client-side validation of the initialize result.
    // Neither an initialized notification nor another handshake is valid here.
    let mut cleanup = peer.next().await;
    if cleanup.request.starts_with("GET ") {
        cleanup.expire().await;
        cleanup = peer.next().await;
    }
    assert!(
        cleanup.request.starts_with("DELETE "),
        "{}",
        cleanup.request
    );
    assert_eq!(cleanup.header("mcp-session-id"), Some("second"));
    cleanup.answer(raw_response("204 No Content", "", "")).await;
    assert!(!client.is_initialized());
    assert!(client.server_info().await.is_none());
}

#[tokio::test]
async fn cancelling_a_post_caller_during_recovery_still_restores_the_listener() {
    let mut peer = Peer::start().await;
    let notified = Arc::new(Notify::new());
    let client = Arc::new(peer.client(notified.clone(), true).await);
    let (info, mut get) = tokio::join!(client.initialize("client", "1"), peer.handshake("first"));
    info.unwrap();
    get.open_sse().await;

    let ping = tokio::spawn({
        let client = client.clone();
        async move { client.ping().await }
    });
    let rejected = peer.next().await;
    assert!(raw_is(&rejected.request, "ping"));
    rejected.expire().await;

    // Reset has happened and the replacement initialize is awaiting our reply.
    let init = peer.next().await;
    ping.abort();
    assert!(ping.await.unwrap_err().is_cancelled());
    let replacement = peer.finish_handshake(init, "second").await;
    replacement
        .answer(raw_response(
            "200 OK",
            SSE,
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
        ))
        .await;
    timeout(WAIT, notified.notified())
        .await
        .expect("replacement listener survived caller cancellation");
}
