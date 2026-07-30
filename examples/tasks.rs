//! Final Tasks extension server (SEP-2663), including task ownership.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example tasks --features protocol-2026-07-28
//! ```
//!
//! Two separate opt-ins gate this feature, and both are required:
//!
//! 1. the `protocol-2026-07-28` Cargo feature compiles the final protocol path;
//! 2. [`McpRouter::with_tasks`] advertises `io.modelcontextprotocol/tasks` and
//!    is what actually permits task dispatch at runtime.
//!
//! A client must *also* declare the extension on its side. A server that opts
//! in still returns a normal synchronous result to a client that did not, so
//! adding `with_tasks()` never changes behavior for existing clients.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, TaskSupportMode, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct ReportInput {
    /// How many records to crunch.
    records: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `Optional` lets one tool serve both worlds: a client that negotiated the
    // extension may ask for a task, and everyone else gets the synchronous
    // result. `Required` instead refuses callers that cannot take a task,
    // answering with -32021 and naming the extension they need to declare.
    let build_report = ToolBuilder::new("build_report")
        .description("Crunch records into a report. Slow enough to be worth a task.")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: ReportInput| async move {
            tokio::time::sleep(Duration::from_millis(50 * u64::from(input.records))).await;
            Ok(CallToolResult::text(format!(
                "report over {} records",
                input.records
            )))
        })
        .build();

    let router = McpRouter::new()
        .server_info("tasks-example", env!("CARGO_PKG_VERSION"))
        .tool(build_report)
        // The runtime opt-in. Without it, registering a task-capable tool
        // advertises nothing and no task is ever created.
        .with_tasks();

    StdioTransport::new(router).run().await?;
    Ok(())
}

// # Task ownership
//
// SEP-2663 requires servers to authorize every task request, and warns that a
// task ID can act as a bearer token: whoever holds it can poll, update, or
// cancel the task. tower-mcp answers that in two layers.
//
// Task IDs are 128 bits from the system CSPRNG, so they cannot be enumerated
// or guessed. That alone is not enough, because it only protects IDs nobody
// has seen. So each task also records the principal that created it, taken
// from the OAuth `sub` claim the HTTP and WebSocket transports bridge into
// request extensions, and every later operation must match it.
//
// Matching is equality, not "protect owned tasks and leave unowned ones open":
//
// | Task owner | Caller     | Result                                  |
// |------------|------------|-----------------------------------------|
// | none       | none       | allowed, no authentication configured   |
// | `alice`    | `alice`    | allowed                                 |
// | `alice`    | `bob`      | denied                                  |
// | `alice`    | none       | denied                                  |
// | none       | `alice`    | denied                                  |
//
// The last row is the surprising one and is deliberate. An unowned task can
// only exist if it was created with no authenticated context, so a request
// that now carries a principal is a different security context rather than an
// upgrade of the same one. Servers mixing public and authenticated paths (see
// `AuthConfig::public_path`) should expect a task created anonymously to be
// unreachable once a token is presented.
//
// This example has no authentication, so every task is unowned and the first
// row applies throughout. Add an `AuthLayer` or OAuth validation, as in
// `http_auth.rs`, and ownership begins to bind automatically. Nothing here
// needs to change: the router reads the principal from the request context.
//
// A denied request returns exactly what an unknown task returns: -32602 with
// "Task not found". SEP-2663 mandates -32602 for an unknown or expired task
// but leaves the authorization failure to the server, so this is tower-mcp
// policy rather than a spec requirement. Answering "forbidden" would confirm
// the ID is real, which is precisely what unguessable IDs exist to prevent.
// A server wanting the opposite behavior can wrap the router and translate.
//
// Expiry follows the same rule. `ttlMs` runs from creation, and once it
// elapses the task reads as absent rather than as expired, so a retention
// window cannot be probed either.
