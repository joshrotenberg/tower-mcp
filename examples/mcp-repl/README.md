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

### Reconnecting

A remote server that restarts, OOMs, or sits behind an edge returning 502/503
loses the session, and every later request fails with a not-initialized or
session-expired error. On an `--http` connection the REPL notices this,
re-runs the handshake against a fresh transport, re-fetches the surface, and
retries the command once:

```text
> search query=tower
[reconnected]
... results ...
```

The retry is bounded to a single attempt, so a server that is really down
fails fast with its original error rather than hanging the prompt. Task ids
do not survive a reconnect (they belong to the session that created them), so
`task`, `wait`, and `cancel` never trigger one.

Pass `--no-reconnect` to turn this off and see session-loss errors as they
arrive. stdio children and `--demo` are never reconnected: there, a lost
session means the server process itself is gone.

## Aliases

Frequent commands get short names, kept in the same config file as the
profiles:

```text
cratesio> alias dl=get_downloads
dl = get_downloads  (profile cratesio)
cratesio> dl crate=serde
...
cratesio> alias
dl  get_downloads  (profile cratesio)
t   tools          (global)
cratesio> unalias dl
removed dl (profile cratesio)
```

- `alias` lists what is in effect, `alias <name>` shows one, `alias
  <name>=<expansion>` defines, and `unalias <name>` removes.
- Expansion is a literal substitution of the first word with whatever
  followed the alias appended: with `dl = "get_downloads"`, `dl crate=serde`
  runs `get_downloads crate=serde`. An expansion that itself starts with an
  alias expands again; a cycle is reported rather than looped.
- An expansion can end in `&`, so an alias can run its tool task-augmented.
- Scope: an alias defined while connected through a profile belongs to that
  profile; otherwise it is global. `alias --global <name>=<expansion>` forces
  the file-level table. A profile alias shadows a global one of the same
  name, and `unalias` removes the definition that is actually in effect
  (`--global` reaches past a profile alias to the global one).
- Aliases cannot be named after a built-in, since expansion happens before
  dispatch and the built-in would become unreachable. An alias that shadows a
  *tool* is allowed, and says so when defined.
- Every change is written back to the config file through `toml_edit`, so
  comments, key order, and formatting elsewhere in the file survive. Removing
  the last alias leaves the (now empty) table, because a comment above
  `[aliases]` belongs to that table and would go with it.

```toml
[aliases]                       # every server
t = "tools"

[servers.cratesio.aliases]      # only through this profile
dl = "get_downloads"
```

With no config file location at all (no `$HOME`, no `--config`), aliases
still work for the session and the REPL says they were not saved.

## What to try

