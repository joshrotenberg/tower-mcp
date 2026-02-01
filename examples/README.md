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

### 6. codegen-mcp

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

## Build Your Own

Now that you've seen what's possible, want to build your own MCP server?

The codegen-mcp server can help. Here's the workflow:

1. **Find something to wrap** - Use crates-mcp to explore crates.io:
   - "Search for crates that do X"
   - "What are the dependencies for crate Y?"
   - "Use the analyze_crate prompt to evaluate Z"

2. **Design your server** - Tell codegen-mcp what you want:
   - "Initialize a project called my-server"
   - "Add a tool that does X with inputs Y and Z"
   - "Validate the generated code compiles"

3. **Generate and iterate** - Get complete Rust code:
   - "Generate the code for my server"
   - Write it to disk, customize the handlers, run it

### Ideas to Get Started

Ask your agent to help you build:

- **A Git MCP server** - Wrap git commands (status, diff, log, blame)
- **A Docker MCP server** - Container and image management
- **A database MCP server** - Query SQLite, PostgreSQL, or Redis
- **A file search server** - Ripgrep or fd wrapper
- **An API client** - Wrap any REST API you use frequently

Or tell your agent what problem you're trying to solve and let it
suggest what tools your server should have.

### Example Session

```
You: "I want to build an MCP server that searches my notes"

Agent: [uses crates-mcp to find tantivy, walkdir]
Agent: [uses codegen-mcp to design tools: index_directory, search, get_document]
Agent: [generates and validates the code]
Agent: "Here's your server. The search tool uses tantivy for full-text
        search. Want me to write this to examples/notes-mcp/?"
```

The combination of crates-mcp (for research) and codegen-mcp (for
scaffolding) lets you go from idea to working server quickly.

## Learn More

- Browse the source code in `examples/` to see how each server is built
- Check the main [README](../README.md) for API documentation
- Read the generated code from codegen-mcp to understand the patterns
