# OAuth authorization with tower-mcp

This guide explains how to assemble tower-mcp's OAuth resource-server and
client APIs. The [MCP 2026-07-28 authorization specification][mcp-auth] remains
the normative source for protocol behavior; this document focuses on library
choices, application policy, and runnable setup.

OAuth is optional in MCP and applies to HTTP-based transports. For stdio,
provide credentials through the process environment instead of running the MCP
HTTP authorization flow.

## Choose the application path

| You are building | tower-mcp entry point | Enable |
|---|---|---|
| Protected MCP HTTP server with local JWT keys | `HttpTransport::into_oauth_router` + `JwtValidator` | `oauth` |
| Protected MCP HTTP server with remote JWKS | `HttpTransport::into_oauth_router` + `JwksValidator` | `jwks` |
| Interactive desktop, CLI, or web client | `OAuthAuthorizationFlow` | `oauth-client` |
| Service-to-service client | `OAuthClientCredentials` | `oauth-client` |
| Existing identity or token system | `TokenValidator` or `TokenProvider` | `oauth` / `oauth-client` |

```toml
[dependencies]
tower-mcp = { version = "0.16", features = ["http", "jwks", "oauth-client"] }
```

The three OAuth roles stay separate:

- The MCP server is an OAuth **protected resource**. It publishes Protected
  Resource Metadata (PRM), validates access tokens, and enforces scopes.
- The MCP client is an OAuth **client**. It discovers metadata, obtains a client
  ID, drives authorization, stores tokens, and sends a bearer token on every
  MCP HTTP request.
- The authorization server authenticates the resource owner and issues tokens.
  tower-mcp deliberately does not implement an authorization server.

## Protect an MCP server

For production, prefer asymmetric access tokens and remote JWKS validation.
The canonical resource URI must be the externally visible identifier clients
use, including a path when that path distinguishes this MCP server.

```rust
use tower_mcp::{HttpTransport, McpRouter};
use tower_mcp::oauth::{
    JwksValidator, ProtectedResourceMetadata, ScopePolicy,
};

async fn protected_app(mcp: McpRouter) -> Result<axum::Router, tower_mcp::BoxError> {
let resource = "https://mcp.example.com/mcp";
let issuer = "https://login.example.com/tenant";

let validator = JwksValidator::builder(
        "https://login.example.com/tenant/.well-known/jwks.json",
    )
    .expected_issuer(issuer)
    .expected_audience(resource)
    .build()
    .await?;

let metadata = ProtectedResourceMetadata::new(resource)
    .authorization_server(issuer)
    .scope("mcp:read")
    .scope("mcp:write")
    .resource_documentation("https://docs.example.com/mcp/access");

// Tool-specific requirements are checked in addition to the default.
let scopes = ScopePolicy::new()
    .default_scope("mcp:read")
    .tool_scope("publish", "mcp:write")
    .resource_scope("secret://report", "reports:read");

let app = HttpTransport::new(mcp)
    .into_oauth_router_at("/mcp", validator, metadata, scopes)?;
Ok(app)
}
```

`into_oauth_router` and `into_oauth_router_at` are the safe composition paths.
They:

1. validate and serve path-aware [RFC 9728][rfc9728] metadata;
2. install `OAuthLayer` outside the HTTP transport to validate bearer tokens
   and the resource audience;
3. bridge validated `TokenClaims` into MCP request extensions; and
4. install fail-closed `ScopeEnforcementLayer` inside the transport for
   tool-, resource-, and prompt-specific policy.

Do not use `HttpTransport::oauth` alone for a protected endpoint. That
lower-level method publishes metadata but intentionally does not install token
or scope enforcement.

### Run the JWKS server example

The repository example uses the same composition and requires a real
authorization server that issues JWT access tokens:

```bash
export MCP_RESOURCE=http://127.0.0.1:3000
export OAUTH_ISSUER=https://login.example.com/tenant
export OAUTH_JWKS_URL=https://login.example.com/tenant/.well-known/jwks.json

cargo run --example http_auth --features jwks -- --auth jwks
```

Inspect the advertised metadata:

```bash
curl http://127.0.0.1:3000/.well-known/oauth-protected-resource
```

The example requires `mcp:read` for all operations and additionally requires
`mcp:write` for its `add` tool. Its `--auth oauth` mode uses a shared secret and
disables expiry validation for local experimentation only.

### JWT/JWKS validation policy

Configure both the validator and PRM from the same canonical resource and
issuer values. `OAuthLayer` independently checks that token `aud` contains the
PRM resource, while `expected_audience` and `expected_issuer` make the JWT
validator reject a token before it reaches MCP dispatch. Keep expiration
validation enabled.

