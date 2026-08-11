//! Passing data from a tower layer to a tool handler.
//!
//! `middleware.rs` shows middleware *composing*: timeouts, concurrency limits,
//! guards. This shows middleware *communicating*. A layer in front of the
//! transport resolves something per request, and a handler reads it.
//!
//! The mechanism is [`HttpTransport::bridge_extension`]. Each transport builds
//! a request's MCP extensions from scratch, so by default nothing a layer
//! attached to the HTTP request is visible to a handler (#1242). Registering a
//! type is what carries it across. Registration is per type rather than
//! wholesale, so nothing reaches handler code that the server did not choose
//! to expose.
//!
//! Three pieces, and all three are required:
//!
//! 1. A layer that inserts `T` into the HTTP request's extensions.
//! 2. `.bridge_extension::<T>()` on the transport.
//! 3. A handler that reads `T` back out.
//!
//! Run with: `cargo run --example middleware_extension --features http`
//!
//! ```bash
//! # No API key: the layer attaches nothing, and `whoami` says so.
//! curl -sX POST http://127.0.0.1:3000/ \
//!   -H "Content-Type: application/json" -H "Accept: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}'
//!
//! # With a key, the same call reports the caller the layer resolved.
//! curl -sX POST http://127.0.0.1:3000/ \
//!   -H "Content-Type: application/json" -H "Accept: application/json" \
//!   -H "x-api-key: key-alice" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"whoami","arguments":{}}}'
//!
//! # `admin_ping` requires an identity, so without a key it is rejected.
//! curl -sX POST http://127.0.0.1:3000/ \
//!   -H "Content-Type: application/json" -H "Accept: application/json" \
//!   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"admin_ping","arguments":{}}}'
//! ```
//!
//! `WebSocketTransport::bridge_extension` is the same call. There the value is
//! read once from the HTTP request that opens the socket, so it applies to
//! every request on that connection.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tower_mcp::extract::{Context, Extension};
use tower_mcp::{BoxError, CallToolResult, HttpTransport, McpRouter, ToolBuilder};

/// What the layer resolves an API key into.
///
/// Any `Clone + Send + Sync + 'static` type works. Prefer a type you own:
/// bridging is keyed on the type, so a newtype or a small struct is what makes
/// the registration unambiguous. Registering `String` would be a request to
/// bridge every `String` any layer happened to insert.
#[derive(Debug, Clone)]
struct Caller {
    name: String,
    admin: bool,
}

/// Stand-in for whatever a real server does here: a database lookup, a JWT
/// signature check, a cache hit.
fn lookup(api_key: &str) -> Option<Caller> {
    match api_key {
        "key-alice" => Some(Caller {
            name: "alice".to_string(),
            admin: true,
        }),
        "key-bob" => Some(Caller {
            name: "bob".to_string(),
            admin: false,
        }),
        _ => None,
    }
}

/// Piece 1: the layer.
///
/// Anything an axum or tower middleware can do belongs here. This one maps a
/// header to a caller and inserts it. That single `insert` is the whole layer
/// side of the pattern.
///
/// A request with no usable key is passed through untagged rather than
/// rejected. Rejecting is equally valid, and is what an auth layer would do:
/// return a response instead of calling `next`. Passing it through is what
/// makes the extension genuinely optional on the handler side, which is the
/// more interesting case to demonstrate.
async fn identify(mut request: Request, next: Next) -> Response {
    let caller = request
        .headers()
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .and_then(lookup);

    if let Some(caller) = caller {
        request.extensions_mut().insert(caller);
    }

    next.run(request).await
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info")
        .init();

    // Piece 3a: read the bridged value straight off the request context.
    //
    // `ctx.extension::<T>()` returns `Option`, so the handler decides what a
    // missing value means. Use this when the tool has something sensible to do
    // for an unidentified caller.
    let whoami = ToolBuilder::new("whoami")
        .description("Report the caller the layer identified, if any")
        .read_only()
        .extractor_handler((), |ctx: Context| async move {
            Ok(CallToolResult::text(match ctx.extension::<Caller>() {
                Some(caller) => format!("you are {} (admin: {})", caller.name, caller.admin),
                None => "you are anonymous".to_string(),
            }))
        })
        .build();

    // Piece 3b: the same value through the `Extension<T>` extractor.
    //
    // This is the shorthand, and it is not merely shorter: a missing value is
    // a rejection before the handler body runs, so a tool that cannot work
    // without an identity never has to write the `None` arm.
    let admin_ping = ToolBuilder::new("admin_ping")
        .description("Requires an identified caller, and an admin one at that")
        .read_only()
        .extractor_handler((), |Extension(caller): Extension<Caller>| async move {
            // The identity is a whole value, not just a name, so authorization
            // reads off the same thing the layer resolved.
            Ok(if caller.admin {
                CallToolResult::text(format!("pong, {}", caller.name))
            } else {
                CallToolResult::error(format!("{} is not an admin", caller.name))
            })
        })
        .build();

    let router = McpRouter::new()
        .server_info("middleware-extension-example", "1.0.0")
        .auto_instructions()
        .tool(whoami)
        .tool(admin_ping);

    // Piece 2: register the type.
    //
    // Without this line the server still runs, the layer still inserts a
    // `Caller`, and every call reports "anonymous". That silent version is
    // #1242 exactly, so it is worth trying: comment the line out and re-run
    // the second curl above.
    let transport = HttpTransport::new(router)
        .bridge_extension::<Caller>()
        // Local development only. In production name the origins you accept.
        .disable_origin_validation();

    // `into_router()` hands back a plain `axum::Router`, which is where the
    // layer goes. It wraps the whole HTTP surface, so it sees every request
    // before the transport decodes any MCP out of it.
    let app = transport
        .into_router()
        .layer(axum::middleware::from_fn(identify));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("Listening on http://127.0.0.1:3000 -- try the curls in this file's header");
    axum::serve(listener, app).await?;

    Ok(())
}
