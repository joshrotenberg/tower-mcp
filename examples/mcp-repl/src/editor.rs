//! The reedline editor: prompt, completer, live highlighter, and the
//! dedicated readline thread.
//!
//! The editor runs on its own std thread; lines cross into async via a
//! tokio mpsc channel and an ack channel paces the prompt so it only
//! reappears after the previous command finished printing. The completer
//! blocks on this thread, which lives outside the tokio runtime, so
//! `Handle::block_on` is safe here.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer, DefaultHinter, Emacs, Highlighter, KeyCode, KeyModifiers, MenuBuilder,
    Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Span, StyledText, Suggestion, default_emacs_keybindings,
};

use tower_mcp::client::McpClient;

use crate::style;
use crate::{BUILTINS, Surface};

const MENU_NAME: &str = "completion_menu";

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

struct ReplPrompt {
    server_name: String,
}

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.server_name)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let status = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            status, history_search.term
        ))
    }
}

// ---------------------------------------------------------------------------
// Completer
// ---------------------------------------------------------------------------

pub struct ReplCompleter {
    surface: Arc<RwLock<Surface>>,
    client: Arc<McpClient>,
    runtime: tokio::runtime::Handle,
}

fn suggestion(value: impl Into<String>, description: Option<String>, span: Span) -> Suggestion {
    Suggestion {
        value: value.into(),
        display_override: None,
        description,
        style: None,
        extra: None,
        span,
        append_whitespace: false,
        match_indices: None,
    }
}

fn word_suggestion(
    value: impl Into<String>,
    description: Option<String>,
    span: Span,
) -> Suggestion {
    Suggestion {
        append_whitespace: true,
        ..suggestion(value, description, span)
    }
}

