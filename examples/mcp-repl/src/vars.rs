//! Session variables, a minimal path selector, and capture/pipe routing (#1011).
//!
//! `name = <command>` binds a command's result JSON to `$name`; `$name.path`
//! references it in later command arguments; `<command> | <path>` filters a
//! result before printing. The path language is deliberately small (`.field`,
//! `[index]`, chained); JMESPath stays the future fuller option.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn store() -> &'static Mutex<HashMap<String, Value>> {
    static STORE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bind a variable to a value.
pub fn set(name: &str, value: Value) {
    store().lock().unwrap().insert(name.to_string(), value);
}

/// Look a variable up.
pub fn get(name: &str) -> Option<Value> {
    store().lock().unwrap().get(name).cloned()
}

/// Remove a variable; returns whether it existed.
pub fn unset(name: &str) -> bool {
    store().lock().unwrap().remove(name).is_some()
}

/// Every bound variable, sorted by name.
pub fn list() -> Vec<(String, Value)> {
    let store = store().lock().unwrap();
    let mut vars: Vec<(String, Value)> =
        store.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

/// Where a command's result should go, parsed from the line.
#[derive(Default)]
pub struct Output {
    /// Bind the result to this variable instead of printing it.
    pub capture: Option<String>,
    /// Select this path out of the result before capturing or printing.
    pub filter: Option<String>,
}

impl Output {
    /// No capture and no filter: render as usual.
    pub fn is_plain(&self) -> bool {
        self.capture.is_none() && self.filter.is_none()
    }
}

/// Split a line into its output routing and the command to run. Recognizes
/// `name = <command>` (capture) and `<command> | <path>` (pipe), in that order,
/// so `x = call foo | .id` captures the filtered value.
pub fn route(line: &str) -> (Output, &str) {
    let (capture, rest) = match split_capture(line) {
        Some((name, rest)) => (Some(name.to_string()), rest),
        None => (None, line),
    };
    let (command, filter) = match split_pipe(rest) {
        Some((cmd, path)) => (cmd.trim_end(), Some(path.trim().to_string())),
        None => (rest, None),
    };
    (Output { capture, filter }, command)
}

/// Find a routing pipe only at the command language's top level. Pipes inside
/// quoted strings or JSON object/array arguments belong to the tool input.
fn split_pipe(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut json_depth = 0usize;
    let mut json_string = false;
    let mut json_escaped = false;

    for (index, &byte) in bytes.iter().enumerate() {
        if json_depth > 0 {
            if json_string {
                if json_escaped {
                    json_escaped = false;
                } else if byte == b'\\' {
                    json_escaped = true;
                } else if byte == b'"' {
                    json_string = false;
                }
            } else {
                match byte {
                    b'"' => json_string = true,
                    b'{' | b'[' => json_depth += 1,
                    b'}' | b']' => json_depth -= 1,
                    _ => {}
                }
            }
            continue;
        }

        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
            }
            Some(b'"') => match byte {
                b'\\' => escaped = true,
                b'"' => quote = None,
                _ => {}
            },
            Some(_) => unreachable!("only quote bytes are stored"),
            None => match byte {
                b'\\' => escaped = true,
                b'\'' | b'"' => quote = Some(byte),
                b'{' | b'[' => json_depth = 1,
                b'|' if bytes.get(index.wrapping_sub(1)) == Some(&b' ')
                    && bytes.get(index + 1) == Some(&b' ') =>
                {
                    return Some((&line[..index - 1], &line[index + 2..]));
                }
                _ => {}
            },
        }
    }
    None
}

