//! A stdio MCP server whose tools are Lua files.
//!
//! ```text
//! cargo run -p lua-tools-example [DIR]
//! ```
//!
//! `DIR` defaults to the `lua/` directory next to this crate. Every
//! `*.lua` file in it defines one tool. A static `reload` tool loads the
//! directory again, so an edit to a tool file takes effect without restarting
//! the server.

use std::path::PathBuf;

use lua_tools_example::LuaTools;
use serde_json::json;
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};

fn main() -> Result<(), BoxError> {
    // Logs go to stderr: stdout carries the JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_env_filter("lua_tools_example=info")
        .with_writer(std::io::stderr)
        .init();

    let dir: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/lua").into());

    // The Lua state cannot leave its thread, so run everything, the transport
    // included, on one thread: a current-thread runtime and a LocalSet.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&runtime, async move {
        let router = McpRouter::new().server_info("lua-tools", "0.1.0");
        // mlua's error type is not `Send` (we do not enable its `send`
        // feature), so it cannot become a `BoxError` directly.
        let (router, lua) = LuaTools::new(router).map_err(|error| error.to_string())?;

        let change = lua.load_dir(&dir).await?;
        tracing::info!(dir = %dir.display(), added = ?change.added, "loaded tools");

        // `reload` is a static tool on the router, not part of the dynamic
        // registry, so `replace_all` never removes it.
        let reload = {
            let (lua, dir) = (lua.clone(), dir.clone());
            ToolBuilder::new("reload")
                .description("Load the Lua tool directory again and report what changed")
                .no_params_handler(move || {
                    let (lua, dir) = (lua.clone(), dir.clone());
                    async move {
                        Ok(match lua.load_dir(&dir).await {
                            Ok(change) => CallToolResult::json(json!({
                                "added": change.added,
                                "removed": change.removed,
                                "replaced": change.replaced,
                            })),
                            // A failed load changed nothing; say why.
                            Err(error) => CallToolResult::error(error.to_string()),
                        })
                    }
                })
                .build()
        };

        StdioTransport::new(router.tool(reload)).run().await?;
        Ok(())
    })
}