```text
getting-started> help                      # built-ins plus the server's tools
getting-started> add a=2 b=3               # tools are commands; args coerced by inputSchema
getting-started> echo message="hi there"   # tab-completes argument names
getting-started> find note                 # keyword search across the surface
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

## bench

The `[142ms]` annotation answers "how slow was that call". `bench` answers
"how slow is this tool", which is the question behind a server sitting on a
network, a cold cache, or a rate limiter.

```text
cratesio> bench get_downloads crate=serde --n 50
50 calls  ok=50 err=0  min=88ms p50=104ms p95=190ms max=311ms
[5.42s]
cratesio> bench get_downloads crate=serde --n 50 --concurrency 8
50 calls  ok=50 err=0 concurrency=8  min=91ms p50=127ms p95=402ms max=655ms
[892ms]
```

- `bench <tool> [k=v...] [--n N] [--concurrency C]`. Arguments are coerced
  against the tool's `inputSchema` exactly as a direct call is, so
  `bench <tool> a=1` benchmarks the request `<tool> a=1` would send. Flags may
  appear anywhere after the tool name, in either spelling (`--n 50`,
  `--n=50`).
- `--n` defaults to 20 and is capped at 100000; `--concurrency` defaults to 1
  (serial) and never exceeds `--n`. Workers pull from a shared counter, so one
  slow call does not leave a worker's remaining share queued behind it.
- Percentiles are nearest-rank over the calls that succeeded, so every number
  reported is a latency that actually happened. Failures are counted
  separately, with the first message shown, rather than folded into the
  distribution: a fast rejection is not a fast call.
- A tool result with `isError` counts as a failure, and any failure makes the
  command exit non-zero, so `mcp-repl -e "bench <tool> --n 20" <server>` works
  as a scripted health check.
- Under `--json`, an object with `calls`, `ok`, `errors`, `concurrency`,
  `firstError`, and `minMs` / `p50Ms` / `p95Ms` / `maxMs` / `totalMs`. The
  latency fields are `null` when nothing succeeded, so a failed run cannot be
  read as an instant one.

## Resource subscriptions

A server that supports `resources.subscribe` will push
`notifications/resources/updated` for the resources you ask about. The REPL
prints those inline, the way progress and log lines arrive:

```text
demo> subscribe note://status
subscribed note://status
demo> subscriptions
note://status
[resource updated] note://status
demo> unsubscribe note://status
unsubscribed note://status
```

- `subscribe <uri>` and `unsubscribe <uri>` complete from the surface and
  from what is actually subscribed, respectively.
- The local set is only updated once the server agrees, so `subscriptions`
  lists what the server is sending updates for, not what was asked for.
  Re-subscribing to something already held says so rather than double-counting.
- A server that does not advertise `resources.subscribe` gets a warning before
  the request goes out, so the rejection is explained rather than bare.
- An update for something this session did not subscribe to is still printed,
  tagged `(not subscribed here)`.
- The resource is not re-read on an update: reading may be expensive, and the
  point is to know it moved. Follow with `read <uri>` when you want the content.

## Wire tracing

Half of any "is it the client, the server, or the network?" question is
answered by the raw JSON-RPC frames. `--trace` prints every frame from the
start; `wire on` / `wire off` toggles it mid-session.

```text
demo> wire on
wire tracing on (frames print to stderr)
demo> echo message=hi
[wire ->] +4.512s
{
  "id": 6,
  "jsonrpc": "2.0",
  "method": "tools/call",
  "params": { "arguments": { "message": "hi" }, "name": "echo" }
}
[wire <-] +4.524s [12ms]
{
  "id": 6,
  "jsonrpc": "2.0",
  "result": { "content": [ { "text": "hi", "type": "text" } ] }
}
```

Each frame carries its direction, a session-relative timestamp, and, on a
response, the time its request was outstanding.

`last` reprints the previous request and its response whether or not tracing
was on: frames are always recorded, so the exchange you did not think to
trace is still there. Under `--json` it prints a
`{"request": ..., "response": ...}` object instead.

Frames print to stderr, so `--json` output on stdout stays pipeable with
tracing on.

Secrets are masked before a frame is stored, so nothing unmasked reaches the
trace or `last`: values under `authorization`, `token`, `apiKey`, `secret`,
`password` and similar keys (separators and case ignored), and anything
following `Bearer ` inside a string. The HTTP `Authorization` header itself
never appears here, since it is not part of a JSON-RPC frame.

## Completion

Tab opens a columnar menu. What gets completed:

- The command word: built-ins, aliases (shown with what they expand to), and
  every tool, each with its description.
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
- `bench <tool> ...`: tool names in the first position, then that tool's
  argument names, and `--n` / `--concurrency` after a leading `-`.
- `unalias <name>`: the aliases in effect, with their scope.

## find

A server with dozens of tools is not navigable by listing it. `find
<keyword>` searches names and descriptions across tools, prompts, resources,
and templates, grouped by kind:

```text
cratesio> find download
tools:
  get_downloads            Get download statistics
  get_version_downloads    Daily download stats for a specific version
2 matches
```

Matching is case-insensitive. Results rank an exact name match first, then a
name prefix, then a name substring, then a description match, and last a
subsequence (`gvd` reaches `get_version_downloads`) so a loose match never
buries a literal one. The search runs against the cached surface, so it
issues no request.

Under `--json` it prints an array of `{kind, name, description, score}`
objects. A search that matched nothing exits non-zero, following grep.

A mistyped command word gets the nearest built-in, tool, or prompt name by
edit distance:

```text
cratesio> serch_crates query=serde
unknown command: serch_crates; did you mean `search_crates`?
```

The tolerance scales with the length of what you typed, so a short word does
not collect a suggestion from across the surface. When nothing is close
enough, the message points at `help` as before.

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

## Capture and filtering

The REPL is a small shell. A command's result can be captured into a variable,
referenced in later arguments, or filtered inline.

```text
demo> x = search_crates query=serde
$x = {2 fields}
demo> get_crate_info name=$x.crates[0].name
...
demo> get_crate_info name=serde | crates[0].downloads
11897234
```

- **Capture:** `name = <command>` binds the command's result to `$name`. The
  spaces around `=` distinguish it from a `k=v` argument and from `alias name=...`.
- **Reference:** `$name` and `$name.path[i].field` expand in later command
  arguments before the command runs.
- **Filter:** `<command> | <path>` prints just the selected value. A scalar
  prints bare; an object or array prints as JSON.
- **Paths** are a small selector: `.field`, `[index]`, chained (`crates[0].name`).
  An undefined variable or a missing path is an error, so a typo fails an `-e`
  chain rather than passing silently. JMESPath is a possible future addition.
- **`vars`** lists what is bound; **`unset <name>`** clears one. Variables live
  for the session, so they persist across an `-e` chain:

```sh
mcp-repl -e "x = search_crates query=serde" \
         -e "get_crate_info name=\$x.crates[0].name" <server>
```

Capture and filtering currently act on tool-call results.

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
