# mcp-repl

An interactive terminal REPL for any MCP server. The server's surface IS the
command set: every tool becomes a top-level command, prompts and resources
get built-ins, tab completion is powered by the server itself where the
protocol allows, and the command table refreshes live when the server's
surface changes.

The editor is [reedline](https://crates.io/crates/reedline) (nushell's line
editor): a columnar completion menu with per-candidate descriptions, live
input highlighting, and fish-style history hints.

## Run

```bash
# Against the bundled in-process demo router (no external server):
cargo run -p mcp-repl -- --demo

# Spawn any stdio MCP server as a child process:
cargo run -p mcp-repl -- cargo run --example getting_started

# Connect to a streamable HTTP server:
cargo run -p mcp-repl -- --http http://127.0.0.1:3001/mcp
```

### Try it against a live server

[cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp) (an MCP server
for the crates.io registry, also built on tower-mcp) runs a public instance:

```bash
cargo run -p mcp-repl -- --http https://cratesio-mcp.fly.dev/
```

```text
cratesio-mcp> search_crates query=tower-mcp per_page=3
cratesio-mcp> get_crate_health name=serde
cratesio-mcp> read crates://tokio/info
cratesio-mcp> prompt analyze_crate crate_name=axum
```

## What to try

```text
getting-started> help                      # built-ins plus the server's tools
getting-started> add a=2 b=3               # tools are commands; args coerced by inputSchema
getting-started> echo message="hi there"   # tab-completes argument names
getting-started> describe add              # input/output schemas, colored
getting-started> read source://getting_started.rs
getting-started> prompt greet name=World   # prompt args tab-complete via completion/complete
getting-started> info                      # replay the startup banner (identity, instructions, counts) + capabilities
```

`resources` lists concrete resources and `templates` lists parameterized
(`{variable}`) ones; each points at the other so a server that splits its
resources across the two MCP lists is not confusing.

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

## Completion

Tab opens a columnar menu. What gets completed:

- The command word: built-ins plus every tool, each with its description.
- Tool argument names from the tool's `inputSchema` properties (with type,
  required flag, and description), and enum values after `key=` when the
  property declares an `enum`.
- `read <uri>`: resource URIs and template URI templates. When the partial
  reaches a template's `{variable}`, the server's `completion/complete` is
  asked to complete the variable (2s timeout, best-effort). Try
  `read note://<Tab>` in `--demo`.
- `prompt <name> <arg>=`: argument values via `completion/complete`, and
  argument names from the prompt definition.
- `describe <name>`: everything on the surface, labeled by kind.

## describe

`describe <name>` looks up a tool, prompt, resource, or template by name:

- Tools: behavior hints, task support, and the input/output schemas as
  syntax-colored JSON.
- Prompts: the argument table (name, required/optional, description).
- Resources and templates: URI, name, MIME type, size, and description.

## Output rendering

- JSON output (schema dumps, `info` capabilities, non-text content) is
  pretty-printed with a small built-in syntax colorizer.
- Text content that looks like markdown gets a light terminal rendering:
  bold headings, dimmed code fences, styled inline code and bold spans,
  colored bullets.
- Progress, log, and task lines are tagged with dim brackets; task statuses
  are colored (working=yellow, completed=green, failed/cancelled=red).

All styling degrades to plain text when `NO_COLOR` is set or stdout is not
a terminal. `--color always|never|auto` overrides the detection.

## Elicitation

Tools that request user input via `elicitation/create` prompt for each
field at the terminal during a foreground call: the field's type, default,
and description are shown, empty input accepts the default, and EOF
cancels. Try `test_elicitation` against the conformance server. If a
background task elicits while the editor owns the terminal, the request is
declined rather than fighting the editor for stdin.

## Related tools

- [mcp-probe](https://github.com/conikeec/mcp-probe): a Rust TUI debugging
  toolkit for MCP servers (ratatui dashboard, protocol analysis, timing
  metrics, compliance checks). Complementary rather than overlapping:
  mcp-probe is a debugging platform you inspect a server with; mcp-repl is a
  shell you drive one from.

## Notes

- Tab completion for prompt argument values and resource template variables
  calls the server's `completion/complete`, one of the least-exercised
  capabilities in the protocol. Servers that do not implement it simply
  contribute nothing.
- A spawned stdio child's stderr passes through to the terminal, which keeps
  server-side tracing visible while you explore.
- `call <tool> <json>` is the escape hatch when `key=value` coercion is not
  enough.
- When stdin is not a tty, the REPL reads lines directly (no editor), so
  piping a script of commands works:
  `printf 'echo message=hi\nquit\n' | mcp-repl --demo`.
