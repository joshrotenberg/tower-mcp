//! Command aliases: short names for frequent commands, kept in the same
//! config file as the server profiles.
//!
//! ```toml
//! [aliases]                       # every server sees these
//! t = "tools"
//! d = "describe"
//!
//! [servers.cratesio.aliases]      # only when connected as `cratesio`
//! dl = "get_downloads crate"
//! ```
//!
//! Expansion is a literal substitution of the first word, with whatever
//! followed the alias appended: with `dl = "get_downloads crate"`,
//! `dl =serde` runs `get_downloads crate=serde`. An expansion whose own
//! first word is an alias expands again; a cycle is reported rather than
//! looped.
//!
//! `alias`, `alias <name>=<expansion>`, and `unalias <name>` read and write
//! this file. Writes go through `toml_edit`, so comments, key order, and
//! formatting elsewhere in the file survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

use crate::BUILTINS;

/// Which table of the config file an alias lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The file-level `[aliases]` table: in effect against every server.
    Global,
    /// `[servers.<name>.aliases]`: in effect only through that profile.
    Profile(String),
}

impl Scope {
    /// How the scope is named in `alias` output and messages.
    pub fn label(&self) -> String {
        match self {
            Scope::Global => "global".to_string(),
            Scope::Profile(name) => format!("profile {name}"),
        }
    }
}

/// One alias as `alias` lists it.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub expansion: String,
    pub scope: Scope,
}

/// What a successful `alias`/`unalias` did. `warning` carries a persistence
/// failure: the alias still applies to this session, but the config file was
/// not updated, and saying so is better than pretending it was saved.
#[derive(Debug)]
pub struct Applied {
    pub scope: Scope,
    pub previous: Option<String>,
    pub warning: Option<String>,
}

/// The alias tables in effect for this session: the file's global table plus
/// the connected profile's, if any.
#[derive(Default)]
pub struct Aliases {
    global: BTreeMap<String, String>,
    profile: BTreeMap<String, String>,
    profile_name: Option<String>,
    path: Option<PathBuf>,
}

