//! MCP tools defined in Lua files, served by tower-mcp.
//!
//! This is an example, not a supported API. tower-mcp stays free of any
//! script engine (see issue #1583); this crate shows how an embedder builds
//! the Lua side on the pieces tower-mcp does provide. Read it next to the
//! `guides::local_runtime` guide in the tower-mcp docs, which explains the
//! shape used here.
//!
//! The pieces:
//!
//! - A Lua state is not `Send`, so one task on a [`tokio::task::LocalSet`]
//!   owns it. The tower-mcp handlers hold only an [`mpsc::Sender`] and a
//!   `oneshot` for the reply, so their futures are `Send`.
//! - Each tool file is `return { name, description, input, run }`. `input` is
//!   a JSON Schema written as a Lua table. [`check_schema`] rejects a bad one
//!   at load time, and the `schema-validation` feature checks every call's
//!   arguments against it before Lua runs.
//! - A load builds and checks every tool first, then swaps the whole set with
//!   one [`DynamicToolRegistry::replace_all`]. A file that fails leaves what
//!   was served as it was, and clients see a single `list_changed`.
//! - Each call runs as its own local task and Lua coroutine, under `pcall`,
//!   raced against `ctx.cancelled()`. Dropping the call drops the coroutine.
//!
//! Prompts and resource templates follow the same pattern, through
//! `PromptBuilder::handler_with_context` and
//! `ResourceTemplateBuilder::handler_with_context`; they are left out to keep
//! this short.
//!
//! The Lua code is trusted. The state has the full standard library, so a tool
//! file can read files and run programs. Do not load tool files you would not
//! run as programs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use mlua::{FromLua, Function, Lua, LuaSerdeExt, UserData, UserDataMethods};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::{
    CallToolResult, DynamicToolRegistry, McpRouter, RegistryChange, RequestContext, Tool,
    ToolBuilder, check_schema,
};

/// A message to the task that owns the Lua state.
enum Call {
    /// Run a tool's Lua function.
    Tool {
        name: String,
        args: Value,
        ctx: RequestContext,
        reply: oneshot::Sender<CallToolResult>,
    },
    /// Load every `*.lua` file in a directory and swap the tool set.
    Load {
        dir: PathBuf,
        /// Cloned into each new tool's handler.
        tx: mpsc::Sender<Call>,
        reply: oneshot::Sender<Result<RegistryChange, LoadError>>,
    },
}

/// Lua-defined tools attached to a router. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct LuaTools {
    tx: mpsc::Sender<Call>,
    registry: DynamicToolRegistry,
}

impl LuaTools {
    /// Enable dynamic tools on `router` and start the task that owns the Lua
    /// state. Call this inside a [`tokio::task::LocalSet`], because the task
    /// is started with `spawn_local`.
    pub fn new(router: McpRouter) -> mlua::Result<(McpRouter, Self)> {
        let (router, registry) = router.with_dynamic_tools();

        let lua = Lua::new();
        // `sleep(seconds)` yields to the runtime, so other calls (and
        // cancellation) run while a tool waits. A plain Lua busy loop would
        // hold the thread instead.
        let sleep = lua.create_async_function(|_, seconds: f64| async move {
            let delay = Duration::try_from_secs_f64(seconds).map_err(mlua::Error::external)?;
            tokio::time::sleep(delay).await;
            Ok(())
        })?;
        lua.globals().set("sleep", sleep)?;
        // Every tool runs through this, so an error value that is a table
        // reaches Rust as a table. A call that raises would otherwise be
        // converted to a string error by mlua.
        let invoke: Function = lua
            .load("return function(f, ...) return pcall(f, ...) end")
            .eval()?;

        let (tx, rx) = mpsc::channel(64);
        tokio::task::spawn_local(serve(lua, invoke, registry.clone(), rx));
        Ok((router, Self { tx, registry }))
    }

    /// The registry the tools are served from, for inspection.
    pub fn registry(&self) -> &DynamicToolRegistry {
        &self.registry
    }

    /// Load every `*.lua` file in `dir` and make exactly those tools the
    /// served set. On error nothing changes.
    pub async fn load_dir(&self, dir: &Path) -> Result<RegistryChange, LoadError> {
        let (reply, rx) = oneshot::channel();
        let call = Call::Load {
            dir: dir.to_path_buf(),
            tx: self.tx.clone(),
            reply,
        };
        if self.tx.send(call).await.is_err() {
            return Err(LoadError::one("the Lua task has stopped"));
        }
        rx.await
            .unwrap_or_else(|_| Err(LoadError::one("the Lua task dropped the load")))
    }
}

