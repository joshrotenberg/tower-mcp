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
//!
//! # Connect to a named profile from ~/.config/mcp-repl/config.toml:
//! cargo run -p mcp-repl -- --server cratesio
//! ```
//!
//! Inside the REPL, `help` lists the built-ins and the server's tools, and
//! `alias <name>=<expansion>` gives a frequent command a short name, kept in
//! the same config file as the server profiles.
//! A trailing `&` runs a tool task-augmented (SEP-2663): the call returns a
//! task id immediately; `jobs`, `task <id>`, `wait <id>`, and `cancel <id>`
//! manage it.

mod alias;
mod config;
mod editor;
mod elicit;
mod sampling;
mod session;
mod style;
mod wire;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use nu_ansi_term::{Color, Style};

use tower_mcp::client::{
    ChannelTransport, HttpClientConfig, HttpClientTransport, McpClient, NotificationHandler,
    StdioClientTransport,
};
use tower_mcp::protocol::{
    Content, LogLevel, PromptDefinition, ResourceDefinition, ResourceTemplateDefinition,
    TaskObject, ToolDefinition,
};

use alias::Aliases;
use elicit::ReplClientHandler;
use session::{Connector, Session, is_not_initialized, is_session_lost};
use style::{json_pretty, paint, tag, task_status_style};
use wire::{TracingTransport, wire};

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
    #[arg(long, conflicts_with_all = ["http", "command", "server"])]
    demo: bool,

    /// Connect using the named `[servers.<name>]` profile from the config
    /// file. A bare positional that matches a profile name works too.
    #[arg(long, value_name = "NAME")]
    server: Option<String>,

    /// Read server profiles from this file instead of
    /// `$XDG_CONFIG_HOME/mcp-repl/config.toml` (or `~/.config/mcp-repl/config.toml`).
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Print the configured server profiles and exit.
    #[arg(long)]
    list_servers: bool,

    /// When to emit ANSI colors (auto detects tty and NO_COLOR).
    #[arg(long, value_enum, default_value = "auto")]
    color: style::ColorMode,

    /// Bearer token for an authenticated `--http` server (sets
    /// `Authorization: Bearer <token>`). Falls back to the `MCP_BEARER`
    /// environment variable, which is preferred since a command-line token is
    /// visible in `ps` and shell history.
    #[arg(long)]
    bearer: Option<String>,

    /// Extra header for an authenticated `--http` server, as `Name: Value`
    /// (repeatable). Split on the first colon.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    headers: Vec<String>,

    /// Run a command and exit instead of starting the interactive prompt.
    /// Repeatable; commands run in order against the same session. Exit status
    /// is non-zero if any command errors. Combine with --http/--demo or a
    /// stdio child.
    #[arg(short = 'e', long = "exec", value_name = "COMMAND")]
    exec: Vec<String>,

    /// In --exec mode, emit raw JSON results instead of the pretty renderer,
    /// for piping to tools like jq.
    #[arg(long)]
    json: bool,

    /// In --exec mode, still print the startup banner and surface listing
    /// (suppressed by default so only command output is emitted).
    #[arg(long)]
    verbose: bool,

    /// How to answer a server's `sampling/createMessage` request: `prompt`
    /// shows it and reads the assistant message on stdin, `canned` answers
    /// with a fixed placeholder, `decline` refuses. Defaults to `prompt`
    /// interactively and `decline` under --exec.
    #[arg(long, value_enum, value_name = "STRATEGY")]
    sampling: Option<sampling::SamplingMode>,

    /// Do not persist command history to ~/.mcp-repl_history.
    #[arg(long)]
    no_history: bool,

    /// Do not transparently re-establish an HTTP session that the server has
    /// lost (restart, OOM, or a 502/503 from the edge in front of it).
    /// Session-loss errors surface as-is instead.
    #[arg(long)]
    no_reconnect: bool,

    /// Print every JSON-RPC frame sent and received, to stderr. Equivalent to
    /// starting with `wire on`; toggle it mid-session with `wire on|off`.
    #[arg(long)]
    trace: bool,

    /// Command (and arguments) of a stdio MCP server to spawn.
    command: Vec<String>,
}

/// Set in `--exec` mode by `--json`: render raw JSON instead of pretty output.
static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);
/// Set whenever a command errors, so `--exec` can exit non-zero.
static HAD_ERROR: AtomicBool = AtomicBool::new(false);

fn json_output() -> bool {
    JSON_OUTPUT.load(Ordering::Relaxed)
}

fn note_error() {
    HAD_ERROR.store(true, Ordering::Relaxed);
}

/// A one-line JSON error object for `--json` mode.
fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
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
    ("alias", "define, list, or show a command alias"),
    ("unalias", "remove a command alias"),
    ("refresh", "re-fetch the server surface"),
    ("info", "replay the connection banner plus capabilities"),
    ("wire", "toggle raw JSON-RPC frame tracing (on|off)"),
    ("last", "reprint the previous request and response"),
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

