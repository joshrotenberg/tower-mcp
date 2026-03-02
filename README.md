# tower-mcp

[![Crates.io](https://img.shields.io/crates/v/tower-mcp.svg)](https://crates.io/crates/tower-mcp)
[![Documentation](https://docs.rs/tower-mcp/badge.svg)](https://docs.rs/tower-mcp)
[![CI](https://github.com/joshrotenberg/tower-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/tower-mcp/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/tower-mcp.svg)](https://github.com/joshrotenberg/tower-mcp#license)
[![MSRV](https://img.shields.io/crates/msrv/tower-mcp.svg)](https://github.com/joshrotenberg/tower-mcp)
[![MCP](https://img.shields.io/badge/MCP-2025--11--25-blue)](https://modelcontextprotocol.io/specification/2025-11-25)
[![Conformance](https://img.shields.io/badge/conformance-39%2F39-brightgreen)](https://github.com/joshrotenberg/tower-mcp/actions/workflows/conformance.yml)

Tower-native [Model Context Protocol](https://modelcontextprotocol.io) (MCP) implementation for Rust.

## Overview

tower-mcp provides a composable, middleware-friendly approach to building MCP servers using the [Tower](https://github.com/tower-rs/tower) service abstraction. Unlike framework-style MCP implementations, tower-mcp treats MCP as just another protocol that can be served through Tower's `Service` trait.

This means:

- Standard tower middleware (tracing, metrics, rate limiting, auth) just works
- Same service can be exposed over multiple transports (stdio, HTTP, WebSocket)
- Easy integration with existing tower-based applications (axum, tonic)

### Familiar to axum Users

If you've used [axum](https://docs.rs/axum), tower-mcp's API will feel familiar:

- **Extractor pattern**: Tool handlers use extractors like `State<T>`, `Json<T>`, and `Context`
- **Router composition**: `McpRouter::merge()` and `McpRouter::nest()` work like axum's router methods
- **Per-handler middleware**: Apply Tower layers to individual tools, resources, or prompts via `.layer()`
- **Builder pattern**: Fluent builders for tools, resources, and prompts

## Why tower-mcp?

### Strengths

| | |
|---|---|
| **Tower-native middleware** | Timeout, rate-limit, auth, tracing -- on the whole server or on individual tools. Any `tower::Layer` works. |
| **All transports** | stdio, HTTP/SSE (with stream resumption), WebSocket, and child process. Same router, any transport. |
| **In-process testing** | `TestClient` lets you test MCP servers without spawning a subprocess or opening a socket. |
| **Conformance** | 39/39 official MCP conformance tests pass in CI on every PR. |
| **Capability filtering** | Session-based tool/resource/prompt visibility for multi-tenant patterns. |
| **No proc macros required** | Builder pattern API with optional trait-based tools. Nothing hidden behind `#[derive]`. |
| **axum ecosystem** | HTTP and WebSocket transports build on axum, so existing axum middleware and extractors work. |

### Trade-offs

- **More boilerplate than macro-based approaches** for simple servers. If you want `#[tool]` on a function and nothing else, a proc-macro SDK may be more concise.
- **Requires Tower/Service familiarity.** The `.layer()` composition model is powerful but has a learning curve if you haven't used Tower before.
- **Heavier dependency tree** than minimal single-transport implementations, especially with `features = ["full"]`.

## Quick Start

```rust
use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

// Define your input type - schema is auto-generated
#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    name: String,
}

// Build a tool with type-safe handler
let greet = ToolBuilder::new("greet")
    .description("Greet someone by name")
    .handler(|input: GreetInput| async move {
        Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
    })
    .build();

// Create router with tools
let router = McpRouter::new()
    .server_info("my-server", "1.0.0")
    .instructions("This server provides greeting functionality")
    .tool(greet);

// The router implements tower::Service and can be composed with middleware
```

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
tower-mcp = "0.7"
```

### Feature Flags

| Feature | Description |
|---------|-------------|
| `full` | Enable all optional features |
| `http` | HTTP transport with SSE support (adds axum, hyper) |
| `websocket` | WebSocket transport for full-duplex communication |
| `childproc` | Child process transport for spawning subprocess MCP servers |
| `oauth` | OAuth 2.1 resource server support (JWT validation) |
| `jwks` | JWKS endpoint fetching for remote key sets (requires `oauth`) |
| `testing` | Test utilities (`TestClient`) for in-process testing |
| `dynamic-tools` | Runtime tool registration/deregistration via `DynamicToolRegistry` |

Example with features:

```toml
[dependencies]
tower-mcp = { version = "0.7", features = ["full"] }
```

### Types Only

If you only need MCP protocol types and error types -- without tower, tokio, or axum --
use the [`tower-mcp-types`](https://crates.io/crates/tower-mcp-types) crate directly.
This is useful for editor integrations, code generators, protocol validators, or
any context where you want to serialize/deserialize MCP messages without a runtime.

```toml
[dependencies]
tower-mcp-types = "0.1"
```

`tower-mcp-types` provides all types from `tower_mcp::protocol` and `tower_mcp::error`
with minimal dependencies (`serde`, `serde_json`, `thiserror`, `base64`). The full
`tower-mcp` crate re-exports everything from `tower-mcp-types`, so there is no
duplication if you use both.

## Tool Definition

### Builder Pattern (Recommended)

```rust
use tower_mcp::{ToolBuilder, CallToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

let add = ToolBuilder::new("add")
    .description("Add two numbers")
    .read_only()  // Hint: this tool doesn't modify state
    .handler(|input: AddInput| async move {
        Ok(CallToolResult::text(format!("{}", input.a + input.b)))
    })
    .build();
```

### Trait-Based (For Complex Tools)

```rust
use tower_mcp::tool::McpTool;
use tower_mcp::{Result, CallToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

struct Calculator {
    precision: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CalcInput {
    expression: String,
}

impl McpTool for Calculator {
    const NAME: &'static str = "calculate";
    const DESCRIPTION: &'static str = "Evaluate a mathematical expression";

    type Input = CalcInput;
    type Output = f64;

    async fn call(&self, input: Self::Input) -> Result<Self::Output> {
        // Your calculation logic here
        Ok(42.0)
    }
}

// Convert to Tool and register
let calc = Calculator { precision: 10 };
let router = McpRouter::new().tool(calc.into_tool());
```

### Handler with Extractors (State, Context, JSON)

Use axum-style extractors to access state, context, and typed input:

```rust
use std::sync::Arc;
use tower_mcp::{ToolBuilder, CallToolResult};
use tower_mcp::extract::{State, Context, Json};

#[derive(Clone)]
struct AppState { db_url: String }

let state = Arc::new(AppState { db_url: "postgres://...".into() });

let search = ToolBuilder::new("search")
    .description("Search with progress updates")
    .extractor_handler(state, |
        State(app): State<Arc<AppState>>,
        ctx: Context,
        Json(input): Json<SearchInput>,
    | async move {
        // Report progress
        ctx.report_progress(0.5, Some(1.0), Some("Searching...")).await;
        // Use state
        let results = format!("Searched {} for: {}", app.db_url, input.query);
        Ok(CallToolResult::text(results))
    })
    .build();
```

See [docs.rs](https://docs.rs/tower-mcp) for more patterns including per-tool middleware, icons and titles, raw JSON handlers, and output schemas.

## Resource Definition

```rust
use tower_mcp::ResourceBuilder;

// Static resource with inline content
let config = ResourceBuilder::new("file:///config.json")
    .name("Configuration")
    .description("Server configuration")
    .json(serde_json::json!({
        "version": "1.0.0",
        "debug": true
    }))
    .build();

// Dynamic resource with handler
let status = ResourceBuilder::new("app:///status")
    .name("Server Status")
    .description("Current server status")
    .handler(|| async {
        Ok("Running".to_string())
    })
    .build();

let router = McpRouter::new()
    .resource(config)
    .resource(status);
```

## Prompt Definition

```rust
use tower_mcp::{PromptBuilder, GetPromptResult};

let greet = PromptBuilder::new("greet")
    .description("Generate a greeting")
    .required_arg("name", "Name to greet")
    .optional_arg("style", "Greeting style (formal/casual)")
    .handler(|args| async move {
        let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
        let style = args.get("style").map(|s| s.as_str()).unwrap_or("casual");

        let text = match style {
            "formal" => format!("Good day, {}. How may I assist you?", name),
            _ => format!("Hey {}!", name),
        };

        // Builder handles message construction
        Ok(GetPromptResult::builder()
            .description("A friendly greeting")
            .user(text)
            .build())
    })
    .build();

let router = McpRouter::new().prompt(greet);
```

## Router Composition

Combine routers like in axum:

```rust
// Merge routers (combines all tools/resources/prompts)
let api_router = McpRouter::new()
    .tool(search_tool)
    .tool(fetch_tool);

let admin_router = McpRouter::new()
    .tool(reset_tool)
    .tool(stats_tool);

let combined = McpRouter::new()
    .merge(api_router)
    .merge(admin_router);

// Nest with prefix (adds prefix to all tool names)
let v1 = McpRouter::new().tool(legacy_tool);
let v2 = McpRouter::new().tool(new_tool);

let versioned = McpRouter::new()
    .nest("v1", v1)   // Tools become "v1_legacy_tool"
    .nest("v2", v2);  // Tools become "v2_new_tool"
```

## Router-Level State

Share state across all handlers using `with_state()`:

```rust
use std::sync::Arc;
use tower_mcp::extract::Extension;

#[derive(Clone)]
struct AppState {
    db: DatabasePool,
    config: Config,
}

let state = Arc::new(AppState { /* ... */ });

// Tools access state via Extension<T> extractor
let tool = ToolBuilder::new("query")
    .extractor_handler(
        (),
        |Extension(app): Extension<Arc<AppState>>, Json(input): Json<QueryInput>| async move {
            let result = app.db.query(&input.sql).await?;
            Ok(CallToolResult::text(result))
        },
    )
    .build();

let router = McpRouter::new()
    .with_state(state)  // Makes AppState available to all handlers
    .tool(tool);
```

## Transports

### Stdio (CLI/local)

```rust
use tower_mcp::{McpRouter, StdioTransport};

let router = McpRouter::new()
    .server_info("my-server", "1.0.0")
    .tool(my_tool);

// Serve over stdin/stdout
StdioTransport::new(router).serve().await?;
```

### HTTP with SSE

```rust
use tower_mcp::{McpRouter, HttpTransport};

let router = McpRouter::new()
    .server_info("my-server", "1.0.0")
    .tool(my_tool);

let transport = HttpTransport::new(router);
let app = transport.into_router();

// Serve with axum
let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
axum::serve(listener, app).await?;
```

### With Authentication Middleware

```rust
use tower_mcp::auth::extract_api_key;
use axum::middleware;

// Add auth layer to the HTTP transport
let app = transport.into_router()
    .layer(middleware::from_fn(auth_middleware));
```

## Testing

tower-mcp includes `TestClient` (feature: `testing`) for in-process server testing -- no subprocess, no network, no port management:

```rust
use tower_mcp::TestClient;
use serde_json::json;

let mut client = TestClient::from_router(router);
client.initialize().await;

// List and call tools
let tools = client.list_tools().await;
assert_eq!(tools.len(), 1);

let result = client.call_tool("greet", json!({"name": "World"})).await;
assert_eq!(result.all_text(), "Hello, World!");

// Typed deserialization
let stats: ServerStats = client.call_tool_typed("stats", json!({})).await;

// Assert expected errors
let err = client.call_tool_expect_error("missing", json!({})).await;
```

`TestClient` handles JSON-RPC framing, request IDs, and protocol initialization. Methods panic on unexpected errors, keeping test code concise.

## Capability Filtering

Control which tools, resources, and prompts each session can see. This enables multi-tenant patterns where different clients get different capabilities based on auth claims or session state:

```rust
use tower_mcp::CapabilityFilter;

// Hide write tools from sessions that aren't authorized
let router = McpRouter::new()
    .tool(read_tool)
    .tool(write_tool)
    .tool_filter(CapabilityFilter::write_guard(|session| {
        session.get::<UserRole>()
            .map(|r| r.is_admin())
            .unwrap_or(false)
    }));
```

`write_guard` uses tool annotations: tools marked `.read_only()` are always visible, while other tools are only shown to sessions where the predicate returns `true`. Hidden tools return "method not found" by default, or configure `DenialBehavior::Unauthorized` to reveal their existence without granting access.

Filters work on resources and prompts too:

```rust
let router = McpRouter::new()
    .resource(public_resource)
    .resource(internal_resource)
    .resource_filter(CapabilityFilter::new(|session, resource: &Resource| {
        !resource.name().contains("internal") || session.get::<AdminClaim>().is_some()
    }));
```

## Architecture

```text
                    +-----------------+
                    |  Your App       |
                    +-----------------+
                           |
                    +-----------------+
                    | Tower Middleware|  <-- tracing, metrics, auth, etc.
                    +-----------------+
                           |
                    +-----------------+
                    | JsonRpcService  |  <-- JSON-RPC 2.0 framing
                    +-----------------+
                           |
                    +-----------------+
                    |   McpRouter     |  <-- Request dispatch
                    +-----------------+
                           |
              +------------+------------+
              |            |            |
         +--------+   +--------+   +--------+
         | Tool 1 |   | Tool 2 |   | Tool N |
         +--------+   +--------+   +--------+
```

## Protocol Compliance

tower-mcp targets the [MCP specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) with backward compatibility for `2025-03-26`. The [official MCP conformance test suite](https://github.com/joshrotenberg/tower-mcp/actions/workflows/conformance.yml) runs in CI on every PR, currently passing 39/39 tests.

- [x] [JSON-RPC 2.0 message format](https://modelcontextprotocol.io/specification/2025-11-25/basic#messages)
- [x] [Protocol version negotiation](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle#version-negotiation) (supports `2025-11-25` and `2025-03-26`)
- [x] [Capability negotiation](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle#capability-negotiation)
- [x] [Initialize/initialized lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [x] [tools/list and tools/call](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [x] [Tool annotations](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [x] [Batch requests](https://modelcontextprotocol.io/specification/2025-11-25/basic#batching)
- [x] [resources/list, resources/read, resources/subscribe](https://modelcontextprotocol.io/specification/2025-11-25/server/resources)
- [x] [resources/templates/list](https://modelcontextprotocol.io/specification/2025-11-25/server/resources#resource-templates)
- [x] [prompts/list, prompts/get](https://modelcontextprotocol.io/specification/2025-11-25/server/prompts)
- [x] [Logging (notifications/message, logging/setLevel)](https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/logging)
- [x] [Icons on tools/resources/prompts (SEP-973)](https://modelcontextprotocol.io/specification/2025-11-25)
- [x] [Implementation metadata](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [x] [Sampling with tools/toolChoice (SEP-1577)](https://modelcontextprotocol.io/specification/2025-11-25/client/sampling)
- [x] [Elicitation (form and URL modes)](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation)
- [x] [Session management](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#session-management)
- [x] [Progress notifications](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/progress)
- [x] [Request cancellation](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation)
- [x] [Completion (autocomplete)](https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/completion)
- [x] [Roots (filesystem discovery)](https://modelcontextprotocol.io/specification/2025-11-25/client/roots)
- [x] [Sampling](https://modelcontextprotocol.io/specification/2025-11-25/client/sampling) (all transports)
- [x] [Async tasks](https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/async) (task ID, status tracking, TTL cleanup, per-tool task support mode)
- [x] [SSE event IDs and stream resumption](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#resumability-and-redelivery) (SEP-1699)
- [x] [`_meta` field on all protocol types](https://modelcontextprotocol.io/specification/2025-11-25)

We track all MCP Specification Enhancement Proposals (SEPs) as [GitHub issues](https://github.com/joshrotenberg/tower-mcp/issues?q=label%3Asep). A weekly workflow syncs status from the upstream spec repository.

## Examples and Live Demo

A full-featured MCP server for querying [crates.io](https://crates.io) is available as a standalone project: [cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp). A demo instance is deployed at **https://cratesio-mcp.fly.dev** -- connect with any MCP client that supports HTTP transport.

The repo includes several example servers you can try with any MCP-enabled agent (like Claude Code). Clone the repo and the `.mcp.json` configures them automatically:

| Server | Description |
|--------|-------------|
| `markdownlint-mcp` | Lint markdown with 66 rules |
| `codegen-mcp` | Helps AI agents build tower-mcp servers |
| `weather` | Weather forecasts via NWS API |
| `conformance` | Full MCP spec conformance server (39/39 tests) |

```bash
git clone https://github.com/joshrotenberg/tower-mcp
cd tower-mcp
# Run your MCP agent here - servers will be available automatically
```

For a guided tour, ask your agent to read [`examples/README.md`](examples/README.md).

## Development

```bash
# Format, lint, and test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## License

MIT OR Apache-2.0
