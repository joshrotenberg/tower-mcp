# rmcp-compat

Wire-format compatibility harness for tower-mcp. Starts minimal rmcp and
tower-mcp HTTP servers side by side, fires identical MCP JSON-RPC requests at
each, and structurally compares the responses.

## What it does

The harness runs three in-process servers:

- **rmcp** on port 4001 via `StreamableHttpService`
- **tower-mcp** on port 4002 in default (bare JSON) mode
- **tower-mcp SSE** on port 4003 with `.sse_responses(true)` to match rmcp's SSE wrapping

It then runs a series of checks across all four operation groups:

| Check | What is verified |
|-------|-----------------|
| `initialize` | `protocolVersion` present and equal; `capabilities` is object; `serverInfo.name/version` are strings |
| `tools/list` | `result.tools` is array; first tool name matches; `inputSchema` field name and shape |
| `tools/call` | `result.content` is array; `content[0].type` matches; echo returns correct value |
| `method-not-found` | `error.code` is `-32601` on both; `error.message` is string on both |
| `resources/list` | `result.resources` is array (may be empty) |
| `prompts/list` | `result.prompts` is array (may be empty) |
| `resources/read` | Not-found URI returns an error; code is `-32602` per SEP-2164 |
| `invalid-params` | Missing required tool argument returns error code `-32602` |
| `initialized-enforcement` | Both reject requests before `notifications/initialized` with `-32600` |
| `sse-response-mode` | With `.sse_responses(true)`, both return `text/event-stream` with valid JSON-RPC |

## How to run

```
cargo run -p rmcp-compat
```

The tool binds ports 4001, 4002, and 4003 on localhost. Make sure those ports
are free before running.

## Output format

Each check prints one of three prefixes:

- **`[PASS]`** -- behavior matches between rmcp and tower-mcp for this property
- **`[FAIL]`** -- unexpected divergence; investigate before releasing
- **`[KNOWN-DIFF]`** -- documented behavioral difference; not a bug (see below)

A final summary line shows counts. The process exits with code 0 if there are
no FAIL or ERROR results; known diffs and passes do not count against the exit
code.

Example output:

```
=== initialize ===
[PASS] initialize: protocolVersion present and equal (2025-11-25)
[PASS] initialize: capabilities is object on both
...

=== method-not-found ===
[PASS] method-not-found: error.code == -32601 on both
[PASS] method-not-found: error.message is string on both
[KNOWN-DIFF] method-not-found: error.message phrasing differs (both use -32601): ...

Results: 18/20 checks passed (0 failed, 2 known-diffs, 0 errors)
```

## Known diffs

Known diffs are documented divergences that are intentional or explained. They
appear as `[KNOWN-DIFF]` and do not cause a non-zero exit code.

Current known diffs:

**`error.message` phrasing for method-not-found:**
rmcp returns just the method name (e.g. `"nonexistent/method"`); tower-mcp
returns `"Method not found: nonexistent/method"`. Both use error code `-32601`.
This is an implementation detail, not a spec requirement.

**SSE response wrapping (default mode):**
rmcp always wraps synchronous JSON-RPC responses as SSE (`Content-Type:
text/event-stream`). tower-mcp returns bare JSON (`Content-Type:
application/json`) by default, and SSE wrapping only when
`.sse_responses(true)` is set on `HttpTransport`. The `sse-response-mode` check
validates that the opt-in SSE mode matches rmcp's behavior exactly.

## What to do when a check fails

1. **Read the failure message.** It will show the actual values from both
   servers and what was expected.
2. **Determine if it is a tower-mcp bug or an intentional rmcp choice.** Check
   the MCP spec, recent rmcp releases, and any relevant tower-mcp issues.
3. **If it is a tower-mcp bug:** file an issue and fix it before releasing.
4. **If it is a new rmcp behavior change:** evaluate whether tower-mcp should
   match it, then either fix or add a KNOWN-DIFF entry with a rationale.
5. **If it is a known diff that needs updating:** update the relevant check in
   `src/main.rs` and update this README.

## When to run

- **Before every release** -- catches regressions against the current rmcp version
- **When bumping the rmcp version** in `Cargo.toml` -- verifies that the new
  rmcp version has not introduced new wire-format changes

## How to update the rmcp version

Change the version constraint in `tools/rmcp-compat/Cargo.toml`:

```toml
rmcp = { version = "1.7.0", features = [...] }
```

Then run the harness and update any KNOWN-DIFF entries that have changed.
