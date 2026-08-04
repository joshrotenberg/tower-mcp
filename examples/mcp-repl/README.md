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

# Import one named server from a repository or client JSON config:
cargo run -p mcp-repl -- path/to/.mcp.json:server-name

# Opt into the final, sessionless 2026-07-28 lifecycle:
cargo run -p mcp-repl -- --protocol 2026-07-28 --http http://127.0.0.1:3001/mcp
```

The binary compiles both stable and final protocol support. Runtime selection
is explicit: `--protocol stable` (the default) uses
`initialize`/`notifications/initialized`, while `--protocol 2026-07-28`
(`--protocol final` is an alias) uses `server/discover` and sends the selected
protocol metadata on every request. Keeping stable as the default means an
mcp-repl upgrade cannot silently change an existing server's lifecycle.

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

For an MCP server using OAuth authorization-code + PKCE, create a named login
without opening an MCP session:

```bash
# Discovers the protected resource and authorization server, opens the browser,
# receives the redirect on an ephemeral loopback port, and saves the credentials.
mcp-repl --login work --http https://mcp.example.com/mcp \
  --oauth-scope openid --oauth-scope offline_access

# Reuse its saved URL directly, retarget it with --http, or select it through
# a server profile.
mcp-repl --oauth work
mcp-repl --oauth work --http https://mcp-alt.example.com/mcp

# If automatic browser launch is unavailable, print the URL and wait for the
# loopback redirect (remote use requires forwarding that loopback callback).
mcp-repl --login work --http https://mcp.example.com/mcp --no-browser

# Remove both profile metadata and credentials.
mcp-repl --logout work
```

Login follows MCP protected-resource and authorization-server discovery,
requires PKCE S256, tries an optional Client ID Metadata Document before
Dynamic Client Registration, and requests refresh-token support when the
server advertises it. Use
`--oauth-client-id-metadata-document https://client.example/metadata.json` for
CIMD or `--oauth-authorization-server ISSUER` to select one exact issuer when
discovery advertises several.

Only non-secret routing metadata is written to `config.toml`. Access tokens,
refresh tokens, and dynamically registered client secrets are kept in macOS
Keychain, Windows Credential Manager, or the Linux Secret Service through the
platform credential store. If no secure store is available, mcp-repl fails
closed; it never writes a plaintext credential fallback. A saved expired token
is refreshed automatically. A failed refresh tells you to run `--login` again;
an explicit login discards the unusable token while retaining reusable DCR
registration.

`--exec`/`--json` never starts an interactive authorization or opens a browser.
It either restores/refreshes the saved credential or exits with an actionable
`--login` command. Runtime insufficient-scope challenges are retried at most
twice; interactive sessions can authorize the added scopes, while one-shot
commands fail immediately with the same login guidance.

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

[oauth.work]
url = "https://mcp.example.com/mcp"
scopes = ["openid", "offline_access"]

[servers.work]
transport = "http"
oauth = "work"
headers = { "X-Tenant" = "acme" }

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
  `command` is stdio, and one with `oauth` is HTTP and may reuse that OAuth
  profile's saved URL. A profile with both a URL/OAuth selection and a command
  must say which.
- Explicit flags override profile fields. `--http <url>` retargets the URL
  while keeping the profile's auth; `--bearer` replaces the profile's token;
  each `--header` overrides the profile header of the same name.
- OAuth precedence is explicit static authorization (`--bearer` or
  `--header Authorization`) first, then explicit `--oauth`, then a server
  profile's `oauth`, then native/imported static credentials, and finally
  `MCP_BEARER`. A server profile cannot combine `oauth` with `bearer`,
  `bearer_env`, or an `Authorization` header; non-auth headers remain valid.
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

### Importing standard MCP configs

An explicit `PATH:ENTRY` selector imports a named server from the common JSON
format used by repository `.mcp.json` files, VS Code, Claude, Cursor, and other
MCP clients. Both `mcpServers` and `servers` roots are accepted; automatic
file discovery is deliberately deferred so the selected source is always
visible in the command:

```bash
mcp-repl .mcp.json:local
mcp-repl .vscode/mcp.json:remote
mcp-repl --server "$HOME/Library/Application Support/Claude/claude_desktop_config.json:github"
```

```json
{
  "mcpServers": {
    "local": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "${workspaceFolder}/server/Cargo.toml"],
      "env": { "API_TOKEN": "${env:HOST_API_TOKEN}" },
      "cwd": "${workspaceFolder}"
    }
  },
  "servers": {
    "remote": {
      "type": "http",
      "url": "${env:MCP_URL}",
      "headers": { "Authorization": "Bearer ${env:MCP_TOKEN}" }
    }
  }
}
```