/// A dimmed `[142ms]` / `[1.23s]` annotation for how long a call took.
/// Printed on its own trailing line after a request-issuing command, so a slow
/// (or timing-out) call is visible without interleaving with streamed output.
pub fn timing(elapsed: Duration) -> String {
    let body = if elapsed.as_millis() < 1000 {
        format!("[{}ms]", elapsed.as_millis())
    } else {
        format!("[{:.2}s]", elapsed.as_secs_f64())
    };
    paint(Style::new().dimmed(), &body)
}

/// A compact tool listing for the startup banner: name and description, capped
/// so a large surface does not flood the screen. The full list is always
/// available via `tools`.
fn print_tool_overview(surface: &Surface) {
    const CAP: usize = 30;
    if surface.tools.is_empty() {
        return;
    }
    for t in surface.tools.iter().take(CAP) {
        println!(
            "{:24} {}",
            paint(Style::new().fg(Color::Green), &t.name),
            t.description.as_deref().unwrap_or("")
        );
    }
    if surface.tools.len() > CAP {
        println!(
            "{}",
            paint(
                Style::new().dimmed(),
                &format!("... +{} more, type `tools`", surface.tools.len() - CAP)
            )
        );
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

/// Run one request, and if it fails because the server lost the session,
/// rebuild the connection and run it exactly once more.
///
/// The retry is deliberately bounded to a single attempt: a server that is
/// down stays down, and a loop here would turn one dead command into a long
/// unresponsive prompt. On the second failure the original error surfaces
/// with a hint, which is what the user would have seen without reconnection.
///
/// `op` runs against whichever client is current, so it takes the client as
/// an argument rather than closing over one: the second call must use the
/// client the reconnect installed, not the dead one.
async fn with_reconnect<T, F, Fut>(
    session: &Session,
    surface: &Arc<RwLock<Surface>>,
    op: F,
) -> Result<T, tower_mcp::Error>
where
    F: Fn(Arc<McpClient>) -> Fut,
    Fut: Future<Output = Result<T, tower_mcp::Error>>,
{
    let seen = session.generation();
    let err = match op(session.client()).await {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    if !session.can_reconnect() || !is_session_lost(&err) {
        return Err(err);
    }
    if let Err(reconnect_err) = session.reconnect(seen).await {
        eprintln!("reconnect failed: {reconnect_err}");
        return Err(err);
    }
    // The surface belongs to the old session: a restarted server may expose a
    // different set of tools, and the completer and command dispatch both read
    // this. Refresh before the retry so the retried command and the next
    // prompt agree on what exists.
    *surface.write().unwrap() = fetch_surface(&session.client()).await;
    // stderr, so the note does not land in the middle of `--json` output
    // being piped somewhere.
    eprintln!("{}", paint(Style::new().dimmed(), "[reconnected]"));

    let retried = op(session.client()).await;
    if let Err(e) = &retried
        && is_session_lost(e)
    {
        eprintln!(
            "still no session after reconnecting. The server is likely down or \
             restart-looping; check its logs, or pass --no-reconnect to see the \
             raw errors."
        );
    }
    retried
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
    // The four list calls are independent reads, so run them concurrently.
    // The McpClient message loop multiplexes requests by id, so this is safe
    // on a single connection, and it means startup costs one round-trip's
    // latency instead of four in series. It also bounds the cost of a slow or
    // unresponsive server: against a server that makes each list time out, the
    // surface fetch now waits one `request_timeout`, not four.
    let (tools, prompts, resources, templates) = tokio::join!(
        client.list_all_tools(),
        client.list_all_prompts(),
        client.list_all_resources(),
        client.list_all_resource_templates(),
    );
    let mut ni = false;
    let surface = Surface {
        tools: take("tools", tools, &mut ni),
        prompts: take("prompts", prompts, &mut ni),
        resources: take("resources", resources, &mut ni),
        templates: take("resource templates", templates, &mut ni),
    };
    (surface, ni)
}

async fn fetch_surface(client: &McpClient) -> Surface {
    fetch_surface_once(client).await.0
}

/// Re-fetch the surface, reconnecting first if the fetch shows the session is
/// gone. The four list calls swallow their own errors, so not-initialized is
/// the one session-loss signal that survives to here; the typed session
/// errors would have shown up as empty lists with a warning.
async fn refresh_surface(session: &Session) -> Surface {
    let (fresh, not_initialized) = fetch_surface_once(&session.client()).await;
    if !not_initialized || !session.can_reconnect() {
        return fresh;
    }
    let seen = session.generation();
    match session.reconnect(seen).await {
        Ok(()) => {
            eprintln!("{}", paint(Style::new().dimmed(), "[reconnected]"));
            fetch_surface(&session.client()).await
        }
        Err(e) => {
            eprintln!("reconnect failed: {e}");
            fresh
        }
    }
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
                 after {ATTEMPTS} attempts. The session the handshake established is not \
                 being recognized on follow-up requests. Two common causes: the server runs \
                 multiple instances without a shared session store, so requests scatter \
                 across instances; or a single instance restarted (crash, OOM, or redeploy) \
                 between requests and lost its in-memory sessions. Try `refresh`. A \
                 persistent session store or the stateless protocol avoids both; if it is a \
                 single instance, check its logs and resources (an OOM-looping machine \
                 flaps like this)."
            );
            return surface;
        }
        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
    }
    unreachable!()
}

