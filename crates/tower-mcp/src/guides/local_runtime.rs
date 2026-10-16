#![doc = r####"
# Handlers backed by a `!Send` runtime

Every tower-mcp handler is `Send + Sync + 'static` and returns a `Send`
future, because a transport may run requests on any thread of a tokio
runtime. Some state cannot cross threads: an embedded interpreter (a Lua
state, a JavaScript context, a Rhai engine built without `sync`), or plain
`Rc<RefCell<_>>` data. This guide shows how to serve tools, resources and
prompts backed by that kind of state without making it `Send`.

The approach: one task on a [`tokio::task::LocalSet`] owns the state. The
tower-mcp handlers hold only a channel sender. Each call becomes a message
carrying its arguments, its [`RequestContext`](crate::RequestContext), and a
`oneshot` sender for the reply.

```text
tools/call
    |
    v
handler (Send): holds mpsc::Sender<Job>
    |   Job { tool, args, ctx, reply }
    v
local task on a LocalSet: owns the !Send state
    |   spawn_local per job, select! against ctx.cancelled()
    v
reply.send(result)  ->  the handler returns it
```

## The bridge

A complete example, with an `Rc` counter table standing in for an
interpreter:

```rust
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::protocol::RequestId;
use tower_mcp::{CallToolResult, RequestContext, Tool, ToolBuilder};

/// One call, sent from a tower-mcp handler to the task that owns the state.
struct Job {
    tool: String,
    args: Value,
    ctx: RequestContext,
    reply: oneshot::Sender<CallToolResult>,
}

/// State that cannot leave its thread: `Rc` and `RefCell` are `!Send`.
/// An embedded interpreter goes here.
#[derive(Default)]
struct Engine {
    counters: RefCell<HashMap<String, i64>>,
}

/// A tool whose handler holds only the sender, so it is `Send + Sync`.
fn bridged_tool(
    name: &str,
    description: &str,
    input: Value,
    tx: mpsc::Sender<Job>,
) -> Result<Tool, tower_mcp::Error> {
    let tool = name.to_string();
    Ok(ToolBuilder::try_new(name)?
        .description(description)
        .input_schema(input)
        .extractor_handler(
            tx,
            move |State(tx): State<mpsc::Sender<Job>>, ctx: Context, RawArgs(args): RawArgs| {
                let tool = tool.clone();
                async move {
                    let (reply, rx) = oneshot::channel();
                    let job = Job { tool, args, ctx: ctx.into_inner(), reply };
                    if tx.send(job).await.is_err() {
                        return Ok(CallToolResult::error("the local task has stopped"));
                    }
                    Ok(rx.await.unwrap_or_else(|_| {
                        CallToolResult::error("the local task dropped the call")
                    }))
                }
            },
        )
        .build())
}

/// Owns the state. Each call runs as its own local task, so calls
/// interleave at their `.await` points.
async fn serve(engine: Rc<Engine>, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        let engine = Rc::clone(&engine);
        tokio::task::spawn_local(async move {
            let result = tokio::select! {
                result = run(&engine, &job.tool, job.args, &job.ctx) => result,
                () = job.ctx.cancelled() => CallToolResult::error("cancelled"),
            };
            // The caller may have gone away; then there is no one to tell.
            let _ = job.reply.send(result);
        });
    }
}

async fn run(engine: &Engine, tool: &str, args: Value, ctx: &RequestContext) -> CallToolResult {
    match tool {
        "incr" => {
            let key = args["key"].as_str().unwrap_or("default").to_string();
            // End the borrow before any `.await`: another call on this
            // thread can run there, and a second borrow_mut would panic.
            let value = {
                let mut counters = engine.counters.borrow_mut();
                let value = counters.entry(key).or_default();
                *value += 1;
                *value
            };
            ctx.report_progress(1.0, Some(1.0), Some("counted")).await;
            CallToolResult::json(json!({ "value": value }))
        }
        "wait" => {
            tokio::time::sleep(Duration::from_secs(60)).await;
            CallToolResult::text("done")
        }
        other => CallToolResult::error(format!("no tool named {other}")),
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let (tx, rx) = mpsc::channel(64);
        tokio::task::spawn_local(serve(Rc::new(Engine::default()), rx));

        let object = json!({ "type": "object", "properties": { "key": { "type": "string" } } });
        let incr = bridged_tool("incr", "Count by key", object.clone(), tx.clone()).unwrap();
        let wait = bridged_tool("wait", "Wait a minute", object, tx.clone()).unwrap();

        let ctx = RequestContext::new(RequestId::Number(1));
        let result = incr.call_with_context(ctx, json!({ "key": "a" })).await;
        assert_eq!(result.structured_content, Some(json!({ "value": 1 })));

        // Cancelling the request ends the call on the local task.
        let ctx = RequestContext::new(RequestId::Number(2));
        let canceller = ctx.clone();
        tokio::task::spawn_local(async move {
            tokio::task::yield_now().await;
            canceller.cancel();
        });
        let result = wait.call_with_context(ctx, json!({})).await;
        assert_eq!(result.first_text(), Some("cancelled"));
    });
}
```

