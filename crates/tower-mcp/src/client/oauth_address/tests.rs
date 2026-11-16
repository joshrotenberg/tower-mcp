use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::client::oauth::OAuthBearerChallenge;
use crate::client::oauth_authcode::discover_oauth_authorization_with_policy;
use crate::client::oauth_flow::{
    OAuthAuthorizationFlow, OAuthHttpClient, OAuthHttpRequest, OAuthHttpResponse,
    OAuthRedirectPolicy,
};

// =============================================================================
// Fixtures
// =============================================================================

/// Name used for the MCP server in end-to-end tests. It resolves to
/// loopback through [`FixedLookup`], which must not widen the policy.
const SERVER_HOST: &str = "mcp.attacker.test";
/// Name standing in for an internal service that resolves to loopback.
const INTERNAL_HOST: &str = "internal.attacker.test";

/// Resolves fixed names; anything else fails like an unknown name.
struct FixedLookup(HashMap<String, Vec<IpAddr>>);

impl FixedLookup {
    fn loopback_names() -> Self {
        let loopback = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
        Self(HashMap::from([
            (SERVER_HOST.to_string(), loopback.clone()),
            (INTERNAL_HOST.to_string(), loopback),
            (
                "public.example.test".to_string(),
                vec!["93.184.216.34".parse().unwrap()],
            ),
            (
                "private.example.test".to_string(),
                vec!["10.1.2.3".parse().unwrap()],
            ),
        ]))
    }
}

impl HostLookup for FixedLookup {
    fn lookup(&self, host: String) -> LookupFuture {
        let result = self.0.get(&host).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown test host {host}"))
        });
        Box::pin(async move { result })
    }
}

fn test_policy() -> OAuthAddressPolicy {
    OAuthAddressPolicy::default().with_lookup(FixedLookup::loopback_names())
}

type Responder = Arc<dyn Fn(&str, u16) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

