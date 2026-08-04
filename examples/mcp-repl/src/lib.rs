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
//! # Authorize once, then reuse a secure OAuth profile:
//! cargo run -p mcp-repl -- --login work --http https://mcp.example.com/mcp
//! cargo run -p mcp-repl -- --oauth work --http https://mcp.example.com/mcp
//!
//! # Connect to a named profile from ~/.config/mcp-repl/config.toml:
//! cargo run -p mcp-repl -- --server cratesio
//! ```
//!
//! Inside the REPL, `help` lists the built-ins and the server's tools,
//! `alias <name>=<expansion>` gives a frequent command a short name, kept in
//! the same config file as the server profiles, and `bench <tool>` reports the
//! latency distribution over repeated calls.
//! A trailing `&` runs a tool task-augmented (SEP-2663): the call returns a
//! task id immediately; `jobs`, `task <id>`, `wait <id>`, and `cancel <id>`
//! manage it.
//!
//! # Reusable seams
//!
//! The published package keeps its application in this library and its binary
//! target delegates to [`run_cli`]. [`config`], [`import_config`], and
//! [`oauth_profile`] are the deliberately reusable connection-facing seams.
//! Editor, rendering, and command-dispatch modules remain private application
//! details so consumers do not accidentally depend on terminal behavior.

mod alias;
mod bench;
mod command;
pub mod config;
mod editor;
mod elicit;
mod exit_status;
mod find;
pub mod import_config;
mod jobs;
pub mod oauth_profile;
mod output;
mod sampling;
mod schema_contract;
mod session;
mod style;
mod subscribe;
mod surface_subscription;
mod vars;
mod wire;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use nu_ansi_term::{Color, Style};

use tokio::io::{AsyncBufReadExt, BufReader};
use tower_mcp::client::{
    ChannelTransport, HttpClientConfig, HttpClientTransport, McpClient, McpClientBuilder,
    NotificationHandler, OAuthAuthorizationFlow, OAuthAuthorizationStart, OAuthClientError,
    OAuthScopeEscalationConfig, StdioClientTransport,
};
use tower_mcp::protocol::{
    Content, DiscoverResult, Implementation, InitializeResult, LogLevel, PromptDefinition,
    ResourceDefinition, ResourceTemplateDefinition, ServerCapabilities, SubscriptionFilter,
    TaskObject, ToolDefinition,
};
use tower_mcp::{ProtocolSupport, ProtocolSupportError};

use alias::Aliases;
use elicit::ReplClientHandler;
use exit_status::ExitStatus;
use jobs::Jobs;
use output::AsyncOutput;
use session::{Connector, Session, is_not_initialized, is_session_lost};
use style::{json_pretty, paint, tag, task_status_style};
use wire::{TracingTransport, wire};

/// Lifecycle selected for this REPL connection.
///
/// The final implementation is compiled into the binary, but stable remains
/// the runtime default so upgrading mcp-repl never silently changes a server's
/// handshake. `final` is accepted as a convenient alias for the dated value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ProtocolMode {
    #[default]
    Stable,
    #[value(name = "2026-07-28", alias = "final")]
    Final,
}

impl ProtocolMode {
    fn support(self) -> Result<ProtocolSupport, ProtocolSupportError> {
        match self {
            Self::Stable => Ok(ProtocolSupport::stable()),
            Self::Final => ProtocolSupport::try_new(["2026-07-28"]),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "mcp-repl",
    about = "Interactive MCP client REPL",
    trailing_var_arg = true
)]
struct Args {
    /// Protocol lifecycle to use. `stable` uses initialize/initialized;
    /// `2026-07-28` (alias: `final`) uses the sessionless discover lifecycle.
    #[arg(long, value_enum, default_value = "stable")]
    protocol: ProtocolMode,

    /// Connect to a streamable HTTP server at this URL instead of spawning
    /// a stdio child process.
    #[arg(long)]
    http: Option<String>,

    /// Serve the bundled demo router in-process (no external server needed).
    #[arg(long, conflicts_with_all = ["http", "command", "server"])]
    demo: bool,

    /// Connect using a native profile name, or import `PATH.json:ENTRY` from a
    /// standard MCP JSON config. Either selector also works as a lone
    /// positional.
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

    /// Use a named OAuth credential profile for this HTTP connection.
    #[arg(long, value_name = "NAME")]
    oauth: Option<String>,

    /// Authorize and securely save a named OAuth profile, then exit without
    /// opening an MCP session. Supply --http for a new profile.
    #[arg(long, value_name = "NAME", conflicts_with = "logout")]
    login: Option<String>,

    /// Remove a named OAuth profile and its credentials, then exit without
    /// opening an MCP session.
    #[arg(long, value_name = "NAME", conflicts_with = "login")]
    logout: Option<String>,

    /// Initial OAuth scope to request during --login (repeatable). Existing
    /// profile scopes are retained when this is omitted.
    #[arg(long = "oauth-scope", value_name = "SCOPE")]
    oauth_scopes: Vec<String>,

    /// HTTPS Client ID Metadata Document URL to try before Dynamic Client
    /// Registration during --login.
    #[arg(long, value_name = "URL")]
    oauth_client_id_metadata_document: Option<String>,

    /// Exact authorization-server issuer to select when discovery advertises
    /// more than one during --login.
    #[arg(long, value_name = "ISSUER")]
    oauth_authorization_server: Option<String>,

    /// Print the authorization URL instead of launching a browser. The
    /// loopback callback is still used.
    #[arg(long)]
    no_browser: bool,

    /// Run a command and exit instead of starting the interactive prompt.
    /// Repeatable; commands run in order against the same session, including
    /// after a failure. The final status is the most severe command outcome.
    /// Combine with --http/--demo or a stdio child.
    #[arg(short = 'e', long = "exec", value_name = "COMMAND")]
    exec: Vec<String>,

    /// In --exec mode, emit one compact JSON value per command (NDJSON), for
    /// piping to tools like jq.
    #[arg(long)]
    json: bool,

    /// In human --exec mode, still print the startup banner and surface
    /// listing. JSON stdout is always machine-only.
    #[arg(long)]
    verbose: bool,

    /// Validate matching tools and prompts before invocation using this saved
    /// schema snapshot (repeatable).
    #[arg(long = "schema-contract", value_name = "PATH")]
    schema_contracts: Vec<std::path::PathBuf>,

    /// Enforcement used by --schema-contract snapshots.
    #[arg(long, value_enum, default_value = "compatible")]
    schema_mode: schema_contract::ValidationMode,

    /// How to answer a server's `sampling/createMessage` request: `prompt`
    /// shows it and reads the assistant message on stdin, `canned` answers
    /// with a fixed placeholder, `decline` refuses. Defaults to `prompt`
    /// interactively and `decline` under --exec.
    #[arg(long, value_enum, value_name = "STRATEGY")]
    sampling: Option<sampling::SamplingMode>,

    /// Do not persist command history to ~/.mcp-repl_history.
    #[arg(long)]
    no_history: bool,

    /// Do not transparently re-establish an interrupted HTTP connection
    /// (restart, OOM, or a 502/503 from the edge in front of it).
    /// Connection-loss errors surface as-is instead.
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

fn json_output() -> bool {
    JSON_OUTPUT.load(Ordering::Relaxed)
}

fn note_error(status: ExitStatus) {
    exit_status::record(status);
}

fn automatic_task_updates(one_shot: bool, json: bool) -> bool {
    !one_shot && !json
}

/// Emit one compact JSON value. Repeated `--exec` commands are therefore
/// newline-delimited JSON (NDJSON), with one independently parseable line per
/// command.
fn print_json(value: &serde_json::Value) {
    println!("{value}");
}

/// A stable one-line JSON error object for `--json` mode.
fn error_json(status: ExitStatus, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": message,
        "kind": status.label(),
        "exitStatus": status.code(),
    })
}

fn report_error(status: ExitStatus, message: &str) {
    note_error(status);
    if json_output() {
        print_json(&error_json(status, message));
    } else {
        println!("{}: {message}", style::error_prefix());
    }
}

fn report_mcp_error(error: &tower_mcp::Error) {
    report_error(ExitStatus::from_mcp_error(error), &error.to_string());
}

fn exit_with_error(status: ExitStatus, message: &str) -> ! {
    if json_output() {
        print_json(&error_json(status, message));
    } else {
        eprintln!("error: {message}");
    }
    std::process::exit(status.code());
}

/// The server surface the REPL turns into commands. Refreshed on connect
/// and whenever a list_changed notification arrives.
#[derive(Default)]
pub(crate) struct Surface {
    pub tools: Vec<ToolDefinition>,
    pub prompts: Vec<PromptDefinition>,
    pub resources: Vec<ResourceDefinition>,
    pub templates: Vec<ResourceTemplateDefinition>,
}

