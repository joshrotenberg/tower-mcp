# tower-mcp Examples

Focused examples demonstrating tower-mcp features. Each file is self-contained
and covers a specific topic.

## Running Examples

```bash
# Stdio examples (no feature flags needed)
cargo run --example getting_started
cargo run --example middleware
cargo run --example rate_limiting

# HTTP transport
cargo run --example http_server --features http

# WebSocket transport
cargo run --example websocket_server --features websocket

# Clients (start http_server first)
cargo run --example http_client --features http-client
cargo run --example http_sse_client --features http

# Dynamic capabilities
cargo run --example dynamic_capabilities --features dynamic-tools

# Testing
cargo run --example testing --features testing

# Macros
cargo run --example tool_macro --features macros
```

## Example Index

### Getting Started

| Example | What it shows |
|---------|---------------|
| [getting_started](getting_started.rs) | Tools, resources, prompts, and stdio transport |

### Transports

| Example | What it shows |
|---------|---------------|
| [http_server](http_server.rs) | HTTP/SSE transport with sessions, progress, resources |
| [websocket_server](websocket_server.rs) | WebSocket transport with sampling support |

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
| [http_auth](http_auth.rs) | API key and OAuth/JWT server-side auth |
| [oauth_client](oauth_client.rs) | Client-side OAuth token acquisition |
| [external_api_auth](external_api_auth.rs) | Downstream API authentication patterns |

### Clients

| Example | What it shows |
|---------|---------------|
| [client_cli](client_cli.rs) | Stdio client connecting to subprocess servers |
| [http_client](http_client.rs) | HTTP client with McpClient API |
| [http_sse_client](http_sse_client.rs) | SSE stream resumption (Last-Event-ID) |

### Bidirectional Communication

| Example | What it shows |
|---------|---------------|
| [sampling_server](sampling_server.rs) | Server-to-client LLM requests and elicitation |
| [client_handler](client_handler.rs) | Client-side sampling and notification handling |

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
| [testing](testing.rs) | In-process testing with TestClient |
| [weather_server](weather_server.rs) | External API integration (NWS) |

### Macros

| Example | What it shows |
|---------|---------------|
| [tool_macro](tool_macro.rs) | `#[tool_fn]`, `#[prompt_fn]`, `#[resource_fn]` proc macros |

## Workspace Examples

These are standalone crates in the workspace:

| Crate | Purpose |
|-------|---------|
| [conformance-server](conformance-server/) | MCP conformance test suite server (39/39) |
| [conformance-client](conformance-client/) | MCP conformance test suite client (265/265) |
| [codegen-mcp](codegen-mcp/) | MCP server that helps build MCP servers |

## Full Application

For a complete production MCP server, see [cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp).