`JwksValidator` caches keys, honors HTTP `Cache-Control: max-age`, uses a
five-minute fallback TTL, and refreshes after an unknown key ID with a minimum
refresh interval. Supply a custom `reqwest::Client` when the deployment needs
specific TLS roots, proxy behavior, or tighter timeouts.

`ScopePolicy` uses exact scope matching by default. If the authorization server
defines hierarchy, install an explicit matcher:

```rust
use tower_mcp::oauth::ScopePolicy;

let scopes = ScopePolicy::new()
    .scope_matcher(|granted: &str, required: &str| {
        granted == required || granted == "mcp:*"
    })
    .default_scope("mcp:read");
```

Keep this mapping authorization-server-specific. Do not infer hierarchy from
punctuation alone.

## Interactive authorization-code clients

`OAuthAuthorizationFlow` is a reusable state machine and token provider. It
implements discovery, client registration selection, PKCE S256, state and
callback validation, authorization-response issuer validation, resource
binding, token refresh, and runtime scope escalation.

For a native or CLI application, use a loopback redirect and let an
`OAuthAuthorizationHandler` present the URL to the user:

```rust
use async_trait::async_trait;
use tower_mcp::client::{
    HttpClientTransport, OAuthAuthorizationAction, OAuthAuthorizationFlow,
    OAuthAuthorizationHandler, OAuthAuthorizationRequest,
    OAuthClientRegistrationOptions, OAuthDynamicClientRegistration,
    OAuthRedirectPolicy, OAuthScopeEscalationConfig,
};

struct OpenBrowser;

#[async_trait]
impl OAuthAuthorizationHandler for OpenBrowser {
    async fn authorize(
        &self,
        request: OAuthAuthorizationRequest,
    ) -> Result<OAuthAuthorizationAction, tower_mcp::client::OAuthClientError> {
        // Open request.authorization_url with the platform browser here.
        println!("Authorize at: {}", request.authorization_url);
        Ok(OAuthAuthorizationAction::AwaitLoopback)
    }
}

async fn authorized_transport() -> Result<HttpClientTransport, tower_mcp::BoxError> {
let resource = "https://mcp.example.com/mcp";
let callback = "http://127.0.0.1:53682/oauth/callback";

let registration = OAuthClientRegistrationOptions::new()
    .with_client_id_metadata_document(
        "https://client.example.com/.well-known/mcp-oauth-client.json",
    )
    .with_dynamic_registration(
        OAuthDynamicClientRegistration::native("My MCP client", [callback])
            .grant_types(["authorization_code", "refresh_token"]),
    );

let flow = OAuthAuthorizationFlow::builder(resource)
    .registration_options(registration)
    .redirect_policy(OAuthRedirectPolicy::loopback_at(
        53682,
        "/oauth/callback",
    ))
    .authorization_handler(OpenBrowser)
    .build()?;

// An empty explicit set lets the flow prefer scopes from the initial
// WWW-Authenticate challenge, then PRM scopes_supported.
flow.authorize(std::iter::empty::<&str>()).await?;
let initial_scopes = flow.authorized_scopes().await.unwrap_or_default();

let transport = HttpClientTransport::new(resource)
    .with_scope_aware_token_provider(
        flow,
        OAuthScopeEscalationConfig::new(initial_scopes).max_attempts(2),
    );
Ok(transport)
}
```

The loopback listener is bound before the authorization URL is handed to the
application. The callback must contain the expected state, and the code is not
sent to the token endpoint until callback and issuer validation succeed. PKCE
uses S256; authorization servers that do not advertise it are rejected.

For a web application, use `OAuthRedirectPolicy::fixed`, store pending state in
a shared `OAuthAuthorizationStateStore`, and call
`OAuthPendingAuthorization::complete_callback_url` from the application's
callback route. Use `begin` instead of `authorize` when the UI needs explicit
`Authorized` versus `Pending` states.

### Run the interactive client example

The example uses a fixed loopback port. Configure that exact redirect URI in a
pre-registered client or Client ID Metadata Document:

```text
http://127.0.0.1:53682/oauth/callback
```

Then run one of these configurations:

```bash
# Pre-registered client (highest priority)
export MCP_SERVER_URL=https://mcp.example.com/mcp
export OAUTH_CLIENT_ID=my-client-id
export OAUTH_CLIENT_SECRET=my-client-secret # omit for a public client
cargo run --example oauth_client --features oauth-client -- \
  --mode authorization-code

# Client ID Metadata Document, with DCR retained as fallback
unset OAUTH_CLIENT_ID OAUTH_CLIENT_SECRET
export OAUTH_CLIENT_ID_METADATA_DOCUMENT=https://client.example.com/oauth/client.json
cargo run --example oauth_client --features oauth-client -- \
  --mode authorization-code
```

