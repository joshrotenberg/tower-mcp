//! Sampling: how the REPL answers a server's `sampling/createMessage`.
//!
//! mcp-repl has no model behind it, so it answers as the operator: the
//! request is printed and the assistant message is typed on stdin, the same
//! way elicitation collects form fields. `--sampling` picks between that and
//! the two non-interactive strategies.
//!
//! Like elicitation, the interactive path only works while the readline
//! editor is parked (a foreground tool call); a request that arrives while
//! the editor owns the terminal in raw mode is declined instead.

use std::io::BufRead;

use clap::ValueEnum;
use nu_ansi_term::{Color, Style};
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::{
    ContentRole, CreateMessageParams, CreateMessageResult, SamplingContent, SamplingContentOrArray,
    SamplingMessage,
};

use crate::style::{paint, tag};

/// How to answer a `sampling/createMessage` request.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum SamplingMode {
    /// Print the request and read the assistant message on stdin.
    Prompt,
    /// Answer with a fixed placeholder message, without asking.
    Canned,
    /// Refuse every request.
    Decline,
}

impl SamplingMode {
    /// The label `info` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Canned => "canned",
            Self::Decline => "decline",
        }
    }
}

/// The text a `canned` reply carries. Deliberately not model-shaped: a server
/// logging this should be able to tell no inference happened.
pub const CANNED_REPLY: &str = "(canned reply from mcp-repl; no model was called)";

/// The `model` field on a reply. The spec wants the name of the model that
/// generated the message; nothing generated it, so say so.
const CANNED_MODEL: &str = "mcp-repl/canned";
const OPERATOR_MODEL: &str = "mcp-repl/operator";

/// Resolve the effective mode: `--sampling` when given, otherwise `prompt`
/// interactively and `decline` under `-e`, where there is no operator to ask
/// and a blocked read would hang the script.
pub fn resolve(flag: Option<SamplingMode>, one_shot: bool) -> SamplingMode {
    match flag {
        Some(mode) => mode,
        None if one_shot => SamplingMode::Decline,
        None => SamplingMode::Prompt,
    }
}

/// The resolved mode, set once at startup. Like the color and wire settings,
/// it is read from wherever it is needed rather than threaded through: both
/// the handler answering a request and `info` reporting the strategy want it.
static MODE: std::sync::OnceLock<SamplingMode> = std::sync::OnceLock::new();

/// Record the resolved mode. Called once, from `main`.
pub fn init(mode: SamplingMode) {
    let _ = MODE.set(mode);
}

/// The mode in effect.
pub fn mode() -> SamplingMode {
    *MODE.get().unwrap_or(&SamplingMode::Prompt)
}

/// A refusal. Sampling has no `decline` result shape the way elicitation
/// does, so the only way to say no is an error; `-32007` says the client
/// refused rather than that something broke.
pub fn declined(reason: &str) -> JsonRpcError {
    JsonRpcError::forbidden(format!("sampling declined: {reason}"))
}

/// The request as the operator sees it before answering: what the server
/// asked for, and every message it wants the model to see.
pub fn render_request(params: &CreateMessageParams) -> String {
    let mut out = String::new();
    let mut head = format!("max {} tokens", params.max_tokens);
    if let Some(t) = params.temperature {
        head.push_str(&format!(", temperature {t}"));
    }
    if let Some(prefs) = &params.model_preferences
        && let Some(first) = prefs.hints.first()
        && let Some(name) = &first.name
    {
        head.push_str(&format!(", model hint {name}"));
    }
    out.push_str(&format!(
        "{} server requests a completion ({head})\n",
        tag(Style::new().fg(Color::Purple), "sampling")
    ));
    if let Some(system) = &params.system_prompt {
        out.push_str(&format!(
            "  {} {}\n",
            paint(Style::new().fg(Color::Cyan), "system:"),
            truncate(system)
        ));
    }
    for message in &params.messages {
        out.push_str(&format!(
            "  {} {}\n",
            paint(
                Style::new().fg(Color::Cyan),
                &format!("{}:", role_label(message.role))
            ),
            truncate(&message_text(message))
        ));
    }
    out
}

fn role_label(role: ContentRole) -> &'static str {
    match role {
        ContentRole::Assistant => "assistant",
        _ => "user",
    }
}

