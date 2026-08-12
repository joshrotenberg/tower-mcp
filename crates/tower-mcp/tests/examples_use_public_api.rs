//! The examples double as a public-API smoke test (#1258, pass 2).
//!
//! Nearly every module in this crate is `pub`, so an example *can* reach into
//! the one a type is defined in. Doing so hides a gap: #1240 was found because
//! two examples imported `tower_mcp::context::notification_channel`, and the
//! fix was to re-export it from the crate root (#1241), not to keep the deep
//! path working.
//!
//! So the rule this enforces is not "these modules are private". It is: if an
//! example has to name an implementation module, the item it wanted is missing
//! from the crate root, and that is the thing to fix.

use std::path::{Path, PathBuf};

/// Modules whose contents are re-exported from the crate root.
///
/// Reaching into one of these from an example bypasses the entry point every
/// user starts from. The namespaces meant to be used by path (`protocol`,
/// `client`, `extract`, `oauth`, `testing`, and the rest) are deliberately
/// absent: they carry too many names to flatten, so naming them is correct.
const REEXPORTED_MODULES: &[&str] = &[
    "context",
    "error",
    "jsonrpc",
    "prompt",
    "resource",
    "router",
    "session",
    "tool",
    "transport",
];

/// Every `.rs` file under `examples/`, including the workspace members there.
fn example_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("the examples directory must exist");

    let mut sources = Vec::new();
    let mut pending = vec![root];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read examples dir").flatten() {
            let path = entry.path();
            // Build output is not source, and it is enormous.
            if path.is_dir() && path.file_name().is_some_and(|name| name != "target") {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    assert!(
        sources.len() > 20,
        "expected to find the example sources, found {}",
        sources.len()
    );
    sources
}

/// The module named in a `tower_mcp::<module>::` path starting at `at`, if the
/// text there is one.
fn module_after_crate_prefix(line: &str, at: usize) -> Option<&str> {
    const PREFIX: &str = "tower_mcp::";
    let rest = line.get(at + PREFIX.len()..)?;
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
    // Only a further `::` makes this a module path rather than a re-exported
    // item like `tower_mcp::McpRouter`.
    rest[end..].starts_with("::").then(|| &rest[..end])
}

#[test]
fn examples_import_from_the_crate_root_rather_than_implementation_modules() {
    let mut offences = Vec::new();

    for path in example_sources() {
        let source = std::fs::read_to_string(&path).expect("read example source");
        for (number, line) in source.lines().enumerate() {
            // A doc comment discussing a path is prose, not an import.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for (at, _) in line.match_indices("tower_mcp::") {
                let Some(module) = module_after_crate_prefix(line, at) else {
                    continue;
                };
                if REEXPORTED_MODULES.contains(&module) {
                    offences.push(format!(
                        "  {}:{}: tower_mcp::{module}::",
                        path.display(),
                        number + 1,
                    ));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "examples reached into implementation modules:\n{}\n\n\
         Use the crate-root re-export instead. If there is no re-export for \
         what the example needs, that is the bug: add one, the way \
         `notification_channel` was added in #1241 rather than leaving \
         examples on `tower_mcp::context::` (#1240).",
        offences.join("\n"),
    );
}

/// The check above is only worth having if it can see a violation, and the
/// parsing it depends on is easy to get subtly wrong.
#[test]
fn the_check_distinguishes_module_paths_from_re_exported_items() {
    assert_eq!(
        module_after_crate_prefix("use tower_mcp::context::notification_channel;", 4),
        Some("context"),
        "a deep path must be recognised"
    );
    assert_eq!(
        module_after_crate_prefix("use tower_mcp::protocol::Tool;", 4),
        Some("protocol"),
        "recognition is separate from whether the module is allowed"
    );
    // A root re-export is the thing we want examples using, so it must not
    // register as a module path at all.
    assert_eq!(
        module_after_crate_prefix("use tower_mcp::McpRouter;", 4),
        None
    );
    assert_eq!(module_after_crate_prefix("let x: tower_mcp::Result<()>;", 7), None);

    // And the allow-list has to actually contain the modules it names.
    assert!(REEXPORTED_MODULES.contains(&"context"));
    assert!(!REEXPORTED_MODULES.contains(&"protocol"));
}
