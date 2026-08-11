//! The examples must import from the crate root, so they double as a smoke
//! test of the public API surface.
//!
//! #1240 was a missing `pub use` for `notification_channel`: every example
//! reached it as `tower_mcp::context::notification_channel`, the internal
//! path, so nothing ever exercised `tower_mcp::notification_channel` and the
//! gap survived until a user hit it. An example that only names crate-root
//! items fails to compile the day a re-export goes missing.
//!
//! Where an internal path is genuinely required, that is a signal the item
//! wants re-exporting from `lib.rs` (#1258).

use std::path::{Path, PathBuf};

/// Modules an example may import from directly.
///
/// These are namespace modules: the crate's own README and `lib.rs` doc
/// examples spell them as `tower_mcp::<module>::Item`, and their contents are
/// deliberately not flattened into the crate root.
///
/// Adding an entry here is a decision that the module is part of the public
/// namespace. The other option, and usually the right one, is to re-export
/// the item from `lib.rs` and import it as `tower_mcp::Item`. The list covers
/// what the examples currently need; a module missing from it is not
/// necessarily private, it just has not had to be decided yet.
const NAMESPACE_MODULES: &[&str] = &[
    // Protocol and JSON-RPC types. Several hundred of them, and the crate
    // root already re-exports the commonly used subset.
    "protocol",
    // Handler extractors: `Json`, `State`, `Context`, `Extension`, `RawArgs`.
    "extract",
    // Client API: `McpClient`, the client transports, the OAuth client flow.
    "client",
    // In-process test utilities, behind the `testing` feature.
    "testing",
    // API key and bearer authentication middleware, behind no feature.
    "auth",
    // OAuth 2.1 validators and metadata, behind the `oauth` feature.
    "oauth",
    // MCP proxy, behind the `proxy` feature.
    "proxy",
    // Pluggable storage for HTTP and WebSocket sessions and for SSE replay.
    // Each defines its own `Error` and `Result<T>` pair, and a crate-root
    // `Result` already means `Result<T, tower_mcp::Error>`, so these cannot be
    // flattened without a collision.
    "event_store",
    "session_store",
    // Long-form prose documentation. Contains no runtime items, so it can
    // only ever be named in a doc link.
    "guides",
];

