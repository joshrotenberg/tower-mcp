# tower-mcp

Tower-native [Model Context Protocol](https://modelcontextprotocol.io) (MCP) implementation for Rust.

## Overview

tower-mcp provides a composable, middleware-friendly approach to building MCP servers using the [Tower](https://github.com/tower-rs/tower) service abstraction. Unlike framework-style MCP implementations, tower-mcp treats MCP as just another protocol that can be served through Tower's `Service` trait.

This means:
- Standard tower middleware (tracing, metrics, rate limiting, auth) just works
- Same service can be exposed over multiple transports (stdio, HTTP, WebSocket)
- Easy integration with existing tower-based applications (axum, tonic)

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
- Protocol version negotiation
- Tool annotations (behavior hints for trust/safety)
- **Transports**: stdio, HTTP (with SSE), WebSocket, child process
- **Resources**: list, read, subscribe/unsubscribe with change notifications
- **Prompts**: list and get with argument support
- **Authentication**: API key and Bearer token middleware helpers
- **Elicitation**: Server-to-client user input requests (form and URL modes)
- **Client support**: MCP client for connecting to external servers
- **Progress notifications**: Via `RequestContext` in tool handlers
- **Request cancellation**: Via `CancellationToken` in tool handlers
- **Completion**: Autocomplete for prompt arguments and resource URIs
- **Sampling types**: `CreateMessageParams`/`CreateMessageResult` for LLM requests
- **Sampling runtime**: Full support on stdio, WebSocket, and HTTP transports

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
tower-mcp = { git = "https://github.com/joshrotenberg/tower-mcp" }
```

### Feature Flags

| Feature | Description |
|---------|-------------|
| `http` | HTTP transport with SSE support (adds axum, hyper dependencies) |
| `websocket` | WebSocket transport for full-duplex communication |
| `childproc` | Child process transport for spawning subprocess MCP servers |
| `client` | MCP client support for connecting to external servers |

Example with features:

```toml
[dependencies]
tower-mcp = { git = "https://github.com/joshrotenberg/tower-mcp", features = ["http", "client"] }
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

### Raw JSON Handler (Escape Hatch)

```rust
let echo = ToolBuilder::new("echo")
    .description("Echo back the input")
    .raw_handler(|args: serde_json::Value| async move {
        Ok(CallToolResult::json(args))
    });
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

tower-mcp targets the [MCP specification 2025-03-26](https://modelcontextprotocol.io/specification/2025-03-26). Current compliance:

- [x] JSON-RPC 2.0 message format
- [x] Protocol version negotiation
- [x] Capability negotiation
- [x] Initialize/initialized lifecycle
- [x] tools/list and tools/call
- [x] Tool annotations
- [x] Batch requests
- [x] resources/list, resources/read, resources/subscribe
- [x] prompts/list, prompts/get
- [x] Elicitation (user input requests)
- [x] Session management with reconnection error codes
- [x] Progress notifications
- [x] Request cancellation
- [x] Completion (autocomplete)
- [x] Roots (filesystem discovery)
- [x] Sampling runtime (stdio transport)

## Development

```bash
# Format, lint, and test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## License

MIT OR Apache-2.0
