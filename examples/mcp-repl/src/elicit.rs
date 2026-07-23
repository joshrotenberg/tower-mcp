//! Terminal elicitation: when a server-side tool calls `elicitation/create`,
//! prompt the user for the requested fields on stdin.
//!
//! This works because during a foreground tool call the readline thread is
//! parked on the ack channel, not reading stdin, so plain blocking reads
//! are safe. If the editor currently owns the terminal (a background task
//! elicited while the user sits at the prompt), the request is declined
//! rather than fighting reedline for raw-mode stdin.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use nu_ansi_term::{Color, Style};
use tower_mcp::client::{ClientHandler, NotificationHandler, ServerNotification};
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::{
    ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitRequestParams, ElicitResult,
    PrimitiveSchemaDefinition,
};

use crate::style::{paint, tag};

/// Client handler that layers terminal elicitation on top of the
/// notification callbacks.
pub struct ReplClientHandler {
    notifications: NotificationHandler,
    at_prompt: Arc<AtomicBool>,
}

impl ReplClientHandler {
    pub fn new(notifications: NotificationHandler, at_prompt: Arc<AtomicBool>) -> Self {
        Self {
            notifications,
            at_prompt,
        }
    }
}

#[async_trait]
impl ClientHandler for ReplClientHandler {
    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        match params {
            ElicitRequestParams::Url(url) => {
                println!(
                    "{} {}",
                    tag(Style::new().fg(Color::Purple), "elicit"),
                    url.message
                );
                println!("  open: {}", paint(Style::new().underline(), &url.url));
                Ok(ElicitResult {
                    action: ElicitAction::Accept,
                    content: None,
                    meta: None,
                })
            }
            ElicitRequestParams::Form(form) => {
                if self.at_prompt.load(Ordering::SeqCst) {
                    // The editor owns the terminal in raw mode; a second
                    // stdin reader would corrupt it. Decline.
                    println!(
                        "\n{} declined a form request from the server (arrived while at the \
                         prompt; run the tool in the foreground to answer it)",
                        tag(Style::new().fg(Color::Purple), "elicit"),
                    );
                    return Ok(ElicitResult::decline());
                }
                // Blocking stdin reads must leave the async runtime.
                tokio::task::spawn_blocking(move || prompt_form(&form))
                    .await
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()))
            }
            _ => Ok(ElicitResult::decline()),
        }
    }

    async fn on_notification(&self, notification: ServerNotification) {
        self.notifications.on_notification(notification).await;
    }
}

/// Prompt for each field of a form elicitation on stdin. EOF at any point
/// cancels. Empty input picks the default when one exists, otherwise skips
/// optional fields.
fn prompt_form(form: &ElicitFormParams) -> ElicitResult {
    println!(
        "{} {}",
        tag(Style::new().fg(Color::Purple), "elicit"),
        form.message
    );
    let mut content: HashMap<String, ElicitFieldValue> = HashMap::new();
    let mut names: Vec<&String> = form.requested_schema.properties.keys().collect();
    names.sort();
    for name in names {
        let schema = &form.requested_schema.properties[name];
        let required = form.requested_schema.required.iter().any(|r| r == name);
        let (ty, detail, default) = describe_field(schema);
        let mut prompt_line = format!("  {} ({ty}", paint(Style::new().fg(Color::Cyan), name));
        if required {
            prompt_line.push_str(", required");
        }
        if let Some(d) = &default {
            prompt_line.push_str(&format!(", default {d}"));
        }
        prompt_line.push(')');
        if let Some(detail) = detail {
            prompt_line.push_str(&format!(" {}", paint(Style::new().dimmed(), &detail)));
        }
        println!("{prompt_line}");
        loop {
            print!("  {name}> ");
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            let read = {
                let mut lock = std::io::stdin().lock();
                std::io::BufRead::read_line(&mut lock, &mut buf)
            };
            match read {
                Ok(0) | Err(_) => return ElicitResult::cancel(),
                Ok(_) => {}
            }
            let raw = buf.trim();
            if raw.is_empty() {
                match (&default, required) {
                    (Some(d), _) => {
                        content.insert(name.clone(), coerce_field(schema, d));
                        break;
                    }
                    (None, false) => break,
                    (None, true) => {
                        println!("  (required)");
                        continue;
                    }
                }
            }
            content.insert(name.clone(), coerce_field(schema, raw));
            break;
        }
    }
    ElicitResult::accept(content)
}

/// Type label, optional detail (description or enum choices), and default
/// value for a field schema.
fn describe_field(schema: &PrimitiveSchemaDefinition) -> (String, Option<String>, Option<String>) {
    match schema {
        PrimitiveSchemaDefinition::String(s) => (
            "string".to_string(),
            s.description.clone(),
            s.default.clone(),
        ),
        PrimitiveSchemaDefinition::Integer(s) => (
            "integer".to_string(),
            s.description.clone(),
            s.default.map(|d| d.to_string()),
        ),
        PrimitiveSchemaDefinition::Number(s) => (
            "number".to_string(),
            s.description.clone(),
            s.default.map(|d| d.to_string()),
        ),
        PrimitiveSchemaDefinition::Boolean(s) => (
            "boolean".to_string(),
            s.description.clone(),
            s.default.map(|d| d.to_string()),
        ),
        PrimitiveSchemaDefinition::SingleSelectEnum(s) => (
            format!("one of {}", s.enum_values.join("|")),
            s.description.clone(),
            s.default.clone(),
        ),
        PrimitiveSchemaDefinition::MultiSelectEnum(s) => (
            "comma-separated list".to_string(),
            s.description.clone(),
            None,
        ),
        _ => ("value".to_string(), None, None),
    }
}

/// Coerce raw input to the field's declared type, falling back to the raw
/// string when parsing fails (the server validates anyway).
fn coerce_field(schema: &PrimitiveSchemaDefinition, raw: &str) -> ElicitFieldValue {
    match schema {
        PrimitiveSchemaDefinition::Integer(_) => raw
            .parse::<i64>()
            .map(ElicitFieldValue::Integer)
            .unwrap_or_else(|_| ElicitFieldValue::String(raw.to_string())),
        PrimitiveSchemaDefinition::Number(_) => raw
            .parse::<f64>()
            .map(ElicitFieldValue::Number)
            .unwrap_or_else(|_| ElicitFieldValue::String(raw.to_string())),
        PrimitiveSchemaDefinition::Boolean(_) => match raw.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => ElicitFieldValue::Boolean(true),
            "false" | "no" | "n" | "0" => ElicitFieldValue::Boolean(false),
            _ => ElicitFieldValue::String(raw.to_string()),
        },
        PrimitiveSchemaDefinition::MultiSelectEnum(_) => {
            ElicitFieldValue::StringArray(raw.split(',').map(|s| s.trim().to_string()).collect())
        }
        _ => ElicitFieldValue::String(raw.to_string()),
    }
}