/// Build the HTTP client config from the auth flags and the resolved profile.
/// `--bearer` wins over the profile's token, which wins over the `MCP_BEARER`
/// environment variable; `--header` flags are applied after the profile's
/// headers so a repeated name overrides it. Each `--header "Name: Value"` is
/// split on the first colon (surrounding whitespace trimmed); a header with no
/// colon is a usage error.
fn build_http_config(
    bearer: Option<String>,
    headers: &[String],
    profile_bearer: Option<String>,
    profile_headers: &[(String, String)],
) -> Result<HttpClientConfig, String> {
    let mut config = HttpClientConfig::default();
    for (name, value) in profile_headers {
        config = config.header(name.as_str(), value.as_str());
    }
    if let Some(token) = bearer
        .or(profile_bearer)
        .or_else(|| std::env::var("MCP_BEARER").ok())
    {
        config = config.bearer_token(token);
    }
    for raw in headers {
        let (name, value) = raw
            .split_once(':')
            .ok_or_else(|| format!("invalid --header {raw:?}: expected `Name: Value`"))?;
        config = config.header(name.trim(), value.trim());
    }
    Ok(config)
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

/// The notification callbacks: log and progress messages print inline,
/// `list_changed` notifications nudge the event loop to refresh the surface.
/// Built per client, since a reconnect installs a new one.
fn notification_handler(refresh_tx: tokio::sync::mpsc::UnboundedSender<()>) -> NotificationHandler {
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
}

/// The recipe for rebuilding an `--http` connection: a brand new transport
/// (so no dead `Mcp-Session-Id` is carried over), a fresh handler, and the
/// initialize handshake, exactly as at startup. The rebuilt transport is
/// wrapped in `TracingTransport` like the startup one, so `wire` and `last`
/// keep reporting frames after a reconnect, and it declares the same
/// capabilities as the startup client: a reconnect must not quietly leave the
/// session less capable than it began.
fn http_connector(
    url: String,
    config: HttpClientConfig,
    make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync>,
) -> Connector {
    Box::new(move || {
        let (url, config, handler) = (url.clone(), config.clone(), make_handler());
        Box::pin(async move {
            let client = McpClient::builder()
                .with_elicitation()
                .with_sampling()
                .connect(
                    TracingTransport::new(HttpClientTransport::with_config(url, config)),
                    handler,
                )
                .await?;
            client
                .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
                .await?;
            Ok(client)
        })
    })
}

/// Load the profile config, exiting with a usage status on a bad file. A
/// missing file at the default location is not an error: profiles are opt-in.
fn load_config(explicit: Option<&str>) -> config::Config {
    let Some((path, explicit)) = config::config_path(explicit) else {
        return config::Config::default();
    };
    match config::Config::load(&path, explicit) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    }
}

/// `--list-servers`: the configured profiles, one per line.
fn print_servers(config: &config::Config) {
    if config.servers.is_empty() {
        println!("no server profiles configured");
        return;
    }
    let width = config.names().iter().map(|n| n.len()).max().unwrap_or(0);
    for (name, profile) in &config.servers {
        println!(
            "{:width$}  {}",
            paint(Style::new().fg(Color::Cyan), name),
            paint(Style::new().dimmed(), &profile.summary()),
        );
    }
}