impl Aliases {
    pub fn new(
        global: BTreeMap<String, String>,
        profile: BTreeMap<String, String>,
        profile_name: Option<String>,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            global,
            profile,
            profile_name,
            path,
        }
    }

    /// The expansion for `name`, and where it came from. A profile alias
    /// shadows a global one of the same name: the narrower scope is the more
    /// deliberate one.
    pub fn lookup(&self, name: &str) -> Option<(&str, Scope)> {
        if let Some(expansion) = self.profile.get(name) {
            let scope = Scope::Profile(self.profile_name.clone().unwrap_or_default());
            return Some((expansion.as_str(), scope));
        }
        self.global.get(name).map(|e| (e.as_str(), Scope::Global))
    }

    /// Every alias in effect, profile ones first, each group by name. A
    /// global alias shadowed by a profile alias is omitted: listing it would
    /// suggest it applies.
    pub fn entries(&self) -> Vec<Entry> {
        let profile_scope = || Scope::Profile(self.profile_name.clone().unwrap_or_default());
        let mut out: Vec<Entry> = self
            .profile
            .iter()
            .map(|(name, expansion)| Entry {
                name: name.clone(),
                expansion: expansion.clone(),
                scope: profile_scope(),
            })
            .collect();
        out.extend(
            self.global
                .iter()
                .filter(|(name, _)| !self.profile.contains_key(*name))
                .map(|(name, expansion)| Entry {
                    name: name.clone(),
                    expansion: expansion.clone(),
                    scope: Scope::Global,
                }),
        );
        out
    }

    /// Expand a line whose first word is an alias. `Ok(None)` means the line
    /// names no alias and should run as typed.
    pub fn expand(&self, line: &str) -> Result<Option<String>, String> {
        let mut current = line.to_string();
        let mut seen: Vec<String> = Vec::new();
        loop {
            let trimmed = current.trim_start();
            let (first, rest) = match trimmed.find(char::is_whitespace) {
                Some(i) => (&trimmed[..i], trimmed[i..].trim_start()),
                None => (trimmed, ""),
            };
            let Some((expansion, _)) = self.lookup(first) else {
                break;
            };
            if seen.iter().any(|s| s == first) {
                seen.push(first.to_string());
                return Err(format!(
                    "alias `{}` expands in a cycle ({}); break it with `unalias {}`",
                    seen[0],
                    seen.join(" -> "),
                    seen[0]
                ));
            }
            seen.push(first.to_string());
            current = if rest.is_empty() {
                expansion.to_string()
            } else {
                format!("{expansion} {rest}")
            };
        }
        Ok((!seen.is_empty()).then_some(current))
    }

    /// Define or redefine an alias, and write it to the config file.
    /// `global` forces the file-level table; without it an alias defined
    /// while connected through a profile belongs to that profile.
    pub fn define(&mut self, name: &str, expansion: &str, global: bool) -> Result<Applied, String> {
        validate(name, expansion)?;
        let scope = self.write_scope(global);
        let table = self.table_mut(&scope);
        let previous = table.insert(name.to_string(), expansion.to_string());
        let warning = self
            .persist(|doc| set_in_document(doc, &scope, name, expansion))
            .err();
        Ok(Applied {
            scope,
            previous,
            warning,
        })
    }

    /// Remove an alias. Without `global`, the profile's own alias goes first;
    /// a global alias is only removed when the profile does not define one,
    /// so `unalias` undoes the definition that is actually in effect.
    pub fn remove(&mut self, name: &str, global: bool) -> Result<Applied, String> {
        let scope = if !global && self.profile.contains_key(name) {
            Scope::Profile(self.profile_name.clone().unwrap_or_default())
        } else if self.global.contains_key(name) {
            Scope::Global
        } else if !global && self.profile_name.is_some() {
            return Err(format!("no alias named `{name}`"));
        } else {
            return Err(format!("no global alias named `{name}`"));
        };
        let previous = self.table_mut(&scope).remove(name);
        let warning = self
            .persist(|doc| remove_from_document(doc, &scope, name))
            .err();
        Ok(Applied {
            scope,
            previous,
            warning,
        })
    }

    /// Where a definition goes when the command did not say.
    fn write_scope(&self, global: bool) -> Scope {
        match (&self.profile_name, global) {
            (Some(name), false) => Scope::Profile(name.clone()),
            _ => Scope::Global,
        }
    }

    fn table_mut(&mut self, scope: &Scope) -> &mut BTreeMap<String, String> {
        match scope {
            Scope::Global => &mut self.global,
            Scope::Profile(_) => &mut self.profile,
        }
    }

    /// Apply `edit` to the config file, read-modify-write. A REPL with no
    /// config path (no `$HOME`, no `--config`) keeps its aliases in memory
    /// for the session and says so.
    fn persist(
        &self,
        edit: impl FnOnce(&mut DocumentMut) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err(
                "no config file location (set $HOME or pass --config), so the alias applies to \
                 this session only"
                    .to_string(),
            );
        };
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut doc = source
            .parse::<DocumentMut>()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        edit(&mut doc)?;
        write_atomic(path, &doc.to_string()).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// An alias name has to be a single word that dispatch can recognize, and it
/// must not bury a built-in: expansion happens before dispatch, so an alias
/// named `help` would make `help` unreachable.
fn validate(name: &str, expansion: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("usage: alias <name>=<expansion>".to_string());
    }
    if name.chars().any(char::is_whitespace) {
        return Err(format!("alias name `{name}` cannot contain whitespace"));
    }
    if name.contains('=') {
        return Err(format!("alias name `{name}` cannot contain `=`"));
    }
    if let Some((builtin, _)) = BUILTINS.iter().find(|(b, _)| *b == name) {
        return Err(format!(
            "`{builtin}` is a built-in command, so an alias by that name would hide it"
        ));
    }
    if expansion.trim().is_empty() {
        return Err(format!(
            "alias `{name}` needs something to expand to (usage: alias {name}=<expansion>)"
        ));
    }
    Ok(())
}