/// Built-in commands with the short descriptions shown in the completion
/// menu and `help`.
pub(crate) const BUILTINS: &[(&str, &str)] = &[
    ("help", "list built-ins and the server's tools"),
    ("tools", "list tools"),
    ("prompts", "list prompts"),
    ("resources", "list resources"),
    ("templates", "list resource templates"),
    ("find", "search the surface by keyword"),
    ("describe", "show schemas and metadata for a name"),
    ("snapshot", "export a tool or prompt schema contract"),
    ("validate", "compare the surface with a schema snapshot"),
    ("read", "read a resource"),
    ("subscribe", "watch a resource for updates"),
    ("unsubscribe", "stop watching a resource"),
    ("subscriptions", "list active resource subscriptions"),
    ("prompt", "get a prompt"),
    ("call", "call a tool with raw JSON"),
    ("bench", "time repeated calls to a tool"),
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
    ("vars", "list captured variables"),
    ("unset", "clear a captured variable"),
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

/// Lifecycle-neutral connection details used by the banner and `info`.
#[derive(Clone, Debug)]
struct ConnectionInfo {
    protocol_version: String,
    capabilities: ServerCapabilities,
    server_info: Implementation,
    instructions: Option<String>,
}

impl From<InitializeResult> for ConnectionInfo {
    fn from(info: InitializeResult) -> Self {
        Self {
            protocol_version: info.protocol_version,
            capabilities: info.capabilities,
            server_info: info.server_info,
            instructions: info.instructions,
        }
    }
}

impl ConnectionInfo {
    fn from_discovery(discovery: DiscoverResult, protocol_version: String) -> Self {
        let server_info = discovery
            .meta
            .as_ref()
            .and_then(|meta| meta.server_info.clone())
            .unwrap_or_else(|| Implementation {
                name: "MCP server".to_string(),
                version: "unknown".to_string(),
                ..Default::default()
            });
        Self {
            protocol_version,
            capabilities: discovery.capabilities,
            server_info,
            instructions: discovery.instructions,
        }
    }
}

async fn connection_info(client: &McpClient) -> Option<ConnectionInfo> {
    if let Some(info) = client.server_info().await {
        return Some(info.into());
    }
    let discovery = client.discovery().await?;
    let protocol_version = client.selected_protocol_version().await?;
    Some(ConnectionInfo::from_discovery(discovery, protocol_version))
}

async fn establish_connection(
    client: &McpClient,
    protocol: ProtocolMode,
) -> tower_mcp::Result<ConnectionInfo> {
    match protocol {
        ProtocolMode::Stable => client
            .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
            .await
            .map(Into::into),
        ProtocolMode::Final => {
            let discovery: DiscoverResult = client
                .discover("mcp-repl", env!("CARGO_PKG_VERSION"))
                .await?;
            let protocol_version = client
                .selected_protocol_version()
                .await
                .unwrap_or_else(|| "2026-07-28".to_string());
            Ok(ConnectionInfo::from_discovery(discovery, protocol_version))
        }
    }
}

fn client_builder(protocol: ProtocolMode) -> Result<McpClientBuilder, ProtocolSupportError> {
    let builder = McpClient::builder()
        .protocol_support(protocol.support()?)
        .with_elicitation()
        .with_sampling();
    Ok(match protocol {
        ProtocolMode::Stable => builder,
        ProtocolMode::Final => builder.with_tasks(),
    })
}

/// The connection banner: server identity, negotiated protocol, and any
/// server instructions (markdown-rendered when it looks like markdown).
/// Printed at startup and replayed by the `info` command.
fn print_banner(info: &ConnectionInfo) {
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
pub(crate) fn timing(elapsed: Duration) -> String {
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

/// The `find` built-in's output: matches grouped by kind under the heading
/// of the list command that shows the same entries, best match first within
/// each group.
fn print_find(surface: &Surface, query: &str) {
    let hits = find::search(surface, query);
    if json_output() {
        let v: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "kind": h.kind.heading(),
                    "name": h.name,
                    "description": h.description,
                    "score": h.score,
                })
            })
            .collect();
        if v.is_empty() {
            note_error(ExitStatus::NoMatch);
        }
        print_json(&serde_json::Value::Array(v));
        return;
    }
    if hits.is_empty() {
        // grep's convention: a search that matched nothing exits non-zero, so
        // `mcp-repl -e "find x"` can be tested in a script.
        note_error(ExitStatus::NoMatch);
        println!("no match for {}", paint(Style::new().fg(Color::Red), query));
        return;
    }
    let total = hits.len();
    for (kind, group) in find::grouped(hits) {
        println!("{}:", paint(Style::new().bold(), kind.heading()));
        for hit in group {
            println!(
                "  {:24} {}",
                paint(Style::new().fg(Color::Green), &hit.name),
                hit.description
            );
        }
    }
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            &format!("{total} match{}", if total == 1 { "" } else { "es" })
        )
    );
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
    build_http_config_with_env(
        bearer,
        headers,
        profile_bearer,
        profile_headers,
        std::env::var("MCP_BEARER").ok(),
    )
}

fn build_http_config_with_env(
    bearer: Option<String>,
    headers: &[String],
    profile_bearer: Option<String>,
    profile_headers: &[(String, String)],
    env_bearer: Option<String>,
) -> Result<HttpClientConfig, String> {
    let mut config = HttpClientConfig::default();
    for (name, value) in profile_headers {
        config = config.header(name.as_str(), value.as_str());
    }
    let selected_has_authorization = profile_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if let Some(token) = bearer.or(profile_bearer).or_else(|| {
        (!selected_has_authorization)
            .then_some(env_bearer)
            .flatten()
    }) {
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

fn selected_oauth_profile(
    cli_oauth: Option<&str>,
    profile_oauth: Option<&str>,
    cli_bearer: bool,
    cli_headers: &[String],
) -> Option<String> {
    let explicit_authorization = cli_bearer
        || cli_headers.iter().any(|header| {
            header
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
        });
    (!explicit_authorization)
        .then(|| cli_oauth.or(profile_oauth).map(str::to_string))
        .flatten()
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
        .with_tasks()
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
        // A concrete resource, so `read`, `subscribe`, and `unsubscribe` all
        // have something to point at without an external server. Subscribing
        // needs a registered URI: the router rejects a subscription to
        // anything it does not serve.
        .resource(
            tower_mcp::resource::ResourceBuilder::new("note://status")
                .name("Status")
                .description("A one-line status note (subscribe to it)")
                .mime_type("text/plain")
                .handler(|| async {
                    Ok(ReadResourceResult::text(
                        "note://status",
                        "all quiet on the demo server",
                    ))
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
fn notification_handler(
    refresh_tx: tokio::sync::mpsc::UnboundedSender<()>,
    output: AsyncOutput,
    jobs: Arc<Jobs>,
) -> NotificationHandler {
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
        .on_task_status_changed({
            let jobs = jobs.clone();
            move |params| jobs.observe_legacy(params)
        })
        .on_final_task_status_changed(move |params| jobs.observe_final(params))
        .on_progress({
            let output = output.clone();
            move |p| {
                let pct = match (p.progress, p.total) {
                    (done, Some(total)) if total > 0.0 => {
                        format!(" {:.0}%", 100.0 * done / total)
                    }
                    _ => String::new(),
                };
                output.line(format!(
                    "{} {}",
                    tag(Style::new().fg(Color::Cyan), &format!("progress{pct}")),
                    p.message.as_deref().unwrap_or("")
                ));
            }
        })
        // A subscribed resource changed. Printed inline like progress and log
        // lines; the content is not re-read, since a `read` may be expensive and
        // the point is to know it moved.
        .on_resource_updated({
            let output = output.clone();
            move |uri| {
                let known = if subscribe::contains(&uri) {
                    String::new()
                } else {
                    format!(" {}", paint(Style::new().dimmed(), "(not subscribed here)"))
                };
                output.line(format!(
                    "{} {uri}{known}",
                    tag(Style::new().fg(Color::Cyan), "resource updated")
                ));
            }
        })
        .on_log_message(move |m| {
            output.line(format!(
                "{} {}",
                tag(log_level_style(m.level), &format!("log {}", m.level)),
                m.data
            ));
        })
}

/// Forward complete child stderr lines through the prompt-safe output sink.
fn forward_child_stderr(stderr: tokio::process::ChildStderr, output: AsyncOutput) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => output.line(line),
                Ok(None) => break,
                Err(error) => {
                    output.line(format!("warning: reading server stderr failed: {error}"));
                    break;
                }
            }
        }
    });
}

/// Watch one task until it reaches a terminal state. Final clients open a
/// task-scoped `subscriptions/listen` stream for immediate notifications; the
/// bounded polling loop remains authoritative for stable servers and for a
/// final notification that is unavailable or dropped.
fn watch_task(session: Arc<Session>, jobs: Arc<Jobs>, task_id: String, poll_interval: Option<u64>) {
    if !jobs.automatic_updates_enabled() || jobs.is_terminal(&task_id) {
        return;
    }
    tokio::spawn(async move {
        let client = session.client();
        let _subscription =
            if client.selected_protocol_version().await.as_deref() == Some("2026-07-28") {
                match client
                    .listen_subscriptions(SubscriptionFilter {
                        task_ids: Some(vec![task_id.clone()]),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(mut handle) => match handle.acknowledged().await {
                        Ok(accepted)
                            if accepted
                                .task_ids
                                .as_ref()
                                .is_some_and(|ids| ids.iter().any(|id| id == &task_id)) =>
                        {
                            Some(handle)
                        }
                        _ => None,
                    },
                    Err(_) => None,
                }
            } else {
                None
            };
        let mut interval_ms = poll_interval.unwrap_or(1000).clamp(50, 30_000);
        let mut consecutive_errors = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            if jobs.is_terminal(&task_id) {
                break;
            }
            match session.client().task_get(&task_id).await {
                Ok(task) => {
                    consecutive_errors = 0;
                    interval_ms = task.poll_interval.unwrap_or(1000).clamp(50, 30_000);
                    let terminal = task.status.is_terminal();
                    jobs.observe_task(&task);
                    if terminal {
                        break;
                    }
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        break;
                    }
                }
            }
        }
    });
}

