//! Output styling: color gating, a JSON syntax colorizer, a light
//! markdown renderer, and status-line helpers.
//!
//! All color degrades to plain text when `NO_COLOR` is set or stdout is
//! not a terminal, so piped output stays clean.

use std::io::IsTerminal;
use std::sync::OnceLock;

use nu_ansi_term::{Color, Style};
use tower_mcp::protocol::TaskStatus;

/// When to emit ANSI colors.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ColorMode {
    /// Color when stdout is a tty and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always color, even when piped.
    Always,
    /// Never color.
    Never,
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Set the color mode once, before any output. `Always` overrides
/// `NO_COLOR` (an explicit request wins, matching cargo's behavior).
pub fn init(mode: ColorMode) {
    let _ = ENABLED.set(match mode {
        ColorMode::Auto => auto_detect(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    });
}

fn auto_detect() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Whether ANSI styling is active.
pub fn colors_enabled() -> bool {
    *ENABLED.get_or_init(auto_detect)
}

/// Apply a style if colors are enabled, otherwise return the text as-is.
pub fn paint(style: Style, text: &str) -> String {
    if colors_enabled() {
        style.paint(text).to_string()
    } else {
        text.to_string()
    }
}

/// A `[label]` tag with dim brackets and a styled label.
pub fn tag(style: Style, label: &str) -> String {
    format!(
        "{}{}{}",
        paint(Style::new().dimmed(), "["),
        paint(style, label),
        paint(Style::new().dimmed(), "]")
    )
}

/// Style for a task status: working=yellow, completed=green,
/// failed/cancelled=red.
pub fn task_status_style(status: TaskStatus) -> Style {
    match status {
        TaskStatus::Working => Style::new().fg(Color::Yellow),
        TaskStatus::InputRequired => Style::new().fg(Color::Purple),
        TaskStatus::Completed => Style::new().fg(Color::Green),
        TaskStatus::Failed | TaskStatus::Cancelled => Style::new().fg(Color::Red),
        _ => Style::new(),
    }
}

/// Style an error line prefix.
pub fn error_prefix() -> String {
    paint(Style::new().fg(Color::Red).bold(), "error")
}

// ---------------------------------------------------------------------------
// JSON colorizer
// ---------------------------------------------------------------------------

/// Pretty-print a JSON value with syntax colors (2-space indent, same
/// shape as `serde_json::to_string_pretty`).
pub fn json_pretty(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, 0);
    out
}

fn write_json(out: &mut String, value: &serde_json::Value, indent: usize) {
    let pad = "  ".repeat(indent);
    let inner_pad = "  ".repeat(indent + 1);
    match value {
        serde_json::Value::Null => out.push_str(&paint(Style::new().fg(Color::Purple), "null")),
        serde_json::Value::Bool(b) => {
            out.push_str(&paint(
                Style::new().fg(Color::Purple),
                if *b { "true" } else { "false" },
            ));
        }
        serde_json::Value::Number(n) => {
            out.push_str(&paint(Style::new().fg(Color::Yellow), &n.to_string()));
        }
        serde_json::Value::String(s) => {
            let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"));
            out.push_str(&paint(Style::new().fg(Color::Green), &quoted));
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&inner_pad);
                write_json(out, item, indent + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (key, val)) in map.iter().enumerate() {
                let quoted = serde_json::to_string(key).unwrap_or_else(|_| format!("{key:?}"));
                out.push_str(&inner_pad);
                out.push_str(&paint(Style::new().fg(Color::Cyan), &quoted));
                out.push_str(": ");
                write_json(out, val, indent + 1);
                if i + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

/// Heuristic: does this text look like markdown worth rendering?
/// Strong signals (headings, fences) decide immediately; weak signals
/// (bullets, inline code, bold) count too.
pub fn looks_like_markdown(text: &str) -> bool {
    let mut weak_signal = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || heading_level(trimmed).is_some() {
            return true;
        }
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.contains("**")
            || trimmed.chars().filter(|c| *c == '`').count() >= 2
        {
            weak_signal = true;
        }
    }
    weak_signal
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// Render markdown-ish text for the terminal: headings bold, fenced code
/// dimmed, inline code and bold spans styled, list bullets colored.
pub fn render_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(&paint(Style::new().dimmed(), line));
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(&paint(Style::new().dimmed(), line));
            out.push('\n');
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            let body = trimmed[level..].trim_start();
            let style = if level <= 2 {
                Style::new().bold().underline()
            } else {
                Style::new().bold()
            };
            out.push_str(&paint(style, body));
            out.push('\n');
            continue;
        }
        // List bullets: color the marker, render the rest inline.
        let indent_len = line.len() - trimmed.len();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str(&line[..indent_len]);
            out.push_str(&paint(Style::new().fg(Color::Cyan), "•"));
            out.push(' ');
            out.push_str(&render_inline(rest));
            out.push('\n');
            continue;
        }
        out.push_str(&line[..indent_len]);
        out.push_str(&render_inline(trimmed));
        out.push('\n');
    }
    // lines() drops a trailing newline; the callers print with println.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Inline spans: `` `code` `` and `**bold**` (markers stripped).
fn render_inline(line: &str) -> String {
    // A span is (start, marker, style); pick whichever marker comes first,
    // consume it, repeat.
    let mut out = String::new();
    let mut rest = line;
    loop {
        let code = rest.find('`').map(|i| (i, "`"));
        let bold = rest.find("**").map(|i| (i, "**"));
        let next = match (code, bold) {
            (Some(c), Some(b)) => Some(if c.0 < b.0 { c } else { b }),
            (Some(c), None) => Some(c),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((start, marker)) = next else {
            out.push_str(rest);
            return out;
        };
        let body_start = start + marker.len();
        let Some(len) = rest[body_start..].find(marker) else {
            // Unterminated marker: emit the rest verbatim.
            out.push_str(rest);
            return out;
        };
        let style = if marker == "`" {
            Style::new().fg(Color::Purple)
        } else {
            Style::new().bold()
        };
        out.push_str(&rest[..start]);
        out.push_str(&paint(style, &rest[body_start..body_start + len]));
        rest = &rest[body_start + len + marker.len()..];
    }
}