#[test]
fn examples_import_from_the_crate_root() {
    let Some(examples) = examples_dir() else {
        // The `examples/` tree lives outside this package, so it is absent
        // from the published archive. Nothing to check there.
        return;
    };

    let mut offenders = Vec::new();
    for file in rust_sources(&examples) {
        let source = std::fs::read_to_string(&file).expect("read example source");
        let relative = file.strip_prefix(&examples).unwrap_or(&file).to_path_buf();
        for (line, module) in module_segments(&source) {
            if !NAMESPACE_MODULES.contains(&module.as_str()) {
                offenders.push(format!(
                    "  examples/{}:{line}: tower_mcp::{module}::",
                    relative.display()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "examples reached into internal module paths:\n{}\n\n\
         An example should name `tower_mcp::Item`, not \
         `tower_mcp::<module>::Item`. If the item is already re-exported from \
         crates/tower-mcp/src/lib.rs, switch the example to the crate-root \
         path. If it is not, the item probably wants re-exporting: add the \
         `pub use` to lib.rs. Only if the module is a deliberate public \
         namespace should it join NAMESPACE_MODULES at the top of this file, \
         with a comment saying why.",
        offenders.join("\n")
    );
}

/// Repository `examples/` directory, if this checkout has one.
fn examples_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    dir.is_dir().then(|| dir.canonicalize().unwrap_or(dir))
}

/// Every `.rs` file under `root`, including the example crates that are
/// workspace members. Build output is skipped.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)
            .expect("read examples dir")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Every `tower_mcp::<module>::` segment in `source`, with its line number.
///
/// Both spellings count: the plain `tower_mcp::context::RequestContext` and
/// the braced `use tower_mcp::{context::notification_channel, McpRouter}`.
/// Without the second, the exact import that hid #1240 could come back in a
/// form the check does not see.
fn module_segments(source: &str) -> Vec<(usize, String)> {
    const PREFIX: &str = "tower_mcp::";

    let mut segments = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = source[cursor..].find(PREFIX) {
        let start = cursor + offset;
        cursor = start + PREFIX.len();

        // `some_tower_mcp::x::y` would name a different crate.
        if source[..start].chars().next_back().is_some_and(is_ident) {
            continue;
        }

        let rest = &source[cursor..];
        if let Some(group) = brace_group(rest) {
            for (offset, module) in leading_modules(group) {
                // +1 for the brace itself.
                segments.push((line_of(source, cursor + 1 + offset), module));
            }
        } else if let Some(module) = leading_module(rest) {
            segments.push((line_of(source, cursor), module));
        }
    }
    segments
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

/// The module name at the head of `s`, when it is followed by `::`.
///
/// `tower_mcp::Content::text(..)` and `tower_mcp::Error::JsonRpc(..)` are
/// crate-root items with an associated item hanging off them, not module
/// paths, so a segment that does not start lowercase is not a module. That is
/// Rust naming convention rather than a guarantee, and it is the only thing
/// that separates the two forms without parsing the crate.
fn leading_module(s: &str) -> Option<String> {
    let end = s.find(|c: char| !is_ident(c))?;
    let head = &s[..end];
    let starts_lowercase = head.starts_with(|c: char| c.is_lowercase());
    (starts_lowercase && s[end..].starts_with("::")).then(|| head.to_string())
}

/// Every `name::` at the head of a `use` group item, with its byte offset.
///
/// A group item starts at the group's beginning or just after a comma, which
/// is what separates `{context::notification_channel}` (a module segment)
/// from `{extract::{Json, State}}` (where `Json` heads an item but is not
/// followed by `::`).
fn leading_modules(group: &str) -> Vec<(usize, String)> {
    let mut modules = Vec::new();
    let mut at_item_start = true;
    for (index, ch) in group.char_indices() {
        match ch {
            _ if ch.is_whitespace() => {}
            ',' | '{' => at_item_start = true,
            _ if at_item_start => {
                at_item_start = false;
                if let Some(module) = leading_module(&group[index..]) {
                    modules.push((index, module));
                }
            }
            _ => {}
        }
    }
    modules
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One-based line number of `offset` in `source`.
fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].lines().count().max(1)
}

mod parsing {
    use super::*;

    #[test]
    fn a_plain_path_is_a_module_segment() {
        let found = module_segments("use tower_mcp::context::RequestContext;");
        assert_eq!(found, vec![(1, "context".to_string())]);
    }

    #[test]
    fn a_crate_root_item_is_not() {
        assert!(module_segments("use tower_mcp::McpRouter;").is_empty());
        assert!(module_segments("use tower_mcp::{McpRouter, ToolBuilder};").is_empty());
    }

    /// The braced form is how the check could be sidestepped by accident.
    #[test]
    fn a_braced_path_is_a_module_segment() {
        let found = module_segments("use tower_mcp::{McpRouter, context::notification_channel};");
        assert_eq!(found, vec![(1, "context".to_string())]);
    }

    #[test]
    fn a_nested_group_reports_only_the_module() {
        let found = module_segments("use tower_mcp::{extract::{Json, State}, McpRouter};");
        assert_eq!(found, vec![(1, "extract".to_string())]);
    }

    #[test]
    fn another_crate_with_a_matching_suffix_is_ignored() {
        assert!(module_segments("use my_tower_mcp::context::Thing;").is_empty());
    }

    #[test]
    fn the_line_number_points_at_the_import() {
        let found = module_segments("use std::io;\n\nuse tower_mcp::router::McpRouter;\n");
        assert_eq!(found, vec![(3, "router".to_string())]);
    }

    /// A multi-line `use` should blame the line the module is on, not the
    /// line `tower_mcp::` is on.
    #[test]
    fn a_multiline_group_blames_the_right_line() {
        let found =
            module_segments("use tower_mcp::{\n    McpRouter,\n    context::Extensions,\n};");
        assert_eq!(found, vec![(3, "context".to_string())]);
    }
}