/// Write to `path` through a temporary file in the same directory, so a
/// failure part-way leaves the existing config intact rather than truncated.
fn write_atomic(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Config file edits
// ---------------------------------------------------------------------------

/// The aliases table of a scope, as it is written in the file. Both spellings
/// are accepted: a standalone `[aliases]` table and an inline
/// `aliases = { t = "tools" }`.
enum AliasTable<'t> {
    Table(&'t mut Table),
    Inline(&'t mut InlineTable),
}

impl AliasTable<'_> {
    fn set(&mut self, name: &str, expansion: &str) {
        match self {
            AliasTable::Table(t) => t[name] = value(expansion),
            AliasTable::Inline(t) => {
                t.insert(name, expansion.into());
            }
        }
    }

    fn remove(&mut self, name: &str) -> bool {
        match self {
            AliasTable::Table(t) => t.remove(name).is_some(),
            AliasTable::Inline(t) => t.remove(name).is_some(),
        }
    }
}

/// Set `name` in the scope's aliases table, creating the table (and, for a
/// profile scope, nothing else: the profile table must already exist).
pub fn set_in_document(
    doc: &mut DocumentMut,
    scope: &Scope,
    name: &str,
    expansion: &str,
) -> Result<(), String> {
    let mut table = aliases_table(doc, scope, true)?
        .ok_or_else(|| format!("no `{}` table in the config file", scope.label()))?;
    table.set(name, expansion);
    Ok(())
}

/// Remove `name` from the scope's aliases table. An emptied table is left in
/// place: a comment above `[aliases]` is that table's decoration, so deleting
/// the table would delete the comment with it.
pub fn remove_from_document(
    doc: &mut DocumentMut,
    scope: &Scope,
    name: &str,
) -> Result<(), String> {
    let Some(mut table) = aliases_table(doc, scope, false)? else {
        return Ok(());
    };
    table.remove(name);
    Ok(())
}

fn aliases_table<'d>(
    doc: &'d mut DocumentMut,
    scope: &Scope,
    create: bool,
) -> Result<Option<AliasTable<'d>>, String> {
    let parent = match scope {
        Scope::Global => doc.as_table_mut(),
        Scope::Profile(profile) => {
            let Some(servers) = child_table(doc.as_table_mut(), "servers")? else {
                return Ok(None);
            };
            match child_table(servers, profile)? {
                Some(t) => t,
                None => return Ok(None),
            }
        }
    };
    if !parent.contains_key("aliases") {
        if !create {
            return Ok(None);
        }
        parent.insert("aliases", Item::Table(Table::new()));
    }
    match parent.get_mut("aliases") {
        Some(Item::Table(t)) => Ok(Some(AliasTable::Table(t))),
        Some(Item::Value(v)) if v.is_inline_table() => Ok(Some(AliasTable::Inline(
            v.as_inline_table_mut().expect("checked"),
        ))),
        Some(_) => Err("`aliases` in the config file is not a table".to_string()),
        None => Ok(None),
    }
}

