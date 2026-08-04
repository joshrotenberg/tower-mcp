//! Import named servers from the common JSON configuration used by MCP
//! clients such as Claude, Cursor, and VS Code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Connection;

/// An explicit `PATH:ENTRY` selector recognized by the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub path: PathBuf,
    pub entry: String,
}

/// A resolved imported server plus its display label.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportedConnection {
    pub selector: Selector,
    pub connection: Connection,
}

impl ImportedConnection {
    pub fn label(&self) -> String {
        format!("{}:{}", self.selector.path.display(), self.selector.entry)
    }
}

/// Recognize an explicit JSON config selector without stealing ordinary
/// executable names containing `:`. A `.json` suffix is explicit even when
/// the file is missing, so the user gets a file error rather than attempting
/// to spawn the whole selector as a command.
pub fn parse_selector(value: &str) -> Option<Result<Selector, String>> {
    let (path, entry) = value.rsplit_once(':')?;
    let path = PathBuf::from(path);
    let looks_like_json = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    if !looks_like_json && !path.exists() {
        return None;
    }
    if path.as_os_str().is_empty() {
        return Some(Err("import selector has an empty file path".to_string()));
    }
    if entry.is_empty() {
        return Some(Err(format!(
            "import selector for {} has an empty entry name",
            path.display()
        )));
    }
    Some(Ok(Selector {
        path,
        entry: entry.to_string(),
    }))
}

/// Read and resolve one selected server. Environment values are supplied by
/// the caller so tests never mutate the process environment.
pub fn load_with(
    selector: Selector,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ImportedConnection, String> {
    let source = std::fs::read_to_string(&selector.path)
        .map_err(|error| format!("{}: {error}", selector.path.display()))?;
    let path = std::fs::canonicalize(&selector.path)
        .map_err(|error| format!("{}: {error}", selector.path.display()))?;
    let connection = parse_document(&source, &path, &selector.entry, &lookup)?;
    Ok(ImportedConnection {
        selector: Selector {
            path,
            entry: selector.entry,
        },
        connection,
    })
}

#[derive(Debug, Default, Deserialize)]
struct Document {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, Entry>,
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
}

#[derive(Debug, Default, Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportedTransport {
    Http,
    Stdio,
}

