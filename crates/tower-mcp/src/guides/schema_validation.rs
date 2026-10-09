#![doc = r####"
# Schema validation

A tool advertises an `inputSchema` and, optionally, an `outputSchema`. serde
enforces the shape of the arguments a typed handler deserializes, but not the
constraints a schema can express (`minimum`, `maximum`, `pattern`,
`minLength`, `format`), and handlers that take `RawArgs` or a hand-written
`.input_schema(Value)` get no schema check without the feature. Without it,
nothing compares a result's `structuredContent` to the `outputSchema`.

The opt-in `schema-validation` feature adds both checks. It pulls in
[`jsonschema`](https://crates.io/crates/jsonschema) with its default features
off, so a schema is never resolved from the network or the filesystem.

```toml
tower-mcp = { version = "0.23", features = ["schema-validation"] }
```

## structuredContent shape before 2026-07-28

Independently of the `schema-validation` feature and any output schema,
tower-mcp checks successful results on MCP 2025-11-25 and earlier requests.
Their `structuredContent` must be a JSON object. An array, scalar, or explicit
null is replaced by an error result with no structured content, for example:

```text
Tool 'snapshots' returned structuredContent of type array; MCP 2025-11-25 and earlier require a JSON object. Wrap a list with CallToolResult::from_list or return an object.
```

The failure emits a `warn` event on `mcp::tools` with the tool name and message.
Both shape and schema diagnostics use the build-time tool name, so later
`with_name_prefix` calls (including `McpRouter::nest`) are not reflected.
Error results and results without structured content pass through. Requests
using MCP 2026-07-28 permit any JSON value and are not shape checked. The shape
check runs before output schema validation, and
`ToolBuilder::skip_output_validation` disables it even without the feature.
`McpTool` implementations returning `Vec<_>` and `mcp_app_tool_result` callers
passing lists receive this error on older requests; use `from_list` or return
an object.

## What is checked

With the feature compiled in, every tool is validated by default:

- `arguments` against the tool's `inputSchema`, before the handler runs. This
  covers typed handlers, `RawArgs` and extractor handlers, tools whose schema
  was set with `.input_schema(Value)`, and `McpTool` implementations. An
  omitted `arguments` is checked as an empty object.
- `structuredContent` against the tool's `outputSchema`, when one is declared,
  before the result is returned. A result with `isError: true`, or with no
  `structuredContent`, is not checked.

`format` is enforced (`email`, `uuid`, `date-time`, `uri`, and the other
standard formats), rather than treated as an annotation.

Each schema is compiled once, when the tool is built. Checking a call does not
recompile anything.

## Where the check runs

The check wraps the tool itself, in the same places `Tool::with_guard` wraps
it, so it applies wherever the tool is called from:

- ordinary `tools/call` requests;
- task-backed calls, both replayed handlers and live task handlers;
- MRTR handlers (with `protocol-2026-07-28`), on every attempt;
- tools registered through the builders, `McpTool::into_tool`, the extractor
  handlers, and dynamic registries.

Middleware added with `.layer()` runs inside the check. A guard added with
`.guard()` on the builder does too. A guard added to the built tool with
`Tool::with_guard` runs first.

One path is not covered: the task preparation callback
(`.task_preparation(...)`) runs when a task is created, before the handler is
invoked, and sees the arguments unchecked. The handler then still refuses them.

## What a failure looks like

A failure is a tool execution error, `isError: true`, and not a protocol
error, so a model can read it and correct the call. The text names the JSON
location and the rule that was violated:

```text
Invalid input: the arguments do not match the tool's input schema.
- at /age: 200 is greater than the maximum of 120 (maximum)
- at /handle: "Bad" does not match "^[a-z]+$" (pattern)
```

`(root)` stands for the arguments object itself, for example when a required
property is missing. The first line uses the same `Invalid input` prefix as the
serde failure a typed handler reports, so a caller sees one shape either way.

At most five violations are listed, followed by `and more violations`, and a
single long message is truncated, so the result stays bounded whatever the
caller sent. The handler is not invoked.

A result that does not match the output schema is replaced by an error result
in the same format:

```text
Tool returned structured content that does not match its output schema.
- at /count: -1 is less than the minimum of 0 (minimum)
```

Both cases emit a `tracing` event on the `mcp::tools` target with the tool
name and the same report. Input failures are logged at `debug`, since they are
the caller's mistake. Output failures are logged at `warn`, since they mean the
server is returning something it advertises it will not.

## Deriving an output schema

`ToolBuilder::output_schema_for::<T>()` derives the output schema from a type
with schemars, the way input schemas are derived. It is available with or
without the feature.

```rust
use schemars::JsonSchema;
use serde::Serialize;
use tower_mcp::{CallToolResult, NoParams, ToolBuilder};

#[derive(Serialize, JsonSchema)]
struct Forecast {
    #[schemars(range(min = -90, max = 60))]
    celsius: i32,
    summary: String,
}

let tool = ToolBuilder::new("forecast")
    .description("Current forecast")
    .output_schema_for::<Forecast>()
    .handler(|_input: NoParams| async {
        CallToolResult::from_serialize(&Forecast {
            celsius: 18,
            summary: "clear".to_string(),
        })
    })
    .build();

assert_eq!(tool.definition().output_schema.unwrap()["type"], "object");
```

The handler still builds its own `CallToolResult`. `McpTool::Output` is not
required to implement `JsonSchema`, so existing implementations keep
compiling; to derive a schema for one, call `output_schema_for` on a builder,
or set `.output_schema(...)` yourself. MCP 2025-11-25 and earlier require
`"type": "object"` at the root; MCP 2026-07-28 accepts any JSON Schema 2020-12.
Neither method rewrites or rejects the schema it is given.

## Opting out

`ToolBuilder::skip_input_validation` and `ToolBuilder::skip_output_validation`
turn each check off for one tool, independently:

```rust
use serde_json::json;
use tower_mcp::extract::RawArgs;
use tower_mcp::{CallToolResult, ToolBuilder};

let tool = ToolBuilder::new("passthrough")
    .input_schema(json!({ "type": "object", "required": ["query"] }))
    .skip_input_validation()
    .extractor_handler((), |RawArgs(args): RawArgs| async move {
        Ok(CallToolResult::json(args))
    })
    .build();
```

Both methods exist whether or not the feature is compiled in. Without it,
`skip_input_validation` does nothing; `skip_output_validation` still disables
the structured-content shape check.

The feature is additive: enabling it anywhere in a dependency graph turns
validation on for every tool built by tower-mcp in that graph, unless the tool
opts out.

## A schema that does not compile

Building a tool never panics over its schemas. If a schema fails to compile
(for example a `required` that is not an array), an `error` event is logged on
the `mcp::tools` target and that schema is left unchecked. The other schema of
the same tool is still checked. This matters for tools built from someone
else's schema, such as a proxy or a dynamic registration, where a panic would
take the process down.

A `$ref` to a remote or file location is not resolved, so such a schema does
not compile and is left unchecked as described above. Bundle the referenced
definitions into the schema (`$defs`) instead.
"####]