/// `name = rest` where `name` is an identifier, distinguishing capture from
/// `k=v` arguments (no surrounding spaces) and `alias name=...` (keyword first).
fn split_capture(line: &str) -> Option<(&str, &str)> {
    let (lhs, rhs) = line.split_once(" = ")?;
    is_ident(lhs).then_some((lhs, rhs.trim_start()))
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Evaluate a path like `crates[0].name` (leading `.` optional) against a value.
/// Returns `None` on any missing key or index. An empty path yields the value.
pub fn get_path(value: &Value, path: &str) -> Option<Value> {
    let mut cur = value;
    for seg in parse_path(path) {
        cur = match seg {
            Seg::Key(k) => cur.get(&k)?,
            Seg::Index(i) => cur.get(i)?,
        };
    }
    Some(cur.clone())
}

enum Seg {
    Key(String),
    Index(usize),
}

fn parse_path(path: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut rest = path.strip_prefix('.').unwrap_or(path);
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            match r
                .find(']')
                .and_then(|end| r[..end].parse().ok().map(|i| (i, end)))
            {
                Some((i, end)) => {
                    segs.push(Seg::Index(i));
                    rest = &r[end + 1..];
                }
                None => break,
            }
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            if end == 0 {
                break;
            }
            segs.push(Seg::Key(rest[..end].to_string()));
            rest = &rest[end..];
        }
        rest = rest.strip_prefix('.').unwrap_or(rest);
    }
    segs
}

/// Replace `$name` and `$name.path` references in `text` with their values.
/// A scalar inserts bare; an array or object inserts as compact JSON. A `$` not
/// followed by an identifier is left as-is. An undefined variable or a missing
/// path is an error.
pub fn substitute(text: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let starts_ident =
            matches!(after.chars().next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
        if !starts_ident {
            out.push('$');
            rest = after;
            continue;
        }
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let tail = &after[name_len..];
        let path_len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']')))
            .unwrap_or(tail.len());
        let path = &tail[..path_len];
        let value = get(name).ok_or_else(|| format!("undefined variable `${name}`"))?;
        let selected = get_path(&value, path)
            .ok_or_else(|| format!("`${name}{path}` not found in `{name}`"))?;
        out.push_str(&render_scalar(&selected));
        rest = &tail[path_len..];
    }
    out.push_str(rest);
    Ok(out)
}

fn render_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_selects_keys_and_indices() {
        let v = json!({ "crates": [{ "name": "serde" }, { "name": "tokio" }] });
        assert_eq!(get_path(&v, "crates[0].name"), Some(json!("serde")));
        assert_eq!(get_path(&v, ".crates[1].name"), Some(json!("tokio")));
        assert_eq!(get_path(&v, ""), Some(v.clone()));
        assert_eq!(get_path(&v, "crates[9].name"), None);
        assert_eq!(get_path(&v, "missing"), None);
    }

    #[test]
    fn route_recognizes_capture_and_pipe() {
        let (o, cmd) = route("x = search query=serde");
        assert_eq!(o.capture.as_deref(), Some("x"));
        assert_eq!(cmd, "search query=serde");

        let (o, cmd) = route("get_crate name=serde | crates[0].name");
        assert_eq!(o.filter.as_deref(), Some("crates[0].name"));
        assert_eq!(cmd, "get_crate name=serde");

        let (o, cmd) = route("y = call foo | .id");
        assert_eq!(o.capture.as_deref(), Some("y"));
        assert_eq!(o.filter.as_deref(), Some(".id"));
        assert_eq!(cmd, "call foo");

        // k=v args and alias definitions are not captures.
        assert!(route("get_crate name=serde").0.is_plain());
        assert!(route("query=serde").0.capture.is_none());
    }

    #[test]
    fn route_ignores_pipes_inside_arguments() {
        let (o, cmd) = route(r#"echo message="left | right""#);
        assert!(o.is_plain());
        assert_eq!(cmd, r#"echo message="left | right""#);

        let (o, cmd) = route(r#"call echo {"message":"left | right"} | .content"#);
        assert_eq!(o.filter.as_deref(), Some(".content"));
        assert_eq!(cmd, r#"call echo {"message":"left | right"}"#);
    }

    #[test]
    fn substitute_resolves_scalars_and_reports_misses() {
        set("x", json!({ "crates": [{ "name": "serde" }] }));
        set("n", json!(42));
        assert_eq!(
            substitute("get_crate name=$x.crates[0].name").unwrap(),
            "get_crate name=serde"
        );
        assert_eq!(substitute("bench t --n $n").unwrap(), "bench t --n 42");
        assert_eq!(
            substitute("a literal $5 sign").unwrap(),
            "a literal $5 sign"
        );
        assert!(substitute("$missing").is_err());
        assert!(substitute("$x.crates[9].name").is_err());
        unset("x");
        unset("n");
    }
}