/// The recipe for rebuilding an `--http` connection: a brand new transport
/// (so no dead `Mcp-Session-Id` is carried over), a fresh handler, and the
/// initialize handshake, exactly as at startup. The rebuilt transport is
/// wrapped in `TracingTransport` like the startup one, so `wire` and `last`
/// keep reporting frames after a reconnect, and it declares the same
/// capabilities as the startup client: a reconnect must not quietly leave the
/// session less capable than it began.
#[derive(Clone)]
struct OAuthRuntime {
    flow: OAuthAuthorizationFlow,
    scopes: Vec<String>,
}

fn http_transport(
    url: String,
    config: HttpClientConfig,
    oauth: Option<OAuthRuntime>,
) -> HttpClientTransport {
    let transport = HttpClientTransport::with_config(url, config);
    match oauth {
        Some(oauth) => transport.with_scope_aware_token_provider(
            oauth.flow,
            OAuthScopeEscalationConfig::new(oauth.scopes).max_attempts(2),
        ),
        None => transport,
    }
}

fn http_connector(
    url: String,
    config: HttpClientConfig,
    oauth: Option<OAuthRuntime>,
    make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync>,
    protocol: ProtocolMode,
) -> Connector {
    Box::new(move || {
        let (url, config, oauth, handler) =
            (url.clone(), config.clone(), oauth.clone(), make_handler());
        Box::pin(async move {
            let client = client_builder(protocol)
                .map_err(|error| tower_mcp::Error::Transport(error.to_string()))?
                .connect(
                    TracingTransport::new(http_transport(url, config, oauth)),
                    handler,
                )
                .await?;
            establish_connection(&client, protocol).await?;
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
            exit_with_error(ExitStatus::Usage, &e);
        }
    }
}

async fn handle_oauth_profile_action(
    args: &Args,
    profiles: &config::Config,
    config_file: Option<&std::path::Path>,
) -> bool {
    let Some(name) = args.login.as_deref().or(args.logout.as_deref()) else {
        if !args.oauth_scopes.is_empty()
            || args.oauth_client_id_metadata_document.is_some()
            || args.oauth_authorization_server.is_some()
        {
            exit_with_error(
                ExitStatus::Usage,
                "--oauth-scope, --oauth-client-id-metadata-document, and \
                 --oauth-authorization-server apply only to --login",
            );
        }
        return false;
    };
    oauth_profile::validate_name(name)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
    if args.demo
        || !args.command.is_empty()
        || !args.exec.is_empty()
        || args.json
        || args.list_servers
        || args.bearer.is_some()
        || !args.headers.is_empty()
        || args.oauth.is_some()
    {
        exit_with_error(
            ExitStatus::Usage,
            "--login/--logout are standalone credential operations; do not combine them with \
             a command, --demo, --exec/--json, --list-servers, --bearer, --header, or --oauth",
        );
    }
    let path = config_file.unwrap_or_else(|| {
        exit_with_error(
            ExitStatus::Usage,
            "no config file location is available; set HOME/XDG_CONFIG_HOME or pass --config",
        )
    });

    if args.logout.is_some() {
        let store = oauth_profile::CredentialStore::keyring(name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
        store
            .clear()
            .await
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
        oauth_profile::remove_metadata(path, name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
        println!("removed OAuth profile {name:?} and its stored credentials");
        return true;
    }

    let existing = profiles.oauth.get(name).cloned().unwrap_or_default();
    let server_url = args.server.as_deref().map(|server_name| {
        let profile = profiles
            .profile(server_name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
        match profile.transport() {
            Ok(config::Transport::Http) => profile
                .url
                .clone()
                .or_else(|| {
                    profile
                        .oauth
                        .as_deref()
                        .and_then(|oauth| profiles.oauth.get(oauth))
                        .map(|metadata| metadata.url.clone())
                })
                .unwrap_or_else(|| {
                    exit_with_error(
                        ExitStatus::Usage,
                        &format!("server profile {server_name:?} has no HTTP URL"),
                    )
                }),
            Ok(config::Transport::Stdio) => exit_with_error(
                ExitStatus::Usage,
                &format!("server profile {server_name:?} is stdio; OAuth requires HTTP"),
            ),
            Err(error) => exit_with_error(ExitStatus::Usage, &error),
        }
    });
    let url = args
        .http
        .clone()
        .or(server_url)
        .or_else(|| (!existing.url.is_empty()).then(|| existing.url.clone()))
        .unwrap_or_else(|| {
            exit_with_error(
                ExitStatus::Usage,
                "a new OAuth profile needs --http URL (or --server with an HTTP profile)",
            )
        });
    let scopes = if args.oauth_scopes.is_empty() {
        existing.scopes
    } else {
        args.oauth_scopes
            .iter()
            .flat_map(|scope| scope.split_ascii_whitespace())
            .map(str::to_string)
            .fold(Vec::new(), |mut scopes, scope| {
                if !scope.is_empty() && !scopes.contains(&scope) {
                    scopes.push(scope);
                }
                scopes
            })
    };
    let metadata = config::OAuthProfile {
        url: url.clone(),
        scopes,
        client_id_metadata_document: args
            .oauth_client_id_metadata_document
            .clone()
            .or(existing.client_id_metadata_document),
        authorization_server: args
            .oauth_authorization_server
            .clone()
            .or(existing.authorization_server),
    };
    let (flow, store) = oauth_profile::build_flow(name, &url, &metadata, true, !args.no_browser)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
    if let Err(error) = flow.authorize(metadata.scopes.clone()).await {
        if matches!(error, OAuthClientError::TokenRequest(_)) {
            store
                .clear_tokens()
                .await
                .unwrap_or_else(|store_error| exit_with_error(ExitStatus::Auth, &store_error));
            let (retry, _) =
                oauth_profile::build_flow(name, &url, &metadata, true, !args.no_browser)
                    .unwrap_or_else(|build_error| exit_with_error(ExitStatus::Auth, &build_error));
            retry
                .authorize(metadata.scopes.clone())
                .await
                .unwrap_or_else(|retry_error| {
                    exit_with_error(ExitStatus::Auth, &retry_error.to_string())
                });
        } else {
            exit_with_error(ExitStatus::Auth, &error.to_string());
        }
    }
    if let Err(error) = oauth_profile::save_metadata(path, name, &metadata) {
        let _ = store.clear().await;
        exit_with_error(ExitStatus::Usage, &error);
    }
    println!(
        "saved OAuth profile {name:?}; credentials are in the operating-system credential store"
    );
    true
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
            exit_with_error(ExitStatus::Usage, &e);
        }
    };
    if profile.bearer.is_some() {
        eprintln!(
            "warning: profile {name:?} stores a literal `bearer` token; prefer \
             `bearer_env = \"VAR\"` so the token is not kept in the config file"
        );
    }
    match config.resolve_profile_with(&name, |var| std::env::var(var).ok()) {
        Ok(connection) => Some((name, connection)),
        Err(e) => {
            exit_with_error(ExitStatus::Usage, &format!("server profile {name:?}: {e}"));
        }
    }
}

/// Resolve an explicit `PATH:ENTRY` selector from `--server` or a lone
/// positional. Imported JSON is intentionally opt-in; ordinary commands and
/// native profile names retain their existing interpretation.
fn resolve_import(args: &Args) -> Option<import_config::ImportedConnection> {
    let candidate = match args.server.as_deref() {
        Some(server) => server,
        None => match args.command.as_slice() {
            [only] => only,
            _ => return None,
        },
    };
    let selector = match import_config::parse_selector(candidate)? {
        Ok(selector) => selector,
        Err(error) => exit_with_error(ExitStatus::Usage, &error),
    };
    Some(
        import_config::load_with(selector, |variable| std::env::var(variable).ok())
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error)),
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

/// Parse the process arguments and run the published `mcp-repl` CLI.
///
/// The binary target intentionally delegates straight here so the application
/// lifecycle remains testable and the package can be extracted without moving
/// its implementation back into a monolithic executable.
#[tokio::main]
pub async fn run_cli() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();
    style::init(args.color);
    wire::init(args.trace);
    JSON_OUTPUT.store(args.json, Ordering::Relaxed);

    if let Err(error) = run(args).await {
        exit_with_error(ExitStatus::from_mcp_error(&error), &error.to_string());
    }
}

async fn run(args: Args) -> tower_mcp::Result<()> {
    // Server profiles are read up front: both --list-servers and profile
    // resolution need them before anything connects.
    let config_file = config::config_path(args.config.as_deref()).map(|(path, _)| path);
    let profiles = if args.login.is_some() || args.logout.is_some() {
        config_file
            .as_deref()
            .map(|path| {
                config::Config::load(path, false)
                    .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error))
            })
            .unwrap_or_default()
    } else {
        load_config(args.config.as_deref())
    };
    if handle_oauth_profile_action(&args, &profiles, config_file.as_deref()).await {
        return Ok(());
    }
    if args.list_servers {
        print_servers(&profiles);
        return Ok(());
    }
    let schema_contracts =
        schema_contract::ContractSet::load(&args.schema_contracts, args.schema_mode)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
    let imported = resolve_import(&args);
    let profile = if imported.is_none() {
        resolve_profile(&args, &profiles)
    } else {
        None
    };
    // --exec runs commands and exits; suppress the banner and surface listing
    // unless --verbose, so scripted output is only the command results.
    let one_shot = !args.exec.is_empty();
    // JSON stdout is a machine-readable stream. Even `--verbose` must not
    // inject a human banner into it.
    let quiet = one_shot && (!args.verbose || args.json);

    // True while the reedline editor owns the terminal; the elicitation
    // handler declines form requests during that window instead of
    // fighting over raw-mode stdin.
    let at_prompt = Arc::new(AtomicBool::new(false));
    let async_output = AsyncOutput::new(at_prompt.clone(), !one_shot);
    // Automatic task transitions are an interactive convenience. `--exec`
    // and `--json` retain deterministic output; their manual task commands
    // remain authoritative.
    let jobs = Arc::new(Jobs::new(
        async_output.clone(),
        automatic_task_updates(one_shot, args.json),
    ));

    // Notifications print inline and trigger surface refreshes.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // A reconnect needs a fresh handler for the new client, so build handlers
    // through a factory rather than once.
    let make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync> = {
        let refresh_tx = refresh_tx.clone();
        let at_prompt = at_prompt.clone();
        let async_output = async_output.clone();
        let jobs = jobs.clone();
        Arc::new(move || {
            ReplClientHandler::new(
                notification_handler(refresh_tx.clone(), async_output.clone(), jobs.clone()),
                at_prompt.clone(),
            )
        })
    };
    drop(refresh_tx);
    // Sampling has no model behind it, so the operator answers. Under --exec
    // there is nobody to ask, so requests are refused unless --sampling says
    // otherwise.
    sampling::init(sampling::resolve(args.sampling, one_shot));

    // Explicit flags override imported or native profile fields: --http
    // retargets the URL while keeping HTTP auth, and --bearer/--header are
    // layered on in build_http_config.
    let (profile_name, import_label, connection) = match (imported, profile) {
        (Some(imported), _) => (None, Some(imported.label()), Some(imported.connection)),
        (None, Some((name, connection))) => (Some(name), None, Some(connection)),
        (None, None) => (None, None, None),
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
                bearer,
                headers,
                oauth,
                ..
            }),
        ) => Some(config::Connection::Http {
            url,
            bearer,
            headers,
            oauth,
        }),
        (Some(url), _) => Some(config::Connection::Http {
            url,
            bearer: None,
            headers: Vec::new(),
            oauth: None,
        }),
        (None, Some(c)) => Some(c),
        (None, None) if args.command.is_empty() && args.oauth.is_some() => {
            let name = args.oauth.as_deref().expect("guarded above");
            let metadata = profiles.oauth.get(name).unwrap_or_else(|| {
                exit_with_error(
                    ExitStatus::Usage,
                    &format!("no OAuth profile named {name:?}; create it with --login"),
                )
            });
            Some(config::Connection::Http {
                url: metadata.url.clone(),
                bearer: None,
                headers: Vec::new(),
                oauth: Some(name.to_string()),
            })
        }
        (None, None) if !args.command.is_empty() => Some(config::Connection::Stdio {
            command: args.command.clone(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        }),
        (None, None) => None,
    };

    let over_http = matches!(connection, Some(config::Connection::Http { .. }));
    if !over_http && (args.bearer.is_some() || !args.headers.is_empty()) {
        eprintln!("warning: --bearer/--header apply only to HTTP servers; ignoring them here");
    }
    if !over_http && args.oauth.is_some() {
        exit_with_error(ExitStatus::Usage, "--oauth applies only to HTTP servers");
    }
    if let Some(name) = &profile_name
        && !quiet
    {
        println!(
            "{}",
            tag(Style::new().fg(Color::Cyan), &format!("profile {name}"))
        );
    } else if let Some(label) = &import_label
        && !quiet
    {
        println!(
            "{}",
            tag(Style::new().fg(Color::Cyan), &format!("import {label}"))
        );
    }

    // Every transport is wrapped, whatever `--trace` says: the wrapper is what
    // records the exchange `last` reprints, and tracing can be switched on
    // mid-session with `wire on`.
    // Sampling is advertised whatever the strategy: a client is allowed to
    // refuse an individual request, and a server can only ask when the
    // capability is declared, so `--sampling decline` still exercises the
    // server's rejection path.
    let builder = client_builder(args.protocol)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error.to_string()));
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
                oauth: profile_oauth,
            }) => {
                let oauth_name = selected_oauth_profile(
                    args.oauth.as_deref(),
                    profile_oauth.as_deref(),
                    args.bearer.is_some(),
                    &args.headers,
                );
                let cli_authorization = oauth_name.is_none()
                    && (args.bearer.is_some()
                        || args.headers.iter().any(|header| {
                            header.split_once(':').is_some_and(|(name, _)| {
                                name.trim().eq_ignore_ascii_case("authorization")
                            })
                        }));
                if cli_authorization && (args.oauth.is_some() || profile_oauth.is_some()) && !quiet
                {
                    eprintln!(
                        "warning: explicit --bearer/--header Authorization takes precedence over OAuth"
                    );
                }
                let profile_headers = if oauth_name.is_some() {
                    headers
                        .into_iter()
                        .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
                        .collect::<Vec<_>>()
                } else {
                    headers
                };
                let config = if oauth_name.is_some() {
                    build_http_config_with_env(
                        args.bearer.clone(),
                        &args.headers,
                        None,
                        &profile_headers,
                        None,
                    )
                } else {
                    build_http_config(args.bearer.clone(), &args.headers, bearer, &profile_headers)
                }
                .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
                let oauth = if let Some(name) = oauth_name {
                    let metadata = profiles.oauth.get(&name).unwrap_or_else(|| {
                        exit_with_error(
                            ExitStatus::Usage,
                            &format!(
                                "no OAuth profile named {name:?}; create it with \
                                 `mcp-repl --login {name} --http {url}`"
                            ),
                        )
                    });
                    let interactive = !one_shot && !args.json;
                    let (flow, store) = oauth_profile::build_flow(
                        &name,
                        &url,
                        metadata,
                        interactive,
                        interactive && !args.no_browser,
                    )
                    .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
                    if interactive {
                        flow.authorize(metadata.scopes.clone())
                            .await
                            .map_err(|error| {
                                tower_mcp::Error::Transport(format!(
                                    "OAuth authorization failed for profile {name:?}: {error}. \
                                     Run `mcp-repl --login {name} --http {url}` to reauthorize"
                                ))
                            })?;
                    } else {
                        if !store.has_tokens().await.map_err(|error| {
                            tower_mcp::Error::Transport(format!(
                                "OAuth credential restore failed for profile {name:?}: {error}"
                            ))
                        })? {
                            return Err(tower_mcp::Error::Transport(format!(
                                "OAuth login required for profile {name:?}; run \
                                 `mcp-repl --login {name} --http {url}` before using --exec/--json"
                            )));
                        }
                        match flow.begin(metadata.scopes.clone()).await.map_err(|error| {
                            tower_mcp::Error::Transport(format!(
                                "OAuth credential restore failed for profile {name:?}: {error}. \
                                 Run `mcp-repl --login {name} --http {url}` to reauthorize"
                            ))
                        })? {
                            OAuthAuthorizationStart::Authorized { .. } => {}
                            OAuthAuthorizationStart::Pending(_) => {
                                return Err(tower_mcp::Error::Transport(format!(
                                    "OAuth login required for profile {name:?}; run \
                                     `mcp-repl --login {name} --http {url}` before using --exec/--json"
                                )));
                            }
                            _ => {
                                return Err(tower_mcp::Error::Transport(format!(
                                    "OAuth login required for profile {name:?}; run \
                                     `mcp-repl --login {name} --http {url}` before using --exec/--json"
                                )));
                            }
                        }
                    }
                    Some(OAuthRuntime {
                        flow,
                        scopes: metadata.scopes.clone(),
                    })
                } else {
                    None
                };
                if !args.no_reconnect {
                    connector = Some(http_connector(
                        url.clone(),
                        config.clone(),
                        oauth.clone(),
                        make_handler.clone(),
                        args.protocol,
                    ));
                }
                builder
                    .connect(
                        TracingTransport::new(http_transport(url, config, oauth)),
                        make_handler(),
                    )
                    .await?
            }
            Some(config::Connection::Stdio { command, env, cwd }) => {
                let mut cmd = tokio::process::Command::new(&command[0]);
                cmd.args(&command[1..]);
                cmd.envs(env);
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                cmd.stderr(std::process::Stdio::piped());
                let mut transport = StdioClientTransport::spawn_command(&mut cmd).await?;
                if let Some(stderr) = transport.take_stderr() {
                    forward_child_stderr(stderr, async_output.clone());
                }
                builder
                    .connect(TracingTransport::new(transport), make_handler())
                    .await?
            }
            None => {
                exit_with_error(
                    ExitStatus::Usage,
                    "usage: mcp-repl <server command...> | --http <url> | \
                     --server <name> | --demo",
                );
            }
        }
    };

    let info = establish_connection(&client, args.protocol).await?;
    let server_name = info.server_info.name.clone();
    if !quiet {
        print_banner(&info);
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
        let instructions_list_tools = info
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
        for cmd in &args.exec {
            if handle_line(
                &session,
                &surface,
                &aliases,
                &jobs,
                &schema_contracts,
                cmd.trim(),
            )
            .await
            {
                break;
            }
        }
        let status = exit_status::current().code();
        drop(client);
        let session = Arc::try_unwrap(session).unwrap_or_else(|_| {
            panic!("one-shot MCP session is still shared after all commands completed")
        });
        session.shutdown().await?;
        std::process::exit(status);
    }

    // Final list-change notifications are subscription-scoped. Start the
    // long-lived stream only for an interactive final connection, after the
    // initial surface fetch; stable notifications already arrive directly,
    // and one-shot output must remain deterministic.
    let _surface_subscription = (args.protocol == ProtocolMode::Final).then(|| {
        surface_subscription::SurfaceSubscription::start(session.clone(), async_output.clone())
    });

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
        async_output
            .external_printer()
            .expect("interactive sessions have an external printer"),
        !args.no_history,
    );

    loop {
        tokio::select! {
            Some(()) = refresh_rx.recv() => {
                let fresh = fetch_surface(&session.client()).await;
                async_output.line(format!("{} {} tools, {} prompts, {} resources",
                    tag(Style::new().fg(Color::Cyan), "surface changed"),
                    fresh.tools.len(), fresh.prompts.len(), fresh.resources.len()));
                *surface.write().unwrap() = fresh;
            }
            maybe_line = line_rx.recv() => {
                let Some(line) = maybe_line else { break };
                let quit = handle_line(
                    &session,
                    &surface,
                    &aliases,
                    &jobs,
                    &schema_contracts,
                    line.trim(),
                )
                .await;
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
    jobs: &Arc<Jobs>,
    schema_contracts: &schema_contract::ContractSet,
    line: &str,
) -> bool {
    if line.is_empty() {
        if json_output() {
            report_error(ExitStatus::Usage, "empty command");
        }
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
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    // Capture (`name = cmd`), pipe (`cmd | path`), and `$var` references make
    // the REPL a small shell: routing and substitution run before dispatch so
    // every command sees resolved arguments. Capture and pipe act on tool
    // results.
    let (output, routed) = vars::route(line);
    let command = match vars::substitute(routed) {
        Ok(c) => c,
        Err(e) => {
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    let line = command.as_str();
    let client = session.client();
    let parsed = match command::parse(line) {
        Ok(parsed) => parsed,
        Err(e) => {
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    let background = parsed.background;
    let tokens: Vec<&str> = parsed.words.iter().map(String::as_str).collect();
    if tokens.is_empty() {
        if json_output() {
            report_error(ExitStatus::Usage, "empty command");
        }
        return false;
    }
    let cmd = tokens[0];
    let rest = &tokens[1..];

    match cmd {
        "quit" | "exit" => {
            if json_output() {
                print_json(&serde_json::json!({ "exit": true }));
            }
            return true;
        }
        "help" => {
            if json_output() {
                let s = surface.read().unwrap();
                print_json(&serde_json::json!({
                    "builtins": BUILTINS
                        .iter()
                        .map(|(name, description)| serde_json::json!({
                            "name": name,
                            "description": description,
                        }))
                        .collect::<Vec<_>>(),
                    "tools": s.tools,
                }));
                return false;
            }
            println!("built-ins:");
            println!("  tools | prompts | resources | templates   list the server surface");
            println!("  find <keyword>                            search the surface");
            println!("  describe <name>                           schemas and metadata");
            println!("  snapshot <name> [path]                    export a schema contract");
            println!("  validate <path> [mode]                    check a schema contract");
            println!("  read <uri>                                read a resource");
            println!("  subscribe <uri> | unsubscribe <uri>       watch a resource for updates");
            println!("  subscriptions                             list active subscriptions");
            println!("  prompt <name> [k=v...]                    get a prompt");
            println!("  call <tool> <json>                        call a tool with raw JSON");
            println!("  bench <tool> [k=v...] [--n N] [--concurrency C]  time repeated calls");
            println!("  <tool> [k=v...]                           call a tool (schema-coerced)");
            println!("  <tool> [k=v...] &                         run task-augmented (SEP-2663)");
            println!("  jobs | task <id> | wait <id> | cancel <id>  manage tasks");
            println!("  alias [<name>=<expansion>] | unalias <name>  command aliases");
            println!("  wire [on|off]                             trace raw JSON-RPC frames");
            println!("  last                                      reprint the previous exchange");
            println!(
                "  vars | unset <name>                       list or clear captured variables"
            );
            println!(
                "  name = <cmd> [| <path>]                   capture a result (filter with | path)"
            );
            println!("  $name.path in args                        reference a captured value");
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
                print_json(&v);
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
        "find" => {
            // Everything after the command word is the query, so a phrase
            // (`find crate info`) is not silently truncated to its first word.
            let query = rest.join(" ");
            if query.is_empty() {
                command_error("usage: find <keyword>");
                return false;
            }
            print_find(&surface.read().unwrap(), &query);
        }
        "describe" => {
            let Some(name) = rest.first() else {
                command_error("usage: describe <tool|prompt|resource|template>");
                return false;
            };
            let surface = surface.read().unwrap();
            if json_output() {
                match describe_value(&surface, name) {
                    Some(value) => print_json(&value),
                    None => report_error(
                        ExitStatus::NoMatch,
                        &format!("nothing on the surface named `{name}`"),
                    ),
                }
            } else {
                describe(&surface, name);
            }
        }
        "snapshot" => {
            let Some(name) = rest.first() else {
                command_error("usage: snapshot <tool|prompt> [path]");
                return false;
            };
            if rest.len() > 2 {
                command_error("usage: snapshot <tool|prompt> [path]");
                return false;
            }
            let snapshot = {
                let surface = surface.read().unwrap();
                schema_contract::Snapshot::from_surface(&surface.tools, &surface.prompts, name)
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report_error(ExitStatus::Usage, &error);
                    return false;
                }
            };
            let Some(snapshot) = snapshot else {
                report_error(
                    ExitStatus::NoMatch,
                    &format!("no tool or prompt named `{name}`"),
                );
                return false;
            };
            if let Some(path) = rest.get(1) {
                let path = std::path::Path::new(path);
                match snapshot.write(path) {
                    Ok(()) if json_output() => print_json(&serde_json::json!({
                        "kind": snapshot.kind,
                        "name": snapshot.name,
                        "path": path,
                    })),
                    Ok(()) => println!(
                        "saved {} {:?} schema snapshot to {}",
                        snapshot.kind,
                        snapshot.name,
                        path.display()
                    ),
                    Err(error) => report_error(ExitStatus::Usage, &error),
                }
            } else if json_output() {
                print_json(&snapshot.canonical_value());
            } else {
                print!("{}", snapshot.to_pretty_json());
            }
        }
        "validate" => {
            let Some(path) = rest.first() else {
                command_error("usage: validate <snapshot-path> [strict|compatible|ignore]");
                return false;
            };
            if rest.len() > 2 {
                command_error("usage: validate <snapshot-path> [strict|compatible|ignore]");
                return false;
            }
            let mode = match rest.get(1) {
                Some(mode) => match schema_contract::ValidationMode::from_str(mode, true) {
                    Ok(mode) => mode,
                    Err(_) => {
                        command_error(
                            "validation mode must be `strict`, `compatible`, or `ignore`",
                        );
                        return false;
                    }
                },
                None => schema_contracts.mode(),
            };
            let snapshot = match schema_contract::Snapshot::load(std::path::Path::new(path)) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report_error(ExitStatus::Usage, &error);
                    return false;
                }
            };
            let current = {
                let surface = surface.read().unwrap();
                snapshot.matching_surface(&surface.tools, &surface.prompts)
            };
            let report = schema_contract::validate(&snapshot, current.as_ref(), mode);
            render_validation_report(&report, true);
        }
        "read" => {
            let Some(uri) = rest.first() else {
                command_error("usage: read <uri>");
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
                    print_json(&serde_json::to_value(&result).unwrap_or_default())
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
                Err(e) => report_mcp_error(&e),
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        "subscribe" | "unsubscribe" => {
            let Some(uri) = rest.first() else {
                command_error(&format!("usage: {cmd} <uri>"));
                return false;
            };
            handle_subscription(&client, cmd, uri).await;
        }
        "subscriptions" => {
            let active = subscribe::list();
            if json_output() {
                print_json(&serde_json::json!(active));
                return false;
            }
            if active.is_empty() {
                println!("no active subscriptions (try `subscribe <uri>`)");
                return false;
            }
            for uri in &active {
                println!("{}", paint(Style::new().fg(Color::Green), uri));
            }
        }
        "prompt" => {
            let Some(name) = rest.first() else {
                command_error("usage: prompt <name> [k=v...]");
                return false;
            };
            if !enforce_prompt_contract(schema_contracts, surface, name) {
                return false;
            }
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
                    print_json(&serde_json::to_value(&result).unwrap_or_default())
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
                Err(e) => report_mcp_error(&e),
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        "call" => {
            let Some(name) = rest.first() else {
                command_error("usage: call <tool> <json>");
                return false;
            };
            let json = rest[1..].join(" ");
            let arguments: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    report_error(ExitStatus::Usage, &format!("invalid JSON: {e}"));
                    return false;
                }
            };
            run_tool(
                session,
                surface,
                jobs,
                schema_contracts,
                name,
                arguments,
                background,
                &output,
            )
            .await;
        }
        "bench" => {
            handle_bench(&client, surface, schema_contracts, rest, background).await;
        }
        "jobs" => {
            if json_output() {
                let mut rendered = Vec::new();
                for job in jobs.list() {
                    match client.task_get(&job.task_id).await {
                        Ok(task) => {
                            jobs.sync(&job.task_id, task.status, task.status_message.clone());
                            rendered.push(serde_json::json!({
                                "taskId": job.task_id,
                                "tool": job.tool,
                                "task": task,
                            }));
                        }
                        Err(error) => {
                            let status = ExitStatus::from_mcp_error(&error);
                            note_error(status);
                            rendered.push(serde_json::json!({
                                "taskId": job.task_id,
                                "tool": job.tool,
                                "error": error.to_string(),
                                "kind": status.label(),
                                "exitStatus": status.code(),
                            }));
                        }
                    }
                }
                print_json(&serde_json::Value::Array(rendered));
                return false;
            }
            if jobs.is_empty() {
                println!("no background tasks");
            }
            for job in jobs.list() {
                match client.task_get(&job.task_id).await {
                    Ok(task) => {
                        jobs.sync(&job.task_id, task.status, task.status_message.clone());
                        println!(
                            "{}  {}  {}",
                            job.task_id,
                            job.tool,
                            paint(task_status_style(task.status), &task.status.to_string())
                        );
                    }
                    Err(error) => {
                        note_error(ExitStatus::from_mcp_error(&error));
                        println!("{}  {}  (gone)", job.task_id, job.tool);
                    }
                }
            }
        }
        // Task commands do not reconnect: a task id belongs to the session
        // that created it, so a fresh session would only report it missing.
        // "(gone)" from `jobs` is the honest answer there.
        "task" | "wait" | "cancel" => {
            let Some(id) = rest.first() else {
                command_error(&format!("usage: {cmd} <task-id>"));
                return false;
            };
            let outcome = match cmd {
                "task" => client.task_get(id).await,
                "wait" => client.task_wait(id).await,
                _ => match client.task_cancel(id, None).await {
                    Ok(()) => {
                        if !json_output() {
                            println!("cancel acknowledged");
                        }
                        client.task_get(id).await
                    }
                    Err(e) => Err(e),
                },
            };
            match outcome {
                Ok(task) if json_output() => {
                    jobs.sync(id, task.status, task.status_message.clone());
                    print_json(&serde_json::to_value(&task).unwrap_or_default());
                }
                Ok(task) => {
                    jobs.sync(id, task.status, task.status_message.clone());
                    render_task(&task);
                }
                Err(e) => report_mcp_error(&e),
            }
        }
        "alias" | "unalias" => {
            // Everything after the command word is taken raw: an expansion is
            // a command line, so its spacing and any `=` belong to it.
            let raw = line.strip_prefix(cmd).unwrap_or("").trim();
            handle_alias(aliases, surface, cmd, raw);
        }
        "wire" => {
            match rest.first().copied() {
                Some("on") => wire().set_trace(true),
                Some("off") => wire().set_trace(false),
                None => {}
                Some(other) => {
                    command_error(&format!("usage: wire [on|off] (got `{other}`)"));
                    return false;
                }
            }
            let enabled = wire().trace_enabled();
            if json_output() {
                print_json(&serde_json::json!({ "wire": enabled }));
            } else if enabled {
                println!("wire tracing on (frames print to stderr)");
            } else {
                println!("wire tracing off");
            }
        }
        // Deliberately independent of the trace toggle: frames are recorded
        // either way, so the exchange you did not think to trace is still there.
        "last" => match wire().last_exchange() {
            None => {
                note_error(ExitStatus::NoMatch);
                if json_output() {
                    print_json(&error_json(ExitStatus::NoMatch, "no exchange yet"));
                } else {
                    println!("no request has been sent yet");
                }
            }
            Some((request, response)) => {
                if json_output() {
                    print_json(&serde_json::json!({
                        "request": request.json,
                        "response": response.map(|r| r.json),
                    }));
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
            if json_output() {
                print_json(&serde_json::json!({
                    "tools": fresh.tools.len(),
                    "prompts": fresh.prompts.len(),
                    "resources": fresh.resources.len(),
                    "templates": fresh.templates.len(),
                }));
            } else {
                println!(
                    "{} tools, {} prompts, {} resources, {} templates",
                    fresh.tools.len(),
                    fresh.prompts.len(),
                    fresh.resources.len(),
                    fresh.templates.len()
                );
            }
            *surface.write().unwrap() = fresh;
        }
        "info" => match connection_info(&client).await {
            Some(info) => {
                if json_output() {
                    print_json(&serde_json::json!({
                        "protocolVersion": info.protocol_version,
                        "serverInfo": info.server_info,
                        "capabilities": info.capabilities,
                        "instructions": info.instructions,
                        "sampling": sampling::mode().as_str(),
                    }));
                    return false;
                }
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
            None => report_error(ExitStatus::Transport, "not initialized"),
        },
        "vars" => {
            let all = vars::list();
            if json_output() {
                let map: serde_json::Map<String, serde_json::Value> = all.into_iter().collect();
                print_json(&serde_json::Value::Object(map));
            } else if all.is_empty() {
                println!("{}", paint(Style::new().dimmed(), "no variables"));
            } else {
                for (name, value) in all {
                    println!(
                        "{} {}",
                        paint(Style::new().fg(Color::Cyan), &format!("${name} =")),
                        value_summary(&value)
                    );
                }
            }
        }
        "unset" => match rest.first() {
            Some(name) => {
                if vars::unset(name) {
                    if json_output() {
                        print_json(&serde_json::json!({ "unset": name }));
                    } else {
                        println!("unset ${name}");
                    }
                } else {
                    command_error(&format!("no such variable `${name}`"));
                }
            }
            None => command_error("usage: unset <name>"),
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
                note_error(ExitStatus::Usage);
                let suggestion = find::did_you_mean(&surface.read().unwrap(), tool_name);
                if json_output() {
                    let mut value =
                        error_json(ExitStatus::Usage, &format!("unknown command: {tool_name}"));
                    if let Some(near) = &suggestion {
                        value["didYouMean"] = serde_json::json!(near);
                    }
                    print_json(&value);
                } else {
                    let name = paint(Style::new().fg(Color::Red), tool_name);
                    match suggestion {
                        Some(near) => println!(
                            "unknown command: {name}; did you mean `{}`?",
                            paint(Style::new().fg(Color::Green), &near)
                        ),
                        None => println!("unknown command: {name} (try `help`)"),
                    }
                }
                return false;
            };
            let arguments = parse_kv_args(&schema, rest);
            run_tool(
                session,
                surface,
                jobs,
                schema_contracts,
                tool_name,
                arguments,
                background,
                &output,
            )
            .await;
        }
    }
    false
}

/// The `bench` built-in: issue one tool call repeatedly and report how long
/// the calls took. Arguments are coerced against the tool's `inputSchema`,
/// exactly as a direct call is, so `bench <tool> a=1` benchmarks the same
/// request `<tool> a=1` would send.
async fn handle_bench(
    client: &Arc<McpClient>,
    surface: &Arc<RwLock<Surface>>,
    schema_contracts: &schema_contract::ContractSet,
    rest: &[&str],
    background: bool,
) {
    // A trailing `&` is stripped before dispatch, so say why it did nothing
    // rather than silently benchmarking the non-task path.
    if background {
        command_error("bench cannot run task-augmented; drop the trailing `&`");
        return;
    }
    let plan = match bench::parse(rest) {
        Ok(plan) => plan,
        Err(e) => {
            command_error(&e);
            return;
        }
    };
    let schema = {
        let s = surface.read().unwrap();
        s.tools
            .iter()
            .find(|t| t.name == plan.tool)
            .map(|t| t.input_schema.clone())
    };
    let Some(schema) = schema else {
        command_error(&format!("no tool named `{}` (try `tools`)", plan.tool));
        return;
    };
    if !enforce_tool_contract(schema_contracts, surface, &plan.tool) {
        return;
    }
    let arg_tokens: Vec<&str> = plan.args.iter().map(String::as_str).collect();
    let arguments = parse_kv_args(&schema, &arg_tokens);

    let outcome = bench::run(client, &plan.tool, arguments, plan.n, plan.concurrency).await;
    // A run with failures in it exits non-zero, like any other failing
    // command, so `-e "bench ..."` works as a health check.
    if outcome.errors > 0 {
        note_error(ExitStatus::Server);
    }
    if json_output() {
        print_json(&bench::render_json(&plan, &outcome));
        return;
    }
    println!("{}", bench::render(&plan, &outcome));
    if let Some(message) = &outcome.first_error {
        println!(
            "{} {}",
            tag(Style::new().fg(Color::Red), "first error"),
            message
        );
    }
    println!("{}", timing(outcome.total));
}

/// The `subscribe` and `unsubscribe` built-ins. The local set is only updated
/// once the server has agreed, so `subscriptions` lists what the server is
/// actually sending updates for, not what was asked for.
async fn handle_subscription(client: &Arc<McpClient>, cmd: &str, uri: &str) {
    // A server that does not advertise the capability will reject the call.
    // Saying so first turns a bare protocol error into an explanation.
    if cmd == "subscribe"
        && let Some(info) = connection_info(client).await
        && !subscribe::server_supports(
            &serde_json::to_value(&info.capabilities).unwrap_or_default(),
        )
    {
        eprintln!(
            "warning: {} does not advertise resources.subscribe; the request will \
             probably be rejected",
            info.server_info.name
        );
    }
    let started = std::time::Instant::now();
    let outcome = if cmd == "subscribe" {
        client.subscribe_resource(uri).await
    } else {
        client.unsubscribe_resource(uri).await
    };
    match outcome {
        Ok(()) => {
            let changed = if cmd == "subscribe" {
                subscribe::add(uri)
            } else {
                subscribe::remove(uri)
            };
            if json_output() {
                print_json(&serde_json::json!({
                    cmd: uri,
                    "alreadyInEffect": !changed,
                }));
            } else {
                let note = if changed {
                    String::new()
                } else {
                    format!(" {}", paint(Style::new().dimmed(), "(already in effect)"))
                };
                println!("{cmd}d {}{note}", paint(Style::new().fg(Color::Green), uri));
            }
        }
        Err(e) => report_mcp_error(&e),
    }
    if !json_output() {
        println!("{}", timing(started.elapsed()));
    }
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
            command_error("usage: unalias [--global] <name>");
            return;
        }
        match aliases.write().unwrap().remove(rest, global) {
            Ok(applied) => {
                report_alias_warning(applied.warning.as_deref());
                if json_output() {
                    print_json(&serde_json::json!({
                        "removed": rest,
                        "expansion": applied.previous,
                        "scope": applied.scope.label(),
                    }));
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
            Err(e) => command_error(&e),
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
            print_json(&serde_json::Value::Array(rendered));
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
            Some((expansion, scope)) if json_output() => print_json(&serde_json::json!({
                "name": rest,
                "expansion": expansion,
                "scope": scope.label(),
            })),
            Some((expansion, scope)) => println!(
                "{} = {}  {}",
                paint(Style::new().fg(Color::Cyan), rest),
                expansion,
                paint(Style::new().dimmed(), &format!("({})", scope.label()))
            ),
            None => command_error(&format!(
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
                print_json(&serde_json::json!({
                    "name": name,
                    "expansion": expansion.trim(),
                    "scope": applied.scope.label(),
                    "replaced": applied.previous,
                }));
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
        Err(e) => command_error(&e),
    }
}

/// A failed write is reported without discarding the alias: it applies to
/// this session, it just did not reach the config file.
fn report_alias_warning(warning: Option<&str>) {
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
}

fn command_error(message: &str) {
    report_error(ExitStatus::Usage, message);
}

/// Render a compatibility report. Pre-invocation checks stay silent on
/// success so one JSON command still emits exactly one JSON value.
fn render_validation_report(
    report: &schema_contract::ValidationReport,
    render_success: bool,
) -> bool {
    if report.compatible && !render_success {
        return true;
    }
    if !report.compatible {
        note_error(ExitStatus::NoMatch);
    }
    if json_output() {
        print_json(&serde_json::to_value(report).unwrap_or_default());
    } else if report.compatible {
        println!(
            "{} {:?} is compatible under {} validation",
            report.kind, report.name, report.mode
        );
    } else {
        println!(
            "{} {:?} is incompatible under {} validation:",
            report.kind, report.name, report.mode
        );
        for issue in &report.issues {
            println!("  {} [{}] {}", issue.path, issue.code, issue.message);
        }
    }
    report.compatible
}

fn enforce_tool_contract(
    contracts: &schema_contract::ContractSet,
    surface: &Arc<RwLock<Surface>>,
    name: &str,
) -> bool {
    let report = {
        let surface = surface.read().unwrap();
        surface
            .tools
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| contracts.check_tool(definition))
    };
    report
        .as_ref()
        .is_none_or(|report| render_validation_report(report, false))
}

fn enforce_prompt_contract(
    contracts: &schema_contract::ContractSet,
    surface: &Arc<RwLock<Surface>>,
    name: &str,
) -> bool {
    let report = {
        let surface = surface.read().unwrap();
        surface
            .prompts
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| contracts.check_prompt(definition))
    };
    report
        .as_ref()
        .is_none_or(|report| render_validation_report(report, false))
}

fn describe_value(surface: &Surface, name: &str) -> Option<serde_json::Value> {
    surface
        .tools
        .iter()
        .find(|definition| definition.name == name)
        .map(|definition| {
            serde_json::json!({
                "kind": "tool",
                "definition": definition,
            })
        })
        .or_else(|| {
            surface
                .prompts
                .iter()
                .find(|definition| definition.name == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "prompt",
                        "definition": definition,
                    })
                })
        })
        .or_else(|| {
            surface
                .resources
                .iter()
                .find(|definition| definition.name == name || definition.uri == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "resource",
                        "definition": definition,
                    })
                })
        })
        .or_else(|| {
            surface
                .templates
                .iter()
                .find(|definition| definition.name == name || definition.uri_template == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "resourceTemplate",
                        "definition": definition,
                    })
                })
        })
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
    note_error(ExitStatus::NoMatch);
    println!("nothing on the surface named `{name}` (try `tools`, `prompts`, `resources`)");
}

