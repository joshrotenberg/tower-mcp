//! Server profiles: a config file of named servers, so a remote MCP server
//! can be reached as `mcp-repl <name>` instead of a URL plus repeated
//! `--bearer`/`--header` flags.
//!
//! The file lives at `$XDG_CONFIG_HOME/mcp-repl/config.toml`, falling back to
//! `~/.config/mcp-repl/config.toml`, and `--config <path>` overrides it:
//!
//! ```toml
//! [servers.cratesio]
//! transport = "http"
//! url = "https://cratesio-mcp.fly.dev/"
//! bearer_env = "CRATESIO_TOKEN"
//! headers = { "X-Api-Key" = "..." }
//!
//! [servers.local]
//! transport = "stdio"
//! command = ["cargo", "run", "--example", "getting_started"]
//!
//! [aliases]
//! t = "tools"
//! ```
//!
//! Command aliases live in the same file: `[aliases]` for every server, and
//! `[servers.<name>.aliases]` for one profile. See [`crate::alias`], which
//! also writes them back.
//!
//! Tokens are read from the environment via `bearer_env` rather than stored in
//! the file; an inline `bearer` literal works but warns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The whole config file: named profiles under `[servers.<name>]`, plus the
/// command aliases every server sees under `[aliases]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub servers: BTreeMap<String, Profile>,
    /// Command aliases in effect against every server. See [`crate::alias`].
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

/// One `[servers.<name>]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `http` or `stdio`. Optional: inferred from `url`/`command` when absent.
    pub transport: Option<Transport>,
    /// The endpoint for an `http` profile.
    pub url: Option<String>,
    /// An inline bearer token. Prefer `bearer_env`; this warns when used.
    pub bearer: Option<String>,
    /// Name of the environment variable holding the bearer token.
    pub bearer_env: Option<String>,
    /// Extra headers sent with every request of an `http` profile.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The command (and arguments) of a `stdio` profile's child process.
    #[serde(default)]
    pub command: Vec<String>,
    /// Command aliases in effect only through this profile. They shadow the
    /// file-level `[aliases]` of the same name.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

/// The transports a profile can name. `ws` and stateless HTTP are not
/// profile-addressable yet, so an unknown value is a config error rather than
/// a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Http,
    Stdio,
}

/// A profile resolved into everything needed to connect. Produced after the
/// CLI flags have had their say.
#[derive(Debug, PartialEq, Eq)]
pub enum Connection {
    Http {
        url: String,
        bearer: Option<String>,
        headers: Vec<(String, String)>,
    },
    Stdio {
        command: Vec<String>,
    },
}

impl Config {
    /// Parse a config file's contents.
    pub fn parse(source: &str) -> Result<Self, String> {
        toml::from_str(source).map_err(|e| e.to_string())
    }

    /// Read the config from `path`. A missing file is an error only when the
    /// path was explicitly requested (`--config`); the default location is
    /// allowed not to exist.
    pub fn load(path: &Path, explicit: bool) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(source) => Self::parse(&source).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Look up a profile by name, with an error listing the known names when
    /// it is missing.
    pub fn profile(&self, name: &str) -> Result<&Profile, String> {
        self.servers.get(name).ok_or_else(|| {
            if self.servers.is_empty() {
                format!("no server profile named {name:?}: no profiles are configured")
            } else {
                format!(
                    "no server profile named {name:?}: known profiles are {}",
                    self.names().join(", ")
                )
            }
        })
    }

    /// The configured profile names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }
}

impl Profile {
    /// The transport this profile connects over: the declared one, else
    /// inferred from whichever of `url`/`command` is present.
    pub fn transport(&self) -> Result<Transport, String> {
        match (self.transport, self.url.is_some(), !self.command.is_empty()) {
            (Some(t), _, _) => Ok(t),
            (None, true, false) => Ok(Transport::Http),
            (None, false, true) => Ok(Transport::Stdio),
            (None, true, true) => Err(
                "profile sets both `url` and `command`: add `transport = \"http\"` or \
                 `transport = \"stdio\"` to say which one applies"
                    .to_string(),
            ),
            (None, false, false) => {
                Err("profile has neither `url` nor `command`, so it cannot connect".to_string())
            }
        }
    }

    /// The bearer token for this profile: `bearer_env` read from `lookup`, or
    /// the inline `bearer`. A `bearer_env` naming an unset variable is an
    /// error, not a silent anonymous connection.
    pub fn bearer_token_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        if let Some(var) = &self.bearer_env {
            return lookup(var).map(Some).ok_or_else(|| {
                format!(
                    "profile sets `bearer_env = {var:?}` but that environment variable is unset"
                )
            });
        }
        Ok(self.bearer.clone())
    }

    /// Resolve into a [`Connection`], validating that the transport has the
    /// fields it needs.
    pub fn resolve_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Connection, String> {
        match self.transport()? {
            Transport::Http => {
                let url = self
                    .url
                    .clone()
                    .ok_or("profile has `transport = \"http\"` but no `url`")?;
                Ok(Connection::Http {
                    url,
                    bearer: self.bearer_token_with(lookup)?,
                    headers: self
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                })
            }
            Transport::Stdio => {
                if self.command.is_empty() {
                    return Err("profile has `transport = \"stdio\"` but no `command`".to_string());
                }
                Ok(Connection::Stdio {
                    command: self.command.clone(),
                })
            }
        }
    }

    /// A one-line summary for `--list-servers`.
    pub fn summary(&self) -> String {
        match self.transport() {
            Ok(Transport::Http) => format!("http   {}", self.url.as_deref().unwrap_or("(no url)")),
            Ok(Transport::Stdio) => format!("stdio  {}", self.command.join(" ")),
            Err(e) => format!("(invalid: {e})"),
        }
    }
}

