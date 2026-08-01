#![doc = r####"
# Client guide

This guide covers the choices an application makes when using tower-mcp as an
MCP client: transport, lifecycle, callbacks, caching, retries, and the common
request helpers. For protocol requirements, use the versioned MCP
specification rather than this guide:

- [2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [2025-11-25 transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [2026-07-28 versioning and compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)
- [2026-07-28 discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)

For authorization-code, client-credentials, and custom token-provider setup,
continue with the [OAuth authorization guide](crate::guides::oauth).

## Choose a transport

| Situation | Transport | Cargo feature | Notes |
|---|---|---|---|
| Your application launches a local MCP server | `StdioClientTransport` | none | The normal choice for CLI tools and editor-managed subprocesses. Keep protocol messages on stdout and server logs on stderr. |
| Your application connects to a remote Streamable HTTP endpoint | `HttpClientTransport` | `http-client` | Supports JSON and SSE responses, legacy sessions and resumption, final-protocol headers, and pluggable authentication. |
| You have an application-specific connection | implement `ClientTransport` | none | Useful for in-memory channels or a custom framing/connection layer. |

tower-mcp does not currently provide a WebSocket client transport. The
`websocket` feature is server-side.

Minimal dependencies for a remote client:

```toml
[dependencies]
tower-mcp = { version = "0.16", features = ["http-client"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde_json = "1"
```

## Connect and select a lifecycle

Connecting starts the background message loop; it does not perform MCP
lifecycle negotiation. Call exactly one lifecycle method before normal
operations:

| Protocol path | Builder default? | First MCP call |
|---|---:|---|
| `2025-11-25` / `2025-03-26` | yes | `client.initialize(name, version)` |
| `2026-07-28` | no | `client.discover(name, version)` |

### Stable lifecycle over stdio

```rust,no_run
use tower_mcp::BoxError;
use tower_mcp::client::{McpClient, StdioClientTransport};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let transport = StdioClientTransport::spawn("my-mcp-server", &[]).await?;
    let client = McpClient::connect(transport).await?;

    let initialized = client.initialize("my-client", env!("CARGO_PKG_VERSION")).await?;
    println!("connected to {}", initialized.server_info.name);

    let tools = client.list_all_tools().await?;
    println!("{} tools", tools.len());

    client.shutdown().await?;
    Ok(())
}
```

Use `spawn_command` when you need environment variables, a working directory,
or piped process settings. `spawn` is the compact convenience API.

### Stable lifecycle over HTTP

```rust,no_run
use tower_mcp::BoxError;
use tower_mcp::client::{HttpClientTransport, McpClient};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let transport = HttpClientTransport::new("https://mcp.example.com/mcp");
    let client = McpClient::connect(transport).await?;
    client.initialize("my-client", "1.0.0").await?;

    let result = client
        .call_tool_text("search", serde_json::json!({ "query": "tower" }))
        .await?;
    println!("{result}");

    client.shutdown().await?;
    Ok(())
}
```

For a static bearer token, API key, or basic credentials, configure the
`HttpClientTransport` before connecting. For token acquisition and refresh,
use a token provider from the [OAuth guide](crate::guides::oauth); avoid copying tokens
into custom headers yourself.

### Released 2026-07-28 lifecycle

Compile the implementation and select it independently at runtime:

```toml
tower-mcp = { version = "0.16", features = [
  "http-client",
  "protocol-2026-07-28",
] }
```

```rust,no_run
use tower_mcp::{BoxError, ProtocolSupport};
use tower_mcp::client::{HttpClientTransport, McpClient};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let support = ProtocolSupport::try_new(["2026-07-28"])?;
    let client = McpClient::builder()
        .protocol_support(support)
        .connect_simple(HttpClientTransport::new("https://mcp.example.com/mcp"))
        .await?;

    let discovered = client.discover("my-client", "1.0.0").await?;
    println!("server supports {:?}", discovered.supported_versions);

    let tools = client.list_all_tools().await?;
    println!("{} tools", tools.len());

    client.shutdown().await?;
    Ok(())
}
```

Do not call `initialize` on this path. `discover` selects a mutually supported
modern version, and the client then adds the required per-request `_meta` and
HTTP headers. See the [protocol-version guide](crate::guides::protocol_versions) for
stable-only, final-only, and dual-era policies.

## Handle callbacks and server requests

`connect_simple` (and `McpClient::connect`) uses the unit handler. It ignores
ordinary notifications and rejects server-initiated requests. Choose a richer
handler only when the host application is ready to honor the capability it
advertises.

For notification callbacks:

```rust,no_run
use tower_mcp::BoxError;
use tower_mcp::client::{
    McpClient, NotificationHandler, StdioClientTransport,
};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let handler = NotificationHandler::with_log_forwarding()
        .on_progress(|progress| {
            eprintln!("{}", progress.message.as_deref().unwrap_or("working"));
        })
        .on_tools_changed(|| eprintln!("tool list changed"));

    let transport = StdioClientTransport::spawn("my-mcp-server", &[]).await?;
    let client = McpClient::connect_with_handler(transport, handler).await?;
    client.initialize("my-client", "1.0.0").await?;
    client.shutdown().await?;
    Ok(())
}
```

Implement `ClientHandler` when the server may request roots, sampling, or
elicitation. Pair the implementation with the matching builder call:

- `with_roots(...)` declares roots and answers `roots/list` from the configured list.
- `with_sampling()` must be paired with `handle_create_message`.
- `with_elicitation()` must be paired with `handle_elicit` and a real consent UI.

The [client handler example](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/client_handler.rs) contains a complete
implementation. On the 2026-07-28 path, the same handler resolves embedded
Multi Round-Trip Request inputs; `max_mrtr_rounds` bounds the automatic loop
and defaults to eight.

## Common request patterns

Prefer the all-pages helpers unless you are exposing pagination to your own
caller:

```rust,ignore
let tools = client.list_all_tools().await?;
let resources = client.list_all_resources().await?;
let templates = client.list_all_resource_templates().await?;
let prompts = client.list_all_prompts().await?;
```

Use the single-page methods (`list_tools`, `list_tools_with_cursor`, and their
resource/prompt equivalents) when you want to control page fetches.

`call_tool` preserves every content item and the protocol-level `is_error`
flag. `call_tool_text` concatenates text items and converts a tool error result
into a Rust error:

```rust,ignore
let full = client
    .call_tool("render", serde_json::json!({ "format": "svg" }))
    .await?;

for item in &full.content {
    println!("{item:?}");
}

let text = client
    .call_tool_text("status", serde_json::json!({}))
    .await?;
```

`read_resource`, `get_prompt`, and `call_tool` automatically follow supported
2026-07-28 MRTR inputs through the configured handler. Their `*_once` variants
expose `RequestOutcome` when the application needs to own the retry and user
interaction. `call_tool` also waits for a final task result; use
`call_tool_once_task_aware` or the task methods when you need direct lifecycle
control.

## Response caching

The built-in cache is consulted only after the 2026-07-28 lifecycle is active.
It covers `server/discover`, list operations, and `resources/read` according to
the server's `ttlMs` and `cacheScope` hints.

```rust,no_run
use std::time::Duration;
use tower_mcp::client::{ClientCacheConfig, McpClient};

let cache = ClientCacheConfig::default()
    .with_partition("tenant-42:user-7")
    .with_max_ttl(Duration::from_secs(60 * 60))
    .with_max_resource_entries(256);

let builder = McpClient::builder().response_cache(cache);
```

Important defaults and boundaries:

- caching is enabled, but a result with no `ttlMs` has a zero fallback TTL;
- server TTLs are capped at 24 hours by default;
- at most 512 distinct resource reads are retained by default;
- stale data is not served after a refresh error unless explicitly enabled;
- private cache entries are isolated by the configured partition;
- list/resource change notifications invalidate matching entries;
- MRTR interim results and task polling are not cached.

Use a stable principal or tenant identifier as the private partition, not an
access token. Call `set_cache_partition` immediately when the authorization
context changes, or `clear_response_cache` when a broader reset is required.
Disable the cache for diagnostics with `disable_response_cache`.

## Retries and failure handling

tower-mcp retries only where the library can establish a safe boundary:

- legacy HTTP session expiry resets the transport, repeats `initialize`, and
  retries the failed request once; call `disable_session_recovery` on the HTTP
  transport if the application must own recovery;
- final version selection retries one recognized unsupported-version response
  with a mutually supported modern version;
- final tool calls refresh `tools/list` and retry once after a pre-execution
  stale-schema/header rejection;
- MRTR rounds are retries by protocol design and are bounded by
  `max_mrtr_rounds`.

There is no blanket network retry for tool calls. A connection failure may
occur after the server performed a side effect but before the response reached
the client. Add application-level retries only when the operation is known to
be idempotent or carries an application idempotency key. Apply an outer timeout
around user-facing operations and retain a separate absolute cap even when
progress notifications arrive.

Use `is_connected` for a cheap transport-state check, but treat the result as
a snapshot. The next operation is the authoritative health signal.

## Shutdown and long-lived streams

Call `client.shutdown().await` when possible so the background transport task
can close cleanly. Dropping the client also closes its command channel, but
does not give the application a completion point.

For final-protocol notifications, keep the returned `SubscriptionHandle`, wait
for `acknowledged()`, and call `cancel()` when finished. Dropping the handle
requests best-effort cancellation; explicit cancellation confirms the stream
closed. The runnable flow is in
[`stateless_http_client.rs`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/stateless_http_client.rs).

## Runnable examples

```bash
# Stdio subprocess client
cargo run --example client_cli

# Stable Streamable HTTP client; start http_server in another terminal
cargo run --example http_server --features http
cargo run --example http_client --features http-client

# Callback and bidirectional request handling
cargo run --example client_handler

# In-process 2026-07-28 server and client
cargo run --example stateless_http_client \
  --features "http,http-client,protocol-2026-07-28"
```

See the [examples index](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/README.md)
for OAuth, task, subscription, and conformance clients.
"####]