#[allow(clippy::too_many_arguments)]
async fn run_tool(
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    jobs: &Arc<Jobs>,
    schema_contracts: &schema_contract::ContractSet,
    name: &str,
    arguments: serde_json::Value,
    background: bool,
    output: &vars::Output,
) {
    if !enforce_tool_contract(schema_contracts, surface, name) {
        return;
    }
    if background {
        match with_reconnect(session, surface, |c| {
            let arguments = arguments.clone();
            async move { c.call_tool_as_task(name, arguments, None).await }
        })
        .await
        {
            Ok(created) => {
                let task_id = created.task.task_id.clone();
                let poll_interval = created.task.poll_interval;
                if json_output() {
                    print_json(&serde_json::to_value(&created).unwrap_or_default());
                } else {
                    println!(
                        "{} started",
                        tag(
                            Style::new().fg(Color::Yellow),
                            &format!("task {}", created.task.task_id)
                        )
                    );
                }
                jobs.register(
                    created.task.task_id,
                    name.to_string(),
                    created.task.status,
                    created.task.status_message,
                );
                watch_task(session.clone(), jobs.clone(), task_id, poll_interval);
            }
            Err(e) => report_mcp_error(&e),
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
                note_error(ExitStatus::Server);
            }
            if output.is_plain() {
                if json_output() {
                    print_json(&serde_json::to_value(&result).unwrap_or_default());
                } else {
                    if result.is_error {
                        println!("{}", tag(Style::new().fg(Color::Red), "tool error"));
                    }
                    render_content(&result.content);
                }
            } else {
                emit_result(result_value(&result), output);
            }
        }
        Err(e) => report_mcp_error(&e),
    }
    if !json_output() {
        println!("{}", timing(started.elapsed()));
    }
}