Leave `OAUTH_SCOPES` unset to follow challenge/PRM scope selection. Set it to a
space-separated list only when the application has an explicit operation plan.

### Registration policy

tower-mcp follows the final MCP selection priority:

1. pre-registered credentials when supplied;
2. a Client ID Metadata Document (CIMD) when configured and advertised;
3. Dynamic Client Registration (DCR) as a backwards-compatible fallback; then
4. an actionable error so the application can ask the user for credentials.

Use `pre_registered_client` when the client ID is known before discovery. Use
`OAuthClientRegistrationOptions` when configuring CIMD and DCR. DCR is
deprecated by the final MCP specification; new broadly distributed clients
should publish a CIMD, while enterprise deployments commonly use
pre-registration.

Pre-registered and dynamically registered credentials are bound to the exact
validated authorization-server issuer. `OAuthClientRegistrationStore`
implementations must key them by issuer and must never reuse them after PRM
selects a different authorization server. CIMD client IDs are portable HTTPS
URLs and are not issuer-bound.

When multiple authorization servers are advertised, set
`preferred_authorization_server` from trusted application/user policy. Without
it, the flow uses the first usable advertised server.

### Scope selection and step-up

With no explicit scopes, the flow uses this order:

1. `scope` from the initial `WWW-Authenticate` challenge;
2. `scopes_supported` from Protected Resource Metadata; then
3. authorization-server metadata as an interoperability fallback.

A non-empty scope list passed to `authorize` is an application override. Keep
it least-privileged. If the authorization server advertises both
`offline_access` and refresh-token support, the flow requests it and accepts
that a server may still decline to issue a refresh token.

`with_scope_aware_token_provider` handles a compliant runtime HTTP 403 Bearer
challenge with `error="insufficient_scope"`. It unions the challenged scopes
with the scopes already held, serializes concurrent reauthorization, and
retries the original operation up to the configured bound. The same
`OAuthAuthorizationFlow` must be installed as both token provider and
reauthorization handler so a new token becomes visible to the retry.

### Persistence and process topology

The default stores are in-memory and are suitable for examples and a
single-process desktop client. Production implementations normally replace all
three stores:

| Trait | Key/binding | Store securely |
|---|---|---|
| `OAuthClientRegistrationStore` | exact AS issuer | client ID, registration method, client secret |
| `OAuthTokenStore` | resource + issuer + client ID | access token, refresh token, scopes, expiry |
| `OAuthAuthorizationStateStore` | random state | PKCE verifier, redirect URI, issuer, resource, client registration |

Protect secrets at rest with the platform keychain, a secret manager, or
application-level authenticated encryption. Pending authorization state is
short-lived and single-use. A multi-instance web client needs shared,
atomic storage so the callback can land on a different instance without
reusing or losing state.

The `OAuthHttpClient` trait lets an application route all discovery,
registration, and token calls through its own HTTP stack. Redirects must remain
disabled: authorization endpoints are user-agent destinations, while metadata,
registration, and token endpoint redirects can cross trust boundaries.

## Service-to-service client credentials

For a client acting on its own behalf, use `OAuthClientCredentials`. Discovery
is preferred because it validates PRM/issuer metadata, binds the RFC 8707
resource, and selects a supported token-endpoint authentication method:

```rust
use tower_mcp::client::{HttpClientTransport, OAuthClientCredentials};

async fn service_transport() -> Result<HttpClientTransport, tower_mcp::BoxError> {
let provider = OAuthClientCredentials::discover(
    "https://mcp.example.com/mcp",
    "inventory-worker",
    "secret-from-vault",
)
.await?;

let transport = HttpClientTransport::new("https://mcp.example.com/mcp")
    .with_token_provider(provider);
Ok(transport)
}
```

The provider supports `client_secret_basic`, `client_secret_post`, and `none`
when selected by discovery or direct builder configuration. It caches access
tokens in memory and obtains a new client-credentials token before expiry;
client-credentials grants do not use refresh tokens. `private_key_jwt` is
supported by `OAuthAuthorizationFlow` when an `OAuthClientAssertionSigner` is
installed, not by `OAuthClientCredentials`.

Clients acting on their own behalf may abort rather than prompting on an
insufficient-scope response. The basic provider does not implement interactive
step-up; request the service's complete least-privileged scope set up front.

## Integrate a custom identity provider

tower-mcp keeps the application-specific seams explicit:

- Implement `TokenValidator` on the server for opaque-token introspection,
  session lookup, or a vendor SDK. Return normalized `TokenClaims`; the outer
  OAuth layer still performs resource-audience enforcement.
- Implement `TokenProvider` on a client when another component owns token
  acquisition or a workload identity system rotates tokens.
- Implement `OAuthAuthorizationHandler` to open a browser, display a device UI,
  or hand authorization to an application shell.
- Implement the three persistence traits to use a database or platform
  keychain, and `OAuthHttpClient` to apply corporate TLS/proxy policy.
- Implement `OAuthClientAssertionSigner` for `private_key_jwt`. Keep the private
  key in an HSM or secret service; the callback receives the exact issuer,
  token endpoint, client ID, audience, and assertion lifetime to sign.

Do not use a custom provider to skip PRM discovery, issuer binding, resource
indicators, callback validation, or audience checks. Those are protocol
security boundaries rather than identity-provider details.

## Production checklist

- Serve resource, metadata, authorization, token, registration, and CIMD HTTPS
  endpoints over authenticated TLS. Loopback redirects are the native-client
  exception described by [RFC 8252][rfc8252].
- Set PRM `resource` to the canonical public URI, not an internal service name;
  keep reverse-proxy path rewriting consistent with that identifier.
- Validate token signature, algorithm, issuer, audience, expiry, and any
  application-required claims. Never disable expiry validation in production.
- Use `into_oauth_router`/`into_oauth_router_at` and keep its middleware order.
  Preserve Host and Origin validation; configure explicit allowed values
  instead of calling `disable_origin_validation` outside local development.
- Keep PRM public, but do not expose broad path prefixes as unauthenticated.
- Require PKCE S256 and exact registered redirects. Keep `state`, PKCE verifier,
  authorization code, client secret, access token, and refresh token out of
  logs and error telemetry.
- Compare authorization-server issuer identifiers as exact strings. Do not
  normalize a trailing slash, case, port, or percent encoding before issuer
  comparison.
- Send the canonical `resource` in authorization and token requests and accept
  tokens only for that audience. Never pass through a token issued for an
  upstream service.
- Encrypt persistent credentials/tokens, use atomic single-use callback state,
  and partition all records by resource, issuer, and client.
- Configure a bounded scope-escalation retry count and require explicit user
  interaction when the application is acting on behalf of a person.
- Treat DCR as a compatibility fallback. Rate-limit registration and callback
  endpoints, and apply SSRF controls when fetching CIMD, PRM, AS metadata, or
  JWKS in custom infrastructure.
- Test missing, expired, wrong-issuer, wrong-audience, wrong-resource, missing
  scope, issuer-change, callback replay, and key-rotation cases before release.

## Normative references

- [MCP authorization (2026-07-28)][mcp-auth]
- [MCP authorization-server discovery][mcp-discovery]
- [MCP client registration][mcp-registration]
- [MCP authorization security considerations][mcp-security]
- [RFC 6750: Bearer Token Usage][rfc6750]
- [RFC 7591: Dynamic Client Registration][rfc7591]
- [RFC 7636: PKCE][rfc7636]
- [RFC 8252: OAuth for Native Apps][rfc8252]
- [RFC 8414: Authorization Server Metadata][rfc8414]
- [RFC 8707: Resource Indicators][rfc8707]
- [RFC 9207: Authorization Server Issuer Identification][rfc9207]
- [RFC 9700: OAuth 2.0 Security Best Current Practice][rfc9700]
- [RFC 9728: Protected Resource Metadata][rfc9728]
- [OAuth Client ID Metadata Document draft][cimd]

[mcp-auth]: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization
[mcp-discovery]: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/authorization-server-discovery
[mcp-registration]: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/client-registration
[mcp-security]: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/security-considerations
[rfc6750]: https://www.rfc-editor.org/rfc/rfc6750
[rfc7591]: https://www.rfc-editor.org/rfc/rfc7591
[rfc7636]: https://www.rfc-editor.org/rfc/rfc7636
[rfc8252]: https://www.rfc-editor.org/rfc/rfc8252
[rfc8414]: https://www.rfc-editor.org/rfc/rfc8414
[rfc8707]: https://www.rfc-editor.org/rfc/rfc8707
[rfc9207]: https://www.rfc-editor.org/rfc/rfc9207
[rfc9700]: https://www.rfc-editor.org/rfc/rfc9700
[rfc9728]: https://www.rfc-editor.org/rfc/rfc9728
[cimd]: https://datatracker.ietf.org/doc/draft-ietf-oauth-client-id-metadata-document/
