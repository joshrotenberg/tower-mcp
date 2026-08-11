//! Every dependency declaration naming one of this workspace's own crates has
//! to name the current release line.
//!
//! The version appears in a dozen or so places outside the manifests: the
//! README install snippets, the crate-level docs, and the long-form guides all
//! show a `[dependencies]` block for a reader to copy. Only one of them used to
//! be guarded, by codegen-mcp's `TOWER_MCP_RELEASE_LINE` constant, and the rest
//! were found by hand every release (#1264). Missing one ships documentation
//! telling a user to depend on a line that is no longer current.
//!
//! This walks the repository and reports all of them at once, so a release that
//! moves the line gets a single list of what to edit instead of a scavenger
//! hunt.
//!
//! Only literal dependency declarations count, in either TOML spelling: the
//! bare version string and the table with a `version` key. A `workspace = true`
//! or bare `path` entry carries no version, a format-string placeholder is not
//! a literal, and prose that merely mentions a version is not a declaration.
//! That last one is deliberate. If a migration guide ever needs to show an old
//! snippet, write it as prose rather than as a copyable dependency line,
//! because a code block a reader can paste should never carry a stale version.

use std::path::{Path, PathBuf};

/// Crates published from this workspace.
///
/// A declaration of any of them has to track the workspace version, because
/// they are all released together off one `[workspace.package] version`.
const OWN_CRATES: &[&str] = &["tower-mcp", "tower-mcp-types", "tower-mcp-macros"];

/// Extensions worth reading.
///
/// `.toml` covers the manifests, `.md` the README, and `.rs` the doc comments
/// and guides, which is where all of the copyable snippets live.
const SCANNED_EXTENSIONS: &[&str] = &["rs", "md", "toml"];

/// Files whose version references are correct as written and must not be
/// rewritten to the current line.
///
/// The changelog is a historical record: every release line named in it belongs
/// to the release it describes.
const HISTORICAL: &[&str] = &["CHANGELOG.md"];

/// Whether a file is exempt from the scan.
///
/// This file is exempt as well, and not by name: the declarations in the
/// `parsing` fixtures below are illustrations of the syntax, not snippets a
/// reader would copy, and a release that moved the line should not be sent to
/// edit the parser's test data.
fn is_exempt(name: &str) -> bool {
    HISTORICAL.contains(&name)
        || Path::new(file!())
            .file_name()
            .is_some_and(|own| own == name)
}

#[test]
fn dependency_declarations_name_the_current_release_line() {
    let Some(root) = workspace_root() else {
        // The repository tree lives outside this package, so it is absent from
        // the published archive. Nothing to check there.
        return;
    };

    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
    let workspace_version = workspace_version(&manifest);
    let expected = release_line(&workspace_version)
        .unwrap_or_else(|| panic!("workspace version {workspace_version} is not major.minor"));

    let mut checked = Vec::new();
    let mut offenders = Vec::new();
    for file in scannable_files(&root) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for found in declarations(&source) {
            let at = format!("{relative}:{}", found.line);
            match release_line(&found.requirement) {
                Some(line) if line == expected => checked.push(at),
                _ => offenders.push(format!(
                    "  {at}: {} names {}, expected {expected}",
                    found.name, found.requirement
                )),
            }
        }
    }

    // A scan that silently matches nothing would pass forever. The README
    // install snippet is the reference this exists to protect, so its absence
    // means the parser broke rather than that the drift is gone.
    assert!(
        checked
            .iter()
            .chain(&offenders)
            .any(|at| at.contains("README.md")),
        "no dependency declaration found in README.md, so the scan is broken \
         rather than clean. Check declarations() against the install snippet."
    );

    assert!(
        offenders.is_empty(),
        "dependency declarations drifted from the {expected} release line:\n{}\n\n\
         The expected line is the major.minor of `[workspace.package] version` \
         in the root Cargo.toml ({workspace_version}). Every declaration above \
         is a snippet a reader can copy, so each one has to name {expected}. \
         Update them, and the codegen-mcp TOWER_MCP_RELEASE_LINE constant with \
         them. {} other declarations already match.",
        offenders.join("\n"),
        checked.len(),
    );
}

/// Repository root, if this checkout has one.
fn workspace_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.join("Cargo.toml")
        .is_file()
        .then(|| root.canonicalize().unwrap_or(root))
}

/// The `[workspace.package] version` of the root manifest.
fn workspace_version(manifest: &str) -> String {
    manifest
        .split_once("[workspace.package]")
        .expect("workspace package section")
        .1
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .find_map(|line| quoted_value(line.trim().strip_prefix("version")?))
        .expect("workspace package version")
        .to_string()
}

/// Every file under `root` worth scanning.
///
/// Build output, the git directory, and the other dot directories are skipped;
/// `.claude/worktrees` in particular holds whole extra checkouts.
fn scannable_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    pending.push(path);
                }
            } else if SCANNED_EXTENSIONS
                .iter()
                .any(|ext| path.extension().is_some_and(|found| found == *ext))
                && !is_exempt(&name)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A dependency declaration naming one of this workspace's crates.
