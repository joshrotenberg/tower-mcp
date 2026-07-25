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

### Authenticated servers

Attach credentials to an `--http` connection:

```bash
# Bearer token. Prefer MCP_BEARER: a --bearer on the command line is visible
# in `ps` and shell history.
MCP_BEARER="$TOKEN" mcp-repl --http https://internal.example/mcp
mcp-repl --http https://internal.example/mcp --bearer "$TOKEN"

# Arbitrary headers, repeatable (split on the first colon):
mcp-repl --http https://internal.example/mcp --header "X-Api-Key: abc"
```

`--bearer` and `--header` apply only to HTTP connections; they are ignored
(with a warning) for the demo and stdio-child transports.

## Profiles

A config file names servers so a connection is `mcp-repl <name>` instead of a
URL plus repeated auth flags, and tokens stay out of shell history. The file
lives at `$XDG_CONFIG_HOME/mcp-repl/config.toml`, falling back to
`~/.config/mcp-repl/config.toml`; `--config <path>` reads a different one.

```toml
[servers.cratesio]
transport = "http"                 # http | stdio
url = "https://cratesio-mcp.fly.dev/"
bearer_env = "CRATESIO_TOKEN"      # read the token from the environment
headers = { "X-Api-Key" = "abc" }

[servers.local]
transport = "stdio"
command = ["cargo", "run", "--example", "getting_started"]
```

```bash
mcp-repl --list-servers            # the configured profiles
mcp-repl --server cratesio         # connect by name
mcp-repl cratesio                  # a bare name works too
```

- `transport` is optional: a profile with a `url` is HTTP, one with a
  `command` is stdio. A profile with both must say which.
- Explicit flags override profile fields. `--http <url>` retargets the URL
  while keeping the profile's auth; `--bearer` replaces the profile's token;
  each `--header` overrides the profile header of the same name.
- The bare-name form only resolves when the single positional matches a
  configured profile, so spawning a stdio server by bare name still works.
  Because everything after the first positional belongs to the spawned
  command, use `--server <name>` when other flags follow.
- Secrets: `bearer_env` names an environment variable holding the token. An
  unset variable is an error rather than a silent anonymous connection. An
  inline `bearer = "..."` works but warns, since it puts the token in the
  file.
- An unknown profile name errors with the list of known names, and a missing
  `--config` file is an error. A missing file at the default location is not:
  profiles are opt-in.

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
- Every tool call, `read`, and `prompt` prints a dimmed `[142ms]` / `[1.23s]`
  annotation with the round-trip time, so a slow (or timing-out) call is
  visible at a glance.

All styling degrades to plain text when `NO_COLOR` is set or stdout is not
a terminal. `--color always|never|auto` overrides the detection.

## Elicitation

Tools that request user input via `elicitation/create` prompt for each
field at the terminal during a foreground call: the field's type, default,
and description are shown, empty input accepts the default, and EOF
cancels. Try `test_elicitation` against the conformance server. If a
background task elicits while the editor owns the terminal, the request is
declined rather than fighting the editor for stdin.

## One-shot / scripting

`-e/--exec <COMMAND>` runs a command and exits instead of opening the prompt.
Repeatable; commands run in order against the same session. The exit status is
non-zero if any command errored, so it drops into scripts and CI.

```bash
# One call, pretty output:
mcp-repl --http https://example/mcp -e "get_crate_info name=serde"

# Raw JSON for piping to jq (--json also silences the banner and timings):
mcp-repl --http https://example/mcp -e "search_crates query=serde" --json | jq '.content'

# Several commands in one session:
mcp-repl --demo -e "echo message=hi" -e "about"
```

The banner and surface listing are suppressed in `--exec` mode (pass
`--verbose` to keep them). `--json` applies to tool calls, `read`, `prompt`,
`tools`/`prompts`/`resources`/`templates`, and errors (`{"error": "..."}`).

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
- Command history persists to `~/.mcp-repl_history` (up to 1000 entries), so
  up-arrow recalls commands from previous sessions. Pass `--no-history` to
  keep it in-memory only.