/// Resolve the profile the invocation names, if any: `--server <name>`, or a
/// bare single positional that matches a configured profile. A positional that
/// matches nothing stays a stdio command, so spawning a server by bare name
/// still works when no profile shadows it.
fn resolve_profile(args: &Args, config: &config::Config) -> Option<(String, config::Connection)> {
    let name = args
        .server
        .clone()
        .or_else(|| match args.command.as_slice() {
            [only] if config.servers.contains_key(only) => Some(only.clone()),
            _ => None,
        })?;
    let profile = match config.profile(&name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    if profile.bearer.is_some() {
        eprintln!(
            "warning: profile {name:?} stores a literal `bearer` token; prefer \
             `bearer_env = \"VAR\"` so the token is not kept in the config file"
        );
    }
    match profile.resolve_with(|var| std::env::var(var).ok()) {
        Ok(connection) => Some((name, connection)),
        Err(e) => {
            eprintln!("error: server profile {name:?}: {e}");
            std::process::exit(2);
        }
    }
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
    wire::init(args.trace);
    JSON_OUTPUT.store(args.json, Ordering::Relaxed);

    // Server profiles are read up front: both --list-servers and profile
    // resolution need them before anything connects.
    let config_file = config::config_path(args.config.as_deref()).map(|(path, _)| path);
    let profiles = load_config(args.config.as_deref());
    if args.list_servers {
        print_servers(&profiles);
        return Ok(());
    }
    let profile = resolve_profile(&args, &profiles);
    // --exec runs commands and exits; suppress the banner and surface listing
    // unless --verbose, so scripted output is only the command results.
    let one_shot = !args.exec.is_empty();
    let quiet = one_shot && !args.verbose;

    // True while the reedline editor owns the terminal; the elicitation
    // handler declines form requests during that window instead of
    // fighting over raw-mode stdin.
    let at_prompt = Arc::new(AtomicBool::new(false));

    // Notifications print inline and trigger surface refreshes.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // A reconnect needs a fresh handler for the new client, so build handlers
    // through a factory rather than once.
    let make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync> = {
        let refresh_tx = refresh_tx.clone();
        let at_prompt = at_prompt.clone();
        Arc::new(move || {
            ReplClientHandler::new(notification_handler(refresh_tx.clone()), at_prompt.clone())
        })
    };
    drop(refresh_tx);
    // Sampling has no model behind it, so the operator answers. Under --exec
    // there is nobody to ask, so requests are refused unless --sampling says
    // otherwise.
    sampling::init(sampling::resolve(args.sampling, one_shot));

    // Explicit flags override profile fields: --http retargets a profile's URL
    // while keeping its auth, and --bearer/--header are layered on in
    // build_http_config.
    let (profile_name, connection) = match profile {
        Some((name, c)) => (Some(name), Some(c)),
        None => (None, None),
    };

    // Aliases come from the same file as the profiles: the global table plus
    // the connected profile's own, which shadows it.
    let aliases = Arc::new(RwLock::new(Aliases::new(
        profiles.aliases.clone(),
        profile_name
            .as_ref()
            .and_then(|name| profiles.servers.get(name))
            .map(|p| p.aliases.clone())
            .unwrap_or_default(),
        profile_name.clone(),
        config_file,
    )));

    let connection = match (args.http.clone(), connection) {
        (
            Some(url),
            Some(config::Connection::Http {
                bearer, headers, ..
            }),
        ) => Some(config::Connection::Http {
            url,
            bearer,
            headers,
        }),
        (Some(url), _) => Some(config::Connection::Http {
            url,
            bearer: None,
            headers: Vec::new(),
        }),
        (None, Some(c)) => Some(c),
        (None, None) if !args.command.is_empty() => Some(config::Connection::Stdio {
            command: args.command.clone(),
        }),
        (None, None) => None,
    };

    let over_http = matches!(connection, Some(config::Connection::Http { .. }));
    if !over_http && (args.bearer.is_some() || !args.headers.is_empty()) {
        eprintln!("warning: --bearer/--header apply only to HTTP servers; ignoring them here");
    }
    if let Some(name) = &profile_name
        && !quiet
    {
        println!(
            "{}",
            tag(Style::new().fg(Color::Cyan), &format!("profile {name}"))
        );
    }

    // Every transport is wrapped, whatever `--trace` says: the wrapper is what
    // records the exchange `last` reprints, and tracing can be switched on
    // mid-session with `wire on`.
    // Sampling is advertised whatever the strategy: a client is allowed to
    // refuse an individual request, and a server can only ask when the
    // capability is declared, so `--sampling decline` still exercises the
    // server's rejection path.
    let builder = McpClient::builder().with_elicitation().with_sampling();
    // Only `--http` can be resurrected. A stdio child that dies takes its
    // stdin and stdout with it (respawning it is a separate concern), and the
    // in-process demo router cannot lose a session at all.
    let mut connector: Option<Connector> = None;
    let client = if args.demo {
        builder
            .connect(
                TracingTransport::new(ChannelTransport::new(demo_router())),
                make_handler(),
            )
            .await?
    } else {
        match connection {
            Some(config::Connection::Http {
                url,
                bearer,
                headers,
            }) => {
                let config =
                    build_http_config(args.bearer.clone(), &args.headers, bearer, &headers)?;
                if !args.no_reconnect {
                    connector = Some(http_connector(
                        url.clone(),
                        config.clone(),
                        make_handler.clone(),
                    ));
                }
                builder
                    .connect(
                        TracingTransport::new(HttpClientTransport::with_config(url, config)),
                        make_handler(),
                    )
                    .await?
            }
            Some(config::Connection::Stdio { command }) => {
                let cmd_args: Vec<&str> = command[1..].iter().map(|s| s.as_str()).collect();
                let transport = StdioClientTransport::spawn(&command[0], &cmd_args).await?;
                builder
                    .connect(TracingTransport::new(transport), make_handler())
                    .await?
            }
            None => {
                eprintln!(
                    "usage: mcp-repl <server command...> | --http <url> | --server <name> | --demo"
                );
                std::process::exit(2);
            }
        }
    };

    let init = client
        .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
        .await?;
    let server_name = init.server_info.name.clone();
    if !quiet {
        print_banner(&init);
    }
    let session = Arc::new(Session::new(client, connector));
    let client = session.client();

    let surface = Arc::new(RwLock::new(fetch_surface_initial(&client).await));
    if !quiet {
        let s = surface.read().unwrap();
        print_counts(&s);
        // List the tools at startup so the surface is browsable immediately,
        // unless the server already enumerated them in its instructions (some
        // servers dump the whole surface there); then it would just repeat.
        let instructions_list_tools = init
            .instructions
            .as_deref()
            .is_some_and(|instr| s.tools.first().is_some_and(|t| instr.contains(&t.name)));
        if !instructions_list_tools {
            print_tool_overview(&s);
        }
    }

    // One-shot: run each --exec command in order, then exit non-zero if any
    // errored. No editor, no event loop.
    if one_shot {
        let mut jobs: Vec<(String, String)> = Vec::new();
        for cmd in &args.exec {
            if handle_line(&session, &surface, &aliases, &mut jobs, cmd.trim()).await {
                break;
            }
        }
        std::process::exit(if HAD_ERROR.load(Ordering::Relaxed) {
            1
        } else {
            0
        });
    }

    // Readline runs on its own thread; lines cross into async via channels.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    editor::spawn_readline_thread(
        server_name,
        surface.clone(),
        session.clone(),
        aliases.clone(),
        tokio::runtime::Handle::current(),
        line_tx,
        ack_rx,
        at_prompt,
        !args.no_history,
    );

    let mut jobs: Vec<(String, String)> = Vec::new();

    loop {
        tokio::select! {
            Some(()) = refresh_rx.recv() => {
                let fresh = fetch_surface(&session.client()).await;
                println!("{} {} tools, {} prompts, {} resources",
                    tag(Style::new().fg(Color::Cyan), "surface changed"),
                    fresh.tools.len(), fresh.prompts.len(), fresh.resources.len());
                *surface.write().unwrap() = fresh;
            }
            maybe_line = line_rx.recv() => {
                let Some(line) = maybe_line else { break };
                let quit = handle_line(&session, &surface, &aliases, &mut jobs, line.trim()).await;
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
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    aliases: &Arc<RwLock<Aliases>>,
    jobs: &mut Vec<(String, String)>,
    line: &str,
) -> bool {
    if line.is_empty() {
        return false;
    }
    // Aliases expand before anything else looks at the line, so an expansion
    // can carry arguments, a trailing `&`, or another alias. An alias can
    // never be named after a built-in, so `alias`/`unalias` stay reachable.
    let expanded;
    let line = match aliases.read().unwrap().expand(line) {
        Ok(None) => line,
        Ok(Some(text)) => {
            expanded = text;
            expanded.trim()
        }
        Err(e) => {
            note_error();
            if json_output() {
                println!("{}", error_json(&e));
            } else {
                println!("{}: {e}", style::error_prefix());
            }
            return false;
        }
    };
    let client = session.client();
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
            println!("  alias [<name>=<expansion>] | unalias <name>  command aliases");
            println!("  wire [on|off]                             trace raw JSON-RPC frames");
            println!("  last                                      reprint the previous exchange");
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
            if json_output() {
                let v = match cmd {
                    "tools" => serde_json::to_value(&s.tools),
                    "prompts" => serde_json::to_value(&s.prompts),
                    "resources" => serde_json::to_value(&s.resources),
                    _ => serde_json::to_value(&s.templates),
                }
                .unwrap_or_default();
                println!("{}", json_pretty(&v));
                return false;
            }
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
            let started = std::time::Instant::now();
            match with_reconnect(
                session,
                surface,
                |c| async move { c.read_resource(uri).await },
            )
            .await
            {
                Ok(result) if json_output() => {
                    println!(
                        "{}",
                        json_pretty(&serde_json::to_value(&result).unwrap_or_default())
                    );
                }
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
                Err(e) => {
                    note_error();
                    if json_output() {
                        println!("{}", error_json(&e.to_string()));
                    } else {
                        println!("{}: {e}", style::error_prefix());
                    }
                }
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
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
            let started = std::time::Instant::now();
            match with_reconnect(session, surface, |c| {
                let prompt_args = prompt_args.clone();
                async move { c.get_prompt(name, Some(prompt_args)).await }
            })
            .await
            {
                Ok(result) if json_output() => {
                    println!(
                        "{}",
                        json_pretty(&serde_json::to_value(&result).unwrap_or_default())
                    );
                }
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
                Err(e) => {
                    note_error();
                    if json_output() {
                        println!("{}", error_json(&e.to_string()));
                    } else {
                        println!("{}: {e}", style::error_prefix());
                    }
                }
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
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
                    note_error();
                    if json_output() {
                        println!("{}", error_json(&format!("invalid JSON: {e}")));
                    } else {
                        println!("invalid JSON: {e}");
                    }
                    return false;
                }
            };
            run_tool(session, surface, jobs, name, arguments, background).await;
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
        // Task commands do not reconnect: a task id belongs to the session
        // that created it, so a fresh session would only report it missing.
        // "(gone)" from `jobs` is the honest answer there.
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
                Ok(task) if json_output() => {
                    println!(
                        "{}",
                        json_pretty(&serde_json::to_value(&task).unwrap_or_default())
                    );
                }
                Ok(task) => render_task(&task),
                Err(e) => {
                    note_error();
                    if json_output() {
                        println!("{}", error_json(&e.to_string()));
                    } else {
                        println!("{}: {e}", style::error_prefix());
                    }
                }
            }
        }
        "alias" | "unalias" => {
            // Everything after the command word is taken raw: an expansion is
            // a command line, so its spacing and any `=` belong to it.
            let raw = line.strip_prefix(cmd).unwrap_or("").trim();
            handle_alias(aliases, surface, cmd, raw);
        }
        "wire" => match rest.first().copied() {
            Some("on") => {
                wire().set_trace(true);
                println!("wire tracing on (frames print to stderr)");
            }
            Some("off") => {
                wire().set_trace(false);
                println!("wire tracing off");
            }
            None => println!(
                "wire tracing is {}",
                if wire().trace_enabled() { "on" } else { "off" }
            ),
            Some(other) => println!("usage: wire [on|off] (got `{other}`)"),
        },
        // Deliberately independent of the trace toggle: frames are recorded
        // either way, so the exchange you did not think to trace is still there.
        "last" => match wire().last_exchange() {
            None => {
                if json_output() {
                    println!("{}", error_json("no exchange yet"));
                } else {
                    println!("no request has been sent yet");
                }
            }
            Some((request, response)) => {
                if json_output() {
                    println!(
                        "{}",
                        json_pretty(&serde_json::json!({
                            "request": request.json,
                            "response": response.map(|r| r.json),
                        }))
                    );
                } else {
                    println!("{}", wire::render(wire::Direction::Sent, &request));
                    match response {
                        Some(response) => {
                            println!("{}", wire::render(wire::Direction::Received, &response));
                        }
                        None => println!("(no response recorded for it)"),
                    }
                }
            }
        },
        "refresh" => {
            let fresh = refresh_surface(session).await;
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
                // What this client does with a request the server sends back.
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        &format!("sampling: {}", sampling::mode().as_str())
                    )
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
                note_error();
                if json_output() {
                    println!("{}", error_json(&format!("unknown command: {tool_name}")));
                } else {
                    println!(
                        "unknown command: {} (try `help`)",
                        paint(Style::new().fg(Color::Red), tool_name)
                    );
                }
                return false;
            };
            let arguments = parse_kv_args(&schema, rest);
            run_tool(session, surface, jobs, tool_name, arguments, background).await;
        }
    }
    false
}

