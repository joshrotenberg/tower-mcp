//! Serve Agent Skills over MCP with the Skills extension (SEP-2640).
//!
//! Loads every skill under `examples/skills/` (or the directory given as the
//! first argument) and serves it on stdio:
//!
//! ```bash
//! cargo run --example skills_server --features skills
//! cargo run --example skills_server --features skills -- path/to/skills
//! ```
//!
//! Each file is a `skill://<skill>/<path>` resource. Clients enumerate skills
//! with `skills/list`, fetch one entry with `skills/get`, browse directories
//! with `resources/directory/read`, and read files with `resources/read`,
//! verifying each against the SHA-256 digest and size in the skill's entry.

use std::path::PathBuf;

use tower_mcp::skills::SkillSet;
use tower_mcp::{BoxError, McpRouter, StdioTransport};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("tower_mcp=info")
        .init();

    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills"));

    // Loading validates every skill (frontmatter, names, per-skill limits)
    // and computes digests once, so a bad skill fails here rather than on a
    // client's read.
    let skills = SkillSet::from_dir(&dir)?.directory_read(true);
    for skill in skills.entries() {
        let files = skill.resources.files().map_or(0, <[_]>::len);
        eprintln!("serving {} ({files} files)", skill.uri);
    }

    let router = McpRouter::new()
        .server_info("skills-server", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Serves this repository's Agent Skills. Call skills/list to see them, \
             then load one by reading its SKILL.md.",
        )
        .skills(skills);

    StdioTransport::new(router).run().await?;
    Ok(())
}