impl ReplCompleter {
    pub fn new(
        surface: Arc<RwLock<Surface>>,
        client: Arc<McpClient>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            surface,
            client,
            runtime,
        }
    }

    /// Server-powered completion for prompt argument values
    /// (`completion/complete` with a prompt reference). Safe to block here:
    /// the completer runs on the dedicated readline thread, outside the
    /// async runtime.
    fn complete_prompt_arg_via_server(
        &self,
        prompt: &str,
        arg: &str,
        partial: &str,
    ) -> Vec<String> {
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

    /// Server-powered completion for a resource template variable
    /// (`completion/complete` with a resource reference). Same blocking
    /// pattern as prompt arguments: 2s timeout, best-effort.
    fn complete_template_var_via_server(
        &self,
        uri_template: &str,
        var: &str,
        partial: &str,
    ) -> Vec<String> {
        let client = self.client.clone();
        let (template, var, partial) = (
            uri_template.to_string(),
            var.to_string(),
            partial.to_string(),
        );
        self.runtime
            .block_on(async move {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    client.complete_resource_uri(&template, &var, &partial),
                )
                .await
            })
            .ok()
            .and_then(|r| r.ok())
            .map(|r| r.completion.values)
            .unwrap_or_default()
    }

    /// Completions for `read <partial>`: literal resource URIs, template
    /// URI templates, and server-completed template variables when the
    /// partial has reached a template's `{variable}`.
    fn complete_resource_word(&self, surface: &Surface, word: &str, span: Span) -> Vec<Suggestion> {
        let mut out = Vec::new();
        for r in &surface.resources {
            if r.uri.starts_with(word) {
                out.push(word_suggestion(&r.uri, Some(r.name.clone()), span));
            }
        }
        for t in &surface.templates {
            if t.uri_template.starts_with(word) {
                out.push(suggestion(&t.uri_template, Some(t.name.clone()), span));
            }
            // Template variable completion: `file:///{path}` with word
            // `file:///src/` asks the server to complete `path` = `src/`.
            let Some(open) = t.uri_template.find('{') else {
                continue;
            };
            let Some(close_rel) = t.uri_template[open..].find('}') else {
                continue;
            };
            let close = open + close_rel;
            let static_prefix = &t.uri_template[..open];
            if word.len() < static_prefix.len() || !word.starts_with(static_prefix) {
                continue;
            }
            let var = &t.uri_template[open + 1..close];
            let suffix = &t.uri_template[close + 1..];
            let partial_value = &word[static_prefix.len()..];
            for v in self.complete_template_var_via_server(&t.uri_template, var, partial_value) {
                let mut full = format!("{static_prefix}{v}");
                if !suffix.contains('{') {
                    full.push_str(suffix);
                }
                out.push(suggestion(full, Some(format!("{var} ({})", t.name)), span));
            }
        }
        out
    }

    /// Completions for `describe <name>`: every named thing on the surface.
    fn complete_describe_word(surface: &Surface, word: &str, span: Span) -> Vec<Suggestion> {
        let mut out = Vec::new();
        for t in &surface.tools {
            if t.name.starts_with(word) {
                out.push(word_suggestion(&t.name, Some("tool".to_string()), span));
            }
        }
        for p in &surface.prompts {
            if p.name.starts_with(word) {
                out.push(word_suggestion(&p.name, Some("prompt".to_string()), span));
            }
        }
        for r in &surface.resources {
            if r.uri.starts_with(word) {
                out.push(word_suggestion(&r.uri, Some("resource".to_string()), span));
            }
        }
        for t in &surface.templates {
            if t.uri_template.starts_with(word) {
                out.push(word_suggestion(
                    &t.uri_template,
                    Some("template".to_string()),
                    span,
                ));
            }
        }
        out
    }
}

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let head = &line[..pos];
        let (word_start, word) = match head.rfind(char::is_whitespace) {
            Some(i) => (i + 1, &head[i + 1..]),
            None => (0, head),
        };
        let span = Span::new(word_start, pos);
        let surface = self.surface.read().unwrap();
        let mut out: Vec<Suggestion> = Vec::new();

        let first = head.split_whitespace().next().unwrap_or("");
        let completing_first = word_start == 0;

        if completing_first {
            // First word: built-ins plus every tool name.
            for (name, desc) in BUILTINS {
                if name.starts_with(word) {
                    out.push(word_suggestion(*name, Some(desc.to_string()), span));
                }
            }
            for t in &surface.tools {
                if t.name.starts_with(word) {
                    out.push(word_suggestion(&t.name, t.description.clone(), span));
                }
            }
            return out;
        }

        match first {
            "read" => {
                out.extend(self.complete_resource_word(&surface, word, span));
            }
            "describe" => {
                out.extend(Self::complete_describe_word(&surface, word, span));
            }
            "prompt" => {
                let words = head.split_whitespace().count();
                let second_word = words == 2 && !head.ends_with(' ');
                let naming_prompt = second_word || words == 1;
                if naming_prompt {
                    for p in &surface.prompts {
                        if p.name.starts_with(word) {
                            out.push(word_suggestion(&p.name, p.description.clone(), span));
                        }
                    }
                } else if let Some(prompt_name) = head.split_whitespace().nth(1) {
                    if let Some((arg_name, partial)) = word.split_once('=') {
                        // Argument value: ask the server (completion/complete).
                        for v in self.complete_prompt_arg_via_server(prompt_name, arg_name, partial)
                        {
                            out.push(word_suggestion(format!("{arg_name}={v}"), None, span));
                        }
                    } else if let Some(p) = surface.prompts.iter().find(|p| p.name == prompt_name) {
                        // Argument name: from the prompt definition.
                        for a in &p.arguments {
                            if a.name.starts_with(word) {
                                let desc = match (&a.description, a.required) {
                                    (Some(d), true) => Some(format!("(required) {d}")),
                                    (Some(d), false) => Some(d.clone()),
                                    (None, true) => Some("(required)".to_string()),
                                    (None, false) => None,
                                };
                                out.push(suggestion(format!("{}=", a.name), desc, span));
                            }
                        }
                    }
                }
            }
            "call" => {
                for t in &surface.tools {
                    if t.name.starts_with(word) {
                        out.push(word_suggestion(&t.name, t.description.clone(), span));
                    }
                }
            }
            tool_name => {
                // Dynamic tool command: complete argument names from the
                // tool's inputSchema properties, and enum values after `=`.
                let Some(tool) = surface.tools.iter().find(|t| t.name == tool_name) else {
                    return out;
                };
                let Some(props) = tool
                    .input_schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                else {
                    return out;
                };
                if let Some((arg_name, partial)) = word.split_once('=') {
                    // Enum values from the property schema, when declared.
                    if let Some(values) = props
                        .get(arg_name)
                        .and_then(|s| s.get("enum"))
                        .and_then(|e| e.as_array())
                    {
                        for v in values {
                            if let Some(v) = v.as_str()
                                && v.starts_with(partial)
                            {
                                out.push(word_suggestion(format!("{arg_name}={v}"), None, span));
                            }
                        }
                    }
                } else {
                    let required: Vec<&str> = tool
                        .input_schema
                        .get("required")
                        .and_then(|r| r.as_array())
                        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    for (key, prop) in props {
                        if !key.starts_with(word) {
                            continue;
                        }
                        let ty = prop.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let desc = prop
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("");
                        let req = if required.contains(&key.as_str()) {
                            "required "
                        } else {
                            ""
                        };
                        let full = format!("{req}{ty} {desc}");
                        let full = full.trim();
                        let desc = (!full.is_empty()).then(|| full.to_string());
                        out.push(suggestion(format!("{key}="), desc, span));
                    }
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Highlighter
// ---------------------------------------------------------------------------

/// Live input highlighting: the command word is styled by validity
/// (built-in, known tool, or unknown), `key=` argument names are cyan,
/// and JSON-literal values get the same palette as output rendering.
pub struct ReplHighlighter {
    surface: Arc<RwLock<Surface>>,
}

impl ReplHighlighter {
    pub fn new(surface: Arc<RwLock<Surface>>) -> Self {
        Self { surface }
    }

    fn command_style(&self, word: &str) -> Style {
        if BUILTINS.iter().any(|(name, _)| *name == word) {
            return Style::new().fg(Color::Cyan).bold();
        }
        let surface = self.surface.read().unwrap();
        if surface.tools.iter().any(|t| t.name == word) {
            return Style::new().fg(Color::Green).bold();
        }
        // Prefix of something completable: neutral while typing.
        let is_prefix = BUILTINS.iter().any(|(name, _)| name.starts_with(word))
            || surface.tools.iter().any(|t| t.name.starts_with(word));
        if is_prefix {
            Style::new()
        } else {
            Style::new().fg(Color::Red)
        }
    }
}

fn value_style(raw: &str) -> Style {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Number(_)) => Style::new().fg(Color::Yellow),
        Ok(serde_json::Value::Bool(_)) | Ok(serde_json::Value::Null) => {
            Style::new().fg(Color::Purple)
        }
        Ok(serde_json::Value::String(_)) => Style::new().fg(Color::Green),
        Ok(_) => Style::new().fg(Color::Green).dimmed(),
        Err(_) => Style::new(),
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        if !style::colors_enabled() {
            styled.push((Style::new(), line.to_string()));
            return styled;
        }

        let mut seen_command = false;
        let mut rest = line;
        while !rest.is_empty() {
            let token_start = match rest.find(|c: char| !c.is_whitespace()) {
                Some(i) => i,
                None => {
                    styled.push((Style::new(), rest.to_string()));
                    break;
                }
            };
            if token_start > 0 {
                styled.push((Style::new(), rest[..token_start].to_string()));
            }
            let token_end = rest[token_start..]
                .find(char::is_whitespace)
                .map(|i| token_start + i)
                .unwrap_or(rest.len());
            let token = &rest[token_start..token_end];

            if !seen_command {
                styled.push((self.command_style(token), token.to_string()));
                seen_command = true;
            } else if token == "&" {
                styled.push((Style::new().fg(Color::Purple).bold(), token.to_string()));
            } else if let Some((key, value)) = token.split_once('=') {
                styled.push((Style::new().fg(Color::Cyan), format!("{key}=")));
                styled.push((value_style(value), value.to_string()));
            } else {
                styled.push((value_style(token), token.to_string()));
            }

            rest = &rest[token_end..];
        }
        if line.is_empty() {
            styled.push((Style::new(), String::new()));
        }
        styled
    }
}

// ---------------------------------------------------------------------------
// Readline thread
// ---------------------------------------------------------------------------

/// Spawn the readline thread. Lines are sent over `line_tx`; after each
/// line, the thread blocks on `ack_rx` until the command has finished
/// printing. `at_prompt` is true while the editor owns the terminal
/// (used to refuse elicitation prompts that would fight over stdin).
///
/// When stdin is not a tty (piped input), falls back to a plain
/// line-reader so non-interactive use keeps working.
pub fn spawn_readline_thread(
    server_name: String,
    surface: Arc<RwLock<Surface>>,
    client: Arc<McpClient>,
    runtime: tokio::runtime::Handle,
    line_tx: tokio::sync::mpsc::Sender<String>,
    ack_rx: std::sync::mpsc::Receiver<()>,
    at_prompt: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        if !std::io::stdin().is_terminal() {
            run_piped(&line_tx, &ack_rx);
            return;
        }
        run_interactive(
            server_name,
            surface,
            client,
            runtime,
            &line_tx,
            &ack_rx,
            at_prompt,
        );
    });
}

