# tower-mcp Examples Tour


Welcome! This guide walks through the example MCP servers in tower-mcp.

## Getting Started

Make sure you're in the tower-mcp directory with your MCP-enabled agent
(like Claude Code). The `.mcp.json` file configures all example servers.

#### Wait, Did You Notice?

This README has some markdown issues! Try asking your agent:

> "Lint examples/README.md for markdown issues"

The markdownlint-mcp server will find them. Then try:

> "Fix the markdown issues in examples/README.md"

## Interactive MCP Servers

These servers are configured in `.mcp.json` and available to MCP agents
automatically.

### 1. markdownlint-mcp

Lint markdown files with 66 rules from mdbook-lint. Demonstrates tools
with different input types and auto-fix capabilities.

**Try these:**

- "Lint this file: examples/README.md"
- "What does rule MD001 check for?"
- "List all available markdown lint rules"

**Source:** `examples/markdownlint-mcp/`

### 3. weather

Weather forecasts using the National Weather Service API. A simple
example showing external API integration.

**Try these:**

- "What's the weather forecast for Seattle?"
- "Are there any weather alerts in California?"
- "Get the forecast for coordinates 40.7128, -74.0060"

**Source:** `examples/weather_server.rs`

### 3. tower-mcp-example

The simplest possible MCP server - echo, add, and reverse tools. Good
starting point for understanding the basics. This server is
self-documenting: it serves its own source code as a resource!

**Try these:**

- "Echo 'Hello World' using tower-mcp-example"
- "Reverse the string 'tower-mcp'"
- "Read the source://stdio_server.rs resource from tower-mcp-example"

**Source:** `examples/stdio_server.rs`

### 4. codegen-mcp

An MCP server that helps you build MCP servers. Define your server
incrementally through tool calls, then generate complete Rust code.

**Try these (tools):**

- "Initialize a project called my-server with stdio transport"
- "Add an echo tool that takes a message string"
- "Remove the echo tool"
- "Validate that the generated code compiles"
- "Generate the code for my server"

**Try these (resources):**

- "Read project://Cargo.toml from codegen-mcp"
- "Read project://src/main.rs from codegen-mcp"
- "Read project://state.json from codegen-mcp"

**Source:** `examples/codegen-mcp/`

## Standalone Examples

These demonstrate specific features. Run them individually with `cargo run`.

| Example | Feature | Run |
|---------|---------|-----|
| `basic` | Core `JsonRpcService` without transport | `cargo run --example basic` |
| `http_server` | HTTP/SSE transport, progress, resources, prompts | `cargo run --example http_server --features http` |
| `websocket_server` | WebSocket transport with middleware | `cargo run --example websocket_server --features websocket` |
| `transport_middleware` | Tower middleware on HTTP and stdio transports | `cargo run --example transport_middleware --features http -- --transport http` |
| `http_auth` | API key and OAuth/JWT authentication | `cargo run --example http_auth --features oauth -- --auth apikey` |
| `tool_middleware` | Per-tool timeout and concurrency limits | `cargo run --example tool_middleware` |
| `middleware_showcase` | Config validation via shared guards and middleware | `cargo run --example middleware_showcase` |
| `capability_filtering` | Session-based tool/resource/prompt visibility | `cargo run --example capability_filtering --features http` |
| `dynamic_tools` | Runtime tool registration/deregistration | `cargo run --example dynamic_tools --features dynamic-tools` |
| `external_api_auth` | Four patterns for downstream API authentication | `cargo run --example external_api_auth` |
| `testing` | In-process testing with `TestClient` | `cargo run --example testing --features testing` |
| `client_cli` | MCP client connecting to a subprocess server | `cargo run --example client_cli` |
| `http_sse_client` | HTTP SSE client with stream resumption | `cargo run --example http_sse_client --features http` |
| `conformance-server` | Full MCP conformance server (39/39 tests) | `cargo run -p conformance-server` |

## How It's Built

Ask your agent to read the `source://stdio_server.rs` resource from
tower-mcp-example. You'll see the complete server in ~90 lines.

Here's the key pattern - defining a tool:

```rust
let echo = ToolBuilder::new("echo")
    .description("Echo a message back")
    .handler(|input: EchoInput| async move {
        Ok(CallToolResult::text(input.message))
    })
    .build();
```

And the input type with automatic JSON Schema generation:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    /// The message to echo back  <-- becomes the schema description
    message: String,
}
```

That's the core pattern. For more complex examples:

- **In-process testing**: `examples/testing.rs` - test servers with `TestClient`
- **Shared state**: `examples/markdownlint-mcp/src/tools.rs` - tools
  sharing a lint engine via `Arc<LintState>`
- **External APIs**: `examples/weather_server.rs` - calling the NWS API
- **Full application**: See [cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp) for a complete MCP server with tools, resources, and prompts

## What You Just Explored

If you linted and fixed this README, you've seen:

1. **Tool discovery** - Your agent found the markdownlint-mcp tools
2. **Tool execution** - lint_file analyzed this document
3. **Structured output** - Violations returned as JSON
4. **Auto-fix** - fix_content corrected the issues

The intentional errors were:

- Extra blank lines after the title (MD012)
- Skipped heading level - jumped from h2 to h4 (MD001)
- Extra space before a list item (MD030)

## Build Your Own

Now that you've seen what's possible, want to build your own MCP server?

The codegen-mcp server can help. Here's the workflow:

1. **Design your server** - Tell codegen-mcp what you want:
   - "Initialize a project called my-server"
   - "Add a tool that does X with inputs Y and Z"
   - "Validate the generated code compiles"

2. **Generate and iterate** - Get complete Rust code:
   - "Generate the code for my server"
   - Write it to disk, customize the handlers, run it

### Ideas to Get Started

Ask your agent to help you build:

- **A Hacker News server** - The [HN API](https://github.com/HackerNews/API)
  is perfect for learning: no auth required, simple JSON responses, and
  lots of tool ideas (top stories, new stories, get item, get user, etc.)
- **A Git MCP server** - Wrap git commands (status, diff, log, blame)
- **A Docker MCP server** - Container and image management
- **A database MCP server** - Query SQLite, PostgreSQL, or Redis
- **A file search server** - Ripgrep or fd wrapper
- **An API client** - Wrap any REST API you use frequently

Or tell your agent what problem you're trying to solve and let it
suggest what tools your server should have.

### Example Session

```text
You: "I want to build an MCP server that searches my notes"

Agent: [uses codegen-mcp to design tools: index_directory, search, get_document]
Agent: [generates and validates the code]
Agent: "Here's your server. The search tool uses tantivy for full-text
        search. Want me to write this to examples/notes-mcp/?"
```

The codegen-mcp server lets you go from idea to working server quickly.

## Learn More

- Browse the source code in `examples/` to see how each server is built
- Check the main [README](../README.md) for API documentation
- Read the generated code from codegen-mcp to understand the patterns