/// Extract a tool result's data value for capture or filtering: the structured
/// content if present, else a lone JSON text block parsed, else the content.
fn result_value(result: &tower_mcp::CallToolResult) -> serde_json::Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    if let [Content::Text { text, .. }] = result.content.as_slice() {
        return serde_json::from_str(text)
            .unwrap_or_else(|_| serde_json::Value::String(text.clone()));
    }
    serde_json::to_value(&result.content).unwrap_or_default()
}

/// Apply a capture/filter [`vars::Output`] to a result value: select a path,
/// bind it to a variable, or print it (bare scalar or pretty JSON).
fn emit_result(mut value: serde_json::Value, output: &vars::Output) {
    if let Some(path) = &output.filter {
        match vars::get_path(&value, path) {
            Some(selected) => value = selected,
            None => {
                command_error(&format!("path `{path}` not found in result"));
                return;
            }
        }
    }
    if let Some(name) = &output.capture {
        vars::set(name, value.clone());
        if json_output() {
            print_json(&value);
        } else {
            println!(
                "{} {}",
                paint(Style::new().fg(Color::Cyan), &format!("${name} =")),
                value_summary(&value)
            );
        }
    } else if json_output() {
        print_json(&value);
    } else {
        render_value(&value);
    }
}

fn value_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("{s:?}"),
        serde_json::Value::Array(a) => format!("[{} items]", a.len()),
        serde_json::Value::Object(o) => format!("{{{} fields}}", o.len()),
        other => other.to_string(),
    }
}