/// A loopback HTTP server that records `METHOD path` for every request and
/// answers from `respond(path, port)`.
struct RecordingServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RecordingServer {
    async fn start(respond: Responder) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                // Read the whole request, body included, so closing the
                // connection never resets it under the client.
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if request_complete(&bytes) {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                let mut parts = request
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split_ascii_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                recorded.lock().unwrap().push(format!("{method} {path}"));
                let (status, headers, body) = respond(&path, port);
                let mut response = format!("HTTP/1.1 {status} X\r\n");
                for (name, value) in headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str(&format!(
                    "content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                ));
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            port,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn requested(&self, path: &str) -> bool {
        self.requests()
            .iter()
            .any(|request| request.ends_with(&format!(" {path}")))
    }
}

/// Whether `bytes` holds a full request head plus its `content-length` body.
fn request_complete(bytes: &[u8]) -> bool {
    let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    bytes.len() >= end + 4 + length
}

fn json(body: serde_json::Value) -> (u16, Vec<(String, String)>, String) {
    (200, Vec::new(), body.to_string())
}

fn not_found() -> (u16, Vec<(String, String)>, String) {
    (404, Vec::new(), String::new())
}

fn redirect(location: String) -> (u16, Vec<(String, String)>, String) {
    (302, vec![("location".to_string(), location)], String::new())
}

fn as_metadata(issuer: &str) -> serde_json::Value {
    serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"]
    })
}

/// A discovery server whose path-aware PRM advertises `issuers` (each built
/// from the bound port) and which serves AS metadata for any
/// `/.well-known/oauth-authorization-server/<name>` request whose issuer is
/// one of them.
async fn discovery_server<F>(issuers: F) -> RecordingServer
where
    F: Fn(u16) -> Vec<String> + Send + Sync + 'static,
{
    RecordingServer::start(Arc::new(move |path, port| {
        let advertised = issuers(port);
        match path {
            "/.well-known/oauth-protected-resource/mcp" => json(serde_json::json!({
                "resource": format!("http://{SERVER_HOST}:{port}/mcp"),
                "authorization_servers": advertised,
            })),
            other => match other.strip_prefix("/.well-known/oauth-authorization-server/") {
                Some(name) => match advertised
                    .iter()
                    .find(|issuer| issuer.ends_with(&format!("/{name}")))
                {
                    Some(issuer) => json(as_metadata(issuer)),
                    None => not_found(),
                },
                None => not_found(),
            },
        }
    }))
    .await
}

fn server_url(port: u16) -> String {
    format!("http://{SERVER_HOST}:{port}/mcp")
}

/// The client `OAuthAuthorizationCode` builds by default: resolver plus
/// checked redirects.
fn default_client(policy: &OAuthAddressPolicy, server_url: &str) -> reqwest::Client {
    AddressGuard::new(policy, server_url, true)
        .client_builder(true)
        .build()
        .unwrap()
}

fn no_challenge() -> Option<OAuthBearerChallenge> {
    Some(OAuthBearerChallenge {
        error: None,
        scopes: Vec::new(),
        resource_metadata: None,
        error_description: None,
    })
}

fn assert_blocked(error: &OAuthClientError, host: &str) {
    let message = error.to_string();
    assert!(
        matches!(error, OAuthClientError::AddressBlocked(_)),
        "expected AddressBlocked, got {error:?}"
    );
    assert!(
        message.contains(host),
        "message should name {host}: {message}"
    );
    assert!(
        message.contains("allow_host"),
        "message should say how to allow the host: {message}"
    );
}

async fn check(
    policy: &OAuthAddressPolicy,
    server: &str,
    url: &str,
) -> Result<(), OAuthClientError> {
    AddressGuard::new(policy, server, true).check(url).await
}

// =============================================================================
// Classification
// =============================================================================

#[test]
fn classifies_blocked_ranges() {
    let cases = [
        ("127.0.0.1", AddressClass::Loopback),
        ("127.255.0.9", AddressClass::Loopback),
        ("::1", AddressClass::Loopback),
        ("169.254.169.254", AddressClass::LinkLocal),
        ("fe80::1", AddressClass::LinkLocal),
        ("10.0.0.1", AddressClass::Private),
        ("172.16.0.1", AddressClass::Private),
        ("172.31.255.255", AddressClass::Private),
        ("192.168.1.1", AddressClass::Private),
        ("100.64.0.1", AddressClass::Private),
        ("100.127.255.255", AddressClass::Private),
        ("fd00::1", AddressClass::Private),
        ("fc00::1", AddressClass::Private),
        ("0.0.0.0", AddressClass::Unspecified),
        ("::", AddressClass::Unspecified),
        ("224.0.0.1", AddressClass::Multicast),
        ("ff02::1", AddressClass::Multicast),
        ("255.255.255.255", AddressClass::Broadcast),
        ("192.0.2.1", AddressClass::Reserved),
        ("198.51.100.1", AddressClass::Reserved),
        ("203.0.113.1", AddressClass::Reserved),
        ("198.18.0.1", AddressClass::Reserved),
        ("240.0.0.1", AddressClass::Reserved),
        ("2001:db8::1", AddressClass::Reserved),
        ("fec0::1", AddressClass::Reserved),
        // IPv6 forms embedding a blocked IPv4 address.
        ("::ffff:127.0.0.1", AddressClass::Loopback),
        ("::ffff:169.254.169.254", AddressClass::LinkLocal),
        ("::ffff:10.0.0.1", AddressClass::Private),
        ("::ffff:0:a00:1", AddressClass::Private),
        ("::127.0.0.1", AddressClass::Loopback),
        ("64:ff9b::7f00:1", AddressClass::Loopback),
        ("64:ff9b::a9fe:a9fe", AddressClass::LinkLocal),
        ("2002:7f00:1::", AddressClass::Loopback),
        ("2002:c0a8:101::1", AddressClass::Private),
    ];
    for (address, expected) in cases {
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(classify(ip), expected, "{address}");
    }
}

#[test]
fn classifies_public_addresses() {
    for address in [
        "1.1.1.1",
        "93.184.216.34",
        "172.32.0.1",
        "100.128.0.1",
        "2606:4700:4700::1111",
        "::ffff:1.1.1.1",
        "2002:0101:0101::1",
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(classify(ip), AddressClass::Public, "{address}");
    }
}

#[test]
fn allow_cidr_parses_networks_and_rejects_garbage() {
    let policy = OAuthAddressPolicy::default()
        .allow_cidr("10.20.0.0/16")
        .unwrap()
        .allow_cidr("fd00::/8")
        .unwrap()
        .allow_cidr("192.168.1.7")
        .unwrap();
    assert!(policy.network_allows("10.20.3.4".parse().unwrap()));
    assert!(!policy.network_allows("10.21.0.1".parse().unwrap()));
    assert!(policy.network_allows("fd12::1".parse().unwrap()));
    assert!(policy.network_allows("192.168.1.7".parse().unwrap()));
    assert!(!policy.network_allows("192.168.1.8".parse().unwrap()));
    // An IPv4 network also matches its IPv4-mapped IPv6 form.
    assert!(policy.network_allows("::ffff:10.20.0.1".parse().unwrap()));

    for bad in ["10.0.0.0/33", "fd00::/129", "not-an-ip/8", "10.0.0.0/x", ""] {
        assert!(
            matches!(
                OAuthAddressPolicy::default().allow_cidr(bad),
                Err(OAuthClientError::BuildError(_))
            ),
            "{bad}"
        );
    }
}

// =============================================================================
// Pre-request check
// =============================================================================

#[tokio::test]
async fn blocks_non_public_literals_for_a_public_server() {
    let policy = test_policy();
    let server = "https://mcp.example.com/mcp";
    for (url, host) in [
        ("http://127.0.0.1:8080/.well-known/x", "127.0.0.1"),
        ("http://localhost:9000/", "localhost"),
        ("http://app.localhost/", "app.localhost"),
        (
            "http://169.254.169.254/latest/meta-data/",
            "169.254.169.254",
        ),
        ("http://10.0.0.1/", "10.0.0.1"),
        ("http://[fd00::1]:8443/", "fd00::1"),
        ("http://[fe80::1]/", "fe80::1"),
        ("http://[::ffff:127.0.0.1]/", "::ffff:127.0.0.1"),
        ("http://[::ffff:a9fe:a9fe]/", "::ffff:169.254.169.254"),
        ("http://0.0.0.0/", "0.0.0.0"),
        ("http://[::]/", "::"),
    ] {
        let error = check(&policy, server, url).await.unwrap_err();
        assert_blocked(&error, host);
    }
    check(&policy, server, "https://auth.example.com/")
        .await
        .expect_err("unknown names fail closed under the test lookup");
    check(&policy, server, "https://public.example.test/")
        .await
        .unwrap();
    check(&policy, server, "https://1.1.1.1/").await.unwrap();
}

#[tokio::test]
async fn a_name_resolving_to_loopback_is_blocked() {
    let policy = test_policy();
    let error = check(
        &policy,
        "https://mcp.example.com/mcp",
        &format!("http://{INTERNAL_HOST}:8080/as"),
    )
    .await
    .unwrap_err();
    assert_blocked(&error, INTERNAL_HOST);
    assert!(error.to_string().contains("127.0.0.1"), "{error}");

    let error = check(
        &policy,
        "https://mcp.example.com/mcp",
        "http://private.example.test/",
    )
    .await
    .unwrap_err();
    assert_blocked(&error, "private.example.test");
}

#[tokio::test]
async fn the_server_origin_is_exempt_but_not_its_other_ports() {
    let policy = test_policy();
    // The server name resolves to loopback; its own origin is exempt.
    let server = format!("http://{SERVER_HOST}:8080/mcp");
    check(
        &policy,
        &server,
        &format!("http://{SERVER_HOST}:8080/.well-known/oauth-protected-resource"),
    )
    .await
    .unwrap();
    // The same host on another port is not the server's origin, and the
    // server name's DNS answers do not widen the policy.
    let error = check(&policy, &server, &format!("http://{SERVER_HOST}:6379/"))
        .await
        .unwrap_err();
    assert_blocked(&error, SERVER_HOST);
    let error = check(&policy, &server, &format!("https://{SERVER_HOST}:8080/"))
        .await
        .unwrap_err();
    assert_blocked(&error, SERVER_HOST);
}

#[tokio::test]
async fn a_same_origin_private_issuer_is_allowed() {
    // rmcp #1393: a resource at a unique-local literal advertising its own
    // origin as the issuer.
    let policy = test_policy();
    check(
        &policy,
        "https://[fd00::1]:8443/mcp",
        "https://[fd00::1]:8443/.well-known/oauth-authorization-server",
    )
    .await
    .unwrap();
    check(
        &policy,
        "http://10.0.0.5:8443/mcp",
        "http://10.0.0.5:8443/.well-known/oauth-authorization-server",
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_loopback_server_may_reach_loopback_on_other_ports() {
    let policy = test_policy();
    for server in [
        "http://127.0.0.1:3000/mcp",
        "http://localhost:3000/mcp",
        "http://[::1]:3000/mcp",
    ] {
        check(&policy, server, "http://localhost:8080/")
            .await
            .unwrap();
        check(&policy, server, "http://127.0.0.1:8080/")
            .await
            .unwrap();
        check(&policy, server, "http://[::1]:8080/").await.unwrap();
        check(&policy, server, &format!("http://{INTERNAL_HOST}:8080/"))
            .await
            .unwrap();
        // Loopback trust does not extend to other classes.
        assert_blocked(
            &check(&policy, server, "http://10.0.0.1/")
                .await
                .unwrap_err(),
            "10.0.0.1",
        );
        assert_blocked(
            &check(&policy, server, "http://169.254.169.254/")
                .await
                .unwrap_err(),
            "169.254.169.254",
        );
    }
}

#[tokio::test]
async fn a_private_literal_server_may_reach_private_but_not_loopback() {
    let policy = test_policy();
    let server = "http://10.0.0.5:8443/mcp";
    check(&policy, server, "http://10.0.0.6/").await.unwrap();
    check(&policy, server, "http://192.168.1.1/").await.unwrap();
    check(&policy, server, "http://private.example.test/")
        .await
        .unwrap();
    for url in [
        "http://127.0.0.1/",
        "http://169.254.169.254/",
        "http://[fe80::1]/",
    ] {
        check(&policy, server, url).await.unwrap_err();
    }
}

#[tokio::test]
async fn the_allowlist_and_opt_out_permit_blocked_destinations() {
    let server = "https://mcp.example.com/mcp";
    let by_host = test_policy().allow_host("Internal.Attacker.Test.");
    check(&by_host, server, &format!("http://{INTERNAL_HOST}:8080/"))
        .await
        .unwrap();
    let by_literal_host = test_policy().allow_host("[::FFFF:7F00:1]");
    check(&by_literal_host, server, "http://[::ffff:127.0.0.1]/")
        .await
        .unwrap();
    let by_network = test_policy().allow_cidr("127.0.0.0/8").unwrap();
    check(
        &by_network,
        server,
        &format!("http://{INTERNAL_HOST}:8080/"),
    )
    .await
    .unwrap();
    check(&by_network, server, "http://[::ffff:127.0.0.1]/")
        .await
        .unwrap();
    // The allowlist is exact: other private destinations stay blocked.
    check(&by_network, server, "http://10.0.0.1/")
        .await
        .unwrap_err();

    let disabled = OAuthAddressPolicy::disabled();
    assert!(!disabled.is_enabled());
    for url in [
        "http://127.0.0.1/",
        "http://169.254.169.254/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        check(&disabled, server, url).await.unwrap();
    }
}

#[tokio::test]
async fn without_name_resolution_only_literals_and_localhost_are_checked() {
    // The mode used for custom `OAuthHttpClient` implementations.
    let guard = AddressGuard::new(&test_policy(), "https://mcp.example.com/mcp", false);
    guard
        .check(&format!("http://{INTERNAL_HOST}/"))
        .await
        .unwrap();
    guard.check("http://127.0.0.1/").await.unwrap_err();
    guard.check("http://localhost/").await.unwrap_err();
}

// =============================================================================
// Connect-time resolver and redirects
// =============================================================================

#[tokio::test]
async fn the_resolver_blocks_a_name_resolving_to_loopback_at_connect_time() {
    let server = RecordingServer::start(Arc::new(|_, _| not_found())).await;
    let policy = test_policy();
    let client = reqwest::Client::builder()
        .dns_resolver(policy.resolver("https://mcp.example.com/mcp"))
        .build()
        .unwrap();

    let error = client
        .get(format!("http://{INTERNAL_HOST}:{}/secret", server.port))
        .send()
        .await
        .unwrap_err();
    let blocked = address_blocked_in(&error).expect("resolver error in the source chain");
    assert_blocked(&blocked, INTERNAL_HOST);
    assert!(server.requests().is_empty(), "{:?}", server.requests());

    // The configured server's own host name resolves without a check.
    let client = reqwest::Client::builder()
        .dns_resolver(policy.resolver(&server_url(server.port)))
        .build()
        .unwrap();
    let response = client
        .get(format!("http://{SERVER_HOST}:{}/ok", server.port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(server.requested("/ok"));
}

#[tokio::test]
async fn redirects_to_blocked_addresses_are_not_followed() {
    let server = RecordingServer::start(Arc::new(|path, port| match path {
        "/to-literal" => redirect(format!("http://127.0.0.1:{port}/internal-literal")),
        "/to-name" => redirect(format!("http://{INTERNAL_HOST}:{port}/internal-name")),
        "/to-self" => redirect(format!("http://{SERVER_HOST}:{port}/landed")),
        _ => not_found(),
    }))
    .await;
    let policy = test_policy();
    let client = default_client(&policy, &server_url(server.port));

    let error = client
        .get(format!("http://{SERVER_HOST}:{}/to-literal", server.port))
        .send()
        .await
        .unwrap_err();
    assert_blocked(&address_blocked_in(&error).unwrap(), "127.0.0.1");

    let error = client
        .get(format!("http://{SERVER_HOST}:{}/to-name", server.port))
        .send()
        .await
        .unwrap_err();
    assert_blocked(&address_blocked_in(&error).unwrap(), INTERNAL_HOST);

    // A redirect back to the server's own origin is followed.
    client
        .get(format!("http://{SERVER_HOST}:{}/to-self", server.port))
        .send()
        .await
        .unwrap();

    assert!(!server.requested("/internal-literal"));
    assert!(!server.requested("/internal-name"));
    assert!(server.requested("/landed"));
}

// =============================================================================
// Discovery end to end
// =============================================================================

#[tokio::test]
async fn discovery_skips_a_loopback_resource_metadata_pointer() {
    let server = RecordingServer::start(Arc::new(|path, port| match path {
        "/internal-prm" => json(serde_json::json!({
            "resource": format!("http://{SERVER_HOST}:{port}/mcp"),
            "authorization_servers": [format!("http://{SERVER_HOST}:{port}/evil")]
        })),
        "/.well-known/oauth-protected-resource/mcp" => json(serde_json::json!({
            "resource": format!("http://{SERVER_HOST}:{port}/mcp"),
            "authorization_servers": [format!("http://{SERVER_HOST}:{port}/as")]
        })),
        "/.well-known/oauth-authorization-server/as" => {
            json(as_metadata(&format!("http://{SERVER_HOST}:{port}/as")))
        }
        _ => not_found(),
    }))
    .await;
    let port = server.port;
    let policy = test_policy();
    let url = server_url(port);
    let challenge = Some(OAuthBearerChallenge {
        error: None,
        scopes: Vec::new(),
        resource_metadata: Some(format!("http://127.0.0.1:{port}/internal-prm")),
        error_description: None,
    });

    // The same-origin issuer on a name that resolves to loopback is allowed:
    // the server's own origin is exempt.
    let discovery = discover_oauth_authorization_with_policy(
        &url,
        challenge,
        &default_client(&policy, &url),
        &policy,
    )
    .await
    .unwrap();

    assert_eq!(
        discovery.authorization_servers[0].issuer,
        format!("http://{SERVER_HOST}:{port}/as")
    );
    assert!(
        !server.requested("/internal-prm"),
        "{:?}",
        server.requests()
    );
}

#[tokio::test]
async fn discovery_rejects_blocked_issuers() {
    for (issuer, host) in [
        // A private literal: never contacted, so its port is irrelevant.
        ("http://192.168.77.1/as".to_string(), "192.168.77.1"),
        (
            "http://[::ffff:127.0.0.1]:{port}/as".to_string(),
            "::ffff:127.0.0.1",
        ),
        ("http://127.0.0.1:{port}/as".to_string(), "127.0.0.1"),
        (format!("http://{INTERNAL_HOST}:{{port}}/as"), INTERNAL_HOST),
    ] {
        let template = issuer.clone();
        let server =
            discovery_server(move |port| vec![template.replace("{port}", &port.to_string())]).await;
        let policy = test_policy();
        let url = server_url(server.port);

        let error = discover_oauth_authorization_with_policy(
            &url,
            no_challenge(),
            &default_client(&policy, &url),
            &policy,
        )
        .await
        .unwrap_err();

        assert_blocked(&error, host);
        assert!(
            !server.requested("/.well-known/oauth-authorization-server/as"),
            "{issuer}: {:?}",
            server.requests()
        );
    }
}

#[tokio::test]
async fn discovery_honors_the_allowlist_and_opt_out() {
    let server = discovery_server(|port| {
        vec![
            format!("http://{INTERNAL_HOST}:{port}/as-internal"),
            format!("http://127.0.0.1:{port}/as-literal"),
        ]
    })
    .await;
    let port = server.port;
    let url = server_url(port);

    // Default policy: both issuers are blocked.
    let policy = test_policy();
    let error = discover_oauth_authorization_with_policy(
        &url,
        no_challenge(),
        &default_client(&policy, &url),
        &policy,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, OAuthClientError::AddressBlocked(_)),
        "{error:?}"
    );

    // Allowing the internal name admits only that issuer.
    let policy = test_policy().allow_host(INTERNAL_HOST);
    let discovery = discover_oauth_authorization_with_policy(
        &url,
        no_challenge(),
        &default_client(&policy, &url),
        &policy,
    )
    .await
    .unwrap();
    let issuers: Vec<_> = discovery
        .authorization_servers
        .iter()
        .map(|metadata| metadata.issuer.clone())
        .collect();
    assert_eq!(
        issuers,
        vec![format!("http://{INTERNAL_HOST}:{port}/as-internal")]
    );

    // Allowing the loopback network admits both.
    let policy = test_policy().allow_cidr("127.0.0.0/8").unwrap();
    let discovery = discover_oauth_authorization_with_policy(
        &url,
        no_challenge(),
        &default_client(&policy, &url),
        &policy,
    )
    .await
    .unwrap();
    assert_eq!(discovery.authorization_servers.len(), 2);

    // Opting out admits both. The disabled policy installs no resolver, so
    // map the test names with reqwest's own override.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let client = reqwest::Client::builder()
        .resolve(SERVER_HOST, addr)
        .resolve(INTERNAL_HOST, addr)
        .build()
        .unwrap();
    let discovery = discover_oauth_authorization_with_policy(
        &url,
        no_challenge(),
        &client,
        &OAuthAddressPolicy::disabled(),
    )
    .await
    .unwrap();
    assert_eq!(discovery.authorization_servers.len(), 2);
}

#[tokio::test]
async fn discovery_rejects_a_redirect_to_a_private_address() {
    let server = RecordingServer::start(Arc::new(|path, port| match path {
        "/.well-known/oauth-protected-resource/mcp" => {
            redirect(format!("http://127.0.0.1:{port}/internal"))
        }
        "/.well-known/oauth-protected-resource" => {
            redirect(format!("http://{INTERNAL_HOST}:{port}/internal-name"))
        }
        _ => not_found(),
    }))
    .await;
    let policy = test_policy();
    let url = server_url(server.port);

    let error = discover_oauth_authorization_with_policy(
        &url,
        no_challenge(),
        &default_client(&policy, &url),
        &policy,
    )
    .await
    .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("127.0.0.1"), "{message}");
    assert!(message.contains(INTERNAL_HOST), "{message}");
    assert!(!server.requested("/internal"));
    assert!(!server.requested("/internal-name"));
}

// =============================================================================
// OAuthAuthorizationFlow
// =============================================================================

/// Records request URLs and answers the probe with a challenge pointing at
/// cloud metadata, and the well-known PRM with a link-local issuer.
#[derive(Clone, Default)]
struct MetadataPointingHttp {
    urls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl OAuthHttpClient for MetadataPointingHttp {
    async fn execute(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<OAuthHttpResponse, OAuthClientError> {
        self.urls.lock().unwrap().push(request.url.clone());
        let path = reqwest::Url::parse(&request.url)
            .unwrap()
            .path()
            .to_string();
        let (status, headers, body) = match path.as_str() {
            "/mcp" => (
                401,
                vec![(
                    "www-authenticate".to_string(),
                    r#"Bearer resource_metadata="http://169.254.169.254/latest/meta-data/""#
                        .to_string(),
                )],
                String::new(),
            ),
            "/.well-known/oauth-protected-resource/mcp" => (
                200,
                Vec::new(),
                serde_json::json!({
                    "resource": "https://mcp.example.com/mcp",
                    "authorization_servers": ["http://[::ffff:a9fe:a9fe]"]
                })
                .to_string(),
            ),
            _ => (404, Vec::new(), String::new()),
        };
        Ok(OAuthHttpResponse {
            status,
            headers,
            body: body.into_bytes(),
        })
    }
}

#[tokio::test]
async fn the_flow_checks_requests_made_through_a_custom_http_client() {
    let http = MetadataPointingHttp::default();
    let flow = OAuthAuthorizationFlow::builder("https://mcp.example.com/mcp")
        .http_client(http.clone())
        .redirect_policy(OAuthRedirectPolicy::fixed("https://app.example.com/cb"))
        .build()
        .unwrap();

    let error = flow.begin(["mcp"]).await.unwrap_err();

    assert_blocked(&error, "::ffff:169.254.169.254");
    let urls = http.urls.lock().unwrap().clone();
    assert!(
        urls.iter()
            .all(|url| url.starts_with("https://mcp.example.com/")),
        "{urls:?}"
    );
}

#[tokio::test]
async fn the_flow_default_client_blocks_a_name_resolving_to_loopback() {
    let server = RecordingServer::start(Arc::new(|path, port| match path {
        "/mcp" => (
            401,
            vec![(
                "www-authenticate".to_string(),
                format!(
                    r#"Bearer resource_metadata="http://{SERVER_HOST}:{port}/.well-known/oauth-protected-resource/mcp""#
                ),
            )],
            String::new(),
        ),
        "/.well-known/oauth-protected-resource/mcp" => json(serde_json::json!({
            "resource": format!("http://{SERVER_HOST}:{port}/mcp"),
            "authorization_servers": [format!("http://{INTERNAL_HOST}:{port}/as")]
        })),
        "/.well-known/oauth-authorization-server/as" => {
            json(as_metadata(&format!("http://{INTERNAL_HOST}:{port}/as")))
        }
        _ => not_found(),
    }))
    .await;
    let flow = OAuthAuthorizationFlow::builder(server_url(server.port))
        .address_policy(test_policy())
        .redirect_policy(OAuthRedirectPolicy::fixed("https://app.example.com/cb"))
        .build()
        .unwrap();

    let error = flow.begin(["mcp"]).await.unwrap_err();

    assert_blocked(&error, INTERNAL_HOST);
    assert!(server.requested("/mcp"));
    assert!(server.requested("/.well-known/oauth-protected-resource/mcp"));
    assert!(
        !server.requested("/.well-known/oauth-authorization-server/as"),
        "{:?}",
        server.requests()
    );
}
