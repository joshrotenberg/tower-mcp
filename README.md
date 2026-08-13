# tower-mcp

[![Crates.io](https://img.shields.io/crates/v/tower-mcp.svg)](https://crates.io/crates/tower-mcp)
[![Documentation](https://docs.rs/tower-mcp/badge.svg)](https://docs.rs/tower-mcp)
[![CI](https://github.com/joshrotenberg/tower-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/tower-mcp/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/tower-mcp.svg)](https://github.com/joshrotenberg/tower-mcp#license)
[![MSRV](https://img.shields.io/crates/msrv/tower-mcp.svg)](https://github.com/joshrotenberg/tower-mcp)
[![MCP](https://img.shields.io/badge/MCP-2026--07--28_%7C_2025--11--25-blue)](https://modelcontextprotocol.io/specification/2026-07-28)
[![Conformance](https://img.shields.io/badge/conformance-server_%2B_client-brightgreen)](https://github.com/joshrotenberg/tower-mcp/actions/workflows/conformance.yml)

A [Model Context Protocol](https://modelcontextprotocol.io) (MCP) implementation for Rust
built on [Tower](https://github.com/tower-rs/tower).

An MCP server is a `tower::Service`, so standard tower middleware (tracing, metrics,
timeouts, rate limiting, auth) composes with `.layer()` on the whole server or on an
individual tool. The same router serves over any transport, and the HTTP and WebSocket
transports are axum routers, so they drop into an existing axum application.

The reference documentation lives on [docs.rs](https://docs.rs/tower-mcp); this README
covers installation and a first server.

## Installation

```toml
[dependencies]
tower-mcp = "0.22"
schemars = "1"
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Tool input types derive `schemars::JsonSchema`, and the derive must come from the same
`schemars` major version tower-mcp uses (currently `1.x`). A version skew surfaces as an
opaque `ExtractorHandler` trait-bound error. To depend on schemars only through
tower-mcp's re-export, drop the direct `schemars` dependency and point the derive at it:

```rust
#[derive(Deserialize, tower_mcp::schemars::JsonSchema)]
#[schemars(crate = "tower_mcp::schemars")]
struct GreetInput { name: String }
```

## Example

A complete server with one tool, served over stdio:

```rust
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    /// Who to greet.
    name: String,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let greet = ToolBuilder::new("greet")
        .title("Greet")
        .description("Greet someone by name")
        .read_only()
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    let router = McpRouter::new()
        .server_info("my-server", "1.0.0")
        .tool(greet);

    StdioTransport::new(router).run().await?;
    Ok(())
}
```

The input schema is derived from `GreetInput`. Resources, prompts, router composition,
per-tool middleware, state, and the trait-based and macro tool forms are covered in the
[crate documentation](https://docs.rs/tower-mcp).

## Transports

The same `McpRouter` serves over any of these.

| Transport | Type | Feature |
|---|---|---|
| Stdio | `StdioTransport` | none |
| Streamable HTTP with SSE | `HttpTransport` | `http` |
| WebSocket | `WebSocketTransport` | `websocket` |
| Unix domain socket | `UnixSocketTransport` | `unix` |

For connecting to other MCP servers, `tower_mcp::client` provides stdio, HTTP, WebSocket,
and in-process channel client transports. `ChildProcessTransport` (feature `childproc`)
spawns a subprocess server and talks to it over its stdio.

## Feature Flags

No features are enabled by default.

| Feature | Description |
|---------|-------------|
| `full` | All optional features |
| `http` | HTTP transport with SSE (adds axum, hyper) |
| `websocket` | WebSocket transport |
| `unix` | Unix domain socket transport (requires `http`) |
| `childproc` | Child process transport for spawning subprocess MCP servers |
| `oauth` | OAuth 2.1 resource server: JWT validation, protected resource metadata (requires `http`) |
| `jwks` | JWKS endpoint fetching for remote key sets (requires `oauth`) |
| `http-client` | HTTP client transport |
| `oauth-client` | OAuth client: authorization code with PKCE, registration, refresh, scope escalation, client credentials, discovery, token providers (requires `http-client`) |
| `testing` | `TestClient` for in-process server testing |
| `dynamic-tools` | Runtime registration and deregistration of tools, prompts, and resources |
| `proxy` | Multi-server aggregation proxy (`McpProxy`) |
| `macros` | Proc macros (`#[tool_fn]`, `#[prompt_fn]`, `#[resource_fn]`, `#[resource_template_fn]`) |
| `resilience` | Re-export of tower-resilience circuit breaker, rate limiter, and bulkhead layers |
| `mcp-apps` | Typed server support for the MCP Apps extension; runtime advertisement stays explicit via `McpRouter::with_mcp_apps()` |
| `protocol-2026-07-28` | Compile the 2026-07-28 protocol implementation |
| `stateless` | Compatibility alias for `protocol-2026-07-28` |

### Types Only

[`tower-mcp-types`](https://crates.io/crates/tower-mcp-types) carries the protocol and
error types with no tower, tokio, or axum dependency (only `serde`, `serde_json`,
`thiserror`, `base64`). Use it for editor integrations, code generators, and protocol
validators. `tower-mcp` re-exports all of it, so there is no duplication if you use both.

```toml
[dependencies]
tower-mcp-types = "0.22"
```

## Documentation

| Guide | Covers |
|---|---|
| [Client usage](https://docs.rs/tower-mcp/latest/tower_mcp/guides/client/) | Transport selection, connecting, callbacks, requests, caching, retry policy |
| [HTTP deployment](https://docs.rs/tower-mcp/latest/tower_mcp/guides/deployment/) | Mounting, proxies and origins, session and scaling policy, middleware placement, timeouts |
| [Protocol versions](https://docs.rs/tower-mcp/latest/tower_mcp/guides/protocol_versions/) | Compile-time and runtime version support, lifecycle differences, interoperability, upgrades |
| [OAuth authorization](https://docs.rs/tower-mcp/latest/tower_mcp/guides/oauth/) | Protecting a resource server, interactive and service clients, registration and storage, production checklist |
| [MCP Apps](https://docs.rs/tower-mcp/latest/tower_mcp/guides/mcp_apps/) | Typed app resources from tools, with negotiation and fallback |

[`examples/`](examples/) holds the runnable programs, indexed in
[`examples/README.md`](examples/README.md). Start with
[`getting_started`](examples/getting_started.rs) for a stdio server,
[`http_server`](examples/http_server.rs) for HTTP,
[`axum_embedding`](examples/axum_embedding.rs) to mount MCP inside an existing axum app,
and [`middleware`](examples/middleware.rs) for layer composition.
[cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp) is a server built with
tower-mcp in a separate repository.

## Protocol Compliance

Every build implements the [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25)
and `2025-03-26` session protocols. The `protocol-2026-07-28` feature compiles the
[2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) implementation,
which is enabled at runtime once compiled; narrow the served set with
`ProtocolSupport::stable()`. HTTP dispatches per request on the `MCP-Protocol-Version`
header. See the
[protocol-version guide](https://docs.rs/tower-mcp/latest/tower_mcp/guides/protocol_versions/)
for the compile-time and runtime distinction and the upgrade path.

Across both revisions tower-mcp implements tools, resources (including templates and
subscriptions), prompts, completion, sampling, elicitation, roots, progress,
cancellation, logging, icons, implementation metadata, `_meta` on all protocol types,
session management, SSE event IDs with stream resumption, and extension declaration and
negotiation. Request batching is accepted on `2025-03-26` only, where the spec defines
it. The 2026-07-28 build adds sessionless dispatch, `server/discover`,
`subscriptions/listen`, per-request `_meta`, response-cache hints, Multi Round-Trip
Requests, and the Tasks extension (opt in with `McpRouter::with_tasks`).

Four of those are Deprecated by the specification as of 2026-07-28 and remain
supported here: sampling, logging, and roots (SEP-2577), and Dynamic Client
Registration (superseded by Client ID Metadata Documents, which this crate
already prefers). They become eligible for removal from the specification in the
first revision released on or after 2027-07-28. Nothing is scheduled for removal
from this crate; see the [deprecated
features](https://docs.rs/tower-mcp/latest/tower_mcp/#deprecated-by-the-specification)
section for the migration paths.

The [official MCP conformance suite](https://github.com/modelcontextprotocol/conformance)
runs on every PR in
[`conformance.yml`](https://github.com/joshrotenberg/tower-mcp/actions/workflows/conformance.yml),
covering server and client for both revisions. All four suites pass with zero failures
and empty expected-failure baselines, pinned to `conformance@0.2.0-alpha.10`. The suite
is maintained upstream and grows with the spec, so scenario and check counts move; the
workflow is the current result.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-Dwarnings" cargo doc --workspace --all-features --no-deps
cargo test --workspace --doc --all-features
```

Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).

MSRV is 1.90.

## License

MIT OR Apache-2.0