fn render_value(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(s) => println!("{s}"),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            println!("{}", json_pretty(value))
        }
        other => println!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tower_mcp::client::ClientTransport;

    /// A single-response transport for pinning the final discovery wire shape.
    struct DiscoveryTransport {
        result: serde_json::Value,
        incoming_tx: tokio::sync::mpsc::Sender<String>,
        incoming_rx: tokio::sync::mpsc::Receiver<String>,
        outgoing: Arc<Mutex<Vec<serde_json::Value>>>,
        connected: bool,
    }

    impl DiscoveryTransport {
        fn new(result: serde_json::Value) -> (Self, Arc<Mutex<Vec<serde_json::Value>>>) {
            let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel(4);
            let outgoing = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    result,
                    incoming_tx,
                    incoming_rx,
                    outgoing: outgoing.clone(),
                    connected: true,
                },
                outgoing,
            )
        }
    }

    #[async_trait]
    impl ClientTransport for DiscoveryTransport {
        async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
            let request: serde_json::Value = serde_json::from_str(message)?;
            self.outgoing.lock().unwrap().push(request.clone());
            if let Some(id) = request.get("id") {
                self.incoming_tx
                    .send(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": self.result,
                        })
                        .to_string(),
                    )
                    .await
                    .map_err(|error| tower_mcp::Error::Transport(error.to_string()))?;
            }
            Ok(())
        }

        async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
            Ok(self.incoming_rx.recv().await)
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        async fn close(&mut self) -> tower_mcp::Result<()> {
            self.connected = false;
            Ok(())
        }
    }

    fn jsonrpc(code: i32, message: &str) -> tower_mcp::Error {
        tower_mcp::Error::JsonRpc(tower_mcp::error::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        })
    }

    #[test]
    fn protocol_selection_is_stable_by_default_and_final_is_exact() {
        let stable = Args::try_parse_from(["mcp-repl", "--demo"]).unwrap();
        assert_eq!(stable.protocol, ProtocolMode::Stable);
        assert_eq!(
            stable.protocol.support().unwrap().versions(),
            tower_mcp::protocol::SUPPORTED_PROTOCOL_VERSIONS
        );

        for value in ["2026-07-28", "final"] {
            let final_args =
                Args::try_parse_from(["mcp-repl", "--protocol", value, "--demo"]).unwrap();
            assert_eq!(final_args.protocol, ProtocolMode::Final);
            assert_eq!(
                final_args.protocol.support().unwrap().versions(),
                ["2026-07-28"]
            );
        }
    }

    #[test]
    fn oauth_cli_parses_standalone_and_connection_workflows() {
        let login = Args::try_parse_from([
            "mcp-repl",
            "--login",
            "work",
            "--http",
            "https://mcp.example/mcp",
            "--oauth-scope",
            "openid",
            "--oauth-scope",
            "offline_access",
            "--no-browser",
        ])
        .unwrap();
        assert_eq!(login.login.as_deref(), Some("work"));
        assert_eq!(login.oauth_scopes, ["openid", "offline_access"]);
        assert!(login.no_browser);

        let connection = Args::try_parse_from([
            "mcp-repl",
            "--oauth",
            "work",
            "--http",
            "https://mcp.example/mcp",
            "--exec",
            "tools",
            "--json",
        ])
        .unwrap();
        assert_eq!(connection.oauth.as_deref(), Some("work"));
        assert_eq!(connection.exec, ["tools"]);

        assert!(Args::try_parse_from(["mcp-repl", "--login", "work", "--logout", "work"]).is_err());
    }

    #[tokio::test]
    async fn stable_selection_uses_initialize() {
        let client = client_builder(ProtocolMode::Stable)
            .unwrap()
            .connect_simple(ChannelTransport::new(demo_router()))
            .await
            .unwrap();
        let info = establish_connection(&client, ProtocolMode::Stable)
            .await
            .unwrap();

        assert_eq!(info.server_info.name, "mcp-repl-demo");
        assert_eq!(
            info.protocol_version,
            tower_mcp::protocol::LATEST_PROTOCOL_VERSION
        );
        assert!(client.server_info().await.is_some());
        assert!(client.discovery().await.is_none());
    }

    #[tokio::test]
    async fn final_selection_uses_discover_with_required_metadata() {
        let (transport, outgoing) = DiscoveryTransport::new(serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "final-test-server",
                    "version": "1.0.0"
                }
            }
        }));
        let client = client_builder(ProtocolMode::Final)
            .unwrap()
            .connect_simple(transport)
            .await
            .unwrap();
        let info = establish_connection(&client, ProtocolMode::Final)
            .await
            .unwrap();

        assert_eq!(info.server_info.name, "final-test-server");
        assert_eq!(info.protocol_version, "2026-07-28");
        assert!(client.server_info().await.is_none());
        assert!(client.discovery().await.is_some());

        let sent = outgoing.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "server/discover");
        assert_eq!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object()
        );
        assert!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                [tower_mcp::protocol::TASKS_EXTENSION_ID]
                .is_object()
        );
        assert_eq!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
            "mcp-repl"
        );
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
    fn oauth_precedence_is_explicit_static_then_cli_then_server_profile() {
        assert_eq!(
            selected_oauth_profile(Some("cli"), Some("server"), false, &[]),
            Some("cli".to_string())
        );
        assert_eq!(
            selected_oauth_profile(None, Some("server"), false, &[]),
            Some("server".to_string())
        );
        assert_eq!(
            selected_oauth_profile(Some("cli"), Some("server"), true, &[]),
            None
        );
        assert_eq!(
            selected_oauth_profile(
                Some("cli"),
                Some("server"),
                false,
                &["authorization: Basic explicit".to_string()],
            ),
            None
        );
        assert_eq!(
            selected_oauth_profile(
                Some("cli"),
                Some("server"),
                false,
                &["X-Tenant: acme".to_string()],
            ),
            Some("cli".to_string())
        );
    }

    #[test]
    fn selected_authorization_header_beats_environment_bearer() {
        let selected_headers = [("authorization".to_string(), "Basic selected".to_string())];
        let cfg = build_http_config_with_env(
            None,
            &[],
            None,
            &selected_headers,
            Some("ambient-token".into()),
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("authorization").map(String::as_str),
            Some("Basic selected")
        );

        let cfg = build_http_config_with_env(
            Some("explicit-token".into()),
            &[],
            None,
            &selected_headers,
            Some("ambient-token".into()),
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer explicit-token")
        );
    }

    #[test]
    fn explicit_oauth_suppresses_profile_and_environment_bearers() {
        let selected = selected_oauth_profile(Some("work"), None, false, &[]);
        assert_eq!(selected.as_deref(), Some("work"));

        let cfg = build_http_config_with_env(
            None,
            &[],
            selected.is_none().then(|| "profile-token".to_string()),
            &[],
            selected.is_none().then(|| "environment-token".to_string()),
        )
        .unwrap();
        assert!(!cfg.headers.contains_key("Authorization"));
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

    // Completion and input highlighting both read BUILTINS, and an alias may
    // not shadow a name in it, so listing a command there is what makes it a
    // first-class built-in rather than a hidden one.
    #[test]
    fn bench_is_a_listed_builtin() {
        assert!(BUILTINS.iter().any(|(name, _)| *name == "bench"));
    }

    // Completion and highlighting both read BUILTINS, so membership is what
    // makes `find` completable rather than any code in the editor.
    #[test]
    fn find_is_a_completable_builtin() {
        assert!(BUILTINS.iter().any(|(name, _)| *name == "find"));
    }

    #[test]
    fn error_json_is_a_valid_object() {
        let v = error_json(ExitStatus::Usage, "boom: it broke");
        assert_eq!(v["error"], "boom: it broke");
        assert_eq!(v["kind"], "usage");
        assert_eq!(v["exitStatus"], 2);
    }

    #[test]
    fn automatic_task_updates_are_interactive_text_only() {
        assert!(automatic_task_updates(false, false));
        assert!(!automatic_task_updates(true, false));
        assert!(!automatic_task_updates(true, true));
        assert!(!automatic_task_updates(false, true));
    }

    #[test]
    fn quoted_task_arguments_reach_schema_coercion_intact() {
        let parsed = command::parse(
            r#"run.start instruction="Reply with exactly hello" mode=interactive &"#,
        )
        .unwrap();
        let tokens: Vec<&str> = parsed.words[1..].iter().map(String::as_str).collect();
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "instruction": { "type": "string" },
                "mode": { "type": "string" }
            }
        });

        assert!(parsed.background);
        assert_eq!(
            parse_kv_args(&schema, &tokens),
            serde_json::json!({
                "instruction": "Reply with exactly hello",
                "mode": "interactive"
            })
        );
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

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_slow_task_announces_completion_without_manual_polling() {
        let session = Arc::new(Session::new(demo_client().await, None));
        let surface = Arc::new(RwLock::new(Surface::default()));
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        let jobs = Arc::new(Jobs::new(output, true));
        let schema_contracts = schema_contract::ContractSet::default();

        run_tool(
            &session,
            &surface,
            &jobs,
            &schema_contracts,
            "slow_add",
            serde_json::json!({ "a": 2, "b": 3 }),
            true,
            &vars::Output::default(),
        )
        .await;

        let line = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(line) = printer.get_line() {
                    break line;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the task watcher should observe slow_add completion");

        assert!(line.contains("completed"), "{line}");
        assert_eq!(
            jobs.list()[0].status,
            tower_mcp::protocol::TaskStatus::Completed
        );
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