fn run_piped(line_tx: &tokio::sync::mpsc::Sender<String>, ack_rx: &std::sync::mpsc::Receiver<()>) {
    let stdin = std::io::stdin();
    let mut buf = String::new();
    loop {
        buf.clear();
        // Scope the stdin lock to the read: holding it while waiting on
        // the ack channel would deadlock the elicitation handler, which
        // reads stdin during a foreground tool call.
        let read = {
            let mut lock = stdin.lock();
            std::io::BufRead::read_line(&mut lock, &mut buf)
        };
        match read {
            Ok(0) | Err(_) => {
                let _ = line_tx.blocking_send("quit".to_string());
                break;
            }
            Ok(_) => {
                if line_tx
                    .blocking_send(buf.trim_end_matches('\n').to_string())
                    .is_err()
                    || ack_rx.recv().is_err()
                {
                    break;
                }
            }
        }
    }
}

fn run_interactive(
    server_name: String,
    surface: Arc<RwLock<Surface>>,
    client: Arc<McpClient>,
    runtime: tokio::runtime::Handle,
    line_tx: &tokio::sync::mpsc::Sender<String>,
    ack_rx: &std::sync::mpsc::Receiver<()>,
    at_prompt: Arc<AtomicBool>,
) {
    let completer = ReplCompleter::new(surface.clone(), client, runtime);
    let highlighter = ReplHighlighter::new(surface);

    let menu = ColumnarMenu::default().with_name(MENU_NAME);
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(MENU_NAME.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );

    let mut editor = Reedline::create()
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_highlighter(Box::new(highlighter))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray)),
        ))
        .with_ansi_colors(style::colors_enabled());

    let prompt = ReplPrompt { server_name };
    loop {
        at_prompt.store(true, Ordering::SeqCst);
        let sig = editor.read_line(&prompt);
        at_prompt.store(false, Ordering::SeqCst);
        match sig {
            Ok(Signal::Success(line)) => {
                if line_tx.blocking_send(line).is_err() {
                    break;
                }
                // Wait until the command finished printing before showing
                // the next prompt.
                if ack_rx.recv().is_err() {
                    break;
                }
            }
            Ok(Signal::CtrlC) => continue,
            _ => {
                let _ = line_tx.blocking_send("quit".to_string());
                break;
            }
        }
    }
}