/// The `alias` and `unalias` built-ins: define, list, show, and remove
/// command aliases, persisting each change to the config file.
///
/// `raw` is everything after the command word, unsplit: an expansion is a
/// command line of its own, so its spacing is part of it.
fn handle_alias(
    aliases: &Arc<RwLock<Aliases>>,
    surface: &Arc<RwLock<Surface>>,
    cmd: &str,
    raw: &str,
) {
    // A leading `--global` targets the file-level table. Only leading: a
    // trailing one would be ambiguous with an expansion that ends in a flag.
    let (global, rest) = match raw.strip_prefix("--global") {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim_start()),
        _ => (false, raw),
    };
    let rest = rest.trim();

    if cmd == "unalias" {
        if rest.is_empty() || rest.contains(char::is_whitespace) {
            println!("usage: unalias [--global] <name>");
            return;
        }
        match aliases.write().unwrap().remove(rest, global) {
            Ok(applied) => {
                report_alias_warning(applied.warning.as_deref());
                if json_output() {
                    println!(
                        "{}",
                        serde_json::json!({
                            "removed": rest,
                            "expansion": applied.previous,
                            "scope": applied.scope.label(),
                        })
                    );
                } else {
                    println!(
                        "removed {} {}",
                        paint(Style::new().fg(Color::Cyan), rest),
                        paint(
                            Style::new().dimmed(),
                            &format!("({})", applied.scope.label())
                        )
                    );
                }
            }
            Err(e) => alias_error(&e),
        }
        return;
    }

    // `alias` with nothing after it lists what is in effect.
    if rest.is_empty() {
        let aliases = aliases.read().unwrap();
        let entries = aliases.entries();
        if json_output() {
            let rendered: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "expansion": e.expansion,
                        "scope": e.scope.label(),
                    })
                })
                .collect();
            println!("{}", json_pretty(&serde_json::Value::Array(rendered)));
            return;
        }
        if entries.is_empty() {
            println!("no aliases defined (try `alias t=tools`)");
            return;
        }
        let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
        for e in &entries {
            println!(
                "{:width$}  {}  {}",
                paint(Style::new().fg(Color::Cyan), &e.name),
                e.expansion,
                paint(Style::new().dimmed(), &format!("({})", e.scope.label()))
            );
        }
        return;
    }

    // `alias <name>` shows one definition; `alias <name>=<expansion>` defines.
    let Some((name, expansion)) = rest.split_once('=') else {
        let aliases = aliases.read().unwrap();
        match aliases.lookup(rest) {
            Some((expansion, scope)) if json_output() => println!(
                "{}",
                serde_json::json!({
                    "name": rest,
                    "expansion": expansion,
                    "scope": scope.label(),
                })
            ),
            Some((expansion, scope)) => println!(
                "{} = {}  {}",
                paint(Style::new().fg(Color::Cyan), rest),
                expansion,
                paint(Style::new().dimmed(), &format!("({})", scope.label()))
            ),
            None => alias_error(&format!(
                "no alias named `{rest}` (define one with `alias {rest}=<expansion>`)"
            )),
        }
        return;
    };
    let name = name.trim();
    match aliases
        .write()
        .unwrap()
        .define(name, expansion.trim(), global)
    {
        Ok(applied) => {
            report_alias_warning(applied.warning.as_deref());
            if json_output() {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": name,
                        "expansion": expansion.trim(),
                        "scope": applied.scope.label(),
                        "replaced": applied.previous,
                    })
                );
                return;
            }
            println!(
                "{} = {}  {}",
                paint(Style::new().fg(Color::Cyan), name),
                expansion.trim(),
                paint(
                    Style::new().dimmed(),
                    &format!("({})", applied.scope.label())
                )
            );
            // An alias wins over a tool of the same name, since expansion
            // happens before dispatch. Worth saying once, at definition.
            if surface.read().unwrap().tools.iter().any(|t| t.name == name) {
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        &format!("note: this shadows the tool `{name}` on this server")
                    )
                );
            }
        }
        Err(e) => alias_error(&e),
    }
}

