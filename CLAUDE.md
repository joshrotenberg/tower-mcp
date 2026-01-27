# tower-mcp

Tower-native Model Context Protocol (MCP) implementation for Rust.

## Philosophy

Unlike framework-style MCP implementations (rmcp, rust-mcp-sdk), tower-mcp treats MCP as just another protocol that can be served through Tower's `Service` trait. This means:

- Standard tower middleware (tracing, metrics, rate limiting, auth) just works
- Same service can be exposed over multiple transports (stdio, HTTP, WebSocket)
- Easy integration with existing tower-based applications (axum, tonic)
- Composable primitives instead of framework lock-in

## Design Goals

1. **Tower-native**: Everything is a `Service<Request, Response>`
2. **Middleware-first**: Auth, metrics, tracing, rate limiting via tower layers
3. **Transport-agnostic**: Same handlers work over stdio, HTTP, WebSocket
4. **Type-safe**: Leverage Rust's type system, derive schemas from types
5. **Minimal magic**: Explicit over implicit, traits over macros

## Architecture

```
┌─────────────────────────────────────────────┐
│  Tower Middleware (composable layers)       │
│  - TracingLayer                             │
│  - MetricsLayer (prometheus)                │
│  - RateLimitLayer (tower-governor)          │
│  - TimeoutLayer                             │
│  - AuthLayer (custom)                       │
├─────────────────────────────────────────────┤
│  JsonRpcService                             │
│  - Parse/serialize JSON-RPC 2.0             │
│  - Error mapping                            │
├─────────────────────────────────────────────┤
│  McpRouter                                  │
│  - Routes to tools/resources/prompts        │
│  - Schema generation via schemars           │
├─────────────────────────────────────────────┤
│  Transport (swappable)                      │
│  - StdioTransport                           │
│  - HttpTransport (SSE for notifications)    │
│  - WebSocketTransport                       │
└─────────────────────────────────────────────┘
```

## MCP Spec Reference

Based on MCP specification 2025-11-25:
https://modelcontextprotocol.io/specification/2025-11-25

### Key Protocol Details

- **Transport**: JSON-RPC 2.0 over stdio, HTTP/SSE, or WebSocket
- **Session**: Stateful, requires initialize handshake
- **Capabilities**: Server declares tools, resources, prompts support

### Tool Definition Schema

```json
{
  "name": "tool_name",
  "title": "Human Readable Title",
  "description": "What the tool does",
  "inputSchema": { /* JSON Schema 2020-12 */ },
  "outputSchema": { /* optional */ }
}
```

### Request/Response Flow

```
Client                    Server
  |                         |
  |-- initialize ---------> |
  |<-------- result --------|
  |                         |
  |-- tools/list ---------> |
  |<-------- tools[] -------|
  |                         |
  |-- tools/call ---------> |
  |<-------- result --------|
```

## Tool Definition Options

### 1. Builder Pattern (recommended for most cases)

```rust
let tool = ToolBuilder::new("evaluate")
    .description("Evaluate a JMESPath expression")
    .handler(|input: EvaluateInput| async move {
        // input is automatically deserialized
        // schema is derived from EvaluateInput via schemars
        Ok(CallToolResult::text(result))
    })
    .build();
```

### 2. Trait-based (for complex tools)

```rust
struct EvaluateTool { runtime: Arc<Runtime> }

impl McpTool for EvaluateTool {
    const NAME: &'static str = "evaluate";
    const DESCRIPTION: &'static str = "Evaluate expression";

    type Input = EvaluateInput;
    type Output = Value;

    async fn call(&self, input: Self::Input) -> Result<Self::Output> {
        // full control over execution
    }
}
```

### 3. Raw JSON (escape hatch)

```rust
let tool = ToolBuilder::new("dynamic")
    .description("Handles any JSON")
    .raw_handler(|args: Value| async move {
        // manual JSON handling
        Ok(CallToolResult::json(args))
    });
```

## Comparison with rmcp

| Aspect | rmcp | tower-mcp |
|--------|------|-----------|
| Style | Framework | Library |
| Tool definition | `#[tool]` macro | Builder or trait |
| Middleware | Bolt-on | Native tower layers |
| Transport | Built-in | Pluggable |
| Learning curve | Lower | Higher (need tower knowledge) |
| Flexibility | Limited | High |
| Integration | Standalone | Composable with axum/tonic |

## Implementation Status

### Done
- [x] JSON-RPC 2.0 types
- [x] MCP protocol types (tools, resources, prompts)
- [x] Tool builder with type-safe handlers
- [x] McpTool trait for complex tools
- [x] McpRouter with tower Service impl
- [x] JsonRpcService layer
- [x] Basic tests

### TODO
- [ ] Stdio transport
- [ ] HTTP transport (with SSE for notifications)
- [ ] WebSocket transport
- [ ] Session management (initialize handshake)
- [ ] Resources support
- [ ] Prompts support
- [ ] Progress notifications
- [ ] Cancellation support
- [ ] Tower layer examples (tracing, metrics, auth)
- [ ] Integration tests with real MCP clients

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Related Projects

- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) - Official Rust MCP SDK
- [rust-mcp-sdk](https://crates.io/crates/rust-mcp-sdk) - Another MCP SDK
- [tower](https://github.com/tower-rs/tower) - Tower service abstraction
- [axum](https://github.com/tokio-rs/axum) - Tower-native web framework

## Motivation

jpx-cloud needs MCP over HTTP with auth, rate limiting, and metrics. rmcp handles the protocol but not the service layer. Rather than bolt tower onto rmcp, building tower-native MCP provides:

1. Same middleware for MCP, REST, and gRPC endpoints
2. Clean separation of protocol and transport
3. Learning opportunity (implement MCP from scratch)
4. Potential ecosystem contribution

## License

MIT OR Apache-2.0
