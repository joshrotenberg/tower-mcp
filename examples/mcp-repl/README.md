# mcp-repl

An interactive terminal REPL for any MCP server. The server's surface IS the
command set: every tool becomes a top-level command, prompts and resources
get built-ins, tab completion is powered by the server itself where the
protocol allows, and the command table refreshes live when the server's
surface changes.

## Run

```bash
# Against the bundled in-process demo router (no external server):
cargo run -p mcp-repl -- --demo

# Spawn any stdio MCP server as a child process:
cargo run -p mcp-repl -- cargo run --example getting_started

# Connect to a streamable HTTP server:
cargo run -p mcp-repl -- --http http://127.0.0.1:3001/mcp
```

## What to try

```text
getting-started> help                      # built-ins plus the server's tools
getting-started> add a=2 b=3               # tools are commands; args coerced by inputSchema
getting-started> echo message="hi there"   # tab-completes argument names
getting-started> read source://getting_started.rs
getting-started> prompt greet name=World   # prompt args tab-complete via completion/complete
```

Task-capable tools support shell-style backgrounding (SEP-2663):

```text
demo> slow_add a=2 b=3 &
[task task-1] started
demo> jobs
task-1  slow_add  working
demo> wait task-1
task task-1  status=completed
5
```

Progress and log notifications print inline as they arrive, and
`list_changed` notifications refresh the command table mid-session, so
dynamic servers (see the `dynamic_capabilities` example) grow and shrink the
REPL's vocabulary live.

## Notes

- Tab completion for prompt argument values calls the server's
  `completion/complete`, one of the least-exercised capabilities in the
  protocol. Servers that do not implement it simply contribute nothing.
- A spawned stdio child's stderr passes through to the terminal, which keeps
  server-side tracing visible while you explore.
- `call <tool> <json>` is the escape hatch when `key=value` coercion is not
  enough.
