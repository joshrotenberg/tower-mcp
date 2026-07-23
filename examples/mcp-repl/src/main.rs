//! mcp-repl: an interactive MCP client REPL.
//!
//! Connects to any MCP server and turns the server's surface into the
//! command set: every tool becomes a top-level command with schema-coerced
//! `key=value` arguments, prompts and resources get built-ins, tab
//! completion is powered by the server itself where the protocol allows
//! (`completion/complete`), and `list_changed` notifications refresh the
//! command table live mid-session.
//!
//! Usage:
//!
//! ```text
//! # Spawn a stdio server as a child process:
//! cargo run -p mcp-repl -- cargo run --example getting_started
//!
//! # Connect to a streamable HTTP server:
//! cargo run -p mcp-repl -- --http http://127.0.0.1:3001/mcp
//! ```
//!
//! Inside the REPL, `help` lists the built-ins and the server's tools.
//! A trailing `&` runs a tool task-augmented (SEP-2663): the call returns a
//! task id immediately; `jobs`, `task <id>`, `wait <id>`, and `cancel <id>`
//! manage it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper, history::DefaultHistory};

use tower_mcp::client::{
    ChannelTransport, HttpClientTransport, McpClient, NotificationHandler, StdioClientTransport,
};
use tower_mcp::protocol::{
    Content, PromptDefinition, ResourceDefinition, ResourceTemplateDefinition, TaskObject,
    ToolDefinition,
};

#[derive(Parser)]
#[command(
    name = "mcp-repl",
    about = "Interactive MCP client REPL",
    trailing_var_arg = true
)]
struct Args {
    /// Connect to a streamable HTTP server at this URL instead of spawning
    /// a stdio child process.
    #[arg(long)]
    http: Option<String>,

    /// Serve the bundled demo router in-process (no external server needed).
    #[arg(long, conflicts_with_all = ["http", "command"])]
    demo: bool,

    /// Command (and arguments) of a stdio MCP server to spawn.
    command: Vec<String>,
}

/// The server surface the REPL turns into commands. Refreshed on connect
/// and whenever a list_changed notification arrives.
#[derive(Default)]
struct Surface {
    tools: Vec<ToolDefinition>,
    prompts: Vec<PromptDefinition>,
    resources: Vec<ResourceDefinition>,
    templates: Vec<ResourceTemplateDefinition>,
}

const BUILTINS: &[&str] = &[
    "help",
    "tools",
    "prompts",
    "resources",
    "templates",
    "read",
    "prompt",
    "call",
    "jobs",
    "task",
    "wait",
    "cancel",
    "refresh",
    "info",
    "quit",
    "exit",
];

struct ReplHelper {
    surface: Arc<RwLock<Surface>>,
    client: Arc<McpClient>,
    runtime: tokio::runtime::Handle,
}

impl ReplHelper {
    /// Server-powered completion for prompt argument values. Safe to block
    /// here: the completer runs on the dedicated readline thread, outside
    /// the async runtime.
    fn complete_via_server(&self, prompt: &str, arg: &str, partial: &str) -> Vec<String> {
        let client = self.client.clone();
        let (prompt, arg, partial) = (prompt.to_string(), arg.to_string(), partial.to_string());
        self.runtime
            .block_on(async move {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    client.complete_prompt_arg(&prompt, &arg, &partial),
                )
                .await
            })
            .ok()
            .and_then(|r| r.ok())
            .map(|r| r.completion.values)
            .unwrap_or_default()
    }
}