The pieces:

- `bridged_tool` uses [`ToolBuilder::try_new`](crate::ToolBuilder::try_new),
  `.input_schema(Value)` and an extractor handler with `RawArgs`, the usual
  route for a tool whose name and schema are only known at run time. The
  schema is enforced before the handler runs when `schema-validation` is
  compiled in, so a call that fails it never reaches the local task.
- The handler forwards the whole [`RequestContext`](crate::RequestContext).
  It is `Send` and `Clone`, so the local side can report progress, log,
  sample, elicit, and see cancellation, exactly as an ordinary handler would.
- `serve` runs each job with `spawn_local`. Two calls that await (a timer,
  I/O, a request to the client) overlap on one thread.
- The `select!` against `ctx.cancelled()` drops the call's future when the
  request is cancelled. tower-mcp cancels the context when the client sends
  `notifications/cancelled`, and on transport paths where the request can no
  longer be answered, such as a stateless HTTP client disconnecting or a
  stdio shutdown whose drain times out. Dropping the future is how the call
  is abandoned: for an interpreter that runs a call as a coroutine, dropping
  it stops the coroutine.
- The channel is bounded, so a burst of calls waits in `tx.send` rather than
  growing a queue without limit.
- If the local task stops, `send` fails, and if a job is dropped before it
  replies, the `oneshot` receiver errors. Both become error results instead
  of hanging the request.

## Running the transport

The local task needs a `LocalSet` to run on. There are two layouts.

On a current-thread runtime, run the transport inside the same `LocalSet`.
Everything shares one thread, which is the simplest layout for an
interpreter that owns the process:

```rust,no_run
# use std::cell::RefCell;
# use std::collections::HashMap;
# use std::rc::Rc;
# use serde_json::{Value, json};
# use tokio::sync::{mpsc, oneshot};
# use tower_mcp::{CallToolResult, RequestContext, Tool};
# struct Job { tool: String, args: Value, ctx: RequestContext, reply: oneshot::Sender<CallToolResult> }
# #[derive(Default)]
# struct Engine { counters: RefCell<HashMap<String, i64>> }
# fn bridged_tool(_: &str, _: &str, _: Value, _: mpsc::Sender<Job>) -> Result<Tool, tower_mcp::Error> { todo!() }
# async fn serve(_: Rc<Engine>, _: mpsc::Receiver<Job>) {}
use tower_mcp::{McpRouter, StdioTransport};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&rt, async {
        let (tx, rx) = mpsc::channel(64);
        tokio::task::spawn_local(serve(Rc::new(Engine::default()), rx));

        let object = json!({ "type": "object" });
        let router = McpRouter::new()
            .server_info("local-engine", "1.0.0")
            .tool(bridged_tool("incr", "Count by key", object, tx)?);
        StdioTransport::new(router).run().await?;
        Ok(())
    })
}
```

