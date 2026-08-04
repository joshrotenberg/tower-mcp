# tower-mcp Examples

Focused examples demonstrating tower-mcp features. Each file is self-contained
and covers a specific topic.

If you are dropping tower-mcp into an existing axum application,
[`axum_embedding`](axum_embedding.rs) is the canonical pattern: it shows how
`HttpTransport::into_router()` composes with your own routes, state, and
tower middleware.

For a guided path, see
[client usage](https://docs.rs/tower-mcp/latest/tower_mcp/guides/client/),
[HTTP deployment](https://docs.rs/tower-mcp/latest/tower_mcp/guides/deployment/),
[protocol versions](https://docs.rs/tower-mcp/latest/tower_mcp/guides/protocol_versions/), and
[OAuth authorization](https://docs.rs/tower-mcp/latest/tower_mcp/guides/oauth/).

## Running Examples

```bash
# Stdio examples (no feature flags needed)
cargo run -p tower-mcp-examples --example getting_started
cargo run -p tower-mcp-examples --example middleware
cargo run -p tower-mcp-examples --example rate_limiting

# HTTP transport
cargo run -p tower-mcp-examples --example http_server --features http

# Typed MCP Apps server (stdio)
cargo run -p tower-mcp-examples --example mcp_apps --features mcp-apps

# Embed MCP inside an existing axum app
cargo run -p tower-mcp-examples --example axum_embedding --features http

# WebSocket transport
cargo run -p tower-mcp-examples --example websocket_server --features websocket

# Clients (start http_server first, or run stateless_http_client standalone)
cargo run -p tower-mcp-examples --example http_client --features http-client
cargo run -p tower-mcp-examples --example http_sse_client --features http
cargo run -p tower-mcp-examples --example stateless_http_client --features "http,http-client,protocol-2026-07-28"

# Released 2026-07-28 discovery and Tasks extension
cargo run -p tower-mcp-examples --example server_discover --features "http,protocol-2026-07-28"
cargo run -p tower-mcp-examples --example tasks --features protocol-2026-07-28

# OAuth resource server and clients (see the published OAuth guide for configuration)
cargo run -p tower-mcp-examples --example http_auth --features jwks -- --auth jwks
cargo run -p tower-mcp-examples --example oauth_client --features oauth-client -- --mode authorization-code
cargo run -p tower-mcp-examples --example oauth_client --features oauth-client -- --mode credentials

# Dynamic capabilities
cargo run -p tower-mcp-examples --example dynamic_capabilities --features dynamic-tools

# Testing
cargo run -p tower-mcp-examples --example testing --features testing

# Macros
cargo run -p tower-mcp-examples --example tool_macro --features macros
```

## Example Index

### Getting Started

| Example | What it shows |
|---------|---------------|
| [getting_started](getting_started.rs) | Tools, resources, prompts, and stdio transport |
| [mcp_apps](mcp_apps.rs) | Typed MCP Apps resources, tool linkage, visibility, negotiation, and text fallback |

### Transports

| Example | What it shows |
|---------|---------------|
| [http_server](http_server.rs) | HTTP/SSE transport with sessions, progress, resources |
| [websocket_server](websocket_server.rs) | WebSocket transport with sampling support |
| [unix_socket_server](unix_socket_server.rs) | Unix domain socket transport (Linux/macOS) |
| [horizontal_scaling](horizontal_scaling.rs) | Two HTTP instances sharing a session/event store behind a load balancer |
| [axum_embedding](axum_embedding.rs) | Canonical pattern for mounting MCP under `/mcp` inside an existing axum app, sharing middleware and state with non-MCP routes |

### Middleware

| Example | What it shows |
|---------|---------------|
| [middleware](middleware.rs) | All four levels: transport, per-tool, per-resource, per-prompt, and guards |
| [rate_limiting](rate_limiting.rs) | Per-tool rate limiting with tower-resilience |
| [capability_filtering](capability_filtering.rs) | Session-based tool/resource/prompt visibility |
| [tool_selection](tool_selection.rs) | Tool groups, safety tiers, allow/deny lists |

### Authentication

| Example | What it shows |
|---------|---------------|
| [http_auth](http_auth.rs) | API key, local JWT, and production JWKS resource-server auth with operation scopes |
| [oauth_client](oauth_client.rs) | Static tokens, client credentials, interactive authorization code with PKCE/scope escalation, and custom providers |
| [external_api_auth](external_api_auth.rs) | Downstream API authentication patterns |

See the [OAuth authorization guide](https://docs.rs/tower-mcp/latest/tower_mcp/guides/oauth/)
for end-to-end setup,
registration and storage policy, identity-provider integration, and the
production checklist.

### Clients

| Example | What it shows |
|---------|---------------|
| [client_cli](client_cli.rs) | Stdio client connecting to subprocess servers |
| [http_client](http_client.rs) | HTTP client with McpClient API |
| [http_sse_client](http_sse_client.rs) | SSE stream resumption (Last-Event-ID) |
| [server_discover](server_discover.rs) | Minimal 2026-07-28 `server/discover` server and response shape |
| [stateless_http_client](stateless_http_client.rs) | 2026-07-28 stateless protocol: server/discover, tools/list, tools/call, subscriptions/listen -- no session ID |

### Bidirectional Communication

| Example | What it shows |
|---------|---------------|
| [sampling_server](sampling_server.rs) | Server-to-client LLM requests and elicitation |
| [client_handler](client_handler.rs) | Client-side sampling and notification handling |
| [external_notifications](external_notifications.rs) | Pushing MCP notifications from background tasks via `HttpTransport::with_notifications` |

### Dynamic Capabilities

| Example | What it shows |
|---------|---------------|
| [dynamic_capabilities](dynamic_capabilities.rs) | Runtime tool/prompt/resource registration |

### Advanced Patterns

| Example | What it shows |
|---------|---------------|
| [proxy](proxy.rs) | Multi-server aggregation with namespace isolation |
| [resource_templates](resource_templates.rs) | URI template matching (RFC 6570) |
| [structured_output](structured_output.rs) | Typed JSON output with schemas |
| [error_handling](error_handling.rs) | Tool error patterns and propagation |
| [event_store](event_store.rs) | Custom `EventStore` for persistent SSE event buffering and cross-instance resumption |
| [session_store](session_store.rs) | Custom `SessionStore` for persistent session metadata |
| [testing](testing.rs) | In-process testing with TestClient |
| [tasks](tasks.rs) | 2026-07-28 Tasks extension lifecycle, notifications, cancellation, and ownership |
| [weather_server](weather_server.rs) | External API integration (NWS) |

### Macros

| Example | What it shows |
|---------|---------------|
| [tool_macro](tool_macro.rs) | `#[tool_fn]`, `#[prompt_fn]`, `#[resource_fn]` proc macros |

## Workspace Examples

These are standalone crates in the workspace:

| Crate | Purpose |
|-------|---------|
| [conformance-server](conformance-server/) | MCP conformance server (48/48 stable; 114/114 final) |
| [conformance-client](conformance-client/) | MCP conformance client (235/235 stable; 399/399 final) |
| [codegen-mcp](codegen-mcp/) | MCP server that helps build MCP servers |
| [mcp-repl](https://github.com/joshrotenberg/mcp-repl) | Interactive client REPL, now developed in its own repository |
| [mcp2md](mcp2md/) | Generate deterministic Markdown documentation and a documentation-coverage assessment from an MCP server |

## Full Application

For a complete production MCP server, see [cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp).
