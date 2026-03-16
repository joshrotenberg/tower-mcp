# AGENTS.md

This repo has MCP servers configured in `.mcp.json` that you can use directly.

## Available MCP Servers

### tower-mcp-example
Simple demo server with `echo`, `add`, `reverse` tools.

Resources: `source://stdio_server.rs` - serves its own source code

### markdownlint-mcp
Lint markdown files with 66 rules.

Tools: `lint_content`, `lint_file`, `lint_url`, `list_rules`,
`explain_rule`, `fix_content`

### weather
Weather forecasts via NWS API (US only).

Tools: `get_forecast` (lat/lon), `get_alerts` (state code)

### codegen-mcp
MCP server that helps build MCP servers (meta!).

Tools: `init_project`, `add_tool`, `remove_tool`, `get_project`,
`generate`, `validate`, `reset`

Resources: `project://Cargo.toml`, `project://src/main.rs`, `project://state.json`

### notes-mcp
Full-featured MCP server with tools, resources, and prompts.

Tools: note management (create, list, search, delete)

Resources: `notes://` URI scheme for individual notes

Prompts: note summarization and analysis

## Getting Started

1. Read `examples/README.md` for a guided tour of the examples
2. That file has intentional markdown errors - try linting and fixing them
3. Read the `source://stdio_server.rs` resource to see how a server is built

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for build commands, PR guidelines, and contribution policy.

## Project Structure

- `crates/tower-mcp/src/` - Main library code
- `crates/tower-mcp-types/src/` - Protocol types (no runtime deps)
- `examples/` - Example MCP servers
  - `markdownlint-mcp/` - Markdown linting server
  - `codegen-mcp/` - MCP server builder (generates tower-mcp code)
  - `notes-mcp/` - Full-featured note-taking server
  - `conformance-server/` - MCP spec conformance tests (39/39)
  - `conformance-client/` - MCP spec conformance client (265/265 checks)
  - `stdio_server.rs` - Minimal example
  - `http_server.rs` - HTTP transport example
  - `weather_server.rs` - External API example
