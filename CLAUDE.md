# tower-mcp

Tower-native Model Context Protocol (MCP) implementation for Rust.

## Why This Exists

### The Bigger Picture: jpx-cloud

This project emerged from building **jpx-cloud**, a hosted MCP service for JSON querying:

- **jpx** is an open-source JMESPath implementation with 400+ extension functions
- **jpx-cloud** is a SaaS offering: hosted MCP server with auth, rate limiting, analytics
- The value prop: "Deterministic JSON for non-deterministic agents" - same input, same output, every time

jpx-cloud needs to serve MCP over HTTP with:
- API key authentication
- Per-tier rate limiting
- Prometheus metrics
- Request tracing
- Multiple protocols (MCP, REST, potentially gRPC)

### The Problem with Existing Options

**rmcp** (official Rust MCP SDK) is framework-style:
- Great for quick starts with `#[tool]` macros
- But no middleware story - you'd bolt tower onto rmcp
- Transport is built-in, not pluggable
- Hard to integrate with existing axum/tonic services

**rust-mcp-sdk** has similar constraints.

### The tower-mcp Approach

Treat MCP as just another protocol that can be served through Tower's `Service` trait:

```rust
// Same jpx handler, different protocols/middleware
let jpx = JpxService::new();

// MCP over stdio (CLI/local)
let mcp_stdio = ServiceBuilder::new()
    .layer(JsonRpcLayer::new())
    .service(jpx.clone());

// MCP over HTTP (cloud)
let mcp_http = ServiceBuilder::new()
    .layer(AuthLayer::new(api_keys))
    .layer(RateLimitLayer::new(tier_limits))
    .layer(MetricsLayer::new())
    .layer(JsonRpcLayer::new())
    .service(jpx.clone());

// REST API (non-MCP users) - same core!
let rest = ServiceBuilder::new()
    .layer(AuthLayer::new(api_keys))
    .service(jpx);
```

## Philosophy

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
- **Tool names**: 1-128 chars, alphanumeric + underscore/hyphen/dot, case-sensitive

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

### Notifications (Server -> Client)

MCP supports server-initiated notifications for:
- Progress updates on long-running tools
- Log messages
- Tool list changes

This is the trickiest part for tower (request/response model). Options:
- Inject `Sender<Notification>` into handler context
- SSE naturally handles this for HTTP
- Stdio needs message multiplexing

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
- [x] JSON-RPC 2.0 types (request, response, notification, error)
- [x] MCP protocol types (tools, resources, prompts)
- [x] Tool builder with type-safe handlers
- [x] McpTool trait for complex tools
- [x] McpRouter with tower Service impl
- [x] JsonRpcService layer for protocol framing
- [x] Stdio transport (async and sync)
- [x] Session management (initialize handshake, capability negotiation)
- [x] Resources support (list, read) with ResourceBuilder and McpResource trait
- [x] Prompts support (list, get) with PromptBuilder and McpPrompt trait
- [x] Batch request support
- [x] Comprehensive integration tests (29 tests)

### TODO
- [ ] HTTP transport (with SSE for notifications)
- [ ] WebSocket transport
- [ ] Resource subscriptions
- [ ] Progress notifications
- [ ] Cancellation support
- [ ] Tower layer examples (tracing, metrics, auth)
- [ ] Integration tests with Claude Desktop or other MCP clients
- [ ] Batching layer (combine multiple 1:1 calls into batch internally)

## MCPaaS Landscape Context (Jan 2026)

For context on where MCP is heading:

**Big players entering:**
- AWS Bedrock AgentCore - context manager, orchestration
- Microsoft Azure Easy MCP - OpenAPI-to-MCP translation
- Cloudflare - edge orchestration
- Salesforce - hosted MCP with SLA

**Gateways/Governance:**
- MintMCP - role-based endpoints, Cursor partnership
- Descope - AI agents as first-class identities

**Marketplaces:**
- MCP.so, Cline Marketplace, MCP Exchange (rental model)
- ~70% of large SaaS brands now offer remote MCP servers
- MCP donated to Linux Foundation (Agentic AI Foundation) Dec 2025

tower-mcp is positioned as infrastructure for building MCP services, not a platform or marketplace.

## Related Projects

### Sibling Projects
- [jmespath-extensions](https://github.com/joshrotenberg/jmespath-extensions) - jpx core library and CLI
- jpx-cloud (private) - SaaS infrastructure for hosted jpx

### Ecosystem
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) - Official Rust MCP SDK
- [rust-mcp-sdk](https://crates.io/crates/rust-mcp-sdk) - Another MCP SDK
- [tower](https://github.com/tower-rs/tower) - Tower service abstraction
- [axum](https://github.com/tokio-rs/axum) - Tower-native web framework
- [tonic](https://github.com/hyperium/tonic) - gRPC for tower

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Project Goals

This is a **learning project first**, potential ecosystem contribution second:

1. Deeply understand MCP protocol by implementing it
2. Learn tower patterns for protocol implementation
3. Build infrastructure for jpx-cloud
4. If useful to others, publish to crates.io

Not trying to replace rmcp for everyone - just providing an alternative for teams that want tower-native composition.

## License

MIT OR Apache-2.0
