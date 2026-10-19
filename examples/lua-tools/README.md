# lua-tools

MCP tools written in Lua, served over stdio by tower-mcp.

This is an example, not a supported API. tower-mcp stays free of any script
engine ([#1583](https://github.com/joshrotenberg/tower-mcp/issues/1583)); this
crate shows how a program that embeds Lua builds the bridge on the pieces
tower-mcp provides:

- `DynamicToolRegistry::replace_all` swaps the whole tool set at once and
  sends one `list_changed`.
- `check_schema` (feature `schema-validation`) rejects a bad script schema
  when it loads.
- `Tool::call_with_context` and `RequestContext` carry cancellation and
  progress to the script.

The structure follows the
[handlers backed by a `!Send` runtime](https://docs.rs/tower-mcp/latest/tower_mcp/guides/local_runtime/)
guide: one task on a `LocalSet` owns the Lua state, and the tower-mcp handlers
hold only a channel sender. Start with `src/lib.rs`, which is commented as a
walkthrough. Prompts and resource templates follow the same pattern with
`PromptBuilder::handler_with_context` and
`ResourceTemplateBuilder::handler_with_context`.

## Tool files

Every `*.lua` file in the tool directory returns one table:

```lua
return {
  name = "greet",
  description = "Greet someone by name",
  -- A JSON Schema, written as a Lua table. Arguments are checked against it
  -- before `run` is called.
  input = {
    type = "object",
    properties = { name = { type = "string", minLength = 1 } },
    required = { "name" },
  },
  run = function(args, ctx)
    return { greeting = "hello, " .. args.name }
  end,
}
```

- A returned table becomes `structuredContent` (and its JSON as text); a
  returned string becomes text. Return an object, not a list, for
  `structuredContent` to be valid on protocol 2025-11-25.
- `error("message")` or `error({ message = "...", kind = "..." })` becomes an
  error result. A table is kept in `structuredContent`, and its `message`
  field is the text.
- `ctx:progress(done, total, message)` sends a progress notification.
  `ctx:cancelled()` is true once the client has cancelled the request, and a
  cancelled call is dropped at its next `sleep` or other yield.
- `sleep(seconds)` is provided by the host and yields to the runtime.

A load validates every file first (Lua errors, missing fields, bad names, bad
schemas) and reports every problem with its file name. If any file fails,
nothing changes and the previous tools stay served. The Lua code is trusted
and not sandboxed.

## Run

```bash
cargo run -p lua-tools-example            # serves ./lua
cargo run -p lua-tools-example -- my/tools # serves another directory
```

Call the static `reload` tool after editing a file to load the directory
again; it reports what was added, removed and replaced.

As an MCP client entry (`.mcp.json`):

```json
{
  "mcpServers": {
    "lua-tools": {
      "command": "cargo",
      "args": ["run", "-q", "-p", "lua-tools-example"]
    }
  }
}
```

The sample tools in `lua/` are `greet` and `countdown` (progress and
cancellation). `cargo test -p lua-tools-example` runs the end-to-end checks in
`tests/smoke.rs`.