/// A child table by key. Only the standalone table spelling is writable here:
/// an inline `servers = { ... }` would have to be rewritten wholesale, which
/// is the user's call, not the REPL's.
fn child_table<'t>(parent: &'t mut Table, key: &str) -> Result<Option<&'t mut Table>, String> {
    match parent.get_mut(key) {
        Some(Item::Table(t)) => Ok(Some(t)),
        Some(_) => Err(format!(
            "`{key}` in the config file is not a standalone table, so the alias cannot be written \
             into it; add it to the file-level [aliases] table with `alias --global` instead"
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn aliases(global: &[(&str, &str)], profile: &[(&str, &str)]) -> Aliases {
        let name = (!profile.is_empty()).then(|| "cratesio".to_string());
        Aliases::new(map(global), map(profile), name, None)
    }

    #[test]
    fn expansion_substitutes_the_first_word_and_keeps_the_rest() {
        let a = aliases(&[("dl", "get_downloads crate")], &[]);
        assert_eq!(
            a.expand("dl =serde").unwrap().as_deref(),
            Some("get_downloads crate =serde")
        );
        assert_eq!(
            a.expand("dl").unwrap().as_deref(),
            Some("get_downloads crate")
        );
    }

    #[test]
    fn a_line_that_names_no_alias_is_left_alone() {
        let a = aliases(&[("t", "tools")], &[]);
        assert_eq!(a.expand("tools").unwrap(), None);
        assert_eq!(a.expand("").unwrap(), None);
        // Only the first word is a candidate; an alias elsewhere is just text.
        assert_eq!(a.expand("describe t").unwrap(), None);
    }

    #[test]
    fn an_expansion_naming_another_alias_expands_again() {
        let a = aliases(&[("t", "tools"), ("lst", "t")], &[]);
        assert_eq!(a.expand("lst").unwrap().as_deref(), Some("tools"));
    }

    #[test]
    fn a_cycle_is_reported_rather_than_looped() {
        let a = aliases(&[("a", "b"), ("b", "a")], &[]);
        let err = a.expand("a x").unwrap_err();
        assert!(err.contains("cycle"), "{err}");
        assert!(err.contains("a -> b -> a"), "{err}");
        assert!(err.contains("unalias a"), "{err}");
    }

    #[test]
    fn a_trailing_ampersand_survives_expansion() {
        // The background marker is read after expansion, so an alias can
        // carry it (or a line can add it to an alias that does not).
        let a = aliases(&[("sa", "slow_add a=1 b=2 &")], &[]);
        assert_eq!(
            a.expand("sa").unwrap().as_deref(),
            Some("slow_add a=1 b=2 &")
        );
        let a = aliases(&[("sa", "slow_add a=1 b=2")], &[]);
        assert_eq!(
            a.expand("sa &").unwrap().as_deref(),
            Some("slow_add a=1 b=2 &")
        );
    }

    #[test]
    fn a_profile_alias_shadows_the_global_one() {
        let a = aliases(&[("t", "tools")], &[("t", "templates")]);
        assert_eq!(a.expand("t").unwrap().as_deref(), Some("templates"));
        assert_eq!(
            a.lookup("t").map(|(e, s)| (e.to_string(), s)),
            Some(("templates".to_string(), Scope::Profile("cratesio".into())))
        );
        // ... and the shadowed global is not listed as if it applied.
        let entries = a.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].expansion, "templates");
    }

    #[test]
    fn entries_list_profile_aliases_before_global_ones() {
        let a = aliases(&[("z", "tools"), ("a", "prompts")], &[("p", "templates")]);
        let names: Vec<String> = a.entries().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["p", "a", "z"]);
    }

    #[test]
    fn a_name_that_would_hide_a_builtin_is_refused() {
        let mut a = aliases(&[], &[]);
        let err = a.define("help", "tools", false).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
        assert!(a.entries().is_empty());
    }

    #[test]
    fn a_malformed_name_or_empty_expansion_is_refused() {
        let mut a = aliases(&[], &[]);
        assert!(
            a.define("two words", "tools", false)
                .unwrap_err()
                .contains("whitespace")
        );
        assert!(a.define("a=b", "tools", false).unwrap_err().contains('='));
        assert!(
            a.define("t", "  ", false)
                .unwrap_err()
                .contains("expand to")
        );
    }

    #[test]
    fn define_reports_the_scope_and_what_it_replaced() {
        let mut a = aliases(&[], &[("t", "tools")]);
        let applied = a.define("t", "templates", false).unwrap();
        assert_eq!(applied.scope, Scope::Profile("cratesio".into()));
        assert_eq!(applied.previous.as_deref(), Some("tools"));
        // No config path in these tests, so persistence warns rather than
        // silently claiming a save.
        assert!(applied.warning.is_some());
        assert_eq!(a.expand("t").unwrap().as_deref(), Some("templates"));
    }

    #[test]
    fn define_without_a_profile_lands_in_the_global_table() {
        let mut a = aliases(&[], &[]);
        assert_eq!(a.define("t", "tools", false).unwrap().scope, Scope::Global);
        // --global forces it there even when a profile is connected.
        let mut a = aliases(&[], &[("x", "tools")]);
        assert_eq!(a.define("t", "tools", true).unwrap().scope, Scope::Global);
    }

    #[test]
    fn unalias_removes_the_definition_in_effect() {
        let mut a = aliases(&[("t", "tools")], &[("t", "templates")]);
        assert_eq!(
            a.remove("t", false).unwrap().scope,
            Scope::Profile("cratesio".into())
        );
        // The global one is still there, and now in effect.
        assert_eq!(a.expand("t").unwrap().as_deref(), Some("tools"));
        assert_eq!(a.remove("t", false).unwrap().scope, Scope::Global);
        assert_eq!(a.expand("t").unwrap(), None);
    }

    #[test]
    fn unalias_global_does_not_reach_a_profile_alias() {
        let mut a = aliases(&[], &[("t", "templates")]);
        let err = a.remove("t", true).unwrap_err();
        assert!(err.contains("no global alias"), "{err}");
        assert_eq!(
            a.remove("nope", false).unwrap_err(),
            "no alias named `nope`"
        );
    }

    // -- config file edits --------------------------------------------------

    fn edited(source: &str, edit: impl FnOnce(&mut DocumentMut)) -> String {
        let mut doc = source.parse::<DocumentMut>().unwrap();
        edit(&mut doc);
        doc.to_string()
    }

    const WITH_PROFILE: &str = r#"# my servers
