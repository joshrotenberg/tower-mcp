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

mod editor;
mod elicit;
mod style;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use nu_ansi_term::{Color, Style};

use tower_mcp::client::{
    ChannelTransport, HttpClientTransport, McpClient, NotificationHandler, StdioClientTransport,
};
use tower_mcp::protocol::{
    Content, LogLevel, PromptDefinition, ResourceDefinition, ResourceTemplateDefinition,
    TaskObject, ToolDefinition,
};

use elicit::ReplClientHandler;
use style::{json_pretty, paint, tag, task_status_style};

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

    /// When to emit ANSI colors (auto detects tty and NO_COLOR).
    #[arg(long, value_enum, default_value = "auto")]
    color: style::ColorMode,

    /// Command (and arguments) of a stdio MCP server to spawn.
    command: Vec<String>,
}

/// The server surface the REPL turns into commands. Refreshed on connect
/// and whenever a list_changed notification arrives.
#[derive(Default)]
pub struct Surface {
    pub tools: Vec<ToolDefinition>,
    pub prompts: Vec<PromptDefinition>,
    pub resources: Vec<ResourceDefinition>,
    pub templates: Vec<ResourceTemplateDefinition>,
}

/// Built-in commands with the short descriptions shown in the completion
/// menu and `help`.
pub const BUILTINS: &[(&str, &str)] = &[
    ("help", "list built-ins and the server's tools"),
    ("tools", "list tools"),
    ("prompts", "list prompts"),
    ("resources", "list resources"),
    ("templates", "list resource templates"),
    ("describe", "show schemas and metadata for a name"),
    ("read", "read a resource"),
    ("prompt", "get a prompt"),
    ("call", "call a tool with raw JSON"),
    ("jobs", "list background tasks"),
    ("task", "show a background task"),
    ("wait", "wait for a background task"),
    ("cancel", "cancel a background task"),
    ("refresh", "re-fetch the server surface"),
    ("info", "replay the connection banner plus capabilities"),
    ("quit", "exit"),
    ("exit", "exit"),
];

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
            Content::Text { text, .. } => {
                if style::colors_enabled() && style::looks_like_markdown(text) {
                    println!("{}", style::render_markdown(text));
                } else {
                    println!("{text}");
                }
            }
            other => {
                let v = serde_json::to_value(other).unwrap_or_default();
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("content");
                match ty {
                    "image" | "audio" => {
                        let mime = v.get("mimeType").and_then(|m| m.as_str()).unwrap_or("?");
                        let len = v.get("data").and_then(|d| d.as_str()).map_or(0, str::len);
                        println!(
                            "{}",
                            tag(Style::new(), &format!("{ty} {mime}, {len} base64 chars"))
                        );
                    }
                    _ => println!("{}", json_pretty(&v)),
                }
            }
        }
    }
}

fn render_task(task: &TaskObject) {
    println!(
        "task {}  status={}  {}",
        paint(Style::new().bold(), &task.task_id),
        paint(task_status_style(task.status), &task.status.to_string()),
        task.status_message.as_deref().unwrap_or("")
    );
    if let Some(result) = &task.result {
        render_content(&result.content);
    }
    if let Some(err) = &task.error {
        println!("{} {}: {}", style::error_prefix(), err.code, err.message);
    }
}

/// The connection banner: server identity, negotiated protocol, and any
/// server instructions (markdown-rendered when it looks like markdown).
/// Printed at startup and replayed by the `info` command.
fn print_banner(info: &tower_mcp::protocol::InitializeResult) {
    println!(
        "connected: {} v{} {}",
        paint(Style::new().bold(), &info.server_info.name),
        info.server_info.version,
        paint(
            Style::new().dimmed(),
            &format!("(protocol {})", info.protocol_version)
        )
    );
    if let Some(instructions) = &info.instructions {
        if style::colors_enabled() && style::looks_like_markdown(instructions) {
            println!("{}", style::render_markdown(instructions));
        } else {
            println!("{instructions}");
        }
    }
}

/// The one-line surface summary.
fn print_counts(surface: &Surface) {
    println!(
        "{} tools, {} prompts, {} resources, {} templates. Type `help`.",
        surface.tools.len(),
        surface.prompts.len(),
        surface.resources.len(),
        surface.templates.len()
    );
}

