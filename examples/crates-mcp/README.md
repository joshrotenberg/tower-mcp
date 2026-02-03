# crates-mcp

An MCP server for querying [crates.io](https://crates.io), the Rust package registry.

This is a comprehensive example demonstrating tower-mcp features including tools, resources, prompts, and tower middleware integration.

## Features

- **10 Tools** - Complete crates.io API coverage:
  - `search_crates` - Find crates by name/keywords
  - `get_crate_info` - Get detailed crate information
  - `get_crate_versions` - Get version history
  - `get_dependencies` - Get dependencies for a version
  - `get_reverse_dependencies` - Find crates that depend on this crate (with progress notifications)
  - `get_downloads` - Get download statistics
  - `get_owners` - Get crate owners/maintainers
  - `get_summary` - Get crates.io global statistics (new crates, popular, trending)
  - `get_crate_authors` - Get authors for a specific crate version
  - `get_user` - Get a user's profile information

- **2 Resources**:
  - `crates://recent-searches` - Tracks recent search queries
  - `crates://{name}/info` - Resource template for crate information

- **2 Prompts** - Guided analysis workflows:
  - `analyze_crate` - Comprehensive crate analysis
  - `compare_crates` - Compare multiple crates for a use case

- **Completions** - Autocomplete suggestions for popular crate names

- **Progress Notifications** - Real-time progress updates during reverse dependency lookups

- **Logging** - Structured logging of API calls for debugging

- **Icons** - Visual icons on all tools

- **Tower Middleware** - Rate limiting and concurrency control

## Building

```bash
cargo build -p crates-mcp --release
```

## Running

### Stdio Transport (default)

```bash
cargo run -p crates-mcp
```

### HTTP Transport

```bash
cargo run -p crates-mcp -- --transport http --host 127.0.0.1 --port 3000
```

### With Options

```bash
cargo run -p crates-mcp -- \
  --transport http \
  --host 0.0.0.0 \
  --port 3000 \
  --max-concurrent 5 \
  --request-timeout-secs 30 \
  --log-level debug
```

## CLI Options

```
Options:
  -t, --transport <TRANSPORT>          Transport to use [default: stdio] [possible values: stdio, http]
      --max-concurrent <N>             Maximum concurrent requests [default: 10]
      --rate-limit-ms <MS>             Rate limit interval for crates.io API [default: 1000]
  -l, --log-level <LEVEL>              Log level [default: info]
      --host <HOST>                    HTTP host to bind to [default: 127.0.0.1]
  -p, --port <PORT>                    HTTP port to bind to [default: 3000]
      --request-timeout-secs <SECS>    Request timeout in seconds [default: 30]
  -h, --help                           Print help
```

## MCP Client Configuration

### Claude Desktop (stdio)

Add to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "crates": {
      "command": "cargo",
      "args": ["run", "-p", "crates-mcp", "--manifest-path", "/path/to/tower-mcp/Cargo.toml"]
    }
  }
}
```

Or if you've built the binary:

```json
{
  "mcpServers": {
    "crates": {
      "command": "/path/to/tower-mcp/target/release/crates-mcp"
    }
  }
}
```

### HTTP Client

Test with curl:

```bash
# Initialize
curl -X POST http://localhost:3000/ \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'

# Search for crates
curl -X POST http://localhost:3000/ \
  -H "Content-Type: application/json" \
  -H "MCP-Session-Id: <session-id-from-init>" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_crates","arguments":{"query":"async http"}}}'
```

### MCP Inspector

For interactive testing, use the MCP Inspector:

```bash
# Start the server with HTTP transport
cargo run -p crates-mcp -- --transport http --port 3000

# In another terminal, connect with Inspector
npx @modelcontextprotocol/inspector --transport http --server-url http://127.0.0.1:3000
```

The Inspector opens at `http://localhost:6274` where you can:

- Browse and call tools interactively
- View resources and resource templates
- Execute prompts with arguments
- Monitor server notifications and progress updates
- Test completions

## Deployment

### Docker

```bash
docker build -f examples/crates-mcp/Dockerfile -t crates-mcp .
docker run -p 3000:3000 crates-mcp
```

### Fly.io

```bash
# First time setup
fly apps create crates-mcp-demo
fly secrets set # if needed

# Deploy
fly deploy --config examples/crates-mcp/fly.toml
```

The GitHub Actions workflow automatically deploys on pushes to main.

## Example Usage

Once connected, you can:

1. **Search for crates:**
   > "Search for async HTTP client libraries"

2. **Get crate details:**
   > "Tell me about the tokio crate"

3. **Compare crates:**
   > "Compare reqwest and ureq for making HTTP requests"

4. **Analyze a crate:**
   > "Should I use serde for my project? I need JSON serialization."

## Project Structure

```
src/
├── main.rs           # Entry point, CLI, router setup
├── state.rs          # Shared state, crates.io client
├── tools/
│   ├── mod.rs
│   ├── search.rs
│   ├── info.rs
│   ├── versions.rs
│   ├── dependencies.rs
│   ├── reverse_deps.rs
│   ├── downloads.rs
│   └── owners.rs
├── resources/
│   ├── mod.rs
│   └── recent_searches.rs
└── prompts/
    ├── mod.rs
    ├── analyze.rs
    └── compare.rs
```

## Tower Middleware

This example demonstrates tower middleware integration for HTTP transport:

```rust
let transport = HttpTransport::new(router)
    .layer(
        ServiceBuilder::new()
            // Request timeout protection
            .layer(TimeoutLayer::new(Duration::from_secs(30)))
            // Limit concurrent requests
            .concurrency_limit(10)
            .into_inner(),
    );
```

The middleware protects against:
- Long-running requests (timeout)
- Resource exhaustion (concurrency limit)
- Downstream service overload (crates.io has rate limits)

## License

MIT OR Apache-2.0
