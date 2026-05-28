//! Embedding tower-mcp inside an existing axum application.
//!
//! Run with: `cargo run --example axum_embedding --features http`
//!
//! Then exercise the routes:
//!
//! ```bash
//! # Plain axum routes
//! curl http://127.0.0.1:3000/health
//! curl http://127.0.0.1:3000/api/users
//!
//! # MCP under /mcp -- initialize a session
//! curl -X POST http://127.0.0.1:3000/mcp \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # Call a tool (use the MCP-Session-Id returned by initialize)
//! curl -X POST http://127.0.0.1:3000/mcp \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"user_count","arguments":{}}}'
//!
//! # A second MCP server mounted at /admin/mcp (initialize separately)
//! curl -X POST http://127.0.0.1:3000/admin/mcp \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//! ```
//!
//! # Why this example exists
//!
//! `HttpTransport::into_router()` returns a plain [`axum::Router`]. That means
//! tower-mcp composes naturally with any axum app: nest it under a path, merge
//! it with other routers, wrap it in tower middleware, share state. You do not
//! need to give MCP its own listener.
//!
//! This file is the canonical embedding pattern. It walks through:
//!
//! 1. A user-owned axum [`Router`] with non-MCP routes (`/health`, `/api/users`)
//!    that pull from a shared [`AppState`] via the [`State`] extractor.
//! 2. Building an `HttpTransport`, calling `into_router()`, and mounting the
//!    MCP router under `/mcp` with [`Router::nest`].
//! 3. Mounting a second MCP server at `/admin/mcp` -- nothing stops you from
//!    serving multiple MCP surfaces from one binary.
//! 4. Wrapping the merged app in a shared [`TraceLayer`] so every request --
//!    user route or MCP -- gets the same logging treatment.
//! 5. Running the combined router with [`axum::serve`] instead of
//!    `transport.serve(...)`. The transport's own `serve()` is a convenience
//!    method; embedding gives you full control of the listener.
//!
//! For a pure MCP-only HTTP server (no other routes), see `http_server.rs`.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, NoParams, ToolBuilder};

/// Shared axum middleware: log method, path, status, and duration for every
/// request -- whether it lands on a user route or on the embedded MCP router.
///
/// This is `tower-http::TraceLayer` in miniature, written inline so the
/// example does not pull in an extra dependency. Anything that implements
/// [`tower::Layer`] (CORS, compression, request-id, your own custom layer)
/// drops into the same `.layer(...)` call below.
async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = Instant::now();
    let response = next.run(req).await;
    tracing::info!(
        %method,
        path = %path,
        status = response.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// User-supplied application state shared across non-MCP routes.
///
/// This is the standard axum pattern: stash any expensive-to-clone state
/// (database pools, HTTP clients, config) behind an [`Arc`] and hand it to
/// [`Router::with_state`].
///
/// Note that this state is wired into the *axum-level* router and is visible
/// to the `/health` and `/api/users` handlers, but it is intentionally not
/// shared with the MCP router below. MCP tools have their own state model:
/// closures captured by `ToolBuilder` handlers, or `extractor_handler` with
/// custom state passed to `ToolBuilder::extractor_handler`. Keeping the two
/// state systems separate avoids tangling axum extractors with MCP handlers.
#[derive(Clone)]
struct AppState {
    users: Arc<Vec<User>>,
}

#[derive(Clone, Serialize)]
struct User {
    id: u64,
    name: String,
}

/// `GET /health` -- a simple liveness endpoint for load balancers.
///
/// Pure axum: no MCP involvement.
async fn health() -> &'static str {
    "ok"
}

/// `GET /api/users` -- returns the configured user list.
///
/// Demonstrates that the user-owned axum routes keep working exactly as they
/// would in any axum app, including [`State`] extraction.
async fn list_users(State(state): State<AppState>) -> (StatusCode, Json<Vec<User>>) {
    (StatusCode::OK, Json((*state.users).clone()))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    name: String,
}