/// True when the server rejected a request because the session is not yet
/// initialized (JSON-RPC `-32600` naming `notifications/initialized`). This
/// is retryable at startup: against a multi-instance server without a shared
/// session store, the initialize handshake and a follow-up request can land
/// on different instances, so a brief retry often lands on a consistent one.
fn is_not_initialized(e: &tower_mcp::Error) -> bool {
    matches!(
        e,
        tower_mcp::Error::JsonRpc(j)
            if j.code == -32600 && j.message.contains("notifications/initialized")
    )
}

/// Fetch the server surface once. Returns the surface plus whether any list
/// call was rejected as not-initialized (the retryable startup condition).
async fn fetch_surface_once(client: &McpClient) -> (Surface, bool) {
    fn take<T>(
        what: &str,
        r: Result<Vec<T>, tower_mcp::Error>,
        not_initialized: &mut bool,
    ) -> Vec<T> {
        match r {
            Ok(v) => v,
            Err(e) => {
                if is_not_initialized(&e) {
                    *not_initialized = true;
                } else {
                    eprintln!("warning: fetching {what} failed: {e}");
                }
                Vec::new()
            }
        }
    }
    let mut ni = false;
    let surface = Surface {
        tools: take("tools", client.list_all_tools().await, &mut ni),
        prompts: take("prompts", client.list_all_prompts().await, &mut ni),
        resources: take("resources", client.list_all_resources().await, &mut ni),
        templates: take(
            "resource templates",
            client.list_all_resource_templates().await,
            &mut ni,
        ),
    };
    (surface, ni)
}

async fn fetch_surface(client: &McpClient) -> Surface {
    fetch_surface_once(client).await.0
}

/// Startup surface fetch with a bounded retry on the not-initialized
/// condition. Explains the likely cause if it never clears.
async fn fetch_surface_initial(client: &McpClient) -> Surface {
    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        let (surface, not_initialized) = fetch_surface_once(client).await;
        if !not_initialized {
            return surface;
        }
        if attempt == ATTEMPTS {
            eprintln!(
                "warning: the server kept rejecting surface requests as not-initialized \
                 after {ATTEMPTS} attempts. This usually means the server runs multiple \
                 instances without a shared session store, so requests are routed to \
                 instances that do not share the initialized session. Try `refresh`, or \
                 connect to a single-instance or stateless server."
            );
            return surface;
        }
        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
    }
    unreachable!()
}

