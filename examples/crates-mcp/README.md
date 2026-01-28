# crates-mcp

An MCP server for querying [crates.io](https://crates.io), the Rust package registry.

This is a comprehensive example demonstrating tower-mcp features including tools, resources, prompts, and tower middleware integration.

## Features

- **7 Tools** - One per crates.io API endpoint:
  - `search_crates` - Find crates by name/keywords
  - `get_crate_info` - Get detailed crate information
  - `get_crate_versions` - Get version history
  - `get_dependencies` - Get dependencies for a version
  - `get_reverse_dependencies` - Find crates that depend on this crate
  - `get_downloads` - Get download statistics
  - `get_owners` - Get crate owners/maintainers

- **1 Resource** - `crates://recent-searches` tracks recent queries

- **2 Prompts** - Guided analysis workflows:
  - `analyze_crate` - Comprehensive crate analysis
  - `compare_crates` - Compare multiple crates for a use case

- **Tower Middleware** - Demonstrates concurrency limiting

## Building

```bash
cargo build -p crates-mcp
```

## Running

### Stdio Transport (default)

```bash
cargo run -p crates-mcp
```

### With Options

```bash
cargo run -p crates-mcp -- \
  --max-concurrent 3 \
  --rate-limit-ms 2000 \
  --log-level debug
```

## CLI Options

```
Options:
  -t, --transport <TRANSPORT>        Transport to use [default: stdio]
      --max-concurrent <N>           Maximum concurrent tool calls [default: 5]
      --bulkhead-timeout-ms <MS>     Max wait time for bulkhead permit [default: 100]
      --requests-per-second <N>      Maximum requests per second [default: 10]
      --rate-limit-ms <MS>           Rate limit interval for crates.io API [default: 1000]
  -l, --log-level <LEVEL>            Log level [default: info]
  -h, --help                         Print help
```

## MCP Client Configuration

### Claude Desktop

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
      "command": "/path/to/tower-mcp/target/debug/crates-mcp"
    }
  }
}
```

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

This example demonstrates tower middleware integration with [tower-resilience](https://crates.io/crates/tower-resilience-ratelimiter) layers:

```rust
use tower::ServiceBuilder;
use tower_resilience_bulkhead::BulkheadLayer;
use tower_resilience_ratelimiter::RateLimiterLayer;

// Rate limiter: token bucket algorithm
let rate_limiter = RateLimiterLayer::builder()
    .limit_for_period(10)              // 10 requests
    .refresh_period(Duration::from_secs(1))  // per second
    .timeout_duration(Duration::from_millis(500))
    .build();

// Bulkhead: isolate concurrent requests with timeout
let bulkhead = BulkheadLayer::builder()
    .max_concurrent_calls(5)           // max 5 in-flight
    .max_wait_duration(Some(Duration::from_millis(100)))
    .build();

// Stack multiple middleware layers
let service = ServiceBuilder::new()
    .layer(rate_limiter)
    .layer(bulkhead)
    .service(router);
```

### Middleware Stack

| Layer | Crate | Purpose |
|-------|-------|---------|
| RateLimiterLayer | tower-resilience-ratelimiter | Token bucket rate limiting (N/sec) |
| BulkheadLayer | tower-resilience-bulkhead | Concurrent request isolation with wait timeout |

### Why Both?

- **RateLimiter**: Smooths traffic over time, prevents burst flooding
- **Bulkhead**: Isolates resources, fails fast when overloaded

Together they protect against:
- Request floods and DoS
- Resource exhaustion (memory, file handles)
- Downstream service overload (crates.io has rate limits)
- Cascading failures

## License

MIT OR Apache-2.0