/// Build the primary MCP router that will be mounted at `/mcp`.
///
/// In a real app this might live in its own module. The point is that the
/// builder API is unchanged whether you serve directly or embed -- only the
/// last step (`serve` vs. `into_router`) differs.
fn build_main_mcp_router(user_count: usize) -> McpRouter {
    let greet = ToolBuilder::new("greet")
        .description("Greet someone by name")
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    // Capture state from the outer app via the handler closure. This is how
    // MCP tools "share" axum state without needing axum extractors: pull what
    // you need out of `AppState` before constructing the tool.
    let user_count_tool = ToolBuilder::new("user_count")
        .description("Return the number of users registered with the app")
        .handler(move |_: NoParams| async move {
            Ok(CallToolResult::text(format!("{} users", user_count)))
        })
        .build();

    McpRouter::new()
        .server_info("axum-embedded-mcp", "1.0.0")
        .auto_instructions()
        .tool(greet)
        .tool(user_count_tool)
}

/// Build a second, smaller MCP router that exposes admin-only tools.
///
/// Mounting more than one MCP server in the same binary is just two calls to
/// `Router::nest`. Each gets its own session store, capability list, and tool
/// surface. You could wrap this one in an auth middleware (see `http_auth.rs`)
/// while leaving the public `/mcp` open.
fn build_admin_mcp_router() -> McpRouter {
    let restart = ToolBuilder::new("restart")
        .description("Pretend to restart the service (no-op in this demo)")
        .handler(|_: NoParams| async move { Ok(CallToolResult::text("restart acknowledged")) })
        .build();

    McpRouter::new()
        .server_info("axum-embedded-admin", "1.0.0")
        .auto_instructions()
        .tool(restart)
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "axum_embedding=info,tower_mcp=info".into()),
        )
        .init();

    // 1. Build the user's application state.
    let app_state = AppState {
        users: Arc::new(vec![
            User {
                id: 1,
                name: "ada".into(),
            },
            User {
                id: 2,
                name: "grace".into(),
            },
            User {
                id: 3,
                name: "linus".into(),
            },
        ]),
    };

    // 2. Build the user's axum router with the non-MCP routes. This is
    //    standard axum -- nothing tower-mcp-specific yet.
    let user_routes = Router::new()
        .route("/health", get(health))
        .route("/api/users", get(list_users))
        .with_state(app_state.clone());

    // 3. Build the primary MCP transport and turn it into an axum Router.
    //
    //    `disable_origin_validation()` is fine for this local demo; in
    //    production replace it with `.allowed_origins(...)` listing the
    //    domains your browser-based clients will use.
    let mcp_router = build_main_mcp_router(app_state.users.len());
    let mcp_axum_router = HttpTransport::new(mcp_router)
        .disable_origin_validation()
        .into_router();

    // 4. Build a second MCP router for admin tools, also as an axum Router.
    let admin_mcp_router = build_admin_mcp_router();
    let admin_mcp_axum_router = HttpTransport::new(admin_mcp_router)
        .disable_origin_validation()
        .into_router();

    // 5. Compose everything. `nest` mounts the MCP router at a path prefix,
    //    leaving the user's routes (and each other) untouched. The order of
    //    `merge` / `nest` does not matter as long as the paths do not
    //    collide.
    //
    //    Wrap the merged app in a shared logging middleware so every request
    //    -- user route or MCP -- is logged the same way. Any other tower
    //    middleware (`tower_http::CompressionLayer`, `CorsLayer`,
    //    `TimeoutLayer`, your own auth layer) plugs in here too.
    let app = Router::new()
        .merge(user_routes)
        .nest("/mcp", mcp_axum_router)
        .nest("/admin/mcp", admin_mcp_axum_router)
        .layer(middleware::from_fn(log_requests));

    // 6. Serve the composed router exactly like any other axum app. The MCP
    //    transport does not need to own the listener; `into_router()` makes
    //    it just another router.
    let addr = "127.0.0.1:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "Embedded axum + MCP app listening on http://{addr} (try /health, /api/users, /mcp, /admin/mcp)"
    );
    axum::serve(listener, app).await?;

    Ok(())
}