fn pair(s: &str) -> Pair {
    Pair {
        display: s.to_string(),
        replacement: s.to_string(),
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let (word_start, word) = match head.rfind(char::is_whitespace) {
            Some(i) => (i + 1, &head[i + 1..]),
            None => (0, head),
        };
        let surface = self.surface.read().unwrap();
        let mut out: Vec<Pair> = Vec::new();

        let mut words = head.split_whitespace();
        let first = words.next().unwrap_or("");
        let completing_first = word_start == 0;

        if completing_first {
            // First word: built-ins plus every tool name.
            out.extend(
                BUILTINS
                    .iter()
                    .filter(|c| c.starts_with(word))
                    .map(|c| pair(c)),
            );
            out.extend(
                surface
                    .tools
                    .iter()
                    .filter(|t| t.name.starts_with(word))
                    .map(|t| pair(&t.name)),
            );
        } else {
            match first {
                "read" => {
                    out.extend(
                        surface
                            .resources
                            .iter()
                            .filter(|r| r.uri.starts_with(word))
                            .map(|r| pair(&r.uri)),
                    );
                    out.extend(
                        surface
                            .templates
                            .iter()
                            .filter(|t| t.uri_template.starts_with(word))
                            .map(|t| pair(&t.uri_template)),
                    );
                }
                "prompt" => {
                    let second_word = head.split_whitespace().count() == 2 && !head.ends_with(' ');
                    let naming_prompt = second_word || (head.split_whitespace().count() == 1);
                    if naming_prompt {
                        out.extend(
                            surface
                                .prompts
                                .iter()
                                .filter(|p| p.name.starts_with(word))
                                .map(|p| pair(&p.name)),
                        );
                    } else if let Some(prompt_name) = head.split_whitespace().nth(1) {
                        if let Some((arg_name, partial)) = word.split_once('=') {
                            // Argument value: ask the server (completion/complete).
                            for v in self.complete_via_server(prompt_name, arg_name, partial) {
                                out.push(Pair {
                                    display: v.clone(),
                                    replacement: format!("{arg_name}={v}"),
                                });
                            }
                        } else if let Some(p) =
                            surface.prompts.iter().find(|p| p.name == prompt_name)
                        {
                            // Argument name: from the prompt definition.
                            out.extend(
                                p.arguments
                                    .iter()
                                    .filter(|a| a.name.starts_with(word))
                                    .map(|a| Pair {
                                        display: format!(
                                            "{}{}",
                                            a.name,
                                            if a.required { " (required)" } else { "" }
                                        ),
                                        replacement: format!("{}=", a.name),
                                    }),
                            );
                        }
                    }
                }
                "call" | "task" | "wait" | "cancel" => {
                    if first == "call" {
                        out.extend(
                            surface
                                .tools
                                .iter()
                                .filter(|t| t.name.starts_with(word))
                                .map(|t| pair(&t.name)),
                        );
                    }
                }
                tool_name => {
                    // Dynamic tool command: complete argument names from the
                    // tool's inputSchema properties.
                    if let Some(tool) = surface.tools.iter().find(|t| t.name == tool_name)
                        && !word.contains('=')
                        && let Some(props) = tool
                            .input_schema
                            .get("properties")
                            .and_then(|p| p.as_object())
                    {
                        out.extend(props.keys().filter(|k| k.starts_with(word)).map(|k| Pair {
                            display: k.to_string(),
                            replacement: format!("{k}="),
                        }));
                    }
                }
            }
        }

        Ok((word_start, out))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

/// Coerce a `key=value` string according to the tool's inputSchema.
fn coerce_arg(schema: &serde_json::Value, key: &str, raw: &str) -> serde_json::Value {
    let ty = schema
        .get("properties")
        .and_then(|p| p.get(key))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str());
    match ty {
        Some("integer") => raw
            .parse::<i64>()
            .map(Into::into)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(|n| serde_json::Number::from_f64(n).map(serde_json::Value::Number))
            .unwrap_or_else(|| serde_json::Value::String(raw.to_string())),
        Some("boolean") => raw
            .parse::<bool>()
            .map(serde_json::Value::Bool)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("array") | Some("object") => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
        _ => {
            // No schema type: accept JSON literals, fall back to string.
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
    }
}

fn parse_kv_args(schema: &serde_json::Value, tokens: &[&str]) -> serde_json::Value {
    // A single JSON object literal wins.
    if tokens.len() == 1
        && tokens[0].starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(tokens[0])
    {
        return v;
    }
    let mut map = serde_json::Map::new();
    for t in tokens {
        if let Some((k, v)) = t.split_once('=') {
            map.insert(k.to_string(), coerce_arg(schema, k, v));
        }
    }
    serde_json::Value::Object(map)
}

fn render_content(content: &[Content]) {
    for c in content {
        match c {
            Content::Text { text, .. } => println!("{text}"),
            other => {
                let v = serde_json::to_value(other).unwrap_or_default();
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("content");
                match ty {
                    "image" | "audio" => {
                        let mime = v.get("mimeType").and_then(|m| m.as_str()).unwrap_or("?");
                        let len = v.get("data").and_then(|d| d.as_str()).map_or(0, str::len);
                        println!("[{ty} {mime}, {len} base64 chars]");
                    }
                    _ => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
                }
            }
        }
    }
}

fn render_task(task: &TaskObject) {
    println!(
        "task {}  status={}  {}",
        task.task_id,
        task.status,
        task.status_message.as_deref().unwrap_or("")
    );
    if let Some(result) = &task.result {
        render_content(&result.content);
    }
    if let Some(err) = &task.error {
        println!("error {}: {}", err.code, err.message);
    }
}

async fn fetch_surface(client: &McpClient) -> Surface {
    fn ok_or_warn<T: Default>(what: &str, r: Result<T, tower_mcp::Error>) -> T {
        r.unwrap_or_else(|e| {
            eprintln!("warning: fetching {what} failed: {e}");
            T::default()
        })
    }
    Surface {
        tools: ok_or_warn("tools", client.list_all_tools().await),
        prompts: ok_or_warn("prompts", client.list_all_prompts().await),
        resources: ok_or_warn("resources", client.list_all_resources().await),
        templates: ok_or_warn(
            "resource templates",
            client.list_all_resource_templates().await,
        ),
    }
}

fn demo_router() -> tower_mcp::McpRouter {
    use tower_mcp::extract::RawArgs;
    use tower_mcp::{CallToolResult, TaskSupportMode, ToolBuilder};
    tower_mcp::McpRouter::new()
        .server_info("mcp-repl-demo", env!("CARGO_PKG_VERSION"))
        .tool(
            ToolBuilder::new("echo")
                .description("Echo a message back")
                .extractor_handler((), |RawArgs(args): RawArgs| async move {
                    let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    Ok(CallToolResult::text(msg.to_string()))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("slow_add")
                .description("Add two numbers, slowly (try running with a trailing &)")
                .task_support(TaskSupportMode::Optional)
                .extractor_handler((), |RawArgs(args): RawArgs| async move {
                    let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    Ok(CallToolResult::text(format!("{}", a + b)))
                })
                .build(),
        )
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();

    // Notifications print inline and trigger surface refreshes.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let handler = {
        let t = refresh_tx.clone();
        let r = refresh_tx.clone();
        let p = refresh_tx;
        NotificationHandler::new()
            .on_tools_changed(move || {
                let _ = t.send(());
            })
            .on_resources_changed(move || {
                let _ = r.send(());
            })
            .on_prompts_changed(move || {
                let _ = p.send(());
            })
            .on_progress(|p| {
                let pct = match (p.progress, p.total) {
                    (done, Some(total)) if total > 0.0 => {
                        format!(" ({:.0}%)", 100.0 * done / total)
                    }
                    _ => String::new(),
                };
                println!("[progress{pct}] {}", p.message.as_deref().unwrap_or(""));
            })
            .on_log_message(|m| {
                println!("[log {:?}] {}", m.level, m.data);
            })
    };

    let client = if args.demo {
        McpClient::connect_with_handler(ChannelTransport::new(demo_router()), handler).await?
    } else if let Some(url) = &args.http {
        McpClient::connect_with_handler(HttpClientTransport::new(url.clone()), handler).await?
    } else if !args.command.is_empty() {
        let cmd_args: Vec<&str> = args.command[1..].iter().map(|s| s.as_str()).collect();
        let transport = StdioClientTransport::spawn(&args.command[0], &cmd_args).await?;
        McpClient::connect_with_handler(transport, handler).await?
    } else {
        eprintln!("usage: mcp-repl <server command...> | --http <url> | --demo");
        std::process::exit(2);
    };
    let client = Arc::new(client);

    let init = client
        .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
        .await?;
    let server_name = init.server_info.name.clone();
    println!(
        "connected: {} v{} (protocol {})",
        server_name, init.server_info.version, init.protocol_version
    );
    if let Some(instructions) = &init.instructions {
        println!("{instructions}");
    }

    let surface = Arc::new(RwLock::new(fetch_surface(&client).await));
    {
        let s = surface.read().unwrap();
        println!(
            "{} tools, {} prompts, {} resources, {} templates. Type `help`.",
            s.tools.len(),
            s.prompts.len(),
            s.resources.len(),
            s.templates.len()
        );
    }

    // Readline runs on its own thread; lines cross into async via channels.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    {
        let helper = ReplHelper {
            surface: surface.clone(),
            client: client.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let prompt = format!("{server_name}> ");
        std::thread::spawn(move || {
            let mut editor: Editor<ReplHelper, DefaultHistory> =
                Editor::new().expect("failed to init line editor");
            editor.set_helper(Some(helper));
            loop {
                match editor.readline(&prompt) {
                    Ok(line) => {
                        let _ = editor.add_history_entry(line.as_str());
                        if line_tx.blocking_send(line).is_err() {
                            break;
                        }
                        // Wait until the command finished printing before
                        // showing the next prompt.
                        if ack_rx.recv().is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Interrupted) => continue,
                    Err(_) => {
                        let _ = line_tx.blocking_send("quit".to_string());
                        break;
                    }
                }
            }
        });
    }

    let mut jobs: Vec<(String, String)> = Vec::new();

    loop {
        tokio::select! {
            Some(()) = refresh_rx.recv() => {
                let fresh = fetch_surface(&client).await;
                println!("[surface changed: {} tools, {} prompts, {} resources]",
                    fresh.tools.len(), fresh.prompts.len(), fresh.resources.len());
                *surface.write().unwrap() = fresh;
            }
            maybe_line = line_rx.recv() => {
                let Some(line) = maybe_line else { break };
                let quit = handle_line(&client, &surface, &mut jobs, line.trim()).await;
                let _ = ack_tx.send(());
                if quit {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn handle_line(
    client: &Arc<McpClient>,
    surface: &Arc<RwLock<Surface>>,
    jobs: &mut Vec<(String, String)>,
    line: &str,
) -> bool {
    if line.is_empty() {
        return false;
    }
    let mut tokens: Vec<&str> = line.split_whitespace().collect();
    let background = tokens.last() == Some(&"&");
    if background {
        tokens.pop();
    }
    let cmd = tokens[0];
    let rest = &tokens[1..];

    match cmd {
        "quit" | "exit" => return true,
        "help" => {
            println!("built-ins:");
            println!("  tools | prompts | resources | templates   list the server surface");
            println!("  read <uri>                                read a resource");
            println!("  prompt <name> [k=v...]                    get a prompt");
            println!("  call <tool> <json>                        call a tool with raw JSON");
            println!("  <tool> [k=v...]                           call a tool (schema-coerced)");
            println!("  <tool> [k=v...] &                         run task-augmented (SEP-2663)");
            println!("  jobs | task <id> | wait <id> | cancel <id>  manage tasks");
            println!("  refresh | info | quit");
            let s = surface.read().unwrap();
            if !s.tools.is_empty() {
                println!("tools:");
                for t in &s.tools {
                    println!("  {:24} {}", t.name, t.description.as_deref().unwrap_or(""));
                }
            }
        }
        "tools" | "prompts" | "resources" | "templates" => {
            let s = surface.read().unwrap();
            match cmd {
                "tools" => {
                    for t in &s.tools {
                        println!("{:24} {}", t.name, t.description.as_deref().unwrap_or(""));
                    }
                }
                "prompts" => {
                    for p in &s.prompts {
                        let args: Vec<String> = p
                            .arguments
                            .iter()
                            .map(|a| {
                                if a.required {
                                    format!("<{}>", a.name)
                                } else {
                                    format!("[{}]", a.name)
                                }
                            })
                            .collect();
                        println!(
                            "{:24} {} {}",
                            p.name,
                            args.join(" "),
                            p.description.as_deref().unwrap_or("")
                        );
                    }
                }
                "resources" => {
                    for r in &s.resources {
                        println!("{:40} {}", r.uri, r.name);
                    }
                }
                _ => {
                    for t in &s.templates {
                        println!("{:40} {}", t.uri_template, t.name);
                    }
                }
            }
        }
        "read" => {
            let Some(uri) = rest.first() else {
                println!("usage: read <uri>");
                return false;
            };
            match client.read_resource(uri).await {
                Ok(result) => {
                    for c in result.contents {
                        if let Some(text) = c.text {
                            println!("{text}");
                        } else if let Some(blob) = c.blob {
                            println!("[binary {} base64 chars]", blob.len());
                        }
                    }
                }
                Err(e) => println!("error: {e}"),
            }
        }
        "prompt" => {
            let Some(name) = rest.first() else {
                println!("usage: prompt <name> [k=v...]");
                return false;
            };
            let mut prompt_args = HashMap::new();
            for t in &rest[1..] {
                if let Some((k, v)) = t.split_once('=') {
                    prompt_args.insert(k.to_string(), v.to_string());
                }
            }
            match client.get_prompt(name, Some(prompt_args)).await {
                Ok(result) => {
                    for m in result.messages {
                        let v = serde_json::to_value(&m).unwrap_or_default();
                        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                        let text = v
                            .pointer("/content/text")
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                v.get("content").map(|c| c.to_string()).unwrap_or_default()
                            });
                        println!("[{role}] {text}");
                    }
                }
                Err(e) => println!("error: {e}"),
            }
        }
        "call" => {
            let Some(name) = rest.first() else {
                println!("usage: call <tool> <json>");
                return false;
            };
            let json = rest[1..].join(" ");
            let arguments: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    println!("invalid JSON: {e}");
                    return false;
                }
            };
            run_tool(client, jobs, name, arguments, background).await;
        }
        "jobs" => {
            if jobs.is_empty() {
                println!("no background tasks");
            }
            for (id, tool) in jobs.iter() {
                match client.task_get(id).await {
                    Ok(task) => println!("{id}  {tool}  {}", task.status),
                    Err(_) => println!("{id}  {tool}  (gone)"),
                }
            }
        }
        "task" | "wait" | "cancel" => {
            let Some(id) = rest.first() else {
                println!("usage: {cmd} <task-id>");
                return false;
            };
            let outcome = match cmd {
                "task" => client.task_get(id).await,
                "wait" => client.task_wait(id).await,
                _ => match client.task_cancel(id, None).await {
                    Ok(()) => {
                        println!("cancel acknowledged");
                        client.task_get(id).await
                    }
                    Err(e) => Err(e),
                },
            };
            match outcome {
                Ok(task) => render_task(&task),
                Err(e) => println!("error: {e}"),
            }
        }
        "refresh" => {
            let fresh = fetch_surface(client).await;
            println!(
                "{} tools, {} prompts, {} resources, {} templates",
                fresh.tools.len(),
                fresh.prompts.len(),
                fresh.resources.len(),
                fresh.templates.len()
            );
            *surface.write().unwrap() = fresh;
        }
        "info" => match client.server_info().await {
            Some(info) => {
                println!("{} v{}", info.server_info.name, info.server_info.version);
                println!("protocol: {}", info.protocol_version);
                println!(
                    "capabilities: {}",
                    serde_json::to_string_pretty(&info.capabilities).unwrap_or_default()
                );
            }
            None => println!("not initialized"),
        },
        tool_name => {
            let schema = {
                let s = surface.read().unwrap();
                s.tools
                    .iter()
                    .find(|t| t.name == tool_name)
                    .map(|t| t.input_schema.clone())
            };
            let Some(schema) = schema else {
                println!("unknown command: {tool_name} (try `help`)");
                return false;
            };
            let arguments = parse_kv_args(&schema, rest);
            run_tool(client, jobs, tool_name, arguments, background).await;
        }
    }
    false
}

async fn run_tool(
    client: &Arc<McpClient>,
    jobs: &mut Vec<(String, String)>,
    name: &str,
    arguments: serde_json::Value,
    background: bool,
) {
    if background {
        match client.call_tool_as_task(name, arguments, None).await {
            Ok(created) => {
                println!("[task {}] started", created.task.task_id);
                jobs.push((created.task.task_id, name.to_string()));
            }
            Err(e) => println!("error: {e}"),
        }
        return;
    }
    match client.call_tool(name, arguments).await {
        Ok(result) => {
            if result.is_error {
                println!("(tool error)");
            }
            render_content(&result.content);
        }
        Err(e) => println!("error: {e}"),
    }
}