#[derive(Debug, PartialEq, Eq)]
struct Declaration {
    /// One-based line of the version literal, which is the line to edit.
    line: usize,
    name: String,
    /// The literal as written, comparator and all: `0.21`, `^0.21.1`.
    requirement: String,
}

/// Every dependency declaration in `source` that names one of our crates with
/// a literal version.
///
/// Both TOML spellings count, and a leading `//!` makes no difference, because
/// most of these live inside doc-comment code fences.
fn declarations(source: &str) -> Vec<Declaration> {
    let mut found = Vec::new();
    for name in OWN_CRATES {
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(name) {
            let start = cursor + offset;
            cursor = start + name.len();

            // `my-tower-mcp` and `tower-mcp-types` are different crates, so the
            // name has to stand alone on both sides.
            if source[..start]
                .chars()
                .next_back()
                .is_some_and(is_name_char)
            {
                continue;
            }
            let rest = &source[cursor..];
            if rest.starts_with(is_name_char) {
                continue;
            }

            let Some((offset, requirement)) = requirement_at(rest) else {
                continue;
            };
            // Not a literal: codegen's templates spell the value as a
            // format-string placeholder, and a wildcard names no line at all.
            if !strip_comparator(requirement).starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            found.push(Declaration {
                line: line_of(source, cursor + offset),
                name: (*name).to_string(),
                requirement: requirement.to_string(),
            });
        }
    }
    found.sort_by_key(|found| found.line);
    found
}

/// The version requirement of the dependency value that follows a crate name,
/// with the offset of the literal within `rest` for blame.
///
/// `rest` starts just past the name. A table without a `version` key, which is
/// how every in-workspace `workspace = true` and `path` entry is written,
/// declares no version and is not a reference that can drift.
fn requirement_at(rest: &str) -> Option<(usize, &str)> {
    let (assigned_offset, assigned) = past(rest, '=')?;
    if assigned.starts_with('"') {
        return quoted(assigned).map(|literal| (assigned_offset, literal));
    }
    let group = brace_group(assigned)?;
    let (offset, literal) = version_key(group)?;
    // +1 for the brace itself.
    Some((assigned_offset + 1 + offset, literal))
}

/// The `version = "..."` entry of a dependency table, with the offset of the
/// literal within `group`.
fn version_key(group: &str) -> Option<(usize, &str)> {
    const KEY: &str = "version";

    let mut cursor = 0;
    while let Some(offset) = group[cursor..].find(KEY) {
        let start = cursor + offset;
        cursor = start + KEY.len();

        // `package_version = ".."` is a different key.
        if group[..start].chars().next_back().is_some_and(is_name_char) {
            continue;
        }
        let Some((offset, assigned)) = past(&group[cursor..], '=') else {
            continue;
        };
        if let Some(literal) = quoted(assigned) {
            return Some((cursor + offset, literal));
        }
    }
    None
}

/// What follows the next `delimiter` in `s`, with its offset, once the spacing
/// around the delimiter is skipped.
///
/// A key and its `=` always share a line in TOML, so the skipping stops at a
/// newline. Without that, a heading underlined with `===` in prose would read
/// as an assignment to whatever word preceded it.
fn past(s: &str, delimiter: char) -> Option<(usize, &str)> {
    const SPACING: [char; 2] = [' ', '\t'];

    let value = s.trim_start_matches(SPACING).strip_prefix(delimiter)?;
    let rest = value.trim_start_matches(SPACING);
    Some((s.len() - rest.len(), rest))
}

/// Contents of the double-quoted string at the head of `s`.
fn quoted(s: &str) -> Option<&str> {
    let body = s.strip_prefix('"')?;
    body.find('"').map(|end| &body[..end])
}

/// The value of a `key = "value"` line, whatever the key.
fn quoted_value(assigned: &str) -> Option<&str> {
    quoted(past(assigned, '=')?.1)
}

/// Contents of the balanced `{...}` starting at `s`, if `s` starts with one.
fn brace_group(s: &str) -> Option<&str> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    for (index, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[1..index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `major.minor` line a version or version requirement names.
///
/// `None` when it names no single line, which a bare `1` and a multi-comparator
/// range both do. Reporting those as a mismatch is right: this repository
/// writes every declaration as a plain `major.minor`.
fn release_line(requirement: &str) -> Option<String> {
    let mut components = strip_comparator(requirement).split('.');
    let major = leading_digits(components.next()?)?;
    let minor = leading_digits(components.next()?)?;
    Some(format!("{major}.{minor}"))
}

/// `requirement` with any leading semver comparator removed.
fn strip_comparator(requirement: &str) -> &str {
    requirement.trim_start_matches(['^', '~', '=', '>', '<', ' '])
}

/// The digits at the head of `s`, if it starts with one.
fn leading_digits(s: &str) -> Option<&str> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (end > 0).then(|| &s[..end])
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// One-based line number of `offset` in `source`.
fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].lines().count().max(1)
}

mod parsing {
    use super::*;