/// The config file location: `--config` if given, else
/// `$XDG_CONFIG_HOME/mcp-repl/config.toml`, else `~/.config/mcp-repl/config.toml`.
/// The bool is true when the path was explicitly requested, which makes a
/// missing file an error.
pub fn config_path(explicit: Option<&str>) -> Option<(PathBuf, bool)> {
    if let Some(p) = explicit {
        return Some((PathBuf::from(p), true));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let mut home = PathBuf::from(std::env::var_os("HOME")?);
            home.push(".config");
            home
        }
    };
    Some((base.join("mcp-repl").join("config.toml"), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[servers.cratesio]
transport = "http"
url = "https://cratesio-mcp.fly.dev/"
bearer_env = "CRATESIO_TOKEN"
headers = { "X-Api-Key" = "abc" }

[servers.local]
transport = "stdio"
command = ["cargo", "run", "--example", "getting_started"]
"#;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn parses_named_profiles() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.names(), vec!["cratesio", "local"]);
    }

    #[test]
    fn http_profile_resolves_transport_and_auth() {
        let config = Config::parse(SAMPLE).unwrap();
        let resolved = config
            .profile("cratesio")
            .unwrap()
            .resolve_with(env(&[("CRATESIO_TOKEN", "secret")]))
            .unwrap();
        assert_eq!(
            resolved,
            Connection::Http {
                url: "https://cratesio-mcp.fly.dev/".to_string(),
                bearer: Some("secret".to_string()),
                headers: vec![("X-Api-Key".to_string(), "abc".to_string())],
            }
        );
    }

    #[test]
    fn stdio_profile_resolves_command() {
        let config = Config::parse(SAMPLE).unwrap();
        let resolved = config
            .profile("local")
            .unwrap()
            .resolve_with(env(&[]))
            .unwrap();
        assert_eq!(
            resolved,
            Connection::Stdio {
                command: vec![
                    "cargo".to_string(),
                    "run".to_string(),
                    "--example".to_string(),
                    "getting_started".to_string(),
                ],
            }
        );
    }

    #[test]
    fn unknown_profile_lists_known_names() {
        let config = Config::parse(SAMPLE).unwrap();
        let err = config.profile("nope").unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("cratesio, local"), "{err}");
    }

    #[test]
    fn unknown_profile_with_empty_config_says_so() {
        let err = Config::default().profile("nope").unwrap_err();
        assert!(err.contains("no profiles are configured"), "{err}");
    }

    #[test]
    fn unset_bearer_env_is_an_error() {
        let config = Config::parse(SAMPLE).unwrap();
        let err = config
            .profile("cratesio")
            .unwrap()
            .resolve_with(env(&[]))
            .unwrap_err();
        assert!(err.contains("CRATESIO_TOKEN"), "{err}");
    }

    #[test]
    fn inline_bearer_is_used_when_no_env_indirection() {
        let profile: Profile = toml::from_str(
            r#"
            url = "https://example/mcp"
            bearer = "literal"
            "#,
        )
        .unwrap();
        assert_eq!(
            profile.bearer_token_with(env(&[])).unwrap(),
            Some("literal".to_string())
        );
    }

    #[test]
    fn transport_is_inferred_from_the_fields() {
        let http: Profile = toml::from_str(r#"url = "https://example/mcp""#).unwrap();
        assert_eq!(http.transport().unwrap(), Transport::Http);
        let stdio: Profile = toml::from_str(r#"command = ["server"]"#).unwrap();
        assert_eq!(stdio.transport().unwrap(), Transport::Stdio);
    }

    #[test]
    fn ambiguous_and_empty_profiles_are_errors() {
        let both: Profile =
            toml::from_str("url = \"https://example/mcp\"\ncommand = [\"server\"]").unwrap();
        assert!(both.transport().unwrap_err().contains("both"));
        assert!(
            Profile::default()
                .transport()
                .unwrap_err()
                .contains("neither")
        );
    }

    #[test]
    fn declared_transport_must_have_its_fields() {
        let profile: Profile = toml::from_str(r#"transport = "http""#).unwrap();
        assert!(profile.resolve_with(env(&[])).unwrap_err().contains("url"));
        let profile: Profile = toml::from_str(r#"transport = "stdio""#).unwrap();
        assert!(
            profile
                .resolve_with(env(&[]))
                .unwrap_err()
                .contains("command")
        );
    }

    #[test]
    fn an_unsupported_transport_names_itself() {
        let err =
            Config::parse("[servers.x]\ntransport = \"ws\"\nurl = \"wss://example\"").unwrap_err();
        assert!(err.contains("ws"), "{err}");
    }

    #[test]
    fn aliases_parse_at_both_scopes() {
        let config = Config::parse(
            r#"
[aliases]
t = "tools"

[servers.cratesio]
url = "https://cratesio-mcp.fly.dev/"
aliases = { dl = "get_downloads crate" }
"#,
        )
        .unwrap();
        assert_eq!(config.aliases.get("t").map(String::as_str), Some("tools"));
        assert_eq!(
            config.servers["cratesio"]
                .aliases
                .get("dl")
                .map(String::as_str),
            Some("get_downloads crate")
        );
    }

    #[test]
    fn a_config_without_aliases_parses_to_none_of_them() {
        assert!(Config::parse(SAMPLE).unwrap().aliases.is_empty());
    }

    #[test]
    fn a_typo_in_a_profile_key_is_rejected() {
        let err =
            Config::parse("[servers.x]\nurl = \"https://example\"\nbearrer = \"x\"").unwrap_err();
        assert!(err.contains("bearrer"), "{err}");
    }
}
