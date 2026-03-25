# AGENTS.md

This repo has MCP servers configured in `.mcp.json` that you can use directly.

## Available MCP Servers

### tower-mcp-example
Simple demo server with `echo`, `add`, `reverse` tools and a `greet` prompt.

Resources: `source://getting_started.rs` - serves its own source code

### weather
Weather forecasts via NWS API (US only).

Tools: `get_forecast` (lat/lon), `get_alerts` (state code)

### codegen-mcp
MCP server that helps build MCP servers.

Tools: `init_project`, `add_tool`, `remove_tool`, `get_project`,
`generate`, `validate`, `reset`

Resources: `project://Cargo.toml`, `project://src/main.rs`, `project://state.json`

## Getting Started

1. Read `examples/README.md` for an index of all examples
2. Read the `source://getting_started.rs` resource to see how a server is built
3. Run any example: `cargo run --example getting_started`

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for build commands, PR guidelines, and contribution policy.

## Project Structure

- `crates/tower-mcp/src/` - Main library code
- `crates/tower-mcp-types/src/` - Protocol types (no runtime deps)
- `crates/tower-mcp-macros/src/` - Optional proc macros
- `examples/` - Example servers and clients (23 standalone `.rs` files)
  - `codegen-mcp/` - MCP server builder (generates tower-mcp code)
  - `conformance-server/` - MCP spec conformance tests (39/39)
  - `conformance-client/` - MCP conformance client (265/265 checks)