fn demo_router() -> tower_mcp::McpRouter {
    use tower_mcp::extract::RawArgs;
    use tower_mcp::protocol::{CompleteResult, CompletionReference, ReadResourceResult};
    use tower_mcp::resource::ResourceTemplateBuilder;
    use tower_mcp::{CallToolResult, PromptBuilder, TaskSupportMode, ToolBuilder};

    const NOTES: &[(&str, &str)] = &[
        ("groceries", "- eggs\n- coffee"),
        ("ideas", "# Ideas\n\n- a REPL for MCP servers"),
        ("todo", "1. ship it"),
    ];

    tower_mcp::McpRouter::new()
        .server_info("mcp-repl-demo", env!("CARGO_PKG_VERSION"))
        .prompt(
            PromptBuilder::new("greet")
                .description("Generate a greeting (name tab-completes via the server)")
                .required_arg("name", "The person to greet")
                .handler(|args| async move {
                    let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                    Ok(tower_mcp::GetPromptResult::user_message(format!(
                        "Please greet {name} warmly."
                    )))
                })
                .build(),
        )
        .resource_template(
            ResourceTemplateBuilder::new("note://{name}")
                .name("Notes")
                .description("Tiny in-memory notes (name tab-completes via the server)")
                .mime_type("text/markdown")
                .handler(
                    |uri: String, vars: std::collections::HashMap<String, String>| async move {
                        let name = vars.get("name").cloned().unwrap_or_default();
                        let text = NOTES
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map(|(_, t)| (*t).to_string())
                            .unwrap_or_else(|| format!("no note named `{name}`"));
                        Ok(ReadResourceResult::text(uri, text))
                    },
                ),
        )
        .completion_handler(|params| async move {
            let partial = params.argument.value;
            let candidates: Vec<String> = match &params.reference {
                CompletionReference::Prompt { name } if name == "greet" => {
                    ["Ada", "Alan", "Grace", "Linus"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }
                CompletionReference::Resource { uri } if uri == "note://{name}" => {
                    NOTES.iter().map(|(n, _)| n.to_string()).collect()
                }
                _ => Vec::new(),
            };
            Ok(CompleteResult::new(
                candidates
                    .into_iter()
                    .filter(|c| c.starts_with(&partial))
                    .collect::<Vec<_>>(),
            ))
        })
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
            ToolBuilder::new("about")
                .description("Markdown-formatted notes about this demo server")
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::text(
                        "# mcp-repl demo\n\n\
                         A tiny in-process router for exploring the REPL.\n\n\
                         - `echo message=hi` echoes back\n\
                         - `slow_add a=2 b=3 &` runs **task-augmented**\n\
                         - `describe slow_add` shows the tool's schemas\n",
                    ))
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

fn log_level_style(level: LogLevel) -> Style {
    match level {
        LogLevel::Emergency | LogLevel::Alert | LogLevel::Critical | LogLevel::Error => {
            Style::new().fg(Color::Red)
        }
        LogLevel::Warning => Style::new().fg(Color::Yellow),
        LogLevel::Notice | LogLevel::Info => Style::new().fg(Color::Green),
        _ => Style::new().dimmed(),
    }
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();
    style::init(args.color);

    // True while the reedline editor owns the terminal; the elicitation
    // handler declines form requests during that window instead of
    // fighting over raw-mode stdin.
    let at_prompt = Arc::new(AtomicBool::new(false));

    // Notifications print inline and trigger surface refreshes.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let notifications = {
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
                        format!(" {:.0}%", 100.0 * done / total)
                    }
                    _ => String::new(),
                };
                println!(
                    "{} {}",
                    tag(Style::new().fg(Color::Cyan), &format!("progress{pct}")),
                    p.message.as_deref().unwrap_or("")
                );
            })
            .on_log_message(|m| {
                println!(
                    "{} {}",
                    tag(log_level_style(m.level), &format!("log {}", m.level)),
                    m.data
                );
            })
    };
    let handler = ReplClientHandler::new(notifications, at_prompt.clone());

    let builder = McpClient::builder().with_elicitation();
    let client = if args.demo {
        builder
            .connect(ChannelTransport::new(demo_router()), handler)
            .await?
    } else if let Some(url) = &args.http {
        builder
            .connect(HttpClientTransport::new(url.clone()), handler)
            .await?
    } else if !args.command.is_empty() {
        let cmd_args: Vec<&str> = args.command[1..].iter().map(|s| s.as_str()).collect();
        let transport = StdioClientTransport::spawn(&args.command[0], &cmd_args).await?;
        builder.connect(transport, handler).await?
    } else {
        eprintln!("usage: mcp-repl <server command...> | --http <url> | --demo");
        std::process::exit(2);
    };
    let client = Arc::new(client);

    let init = client
        .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
        .await?;
    let server_name = init.server_info.name.clone();
    print_banner(&init);

    let surface = Arc::new(RwLock::new(fetch_surface_initial(&client).await));
    print_counts(&surface.read().unwrap());

    // Readline runs on its own thread; lines cross into async via channels.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    editor::spawn_readline_thread(
        server_name,
        surface.clone(),
        client.clone(),
        tokio::runtime::Handle::current(),
        line_tx,
        ack_rx,
        at_prompt,
    );

    let mut jobs: Vec<(String, String)> = Vec::new();

    loop {
        tokio::select! {
            Some(()) = refresh_rx.recv() => {
                let fresh = fetch_surface(&client).await;
                println!("{} {} tools, {} prompts, {} resources",
                    tag(Style::new().fg(Color::Cyan), "surface changed"),
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
    if tokens.is_empty() {
        return false;
    }
    let cmd = tokens[0];
    let rest = &tokens[1..];

    match cmd {
        "quit" | "exit" => return true,
        "help" => {
            println!("built-ins:");
            println!("  tools | prompts | resources | templates   list the server surface");
            println!("  describe <name>                           schemas and metadata");
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
                    println!(
                        "  {:24} {}",
                        paint(Style::new().fg(Color::Green), &t.name),
                        t.description.as_deref().unwrap_or("")
                    );
                }
            }
        }
        "tools" | "prompts" | "resources" | "templates" => {
            let s = surface.read().unwrap();
            match cmd {
                "tools" => {
                    for t in &s.tools {
                        println!(
                            "{:24} {}",
                            paint(Style::new().fg(Color::Green), &t.name),
                            t.description.as_deref().unwrap_or("")
                        );
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
                            paint(Style::new().fg(Color::Green), &p.name),
                            paint(Style::new().fg(Color::Cyan), &args.join(" ")),
                            p.description.as_deref().unwrap_or("")
                        );
                    }
                }
                "resources" => {
                    for r in &s.resources {
                        println!(
                            "{:40} {}",
                            paint(Style::new().fg(Color::Green), &r.uri),
                            r.name
                        );
                    }
                    // Templates (parameterized URIs) are a separate MCP list
                    // and easy to miss; point at them.
                    if !s.templates.is_empty() {
                        println!(
                            "{}",
                            paint(
                                Style::new().dimmed(),
                                &format!(
                                    "(+ {} resource template(s) with variables, see `templates`)",
                                    s.templates.len()
                                )
                            )
                        );
                    }
                }
                _ => {
                    for t in &s.templates {
                        println!(
                            "{:40} {}",
                            paint(Style::new().fg(Color::Green), &t.uri_template),
                            t.name
                        );
                    }
                    if !s.resources.is_empty() {
                        println!(
                            "{}",
                            paint(
                                Style::new().dimmed(),
                                &format!(
                                    "(+ {} concrete resource(s), see `resources`)",
                                    s.resources.len()
                                )
                            )
                        );
                    }
                }
            }
        }
        "describe" => {
            let Some(name) = rest.first() else {
                println!("usage: describe <tool|prompt|resource|template>");
                return false;
            };
            describe(&surface.read().unwrap(), name);
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
                            let is_md = c
                                .mime_type
                                .as_deref()
                                .is_some_and(|m| m.contains("markdown"))
                                || style::looks_like_markdown(&text);
                            if style::colors_enabled() && is_md {
                                println!("{}", style::render_markdown(&text));
                            } else {
                                println!("{text}");
                            }
                        } else if let Some(blob) = c.blob {
                            println!(
                                "{}",
                                tag(Style::new(), &format!("binary {} base64 chars", blob.len()))
                            );
                        }
                    }
                }
                Err(e) => println!("{}: {e}", style::error_prefix()),
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
                        println!("{} {}", tag(Style::new().fg(Color::Cyan), role), text);
                    }
                }
                Err(e) => println!("{}: {e}", style::error_prefix()),
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
                    Ok(task) => println!(
                        "{id}  {tool}  {}",
                        paint(task_status_style(task.status), &task.status.to_string())
                    ),
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
                Err(e) => println!("{}: {e}", style::error_prefix()),
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
                // Replay the full startup banner, then add capabilities.
                print_banner(&info);
                print_counts(&surface.read().unwrap());
                let caps = serde_json::to_value(&info.capabilities).unwrap_or_default();
                println!("capabilities: {}", json_pretty(&caps));
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
                println!(
                    "unknown command: {} (try `help`)",
                    paint(Style::new().fg(Color::Red), tool_name)
                );
                return false;
            };
            let arguments = parse_kv_args(&schema, rest);
            run_tool(client, jobs, tool_name, arguments, background).await;
        }
    }
    false
}