[servers.cratesio]
url = "https://cratesio-mcp.fly.dev/"  # the public one
"#;

    #[test]
    fn writing_an_alias_preserves_the_rest_of_the_file() {
        let out = edited(WITH_PROFILE, |doc| {
            set_in_document(doc, &Scope::Global, "t", "tools").unwrap()
        });
        assert!(out.contains("# my servers"), "{out}");
        assert!(out.contains("# the public one"), "{out}");
        assert!(out.contains("[aliases]"), "{out}");
        assert!(out.contains(r#"t = "tools""#), "{out}");
        // The result still parses as the config it claims to be.
        out.parse::<DocumentMut>().unwrap();
    }

    #[test]
    fn a_profile_alias_is_written_under_the_profile() {
        let out = edited(WITH_PROFILE, |doc| {
            set_in_document(
                doc,
                &Scope::Profile("cratesio".into()),
                "dl",
                "get_downloads",
            )
            .unwrap()
        });
        assert!(out.contains("[servers.cratesio.aliases]"), "{out}");
        assert!(out.contains(r#"dl = "get_downloads""#), "{out}");
    }

    #[test]
    fn a_profile_that_is_not_in_the_file_is_an_error_not_a_new_profile() {
        let mut doc = WITH_PROFILE.parse::<DocumentMut>().unwrap();
        let err =
            set_in_document(&mut doc, &Scope::Profile("absent".into()), "t", "tools").unwrap_err();
        assert!(err.contains("absent"), "{err}");
    }

    #[test]
    fn redefining_replaces_the_value_in_place() {
        let out = edited("[aliases]\nt = \"tools\"\nd = \"describe\"\n", |doc| {
            set_in_document(doc, &Scope::Global, "t", "templates").unwrap()
        });
        assert!(out.contains(r#"t = "templates""#), "{out}");
        assert!(out.contains(r#"d = "describe""#), "{out}");
        assert!(!out.contains(r#""tools""#), "{out}");
    }

    #[test]
    fn an_inline_aliases_table_is_edited_in_place() {
        let out = edited(
            "[servers.x]\nurl = \"https://example/mcp\"\naliases = { t = \"tools\" }\n",
            |doc| {
                set_in_document(doc, &Scope::Profile("x".into()), "d", "describe").unwrap();
            },
        );
        // Still inline, now with both aliases in it.
        assert!(out.contains("aliases = {"), "{out}");
        assert!(out.contains(r#"t = "tools""#), "{out}");
        assert!(out.contains(r#"d = "describe""#), "{out}");
    }

    #[test]
    fn removing_the_last_alias_empties_the_table_but_keeps_its_comment() {
        // The comment above `[aliases]` belongs to that table, so removing
        // the table would take the comment with it. The stub stays instead.
        let out = edited("# keep me\n[aliases]\nt = \"tools\"\n", |doc| {
            remove_from_document(doc, &Scope::Global, "t").unwrap()
        });
        assert!(!out.contains(r#"t = "tools""#), "{out}");
        assert!(out.contains("# keep me"), "{out}");
        assert!(
            crate::config::Config::parse(&out)
                .unwrap()
                .aliases
                .is_empty(),
            "the emptied table should read back as no aliases: {out}"
        );

        // A table with other aliases left in it keeps them.
        let out = edited("[aliases]\nt = \"tools\"\nd = \"describe\"\n", |doc| {
            remove_from_document(doc, &Scope::Global, "t").unwrap()
        });
        assert!(out.contains("[aliases]"), "{out}");
        assert!(out.contains(r#"d = "describe""#), "{out}");
    }

    #[test]
    fn removing_from_a_file_without_the_table_is_not_an_error() {
        let out = edited(WITH_PROFILE, |doc| {
            remove_from_document(doc, &Scope::Global, "t").unwrap();
            remove_from_document(doc, &Scope::Profile("cratesio".into()), "t").unwrap();
            remove_from_document(doc, &Scope::Profile("absent".into()), "t").unwrap();
        });
        assert_eq!(out, WITH_PROFILE);
    }

    #[test]
    fn a_full_round_trip_through_a_real_file() {
        let dir = std::env::temp_dir().join(format!("mcp-repl-alias-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = Aliases::new(BTreeMap::new(), BTreeMap::new(), None, Some(path.clone()));

        // The config file does not exist yet: defining creates it.
        let applied = a.define("t", "tools", false).unwrap();
        assert!(applied.warning.is_none(), "{:?}", applied.warning);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[aliases]"), "{written}");
        assert!(written.contains(r#"t = "tools""#), "{written}");

        // And a later session reads it back as the same alias.
        let config = crate::config::Config::parse(&written).unwrap();
        assert_eq!(config.aliases.get("t").map(String::as_str), Some("tools"));

        a.remove("t", false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            crate::config::Config::parse(&written)
                .unwrap()
                .aliases
                .is_empty(),
            "{written}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
