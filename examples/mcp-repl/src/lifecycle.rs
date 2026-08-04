//! Project lifecycle and extraction guide.
//!
//! `mcp-repl` is independently versioned and published even though its source
//! currently lives in the tower-mcp workspace. This guide records the release
//! contract and the work required before moving it to a standalone repository.
//! It is a plan, not authorization to perform the move.
//!
//! # Supported boundaries
//!
//! The application lives in the `mcp_repl` library and the binary is only a
//! thin call to [`crate::run_cli`]. The deliberately reusable seams are:
//!
//! - [`crate::config`] for native server and alias profiles;
//! - [`crate::import_config`] for explicit imports from standard MCP JSON
//!   configuration files; and
//! - [`crate::oauth_profile`] for non-secret OAuth profile metadata and secure
//!   credential-store access.
//!
//! Terminal editing, rendering, and command dispatch remain private. A related
//! tool such as `mcp2md` should not depend on all of `mcp-repl` merely to reuse
//! configuration: that would also couple it to the interactive terminal stack.
//! Keep such a tool independent unless real duplication justifies extracting a
//! narrow configuration or connection crate used by both projects.
//!
//! # Compatibility and release lanes
//!
//! The path-scoped `mcp-repl` workflow owns the package's checks independently
//! from the rest of the workspace:
//!
//! ```text
//! cargo fmt -p mcp-repl -- --check
//! cargo clippy -p mcp-repl --all-targets --all-features -- -D warnings
//! cargo test -p mcp-repl --all-targets --all-features
//! RUSTDOCFLAGS=-Dwarnings cargo doc -p mcp-repl --no-deps --all-features
//! ```
//!
//! Those commands test the workspace's current `tower-mcp`, which is the main
//! compatibility lane. `cargo package -p mcp-repl` creates and verifies the
//! normalized publishable package. In that package, Cargo replaces the
//! workspace path dependency with the declared crates.io version, so this is
//! also the released-framework compatibility lane. A tower-mcp version change
//! must be published before that package lane can pass.
//!
//! While the package remains in this workspace, the repository's release-plz
//! workflow owns crates.io publication, tags, GitHub releases, and changelog
//! updates. Do not publish the same version manually in parallel. Before an
//! extraction, let the final workspace release finish and start the new
//! repository at the next version. Copy and dry-run the release workflow before
//! enabling its registry credential.
//!
//! # Extraction checklist
//!
//! Perform history rewriting only in a disposable clone. A starting point is:
//!
//! ```text
//! git filter-repo \
//!   --path examples/mcp-repl/ \
//!   --path LICENSE-APACHE \
//!   --path LICENSE-MIT \
//!   --path-rename examples/mcp-repl/:
//! git log --follow -- src/lib.rs
//! ```
//!
//! Before making the new repository authoritative:
//!
//! 1. Move or reproduce the black-box fixture currently located at
//!    `examples/mcp_repl_fixture.rs`; it intentionally is not part of the
//!    published package today.
//! 2. Replace workspace-inherited package fields and dependencies with explicit
//!    standalone manifest values, then run all checks and `cargo package` from
//!    the extracted repository.
//! 3. Carry over the licenses, contribution and security policy, code-owner and
//!    dependency-update settings, supported Rust version, and the path-scoped
//!    quality workflow.
//! 4. Transfer open mcp-repl issues when possible. Otherwise recreate them with
//!    bidirectional links, leave a migration notice in tower-mcp, and update
//!    repository links in the README, Cargo manifest, crates.io, docs.rs, and
//!    release notes.
//! 5. Confirm the maintainers and crates.io owners who will handle releases and
//!    security reports. Publish the new repository's security contact before
//!    changing the crate's canonical repository URL.
//! 6. Verify tags, changelog history, and `git log --follow` before archiving the
//!    old source location or closing the extraction tracker.
//!
//! Until those steps are complete, source, issue triage, release notes, and
//! security reporting remain owned by the tower-mcp repository.
