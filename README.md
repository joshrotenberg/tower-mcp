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

## Live Demo

A full-featured MCP server for querying [crates.io](https://crates.io) is available as a standalone project: [cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp). It includes tools, prompts, and resources for crate search, docs.rs integration, and vulnerability auditing via OSV.dev.

A demo instance is deployed at **https://crates-mcp-demo.fly.dev** -- connect with any MCP client that supports HTTP transport.

## Try the Examples

Clone the repo and run your MCP-enabled agent (like Claude Code) in the
tower-mcp directory. The `.mcp.json` configures several example servers:

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
Or jump straight in:

- "Lint examples/README.md for issues" (markdownlint-mcp)
- "What's the weather in Seattle?" (weather)

## Status

**Active development** - Core protocol, routing, and transports are implemented. Used in production for MCP server deployments.

### Implemented
- JSON-RPC 2.0 message types, validation, and batch request handling
- MCP protocol types (tools, resources, prompts)
- Tool builder with type-safe handlers and JSON Schema generation via [schemars](https://crates.io/crates/schemars)
- `McpTool` trait for complex tools
- `McpRouter` implementing Tower's `Service` trait
- `JsonRpcService` layer for protocol framing
- Session state management with reconnection support
- Protocol version negotiation (supports `2025-11-25` with `2025-03-26` backward compat)
- Tool annotations (behavior hints for trust/safety)
- **Transports**: stdio (`StdioTransport`, `SyncStdioTransport`, `BidirectionalStdioTransport`, `GenericStdioTransport`), HTTP (with SSE and stream resumption), WebSocket, child process
- **Resources**: list, read, subscribe/unsubscribe with change notifications
- **Resource templates**: `resources/templates/list` with URI template matching (RFC 6570)
- **Prompts**: list and get with argument support
- **Logging**: `notifications/message` and `logging/setLevel` with structured log data
- **Authentication**: API key and Bearer token middleware helpers
- **Elicitation**: Server-to-client user input requests (form mode via `elicit()` and URL mode via `elicit_url()`)
- **Client support**: MCP client for connecting to external servers
- **Progress notifications**: Via `RequestContext` in tool handlers
- **Request cancellation**: Via `CancellationToken` in tool handlers
- **Completion**: Autocomplete for prompt arguments and resource URIs
- **Sampling types**: `CreateMessageParams`/`CreateMessageResult` for LLM requests
- **Sampling runtime**: Full support on stdio, WebSocket, and HTTP transports
- **Async tasks**: Task ID generation, status tracking, TTL-based cleanup, per-tool `task_support` mode
- **Per-tool guards**: Request-level access control for individual tools
- **Capability filters**: Session-based tool/resource/prompt visibility
- **`ResultExt`**: Ergonomic error handling in tool handlers
- **Auto-generated instructions**: Server instructions derived from registered capabilities
- **Convenience helpers**: `Content::text()`, `CallToolResult::from_list()`, JSON helpers
- **Tool-level testing**: Unit test tools directly via `Tool::call()`
- **Infallible builds**: `ToolBuilder::build()` is infallible; `try_new()` available for runtime names
- **Cursor-based pagination**: `McpRouter::page_size()` for paginated list responses
- **Tool output schema**: `ToolBuilder::output_schema()` for structured output validation
- **Server metadata**: `server_title()`, `server_description()`, `server_icons()`, `server_website_url()`
- **`McpTracingLayer`**: Built-in Tower middleware for structured request logging
- **`list_changed` notifications**: `notify_tools_list_changed()`, `notify_resources_list_changed()`, `notify_prompts_list_changed()`
- **Dynamic tools**: Runtime tool registration/deregistration via `DynamicToolRegistry` (feature: `dynamic-tools`)
- **Experimental capabilities**: `experimental` field on both client and server capabilities
- **Extension support**: `extensions` field on server and client capabilities for declared extension negotiation

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
tower-mcp = "0.6"
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
tower-mcp = { version = "0.6", features = ["full"] }
```

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

### Tool with Icons and Title

```rust
let tool = ToolBuilder::new("analyze")
    .title("Code Analyzer")          // Human-readable display name
    .description("Analyze code quality")
    .icon("https://example.com/icon.svg")
    .read_only()
    .idempotent()
    .build();
```

### Per-Tool Middleware

Apply Tower layers to individual tools:

```rust
use std::time::Duration;
use tower::timeout::TimeoutLayer;

let slow_tool = ToolBuilder::new("slow_search")
    .description("Thorough search (may take a while)")
    .handler(|input: SearchInput| async move {
        // ... slow operation ...
        Ok(CallToolResult::text("results"))
    })
    .layer(TimeoutLayer::new(Duration::from_secs(60)))  // 60s for this tool
    .build();
```

### Raw JSON Handler (Escape Hatch)

Use `RawArgs` extractor when you need the raw JSON:

```rust
use tower_mcp::extract::RawArgs;

let echo = ToolBuilder::new("echo")
    .description("Echo back the input")
    .extractor_handler((), |RawArgs(args): RawArgs| async move {
        Ok(CallToolResult::json(args))
    })
    .build();
```

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

## Architecture

```
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

## Design Philosophy

| Aspect | tower-mcp |
|--------|-----------|
| Style | Library, not framework |
| Tool definition | Builder pattern or trait-based |
| Middleware | Native tower layers |
| Transport | Pluggable (stdio, HTTP, WebSocket, child process) |
| Integration | Composable with axum/tonic |

## Protocol Compliance

tower-mcp targets the [MCP specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) with backward compatibility for `2025-03-26`. Current compliance:

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

## Development

```bash
# Format, lint, and test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## License

MIT OR Apache-2.0