- `stdio` entries preserve `command`, ordered `args`, `env`, and `cwd`.
  Relative working directories resolve from the config's workspace directory
  (the parent of `.vscode` for `.vscode/mcp.json`, otherwise the file's
  directory). The child inherits the current environment, with imported `env`
  values overriding matching keys.
- `http` and `streamable-http` entries preserve `url` and `headers`. Legacy
  `sse` entries are rejected because mcp-repl connects with Streamable HTTP.
- `${env:NAME}` and `${NAME}` read the launching environment;
  `${workspaceFolder}`, `${workspaceFolderBasename}`, and `${userHome}` are
  also supported. A missing variable is an error. `${input:...}` is rejected
  with guidance because an imported interactive input has no portable value
  outside the client that defined it.
- Precedence is explicit flags first, then the imported entry, then native
  profiles when no import was selected. `--http` can retarget an imported HTTP
  entry while retaining its headers; `--bearer` and repeated `--header` values
  override imported authentication. `MCP_BEARER` remains the final bearer
  fallback when no selected configuration or flag supplies one.
- Unknown entries list available names in sorted order. Entries with both a
  command and URL, conflicting transport declarations, missing required
  fields, or transport-specific fields on the wrong transport are refused.
- **Trust boundary:** selecting an imported `stdio` entry executes its command
  directly with the declared arguments, environment, and working directory.
  Treat the JSON file as executable code and review files from repositories or
  people you do not trust. mcp-repl never invokes a shell for the entry and
  does not print imported environment or header values, but literal secrets in
  the source file are still secrets at rest.
- Native global aliases remain available for imported connections. Imported
  files do not define mcp-repl aliases, so aliases created while using one are
  global.

### Reconnecting

A remote server that restarts, OOMs, or sits behind an edge returning 502/503
can interrupt a connection. On an `--http` connection the REPL notices this,
creates a fresh transport, repeats the selected stable or final handshake,
re-fetches the surface, and retries the command once. For stable servers this
also replaces the lost session; final connections are sessionless.

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

Single or double quotes group whitespace into one argument, and the REPL
removes those grouping quotes before schema coercion. A backslash escapes the
next character outside single quotes. JSON object and array arguments retain
their JSON quotes and spaces exactly, including when passed through `call`:

```text
getting-started> echo message="hello world"
getting-started> call echo {"message": "hello world"}
```

An unmatched quote, trailing escape, or unclosed JSON argument is reported
locally without calling the server. A quoted or escaped `&` is ordinary input;
only a plain trailing `&` requests task-augmented execution.

`resources` lists concrete resources and `templates` lists parameterized
(`{variable}`) ones; each points at the other so a server that splits its
resources across the two MCP lists is not confusing.

Task-capable tools support shell-style backgrounding (SEP-2663):

```text
demo> slow_add a=2 b=3 &
[task task-1] started
[task task-1] completed  run `task task-1` for details
demo> task task-1
task task-1  status=completed
5
```

The REPL tracks only tasks it started and consumes both legacy and final typed
task-status notifications, deduplicating repeated transitions. A final client
opens a task-scoped `subscriptions/listen` stream; a bounded per-task poller
remains authoritative for stable servers and for unavailable or dropped final
notifications. It honors the server's suggested interval, ends at a terminal
state, and gives up after three consecutive read failures. `jobs`, `task`,
`wait`, and `cancel` remain the authoritative manual controls.

Automatic transition lines are interactive-only. `--exec` and `--json`
suppress them for deterministic scripted output; explicit task commands still
return their normal text or JSON results.

Progress and log notifications print inline as they arrive, and
`list_changed` notifications refresh the command table mid-session, so
dynamic servers (see the `dynamic_capabilities` example) grow and shrink the
REPL's vocabulary live. Stable connections receive those notifications on
their ordinary transport. An interactive final connection opens one
`subscriptions/listen` stream for tool, prompt, and resource list changes
after its initial surface fetch, validates the server's acknowledged subset,
and reopens the stream after a reconnect or unexpected ending with bounded
backoff. `--exec` never opens this background stream, preserving deterministic
one-shot output.

For a spawned stdio server, child diagnostics remain visible but are read from
the child's stderr and passed through reedline's external printer. Logs that
arrive while you are typing therefore appear above a cleanly redrawn prompt
instead of splitting the current input. In `--exec` mode they remain on stderr,
so `--json` stdout contains only command results.

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

## Schema snapshots and compatibility checks

Tool and prompt definitions can be saved as versioned, canonical JSON
contracts. Snapshots intentionally omit descriptions, icons, annotations, and
other presentation metadata: a documentation edit should not break a caller.