fn parse_document(
    source: &str,
    path: &Path,
    selected: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Connection, String> {
    let document: Document = serde_json::from_str(source)
        .map_err(|error| format!("{}: invalid MCP JSON config: {error}", path.display()))?;
    let mut entries = document.mcp_servers;
    for (name, entry) in document.servers {
        if entries.insert(name.clone(), entry).is_some() {
            return Err(format!(
                "{} defines server {name:?} in both `mcpServers` and `servers`",
                path.display()
            ));
        }
    }
    let entry = entries.get(selected).ok_or_else(|| {
        let available = entries.keys().cloned().collect::<Vec<_>>();
        if available.is_empty() {
            format!(
                "{} has no entries under `mcpServers` or `servers`",
                path.display()
            )
        } else {
            format!(
                "{} has no server named {selected:?}; available servers: {}",
                path.display(),
                available.join(", ")
            )
        }
    })?;
    resolve_entry(entry, path, selected, lookup)
}

fn resolve_entry(
    entry: &Entry,
    path: &Path,
    name: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Connection, String> {
    let workspace = workspace_folder(path);
    let declared = match (&entry.kind, &entry.transport) {
        (Some(kind), Some(transport)) => {
            let kind = parse_transport(kind)?;
            let transport = parse_transport(transport)?;
            if kind != transport {
                return Err(format!(
                    "server {name:?} has conflicting `type` and `transport` values"
                ));
            }
            Some(kind)
        }
        (Some(kind), None) | (None, Some(kind)) => Some(parse_transport(kind)?),
        (None, None) => None,
    };
    let has_command = entry.command.is_some();
    let has_url = entry.url.is_some();
    let transport = match (declared, has_command, has_url) {
        (Some(transport), _, _) => transport,
        (None, true, false) => ImportedTransport::Stdio,
        (None, false, true) => ImportedTransport::Http,
        (None, true, true) => {
            return Err(format!(
                "server {name:?} sets both `command` and `url`; add `type` to choose a transport"
            ));
        }
        (None, false, false) => {
            return Err(format!(
                "server {name:?} has neither `command` nor `url`, so its transport cannot be inferred"
            ));
        }
    };

    match transport {
        ImportedTransport::Stdio => {
            if entry.url.is_some() || !entry.headers.is_empty() {
                return Err(format!(
                    "stdio server {name:?} also sets HTTP-only `url` or `headers`"
                ));
            }
            let command = entry
                .command
                .as_deref()
                .ok_or_else(|| format!("stdio server {name:?} has no `command`"))?;
            let mut command_and_args = Vec::with_capacity(entry.args.len() + 1);
            command_and_args.push(expand(command, &workspace, lookup)?);
            for argument in &entry.args {
                command_and_args.push(expand(argument, &workspace, lookup)?);
            }
            if command_and_args[0].is_empty() {
                return Err(format!("stdio server {name:?} has an empty `command`"));
            }
            let env = entry
                .env
                .iter()
                .map(|(key, value)| {
                    expand(value, &workspace, lookup).map(|value| (key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let cwd = entry
                .cwd
                .as_deref()
                .map(|cwd| expand(cwd, &workspace, lookup))
                .transpose()?
                .map(PathBuf::from)
                .map(|cwd| {
                    if cwd.is_absolute() {
                        cwd
                    } else {
                        workspace.join(cwd)
                    }
                });
            Ok(Connection::Stdio {
                command: command_and_args,
                env,
                cwd,
            })
        }
        ImportedTransport::Http => {
            if entry.command.is_some()
                || !entry.args.is_empty()
                || !entry.env.is_empty()
                || entry.cwd.is_some()
            {
                return Err(format!(
                    "HTTP server {name:?} also sets stdio-only `command`, `args`, `env`, or `cwd`"
                ));
            }
            let url = entry
                .url
                .as_deref()
                .ok_or_else(|| format!("HTTP server {name:?} has no `url`"))?;
            let headers = entry
                .headers
                .iter()
                .map(|(key, value)| {
                    expand(value, &workspace, lookup).map(|value| (key.clone(), value))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Connection::Http {
                url: expand(url, &workspace, lookup)?,
                bearer: None,
                headers,
            })
        }
    }
}

fn parse_transport(value: &str) -> Result<ImportedTransport, String> {
    match value.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "stdio" => Ok(ImportedTransport::Stdio),
        "http" | "streamablehttp" => Ok(ImportedTransport::Http),
        "sse" => Err(
            "transport `sse` is not supported; mcp-repl requires Streamable HTTP (`http`)"
                .to_string(),
        ),
        _ => Err(format!(
            "unsupported imported transport {value:?}; expected `stdio` or `http`"
        )),
    }
}

fn workspace_folder(config_path: &Path) -> PathBuf {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name().is_some_and(|name| name == ".vscode") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn expand(
    input: &str,
    workspace: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut rendered = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find('}') else {
            return Err("unterminated `${...}` substitution in imported config".to_string());
        };
        let variable = &after_open[..end];
        let replacement = match variable {
            "workspaceFolder" => workspace.to_string_lossy().into_owned(),
            "workspaceFolderBasename" => workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "userHome" => lookup("HOME")
                .or_else(|| lookup("USERPROFILE"))
                .ok_or_else(|| {
                    "`${userHome}` requires the HOME or USERPROFILE environment variable"
                        .to_string()
                })?,
            variable if variable.starts_with("input:") => {
                return Err(format!(
                    "`${{{variable}}}` requires interactive client input, which mcp-repl cannot import; use an environment variable instead"
                ));
            }
            variable => {
                let variable = variable.strip_prefix("env:").unwrap_or(variable);
                if variable.is_empty() {
                    return Err(
                        "imported config contains an empty environment substitution".to_string()
                    );
                }
                lookup(variable).ok_or_else(|| {
                    format!("imported config requires environment variable {variable:?}, but it is unset")
                })?
            }
        };
        rendered.push_str(&replacement);
        rest = &after_open[end + 1..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let values: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn parses_claude_stdio_shape_with_workspace_and_environment() {
        let source = r#"{
          "mcpServers": {
            "local": {
              "command": "${workspaceFolder}/bin/server",
              "args": ["--repo", "${workspaceFolderBasename}"],
              "env": {"API_TOKEN": "${env:HOST_TOKEN}"},
              "cwd": "work"
            }
          }
        }"#;
        let resolved = parse_document(
            source,
            Path::new("/repo/.mcp.json"),
            "local",
            &env(&[("HOST_TOKEN", "secret")]),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Connection::Stdio {
                command: vec![
                    "/repo/bin/server".to_string(),
                    "--repo".to_string(),
                    "repo".to_string(),
                ],
                env: BTreeMap::from([("API_TOKEN".to_string(), "secret".to_string())]),
                cwd: Some(PathBuf::from("/repo/work")),
            }
        );
    }

    #[test]
    fn parses_vscode_http_shape_and_uses_workspace_parent() {
        let source = r#"{
          "servers": {
            "remote": {
              "type": "streamable-http",
              "url": "${env:MCP_URL}",
              "headers": {"Authorization": "Bearer ${TOKEN}"}
            }
          }
        }"#;
        let resolved = parse_document(
            source,
            Path::new("/repo/.vscode/mcp.json"),
            "remote",
            &env(&[("MCP_URL", "https://example/mcp"), ("TOKEN", "secret")]),
        )
        .unwrap();
        assert_eq!(
            resolved,
            Connection::Http {
                url: "https://example/mcp".to_string(),
                bearer: None,
                headers: vec![("Authorization".to_string(), "Bearer secret".to_string())],
            }
        );
    }

    #[test]
    fn missing_entry_lists_sorted_names() {
        let error = parse_document(
            r#"{"mcpServers":{"z":{"command":"z"},"a":{"command":"a"}}}"#,
            Path::new("/repo/.mcp.json"),
            "missing",
            &env(&[]),
        )
        .unwrap_err();
        assert!(error.contains("a, z"), "{error}");
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_transports() {
        let ambiguous = parse_document(
            r#"{"mcpServers":{"x":{"command":"x","url":"https://example"}}}"#,
            Path::new("/repo/.mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(ambiguous.contains("both"), "{ambiguous}");

        let sse = parse_document(
            r#"{"servers":{"x":{"type":"sse","url":"https://example"}}}"#,
            Path::new("/repo/mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(sse.contains("Streamable HTTP"), "{sse}");
    }

    #[test]
    fn missing_substitutions_name_the_variable_without_leaking_values() {
        let error = parse_document(
            r#"{
              "mcpServers": {
                "x": {
                  "command": "server",
                  "args": ["${env:MISSING}"],
                  "env": {"LITERAL_SECRET": "do-not-print-me"}
                }
              }
            }"#,
            Path::new("/repo/.mcp.json"),
            "x",
            &env(&[]),
        )
        .unwrap_err();
        assert!(error.contains("MISSING"), "{error}");
        assert!(!error.contains("do-not-print-me"), "{error}");
    }

    #[test]
    fn interactive_input_substitutions_are_actionable_errors() {
        let error = expand("${input:token}", Path::new("/repo"), &env(&[])).unwrap_err();
        assert!(error.contains("interactive"), "{error}");
        assert!(error.contains("environment variable"), "{error}");
    }

    #[test]
    fn selector_recognition_does_not_steal_ordinary_commands() {
        assert!(parse_selector("registry:serve").is_none());
        assert_eq!(
            parse_selector("path/to/.mcp.json:server").unwrap().unwrap(),
            Selector {
                path: PathBuf::from("path/to/.mcp.json"),
                entry: "server".to_string(),
            }
        );
    }
}