/// Why a load was refused: one line per problem, each naming its file.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub problems: Vec<String>,
}

impl LoadError {
    fn one(problem: impl fmt::Display) -> Self {
        Self {
            problems: vec![problem.to_string()],
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.problems.join("\n"))
    }
}

impl std::error::Error for LoadError {}

/// Owns the Lua state. Runs on the `LocalSet`, so it may hold `Rc` and `Lua`.
async fn serve(
    lua: Lua,
    invoke: Function,
    registry: DynamicToolRegistry,
    mut rx: mpsc::Receiver<Call>,
) {
    // Tool name to Lua function. Replaced whole by a load, never edited.
    let functions: Rc<RefCell<HashMap<String, Function>>> = Rc::default();
    while let Some(call) = rx.recv().await {
        let (lua, invoke, functions) = (lua.clone(), invoke.clone(), Rc::clone(&functions));
        match call {
            Call::Tool {
                name,
                args,
                ctx,
                reply,
            } => {
                // Look the function up once. A reload during the call does not
                // change what this call runs.
                let run = functions.borrow().get(&name).cloned();
                let Some(run) = run else {
                    let _ = reply.send(CallToolResult::error(format!("no Lua tool named {name}")));
                    continue;
                };
                // One local task per call: calls overlap at their awaits.
                tokio::task::spawn_local(async move {
                    let result = tokio::select! {
                        result = call_lua(&lua, &invoke, run, args, &ctx) => result,
                        // Dropping the call above drops the coroutine.
                        () = ctx.cancelled() => CallToolResult::error("cancelled"),
                    };
                    let _ = reply.send(result);
                });
            }
            Call::Load { dir, tx, reply } => {
                let registry = registry.clone();
                tokio::task::spawn_local(async move {
                    let _ = reply.send(load(&lua, &dir, &tx, &functions, &registry).await);
                });
            }
        }
    }
}

/// Build and check every tool in `dir`, then swap them in.
async fn load(
    lua: &Lua,
    dir: &Path,
    tx: &mpsc::Sender<Call>,
    functions: &RefCell<HashMap<String, Function>>,
    registry: &DynamicToolRegistry,
) -> Result<RegistryChange, LoadError> {
    let at = |error: std::io::Error| LoadError::one(format!("{}: {error}", dir.display()));
    let mut paths = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await.map_err(at)?;
    while let Some(entry) = entries.next_entry().await.map_err(at)? {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "lua") {
            paths.push(path);
        }
    }
    paths.sort();

    // Validate everything before changing anything, and report every problem
    // at once rather than the first.
    let (mut tools, mut staged, mut problems) = (Vec::new(), HashMap::new(), Vec::new());
    for path in paths {
        match define(lua, &path, tx).await {
            Ok((tool, _)) if staged.contains_key(&tool.name) => {
                problems.push(format!(
                    "{}: tool name {} is already defined",
                    path.display(),
                    tool.name
                ));
            }
            Ok((tool, run)) => {
                staged.insert(tool.name.clone(), run);
                tools.push(tool);
            }
            Err(message) => problems.push(format!("{}: {message}", path.display())),
        }
    }
    if !problems.is_empty() {
        return Err(LoadError { problems });
    }

    // Swap the dispatch table first, so the first call to a new tool finds its
    // function, then publish the tools: one write, one `list_changed`.
    *functions.borrow_mut() = staged;
    Ok(registry.replace_all(tools))
}

/// Evaluate one tool file and build its tool. Errors are plain text; the
/// caller adds the file name.
async fn define(
    lua: &Lua,
    path: &Path,
    tx: &mpsc::Sender<Call>,
) -> Result<(Tool, Function), String> {
    let code = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())?;
    let value: mlua::Value = lua
        .load(code)
        .set_name(format!("@{}", path.display()))
        .eval_async()
        .await
        .map_err(|e| e.to_string())?;
    let mlua::Value::Table(def) = value else {
        return Err(format!("must return a table, not {}", value.type_name()));
    };

    let name: String = field(&def, "name")?;
    let description: String = field(&def, "description")?;
    let run: Function = field(&def, "run")?;
    let input = to_json(lua, field(&def, "input")?)?;
    if !input.is_object() {
        return Err("`input` must be a JSON Schema table".to_string());
    }
    // Without this a bad schema would only be logged when the tool is built,
    // and the tool would be served unchecked.
    check_schema(&input).map_err(|error| format!("`input` is not a valid schema {error}"))?;

    let tool_name = name.clone();
    let tool = ToolBuilder::try_new(&name)
        .map_err(|error| error.to_string())?
        .description(description)
        .input_schema(input)
        .extractor_handler(
            tx.clone(),
            // The handler holds only the sender, so it is `Send + Sync`.
            move |State(tx): State<mpsc::Sender<Call>>, ctx: Context, RawArgs(args): RawArgs| {
                let name = tool_name.clone();
                async move {
                    let (reply, rx) = oneshot::channel();
                    let call = Call::Tool {
                        name,
                        args,
                        ctx: ctx.into_inner(),
                        reply,
                    };
                    if tx.send(call).await.is_err() {
                        return Ok(CallToolResult::error("the Lua task has stopped"));
                    }
                    Ok(rx
                        .await
                        .unwrap_or_else(|_| CallToolResult::error("the Lua task dropped the call")))
                }
            },
        )
        .build();
    Ok((tool, run))
}