/// The `describe` built-in: schemas for a tool, the argument table for a
/// prompt, metadata for a resource or template.
fn describe(surface: &Surface, name: &str) {
    if let Some(t) = surface.tools.iter().find(|t| t.name == name) {
        println!(
            "tool {}  {}",
            paint(Style::new().fg(Color::Green).bold(), &t.name),
            t.description.as_deref().unwrap_or("")
        );
        if let Some(a) = &t.annotations {
            let mut hints = Vec::new();
            if a.read_only_hint {
                hints.push("read-only");
            }
            if a.idempotent_hint {
                hints.push("idempotent");
            }
            if a.destructive_hint && !a.read_only_hint {
                hints.push("destructive");
            }
            if a.open_world_hint {
                hints.push("open-world");
            }
            if !hints.is_empty() {
                println!("  hints: {}", hints.join(", "));
            }
        }
        if let Some(e) = &t.execution {
            let v = serde_json::to_value(e).unwrap_or_default();
            if let Some(mode) = v.get("taskSupport").and_then(|m| m.as_str()) {
                println!("  task support: {mode}");
            }
        }
        println!("input schema:");
        println!("{}", json_pretty(&t.input_schema));
        if let Some(out) = &t.output_schema {
            println!("output schema:");
            println!("{}", json_pretty(out));
        }
        return;
    }
    if let Some(p) = surface.prompts.iter().find(|p| p.name == name) {
        println!(
            "prompt {}  {}",
            paint(Style::new().fg(Color::Green).bold(), &p.name),
            p.description.as_deref().unwrap_or("")
        );
        if p.arguments.is_empty() {
            println!("  (no arguments)");
        } else {
            println!("arguments:");
            for a in &p.arguments {
                println!(
                    "  {:20} {:10} {}",
                    paint(Style::new().fg(Color::Cyan), &a.name),
                    if a.required { "required" } else { "optional" },
                    a.description.as_deref().unwrap_or("")
                );
            }
        }
        return;
    }
    if let Some(r) = surface
        .resources
        .iter()
        .find(|r| r.uri == name || r.name == name)
    {
        println!(
            "resource {}",
            paint(Style::new().fg(Color::Green).bold(), &r.uri)
        );
        println!("  name: {}", r.name);
        if let Some(t) = &r.title {
            println!("  title: {t}");
        }
        if let Some(d) = &r.description {
            println!("  description: {d}");
        }
        if let Some(m) = &r.mime_type {
            println!("  mimeType: {m}");
        }
        if let Some(s) = r.size {
            println!("  size: {s} bytes");
        }
        return;
    }
    if let Some(t) = surface
        .templates
        .iter()
        .find(|t| t.uri_template == name || t.name == name)
    {
        println!(
            "template {}",
            paint(Style::new().fg(Color::Green).bold(), &t.uri_template)
        );
        println!("  name: {}", t.name);
        if let Some(d) = &t.description {
            println!("  description: {d}");
        }
        if let Some(m) = &t.mime_type {
            println!("  mimeType: {m}");
        }
        if !t.arguments.is_empty() {
            println!("arguments:");
            for a in &t.arguments {
                println!(
                    "  {:20} {:10} {}",
                    paint(Style::new().fg(Color::Cyan), &a.name),
                    if a.required { "required" } else { "optional" },
                    a.description.as_deref().unwrap_or("")
                );
            }
        }
        return;
    }
    println!("nothing on the surface named `{name}` (try `tools`, `prompts`, `resources`)");
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
                println!(
                    "{} started",
                    tag(
                        Style::new().fg(Color::Yellow),
                        &format!("task {}", created.task.task_id)
                    )
                );
                jobs.push((created.task.task_id, name.to_string()));
            }
            Err(e) => println!("{}: {e}", style::error_prefix()),
        }
        return;
    }
    match client.call_tool(name, arguments).await {
        Ok(result) => {
            if result.is_error {
                println!("{}", tag(Style::new().fg(Color::Red), "tool error"));
            }
            render_content(&result.content);
        }
        Err(e) => println!("{}: {e}", style::error_prefix()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonrpc(code: i32, message: &str) -> tower_mcp::Error {
        tower_mcp::Error::JsonRpc(tower_mcp::error::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        })
    }

    #[test]
    fn detects_not_initialized_startup_error() {
        assert!(is_not_initialized(&jsonrpc(
            -32600,
            "Client must send notifications/initialized before making requests"
        )));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        // Same code, different message.
        assert!(!is_not_initialized(&jsonrpc(
            -32600,
            "some other invalid request"
        )));
        // Right message text, different code.
        assert!(!is_not_initialized(&jsonrpc(
            -32602,
            "notifications/initialized"
        )));
        // A transport error is never the not-initialized case.
        assert!(!is_not_initialized(&tower_mcp::Error::Transport(
            "boom".into()
        )));
    }
}
