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

## The Example Servers

### 1. crates-mcp

Query the Rust crate registry (crates.io). The most feature-complete
example, demonstrating tools, resources, and prompts.

**Try these (tools):**

-  "Search for async HTTP client crates"
- "What are tokio's dependencies?"
- "Get download stats for tower-mcp"

**Try these (resources):**

- "Read the crates://tokio/info resource from crates-mcp-local"
- "Read crates://recent-searches to see my search history"

**Try these (prompts):**

- "Use the analyze_crate prompt for tower-mcp"
- "Use the compare_crates prompt to compare serde and rkyv"

**Local vs Remote:** This example runs as two servers in `.mcp.json`:

- `crates-mcp-local` - stdio transport (spawned as child process)
- `crates-mcp-remote` - HTTP transport (deployed at Fly.io)

Same code, different transports. Try calling the same tool on both to see
they work identically.

**Source:** `examples/crates-mcp/`

### 2. markdownlint-mcp

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

### 4. tower-mcp-example

The simplest possible MCP server - echo, add, and reverse tools. Good
starting point for understanding the basics. This server is
self-documenting: it serves its own source code as a resource!

**Try these:**

- "Echo 'Hello World' using tower-mcp-example"
- "Reverse the string 'tower-mcp'"
- "Read the source://stdio_server.rs resource from tower-mcp-example"

**Source:** `examples/stdio_server.rs`

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
    .build()?;
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

- **Shared state**: `examples/markdownlint-mcp/src/tools.rs` - tools
  sharing a lint engine via `Arc<LintState>`
- **External APIs**: `examples/weather_server.rs` - calling the NWS API
- **Resources**: `examples/crates-mcp/src/resources/` - dynamic content
- **Full application**: `examples/crates-mcp/` - tools, resources, prompts

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

## Next Steps

- Browse the source code in `examples/` to see how each server is built
- Check the main [README](../README.md) for API documentation
- Try building your own MCP server with tower-mcp!