/// A failed write is reported without discarding the alias: it applies to
/// this session, it just did not reach the config file.
fn report_alias_warning(warning: Option<&str>) {
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
}

fn alias_error(message: &str) {
    note_error();
    if json_output() {
        println!("{}", error_json(message));
    } else {
        println!("{}: {message}", style::error_prefix());
    }
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
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    jobs: &mut Vec<(String, String)>,
    name: &str,
    arguments: serde_json::Value,
    background: bool,
) {
    if background {
        match with_reconnect(session, surface, |c| {
            let arguments = arguments.clone();
            async move { c.call_tool_as_task(name, arguments, None).await }
        })
        .await
        {
            Ok(created) => {
                if json_output() {
                    println!(
                        "{}",
                        json_pretty(&serde_json::to_value(&created).unwrap_or_default())
                    );
                } else {
                    println!(
                        "{} started",
                        tag(
                            Style::new().fg(Color::Yellow),
                            &format!("task {}", created.task.task_id)
                        )
                    );
                }
                jobs.push((created.task.task_id, name.to_string()));
            }
            Err(e) => {
                note_error();
                if json_output() {
                    println!("{}", error_json(&e.to_string()));
                } else {
                    println!("{}: {e}", style::error_prefix());
                }
            }
        }
        return;
    }
    let started = std::time::Instant::now();
    match with_reconnect(session, surface, |c| {
        let arguments = arguments.clone();
        async move { c.call_tool(name, arguments).await }
    })
    .await
    {
        Ok(result) => {
            if result.is_error {
                note_error();
            }
            if json_output() {
                println!(
                    "{}",
                    json_pretty(&serde_json::to_value(&result).unwrap_or_default())
                );
            } else {
                if result.is_error {
                    println!("{}", tag(Style::new().fg(Color::Red), "tool error"));
                }
                render_content(&result.content);
            }
        }
        Err(e) => {
            note_error();
            if json_output() {
                println!("{}", error_json(&e.to_string()));
            } else {
                println!("{}: {e}", style::error_prefix());
            }
        }
    }
    if !json_output() {
        println!("{}", timing(started.elapsed()));
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
    fn build_http_config_sets_bearer_and_trims_headers() {
        let cfg = build_http_config(
            Some("tok".into()),
            &["X-Api-Key: abc".into(), "X-Trim :  v ".into()],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("abc")
        );
        assert_eq!(cfg.headers.get("X-Trim").map(String::as_str), Some("v"));
    }

    #[test]
    fn profile_auth_applies_and_flags_override_it() {
        let profile_headers = [
            ("X-Api-Key".to_string(), "from-profile".to_string()),
            ("X-Kept".to_string(), "profile".to_string()),
        ];
        // No flags: the profile's token and headers are used as-is.
        let cfg =
            build_http_config(None, &[], Some("profile-tok".into()), &profile_headers).unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer profile-tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("from-profile")
        );

        // Flags win over the profile, header by header.
        let cfg = build_http_config(
            Some("flag-tok".into()),
            &["X-Api-Key: from-flag".into()],
            Some("profile-tok".into()),
            &profile_headers,
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer flag-tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("from-flag")
        );
        assert_eq!(
            cfg.headers.get("X-Kept").map(String::as_str),
            Some("profile")
        );
    }

    #[test]
    fn build_http_config_rejects_header_without_colon() {
        let err = build_http_config(Some("tok".into()), &["nope".into()], None, &[]).unwrap_err();
        assert!(
            err.contains("nope"),
            "error should name the bad header: {err}"
        );
        assert!(
            err.contains("Name: Value"),
            "error should show the format: {err}"
        );
    }

    #[test]
    fn timing_formats_sub_second_and_seconds() {
        assert!(timing(Duration::from_millis(142)).contains("[142ms]"));
        assert!(timing(Duration::from_millis(2500)).contains("[2.50s]"));
    }

    #[test]
    fn error_json_is_a_valid_object() {
        let v: serde_json::Value = serde_json::from_str(&error_json("boom: it broke")).unwrap();
        assert_eq!(v["error"], "boom: it broke");
    }

    // The persistent-history path relies on FileBackedHistory buffering saves
    // in memory and only writing on sync() (which sync_history() calls). The
    // REPL exits abruptly without dropping the editor, so it syncs after each
    // accepted line; this pins the assumption that save + sync reaches disk.
    #[test]
    fn file_backed_history_writes_on_sync() {
        use reedline::{FileBackedHistory, History, HistoryItem};
        let path = std::env::temp_dir().join(format!("mcp-repl-hist-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut h = FileBackedHistory::with_file(10, path.clone()).unwrap();
            h.save(HistoryItem::from_command_line("echo persisted"))
                .unwrap();
            h.sync().unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("echo persisted"),
            "history was not written to disk: {contents:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A connected, initialized client over the in-process demo router, so
    /// the reconnect path can be exercised without a socket.
    async fn demo_client() -> McpClient {
        let client = McpClient::builder()
            .connect_simple(ChannelTransport::new(demo_router()))
            .await
            .unwrap();
        client.initialize("mcp-repl-test", "0").await.unwrap();
        client
    }

    /// A session whose connector builds a fresh demo client, counting how
    /// many times it is asked to.
    async fn demo_session() -> (Arc<Session>, Arc<std::sync::atomic::AtomicUsize>) {
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connects.clone();
        let connector: Connector = Box::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(demo_client().await)
            })
        });
        (
            Arc::new(Session::new(demo_client().await, Some(connector))),
            connects,
        )
    }

    /// The regression this fixes: the server drops the session mid-command,
    /// so the call fails with not-initialized. The next attempt must succeed
    /// on a rebuilt session rather than leaving a dead prompt.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_session_is_rebuilt_and_the_command_retried() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dead = Arc::as_ptr(&session.client()) as usize;
        let seen: Arc<RwLock<Vec<usize>>> = Arc::new(RwLock::new(Vec::new()));

        let (calls, saw) = (attempts.clone(), seen.clone());
        let result = with_reconnect(&session, &surface, |c| {
            let (calls, saw) = (calls.clone(), saw.clone());
            async move {
                saw.write().unwrap().push(Arc::as_ptr(&c) as usize);
                // First attempt sees the session the server has forgotten.
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(jsonrpc(
                        -32600,
                        "Client must send notifications/initialized before making requests",
                    ));
                }
                c.call_tool("echo", serde_json::json!({ "message": "alive" }))
                    .await
            }
        })
        .await
        .expect("the retried call should succeed on the rebuilt session");

        assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry, not a loop");
        // The retry has to run against the rebuilt client, not the dead one.
        let seen = seen.read().unwrap();
        assert_eq!(seen[0], dead);
        assert_ne!(seen[1], dead, "the retry reused the dead client");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "reconnected exactly once"
        );
        assert_eq!(session.generation(), 1);
        match result.content.first() {
            Some(Content::Text { text, .. }) => assert_eq!(text, "alive"),
            other => panic!("unexpected content: {other:?}"),
        }
        // The surface is re-fetched from the new session, not left stale.
        assert!(
            !surface.read().unwrap().tools.is_empty(),
            "surface should be refreshed after reconnect"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_dead_server_surfaces_the_error_after_one_retry() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(tower_mcp::Error::Transport(
                    "HTTP 503 Service Unavailable from server: ".into(),
                ))
            }
        })
        .await
        .unwrap_err();

        assert!(is_session_lost(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "bounded to one retry");
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ordinary_errors_do_not_reconnect() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(jsonrpc(-32602, "Invalid params"))
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(err, tower_mcp::Error::JsonRpc(j) if j.code == -32602));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no retry");
        assert_eq!(connects.load(Ordering::SeqCst), 0, "no reconnect");
    }

    /// `--no-reconnect`, and the stdio/demo transports, produce a session with
    /// no connector: session-loss errors must pass straight through.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_without_a_connector_never_retries() {
        let session = Arc::new(Session::new(demo_client().await, None));
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        assert!(!session.can_reconnect());
        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(tower_mcp::Error::SessionExpired)
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(err, tower_mcp::Error::SessionExpired));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