```text
demo> snapshot add add.schema.json
saved tool "add" schema snapshot to add.schema.json
demo> validate add.schema.json compatible
tool "add" is compatible under compatible validation
```

Without a path, `snapshot <name>` prints the canonical JSON. Use
`snapshot tool:<name>` or `snapshot prompt:<name>` when both namespaces expose
the same name. `validate <path> [strict|compatible|ignore]` reads a snapshot
and compares it with the advertised surface without invoking anything:

- `strict` requires the entire canonical contract to match.
- `compatible` protects existing callers: removed or retyped inputs, newly
  required inputs, removed or retyped expected outputs, and prompt argument
  breaks fail. Additive optional inputs/arguments and additive outputs pass;
  input widening and output narrowing (for example, integer output replacing
  number output) also pass.
- `ignore` loads the snapshot but deliberately skips enforcement, which is
  useful while rolling out contracts in automation.

Nested object/array schemas and local JSON Schema references such as
`#/$defs/filter` are followed recursively. External references are rejected
because validation is offline and must not fetch code or schemas implicitly.
Changes to complex `anyOf`, `oneOf`, `allOf`, or `not` compositions are treated
conservatively as incompatible.

Repeat `--schema-contract <path>` to enforce snapshots before matching tool
calls, task-augmented calls, benchmarks, or prompt retrievals. The default is
compatible mode; `--schema-mode strict|compatible|ignore` changes it:

```bash
mcp-repl --schema-contract add.schema.json \
  --schema-mode compatible --http https://example/mcp
```

A successful preflight is silent. An incompatible preflight sends no MCP
request, returns status 1, and explains every finding. Under `--json` the
validation report is the command's single NDJSON value, so the scripting
framing contract is preserved.

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
Repeatable; commands run in order against the same session, including after a
failure, so later cleanup or inspection commands still run. The final status
is the highest-severity outcome seen across the sequence.

```bash
# One call, pretty output:
mcp-repl --http https://example/mcp -e "get_crate_info name=serde"

# One JSON result for piping to jq (--json also silences the banner and timings):
mcp-repl --http https://example/mcp -e "search_crates query=serde" --json | jq '.content'

# Several commands in one session; JSON output is NDJSON, one value per line:
mcp-repl --demo --json -e "tools" -e "echo message=hi" | jq -c .

# Human output from several commands:
mcp-repl --demo -e "echo message=hi" -e "about"
```

In human `--exec` mode the banner and surface listing are suppressed by
default; pass `--verbose` to keep them. Under `--json`, stdout is always a
machine-only [NDJSON](https://github.com/ndjson/ndjson-spec) stream: every
executed command emits exactly one compact, independently parseable value on
one line. `--verbose` never adds a banner there. Timings, tracing, progress,
notifications, reconnect notices, spawned-child diagnostics, and warnings go
to stderr.

Successful protocol operations preserve their MCP result shape: foreground
tool calls return `CallToolResult`, `read` returns `ReadResourceResult`,
`prompt` returns `GetPromptResult`, task commands return `TaskObject`, and a
task-augmented tool call returns its task-creation result. Surface list commands
return convenience arrays of their protocol definitions (without pagination
wrappers). REPL-only commands use documented convenience values or envelopes:
`find` and `subscriptions` return arrays; `describe` returns
`{"kind": ..., "definition": ...}`; `snapshot` returns its canonical contract
or a file acknowledgement; `validate` returns its compatibility report; and
`help`, `bench`, `jobs`, aliases, `wire`, `last`, `refresh`, `info`, `vars`,
`unset`, and `quit` return objects.

JSON errors also stay on stdout so they occupy that command's one output line:

```json
{"error":"unknown command: nope","kind":"usage","exitStatus":2}
```

Diagnostics explaining the failure may still appear on stderr. Human mode
keeps readable text output. Process statuses are stable:

| Status | Meaning |
| ---: | --- |
| 0 | success |
| 1 | no-match/check-style result (for example, `find` found nothing) |
| 2 | local invocation or command usage error |
| 3 | server rejection or tool error result |
| 4 | transport or protocol connection failure |
| 5 | authentication or authorization failure |

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

## Testing

`cargo test -p mcp-repl` includes a black-box process suite in addition to the
unit tests. It builds a repository-only MCP fixture, launches the published
`mcp-repl` binary, and covers stdio and ephemeral localhost HTTP with both the
stable and exact `2026-07-28` lifecycles. The cases are network-independent,
bounded by per-process and suite timeouts, and assert fixture cleanup.

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