On a multi-thread runtime, give the state a thread of its own with its own
current-thread runtime. Create the state inside that thread, so it never
moves:

```rust,no_run
# use std::cell::RefCell;
# use std::collections::HashMap;
# use std::rc::Rc;
# use serde_json::{Value, json};
# use tokio::sync::{mpsc, oneshot};
# use tower_mcp::{CallToolResult, RequestContext, Tool};
# struct Job { tool: String, args: Value, ctx: RequestContext, reply: oneshot::Sender<CallToolResult> }
# #[derive(Default)]
# struct Engine { counters: RefCell<HashMap<String, i64>> }
# fn bridged_tool(_: &str, _: &str, _: Value, _: mpsc::Sender<Job>) -> Result<Tool, tower_mcp::Error> { todo!() }
# async fn serve(_: Rc<Engine>, _: mpsc::Receiver<Job>) {}
use tower_mcp::{McpRouter, StdioTransport};

fn spawn_engine() -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("engine runtime");
        let engine = Rc::new(Engine::default());
        tokio::task::LocalSet::new().block_on(&rt, serve(engine, rx));
    });
    tx
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tx = spawn_engine();
    let router = McpRouter::new()
        .server_info("local-engine", "1.0.0")
        .tool(bridged_tool("incr", "Count by key", json!({ "type": "object" }), tx)?);
    StdioTransport::new(router).run().await?;
    Ok(())
}
```

Either way the state runs on one thread. A call that computes without
awaiting holds that thread until it finishes: other calls wait, and in the
shared layout so does the transport. Interpreters usually offer a way to
yield periodically (an instruction-count hook, for example); that is a
setting of the embedded engine, not of tower-mcp. To use more cores, run
several engines, each on its own thread with its own state, and choose one
per call. Each engine then loads its definitions separately.

## Loading and reloading definitions

When tools come from files the engine loads, build every tool first and then
swap the whole set with
[`DynamicToolRegistry::replace_all`](crate::DynamicToolRegistry::replace_all).
A file that fails to load leaves the served set as it was, and clients get a
single `notifications/tools/list_changed` for the reload, never a list that
is half old and half new.
[`check_schema`](crate::check_schema) reports a schema that does not compile
against the file it came from, where building the tool would only log it:

```rust
# use serde_json::{Value, json};
# use tokio::sync::{mpsc, oneshot};
# use tower_mcp::extract::{Context, RawArgs, State};
# use tower_mcp::{CallToolResult, RequestContext, Tool, ToolBuilder};
# struct Job { tool: String, args: Value, ctx: RequestContext, reply: oneshot::Sender<CallToolResult> }
# fn bridged_tool(name: &str, description: &str, input: Value, tx: mpsc::Sender<Job>) -> Result<Tool, tower_mcp::Error> {
#     Ok(ToolBuilder::try_new(name)?.description(description).input_schema(input)
#         .extractor_handler(tx, |_: State<mpsc::Sender<Job>>, _: Context, _: RawArgs| async {
#             Ok(CallToolResult::text("ok"))
#         }).build())
# }
use tower_mcp::{DynamicToolRegistry, McpRouter, RegistryChange, check_schema};

/// A tool definition as the engine loaded it.
struct Definition {
    source: String,
    name: String,
    description: String,
    input: Value,
}

fn reload(
    registry: &DynamicToolRegistry,
    definitions: &[Definition],
    tx: &mpsc::Sender<Job>,
) -> Result<RegistryChange, String> {
    let mut tools = Vec::new();
    for def in definitions {
        check_schema(&def.input)
            .map_err(|error| format!("{}: inputSchema {error}", def.source))?;
        let tool = bridged_tool(&def.name, &def.description, def.input.clone(), tx.clone())
            .map_err(|error| format!("{}: {error}", def.source))?;
        tools.push(tool);
    }
    Ok(registry.replace_all(tools))
}

let (_router, registry) = McpRouter::new()
    .server_info("local-engine", "1.0.0")
    .with_dynamic_tools();
let (tx, _rx) = mpsc::channel(64);
let def = |source: &str, name: &str, input: Value| Definition {
    source: source.to_string(),
    name: name.to_string(),
    description: format!("Loaded from {source}"),
    input,
};

let first = [
    def("tools/incr.def", "incr", json!({ "type": "object" })),
    def("tools/wait.def", "wait", json!({ "type": "object" })),
];
let change = reload(&registry, &first, &tx).unwrap();
assert_eq!(change.added, ["incr", "wait"]);

// A bad schema fails the whole reload, and the served set is unchanged.
let broken = [def("tools/incr.def", "incr", json!({ "type": "objekt" }))];
let error = reload(&registry, &broken, &tx).unwrap_err();
assert!(error.starts_with("tools/incr.def: inputSchema at /type: "), "{error}");
assert_eq!(registry.list().len(), 2);

let second = [
    def("tools/incr.def", "incr", json!({ "type": "object" })),
    def("tools/reset.def", "reset", json!({ "type": "object" })),
];
let change = reload(&registry, &second, &tx).unwrap();
assert_eq!(change.added, ["reset"]);
assert_eq!(change.removed, ["wait"]);
assert_eq!(change.replaced, ["incr"]);
```

