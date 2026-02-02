# AGENTS.md

This repo has MCP servers configured in `.mcp.json` that you can use directly.

## Available MCP Servers

### crates-mcp-local
Query the Rust crate registry (crates.io).

Tools: `search_crates`, `get_crate_info`, `get_crate_versions`,
`get_dependencies`, `get_reverse_dependencies`, `get_downloads`,
`get_owners`, `get_summary`, `get_crate_authors`, `get_user`

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

### conformance
Full MCP protocol conformance server (39/39 tests passing).

Tools: `test_simple_text`, `test_tool_with_progress`, `test_sampling`,
`test_elicitation`, and more

### tower-mcp-example
Simple demo server with `echo`, `add`, `reverse` tools.

Resources: `source://stdio_server.rs` - serves its own source code

## Getting Started

1. Read `examples/README.md` for a guided tour of the examples
2. That file has intentional markdown errors - try linting and fixing them
3. Read the `source://stdio_server.rs` resource to see how a server is built

## Development

Before committing, run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib --all-features
cargo test --test '*' --all-features
cargo test --doc --all-features
```

## Project Structure

- `src/` - Main library code
- `examples/` - Example MCP servers
  - `crates-mcp/` - Full-featured crates.io server
  - `markdownlint-mcp/` - Markdown linting server
  - `codegen-mcp/` - MCP server builder (generates tower-mcp code)
  - `conformance-server/` - MCP spec conformance tests
  - `stdio_server.rs` - Minimal example
  - `weather_server.rs` - External API example