    fn parsed(source: &str) -> Vec<(usize, String, String)> {
        declarations(source)
            .into_iter()
            .map(|found| (found.line, found.name, found.requirement))
            .collect()
    }

    fn one(source: &str) -> (usize, String, String) {
        let mut found = parsed(source);
        assert_eq!(found.len(), 1, "expected one declaration in {source:?}");
        found.remove(0)
    }

    #[test]
    fn a_bare_version_string_is_a_declaration() {
        assert_eq!(
            one(r#"tower-mcp = "0.21""#),
            (1, "tower-mcp".to_string(), "0.21".to_string())
        );
    }

    #[test]
    fn a_table_with_a_version_is_a_declaration() {
        assert_eq!(
            one(r#"tower-mcp = { version = "0.21", features = ["http"] }"#),
            (1, "tower-mcp".to_string(), "0.21".to_string())
        );
    }

    /// Nearly every snippet in the crate lives inside a doc-comment fence.
    #[test]
    fn a_doc_comment_prefix_makes_no_difference() {
        assert_eq!(
            one(r#"//! tower-mcp-types = "0.21""#),
            (1, "tower-mcp-types".to_string(), "0.21".to_string())
        );
    }

    #[test]
    fn a_table_without_a_version_declares_none() {
        assert!(parsed(r#"tower-mcp = { workspace = true, features = ["http"] }"#).is_empty());
        assert!(parsed(r#"tower-mcp = { path = "../../crates/tower-mcp" }"#).is_empty());
    }

    /// The value in codegen's generated manifest is a format-string
    /// placeholder, and the constant behind it is guarded where it is defined.
    #[test]
    fn a_placeholder_version_declares_none() {
        assert!(parsed(r#"tower-mcp = {{ version = "{release_line}" }}"#).is_empty());
        assert!(parsed(r#"tower-mcp = {{ version = \"{RELEASE_LINE}\" }}"#).is_empty());
        assert!(parsed(r#"tower-mcp = "*""#).is_empty());
    }

    /// `format!("tower-mcp={t}")` appears throughout the rmcp comparison
    /// harness and declares nothing.
    #[test]
    fn an_interpolated_name_is_not_a_declaration() {
        assert!(parsed(r#"format!("protocol mismatch: rmcp={r}, tower-mcp={t}")"#).is_empty());
    }

    #[test]
    fn a_longer_crate_name_is_not_the_shorter_one() {
        let found = parsed(r#"tower-mcp-types = "0.21""#);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "tower-mcp-types");
    }

    #[test]
    fn another_crate_with_a_matching_suffix_is_ignored() {
        assert!(parsed(r#"my-tower-mcp = "0.5""#).is_empty());
    }

    /// A path mentioning the crate sits inside a real declaration, so the scan
    /// sees the name twice and must report only the outer one.
    #[test]
    fn a_path_value_naming_the_crate_is_not_a_second_declaration() {
        assert!(
            parsed(r#"tower-mcp = { path = "../../crates/tower-mcp", features = [] }"#).is_empty()
        );
    }

    #[test]
    fn a_comparator_is_kept_but_does_not_hide_the_line() {
        let (_, _, requirement) = one(r#"tower-mcp = { version = "^0.21.1" }"#);
        assert_eq!(requirement, "^0.21.1");
        assert_eq!(release_line(&requirement).as_deref(), Some("0.21"));
    }

    #[test]
    fn the_line_number_points_at_the_version() {
        assert_eq!(
            one("[dependencies]\nserde = \"1\"\ntower-mcp = \"0.20\"\n").0,
            3
        );
    }

    /// A table spread over several lines should blame the line the version is
    /// on, not the line the crate name is on.
    #[test]
    fn a_multiline_table_blames_the_version_line() {
        assert_eq!(one("tower-mcp = {\n  version = \"0.21\",\n}").0, 2);
    }

    /// The feature list of the client guide runs past the version, so the
    /// closing brace is several lines down.
    #[test]
    fn a_trailing_multiline_list_does_not_hide_the_version() {
        let source = "tower-mcp = { version = \"0.21\", features = [\n  \"http-client\",\n] }";
        assert_eq!(
            one(source),
            (1, "tower-mcp".to_string(), "0.21".to_string())
        );
    }

    #[test]
    fn a_release_line_is_the_major_and_minor() {
        assert_eq!(release_line("0.21").as_deref(), Some("0.21"));
        assert_eq!(release_line("0.21.1").as_deref(), Some("0.21"));
        assert_eq!(release_line("=1.0.0").as_deref(), Some("1.0"));
        assert_eq!(release_line("1"), None);
    }

    #[test]
    fn the_workspace_version_comes_from_the_package_section() {
        let manifest = "[workspace]\nversion = \"9.9.9\"\n\n\
             [workspace.package]\nedition = \"2024\"\nversion = \"0.21.1\"\n\n\
             [workspace.dependencies]\nversion = \"0.0.0\"\n";
        assert_eq!(workspace_version(manifest), "0.21.1");
    }
}