The engine keeps its own table from tool name to whatever runs the call (a
function handle, a compiled script). Update that table on the local task
before calling `replace_all`, so the first call to a new tool finds its
entry. A call already running keeps what it started with: the registry keeps
the old `Tool` alive for as long as a call holds it, and the local task holds
its own handle to the code. A call queued for a tool the reload removed
reaches the local task after the table changed, and gets the `no tool named`
error result from `run` above.

The prompt and resource registries have the same `replace_all`.

## Resources and prompts

The same bridge serves resource templates and prompts. Their builders take a
handler that receives the context:
[`ResourceTemplateBuilder::handler_with_context`](crate::ResourceTemplateBuilder::handler_with_context)
gets the context, the URI that was read, and the variables matched from the
template;
[`PromptBuilder::handler_with_context`](crate::PromptBuilder::handler_with_context)
gets the context and the prompt arguments;
[`ResourceBuilder::handler_with_context`](crate::ResourceBuilder::handler_with_context)
gets the context alone. Each forwards a message as the tool handler does:

```rust
use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};
use tower_mcp::protocol::ReadResourceResult;
use tower_mcp::{Error, RequestContext, ResourceTemplate, ResourceTemplateBuilder};

/// A template read, sent to the local task.
struct Read {
    uri: String,
    vars: HashMap<String, String>,
    ctx: RequestContext,
    reply: oneshot::Sender<tower_mcp::Result<ReadResourceResult>>,
}

fn bridged_template(pattern: &str, tx: mpsc::Sender<Read>) -> Result<ResourceTemplate, Error> {
    ResourceTemplateBuilder::new(pattern)
        .name(pattern)
        .try_handler_with_context(move |ctx: RequestContext, uri: String, vars| {
            let tx = tx.clone();
            async move {
                let (reply, rx) = oneshot::channel();
                tx.send(Read { uri, vars, ctx, reply })
                    .await
                    .map_err(|_| Error::internal("the local task has stopped"))?;
                rx.await
                    .map_err(|_| Error::internal("the local task dropped the read"))?
            }
        })
}

# let (tx, _rx) = mpsc::channel(8);
let template = bridged_template("engine://runs/{id}", tx).unwrap();
assert_eq!(template.match_uri("engine://runs/7").unwrap()["id"], "7");
```

A resource or prompt handler reports failure as an `Err`, which the router
sends as a JSON-RPC error. A tool reports failure inside its result, as
`isError: true`, and can carry structured data about the failure: set
`structured_content` on the result from `CallToolResult::error`.
"####]
