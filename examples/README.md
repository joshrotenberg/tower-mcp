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

**Try these:**

- "Search for async HTTP client crates"
- "What are tokio's dependencies?"
- "Compare serde and rkyv crates"
- "Get download stats for tower-mcp"

**Source:** `examples/crates-mcp/`

### 2. markdownlint-mcp

Lint markdown files with 66 rules from mdbook-lint. Demonstrates tools
with different input types and auto-fix capabilities.

**Try these:**

-  "Lint this file: examples/README.md"
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

### 4. conformance

Full MCP protocol conformance server that passes all 39 official tests.
Demonstrates every protocol feature: tools, resources, prompts, progress
notifications, sampling, and elicitation.

**Try these:**

- "Use the conformance server to add 17 and 25"
- "Echo 'Hello MCP' using the conformance server"
- "Run the long-running operation tool and watch for progress"

**Source:** `examples/conformance-server/`

### 5. tower-mcp-example

The simplest possible MCP server - echo, add, and reverse tools. Good
starting point for understanding the basics.

**Try these:**

- "Echo 'Hello World' using tower-mcp-example"
- "Reverse the string 'tower-mcp'"

**Source:** `examples/stdio_server.rs`

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