fn field<T: FromLua>(def: &mlua::Table, key: &str) -> Result<T, String> {
    def.get(key).map_err(|error| format!("`{key}`: {error}"))
}

/// Lua to JSON, with object keys sorted so the schema is stable.
fn to_json(lua: &Lua, value: mlua::Value) -> Result<Value, String> {
    let options = mlua::serde::DeserializeOptions::new().sort_keys(true);
    lua.from_value_with(value, options)
        .map_err(|error| error.to_string())
}

/// Run `run(args, ctx)` as a coroutine, under `pcall`, and turn what it
/// returned or raised into a tool result.
async fn call_lua(
    lua: &Lua,
    invoke: &Function,
    run: Function,
    args: Value,
    ctx: &RequestContext,
) -> CallToolResult {
    let call = async {
        // JSON null becomes nil, so `args.x == nil` holds for an explicit null.
        let options = mlua::serde::SerializeOptions::new()
            .serialize_none_to_null(false)
            .serialize_unit_to_null(false);
        let args = lua.to_value_with(&args, options)?;
        let context = lua.create_userdata(LuaContext(ctx.clone()))?;
        invoke
            .call_async::<(bool, mlua::Value)>((run, args, context))
            .await
    };
    match call.await {
        Ok((ok, value)) => tool_result(lua, ok, value),
        Err(error) => CallToolResult::error(error.to_string()),
    }
}

/// A string is text. A table is JSON, in `structuredContent` and as text. A
/// raised table keeps its data in `structuredContent`, and its `message`
/// field is the error text.
fn tool_result(lua: &Lua, ok: bool, value: mlua::Value) -> CallToolResult {
    match (ok, value) {
        (true, mlua::Value::Nil) => CallToolResult::text(""),
        (true, mlua::Value::String(text)) => CallToolResult::text(text.to_string_lossy()),
        (true, mlua::Value::Table(table)) => match to_json(lua, mlua::Value::Table(table)) {
            Ok(json) => CallToolResult::json(json),
            Err(error) => CallToolResult::error(format!(
                "the tool returned a table that is not JSON: {error}"
            )),
        },
        (true, other) => CallToolResult::text(display(&other)),
        (false, mlua::Value::String(text)) => CallToolResult::error(text.to_string_lossy()),
        (false, mlua::Value::Table(table)) => match to_json(lua, mlua::Value::Table(table)) {
            Ok(json) => {
                let text = json["message"]
                    .as_str()
                    .map_or_else(|| json.to_string(), str::to_string);
                let mut result = CallToolResult::error(text);
                result.structured_content = Some(json);
                result
            }
            Err(error) => {
                CallToolResult::error(format!("the tool raised a table that is not JSON: {error}"))
            }
        },
        (false, other) => CallToolResult::error(display(&other)),
    }
}

fn display(value: &mlua::Value) -> String {
    value.to_string().unwrap_or_else(|error| error.to_string())
}

/// The `ctx` argument of `run(args, ctx)`: the call's [`RequestContext`].
struct LuaContext(RequestContext);

impl UserData for LuaContext {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `ctx:cancelled()` is true once the client cancelled the request. The
        // call is dropped at its next yield anyway; this lets a loop stop early.
        methods.add_method("cancelled", |_, this, ()| Ok(this.0.is_cancelled()));

        // `ctx:progress(done, total, message)` sends `notifications/progress`
        // if the client asked for progress.
        methods.add_async_method(
            "progress",
            |_, this, (done, total, message): (f64, Option<f64>, Option<String>)| async move {
                this.0
                    .report_progress(done, total, message.as_deref())
                    .await;
                Ok(())
            },
        );
    }
}