/// One message flattened to a line. Non-text parts are summarized rather
/// than dumped: a base64 image is not readable at the prompt.
fn message_text(message: &SamplingMessage) -> String {
    let parts: Vec<String> = message
        .content
        .items()
        .iter()
        .map(|c| match c {
            SamplingContent::Text { text, .. } => text.clone(),
            SamplingContent::Image {
                mime_type, data, ..
            } => {
                format!("[image {mime_type}, {} base64 chars]", data.len())
            }
            SamplingContent::Audio {
                mime_type, data, ..
            } => {
                format!("[audio {mime_type}, {} base64 chars]", data.len())
            }
            other => serde_json::to_value(other)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| format!("[{t}]"))
                })
                .unwrap_or_else(|| "[content]".to_string()),
        })
        .collect();
    parts.join(" ")
}

/// Cap a message at something a terminal can show, keeping the tail count so
/// it is clear the model would have seen more.
fn truncate(text: &str) -> String {
    const CAP: usize = 2000;
    let flat = text.replace('\n', "\n    ");
    if flat.chars().count() <= CAP {
        return flat;
    }
    let kept: String = flat.chars().take(CAP).collect();
    let dropped = flat.chars().count() - CAP;
    format!("{kept}... (+{dropped} more chars)")
}

/// Read the assistant message. Lines accumulate until a lone `.` submits or
/// input ends; ending input having typed nothing cancels, which is how
/// Ctrl-D refuses a request. Returns `None` for a refusal.
pub fn read_reply(input: &mut impl BufRead) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    loop {
        let mut buf = String::new();
        match input.read_line(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = buf.trim_end_matches(['\n', '\r']);
        if line == "." {
            break;
        }
        lines.push(line.to_string());
    }
    let text = lines.join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build the reply. `max_tokens` is honored crudely (four chars a token),
/// enough that a server asking for a short answer is not handed an essay,
/// and the stop reason says which way it ended.
fn reply(text: &str, model: &str, max_tokens: u32) -> CreateMessageResult {
    let cap = (max_tokens as usize).saturating_mul(4);
    let (text, stop_reason) = if cap > 0 && text.chars().count() > cap {
        (text.chars().take(cap).collect::<String>(), "maxTokens")
    } else {
        (text.to_string(), "endTurn")
    };
    CreateMessageResult {
        content: SamplingContentOrArray::Single(SamplingContent::Text {
            text,
            annotations: None,
            meta: None,
        }),
        model: model.to_string(),
        role: ContentRole::Assistant,
        stop_reason: Some(stop_reason.to_string()),
        meta: None,
    }
}

/// The `canned` answer.
pub fn canned(params: &CreateMessageParams) -> CreateMessageResult {
    reply(CANNED_REPLY, CANNED_MODEL, params.max_tokens)
}

/// The `prompt` answer: show the request, read a message, wrap it. Blocking
/// stdin reads, so callers run this off the async runtime.
pub fn prompt(params: &CreateMessageParams) -> Result<CreateMessageResult, JsonRpcError> {
    print!("{}", render_request(params));
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            "  type the assistant message; `.` on its own line submits, Ctrl-D declines"
        )
    );
    print!("  reply> ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut stdin = std::io::stdin().lock();
    match read_reply(&mut stdin) {
        Some(text) => Ok(reply(&text, OPERATOR_MODEL, params.max_tokens)),
        None => {
            println!("  (declined)");
            Err(declined("no reply given"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::protocol::{ModelHint, ModelPreferences};

    fn user_text(text: &str) -> SamplingMessage {
        SamplingMessage {
            role: ContentRole::User,
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: text.to_string(),
                annotations: None,
                meta: None,
            }),
            meta: None,
        }
    }

    fn params(max_tokens: u32) -> CreateMessageParams {
        CreateMessageParams::new(vec![user_text("summarize this")], max_tokens)
    }

    fn text_of(result: &CreateMessageResult) -> String {
        result.first_text().unwrap_or_default().to_string()
    }

    #[test]
    fn mode_defaults_to_prompt_interactively_and_decline_one_shot() {
        assert_eq!(resolve(None, false), SamplingMode::Prompt);
        assert_eq!(resolve(None, true), SamplingMode::Decline);
        // An explicit flag wins either way, including asking for the
        // interactive path under -e.
        assert_eq!(
            resolve(Some(SamplingMode::Canned), false),
            SamplingMode::Canned
        );
        assert_eq!(
            resolve(Some(SamplingMode::Prompt), true),
            SamplingMode::Prompt
        );
    }

    #[test]
    fn canned_reply_is_marked_as_not_a_model() {
        let result = canned(&params(200));
        assert_eq!(text_of(&result), CANNED_REPLY);
        assert_eq!(result.model, CANNED_MODEL);
        assert_eq!(result.role, ContentRole::Assistant);
        assert_eq!(result.stop_reason.as_deref(), Some("endTurn"));
    }

    #[test]
    fn a_refusal_is_forbidden_not_an_internal_error() {
        let err = declined("no reply given");
        assert_eq!(err.code, -32007);
        assert!(
            err.message.contains("no reply given"),
            "the reason should survive: {}",
            err.message
        );
    }

    #[test]
    fn max_tokens_truncates_and_says_so() {
        let long = "x".repeat(100);
        // 10 tokens is 40 chars.
        let result = reply(&long, OPERATOR_MODEL, 10);
        assert_eq!(text_of(&result).chars().count(), 40);
        assert_eq!(result.stop_reason.as_deref(), Some("maxTokens"));

        let result = reply("short", OPERATOR_MODEL, 10);
        assert_eq!(text_of(&result), "short");
        assert_eq!(result.stop_reason.as_deref(), Some("endTurn"));
    }

    #[test]
    fn a_dot_submits_and_the_lines_before_it_are_the_message() {
        let mut input = std::io::Cursor::new("first line\nsecond line\n.\nnot read\n");
        assert_eq!(
            read_reply(&mut input).as_deref(),
            Some("first line\nsecond line")
        );
    }

    #[test]
    fn input_ending_submits_what_was_typed() {
        // A piped reply has no terminator, only EOF.
        let mut input = std::io::Cursor::new("piped reply\n");
        assert_eq!(read_reply(&mut input).as_deref(), Some("piped reply"));
    }

    #[test]
    fn typing_nothing_declines() {
        // Ctrl-D at the first prompt.
        let mut input = std::io::Cursor::new("");
        assert_eq!(read_reply(&mut input), None);
        // A dot with nothing before it is the same refusal, not an empty
        // assistant message.
        let mut input = std::io::Cursor::new(".\n");
        assert_eq!(read_reply(&mut input), None);
        // Whitespace is not a message either.
        let mut input = std::io::Cursor::new("   \n\n.\n");
        assert_eq!(read_reply(&mut input), None);
    }

    #[test]
    fn the_request_shows_what_the_server_asked_for() {
        let mut p = CreateMessageParams::new(
            vec![
                user_text("summarize this"),
                SamplingMessage {
                    role: ContentRole::Assistant,
                    content: SamplingContentOrArray::Single(SamplingContent::Text {
                        text: "sure".into(),
                        annotations: None,
                        meta: None,
                    }),
                    meta: None,
                },
            ],
            200,
        )
        .system_prompt("You are a concise summarizer.");
        p.temperature = Some(0.2);
        p.model_preferences = Some(ModelPreferences {
            hints: vec![ModelHint {
                name: Some("claude-3-5-sonnet".into()),
            }],
            ..Default::default()
        });

        let rendered = render_request(&p);
        assert!(rendered.contains("max 200 tokens"), "{rendered}");
        assert!(rendered.contains("temperature 0.2"), "{rendered}");
        assert!(
            rendered.contains("model hint claude-3-5-sonnet"),
            "{rendered}"
        );
        assert!(
            rendered.contains("You are a concise summarizer."),
            "{rendered}"
        );
        assert!(rendered.contains("summarize this"), "{rendered}");
        // Both roles are labeled, so a multi-turn request reads as a
        // conversation rather than one blob.
        assert!(rendered.contains("user:"), "{rendered}");
        assert!(rendered.contains("assistant:"), "{rendered}");
    }

    #[test]
    fn non_text_content_is_summarized_not_dumped() {
        let message = SamplingMessage {
            role: ContentRole::User,
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Text {
                    text: "what is this".into(),
                    annotations: None,
                    meta: None,
                },
                SamplingContent::Image {
                    data: "A".repeat(4096),
                    mime_type: "image/png".into(),
                    annotations: None,
                    meta: None,
                },
            ]),
            meta: None,
        };
        let line = message_text(&message);
        assert!(line.contains("what is this"), "{line}");
        assert!(
            line.contains("[image image/png, 4096 base64 chars]"),
            "{line}"
        );
        assert!(!line.contains("AAAA"), "base64 must not reach the terminal");
    }

    #[test]
    fn a_long_message_is_capped_with_the_dropped_count() {
        let rendered = render_request(&CreateMessageParams::new(
            vec![user_text(&"y".repeat(2500))],
            200,
        ));
        assert!(rendered.contains("(+500 more chars)"), "not capped");
    }
}
