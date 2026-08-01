#![doc = r####"
# MCP Apps server support

tower-mcp provides an opt-in, typed server surface for the stable
[MCP Apps extension](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx)
(SEP-1865).

MCP Apps support has two independent gates:

1. Enable `features = ["mcp-apps"]` to compile the typed API.
2. Call `McpRouter::with_mcp_apps()` to advertise
   `io.modelcontextprotocol/ui` at runtime.

The extension becomes active only when the client declares the same extension
and both peers include `text/html;profile=mcp-app` in `mimeTypes`.
`RequestContext::supports_mcp_apps()` performs that complete check. Merely
receiving an unknown or one-sided extension declaration never activates it.

## Minimal server

```rust
use serde_json::json;
use tower_mcp::{
    McpAppResourceBuilder, McpAppUri, McpRouter, McpUiToolMeta, ToolBuilder,
    mcp_app_tool_result,
};

let uri = McpAppUri::new("ui://weather/dashboard")?;
let resource = McpAppResourceBuilder::new(
    uri.as_str(),
    "Weather dashboard",
    "<!doctype html><html><body>Weather</body></html>",
)?
.build()?;

let tool = ToolBuilder::new("weather")
    .handler(|()| async {
        Ok(mcp_app_tool_result(
            "72 °F and sunny",
            json!({"temperatureF": 72, "conditions": "sunny"}),
        ).expect("JSON value serialization is infallible"))
    })
    .build()
    .with_mcp_app(McpUiToolMeta::new(uri))?;

let router = McpRouter::new()
    .with_mcp_apps()
    .tool(tool)
    .resource(resource);

# Ok::<(), tower_mcp::BoxError>(())
```

See the complete runnable
[`mcp_apps`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/mcp_apps.rs)
example.

## What the typed API guarantees

- `McpAppUri` accepts only unambiguous `ui://` resource identifiers without
  credentials, ports, query strings, or fragments.
- `McpAppResourceBuilder` always emits
  `text/html;profile=mcp-app` on both the resource declaration and
  `resources/read` content.
- `McpAppHtml` requires an HTML5 doctype and `<html` root and rejects NUL
  bytes. This is a structural check, not an HTML sanitizer.
- `McpUiToolMeta` emits nested `_meta.ui` metadata and typed `model`/`app`
  visibility. It does not emit the deprecated flat `ui/resourceUri` key.
- Existing unrelated tool metadata is preserved when Apps metadata is added.
- `McpUiResourceCsp` accepts origins, not arbitrary CSP fragments. It rejects
  credentials, paths, queries, fragments, unsupported schemes, and wildcards
  outside `resourceDomains`.
- Omitting CSP fields retains the specification's restrictive host defaults.
  There is no permissive fallback in the API.
- `mcp_app_tool_result` always includes useful text content for models and
  hosts that did not negotiate Apps, plus structured content for the UI.

## Security boundary

This crate builds and serves declarations. The host that renders an App is
still responsible for the security controls required by SEP-1865:

- render untrusted HTML in a sandboxed iframe;
- for web hosts, use the separate-origin sandbox proxy architecture;
- construct and enforce CSP from the resource's `_meta.ui.csp`;
- never allow undeclared domains and consider additional global
  allowlists/blocklists;
- validate and audit all iframe-to-host JSON-RPC messages;
- gate App-originated tool calls by tool visibility, server connection, and
  user approval policy;
- impose CPU, memory, network, and lifetime limits;
- visibly distinguish untrusted App UI to reduce phishing risk.

`McpAppHtml` does not make HTML trusted. Inline scripts and styles are part of
the stable raw-HTML profile, so sandbox isolation and host CSP enforcement are
mandatory even when the server uses no external domains.

## CSP and permissions

```rust
use tower_mcp::{
    McpAppDomain, McpUiPermissions, McpUiResourceCsp, McpUiResourceMeta,
};

let csp = McpUiResourceCsp::default()
    .allow_connect("https://api.example.com")?
    .allow_connect("wss://events.example.com")?
    .allow_resource("https://*.cdn.example.com")?
    .allow_frame("https://player.example.com")?
    .allow_base_uri("https://assets.example.com")?;

let metadata = McpUiResourceMeta::default()
    .csp(csp)
    .permissions(McpUiPermissions::default().geolocation())
    .domain(McpAppDomain::new("app-sandbox.example.com")?)
    .prefers_border(true);

# Ok::<(), tower_mcp::McpAppError>(())
```

Permissions are requests, not grants. Apps must use browser feature detection
and continue working when the host denies them. Dedicated domains are
host-dependent; the Rust type validates a bare DNS-style value, but the server
must still follow the target host's naming and registration rules.

## Visibility

Omitted visibility means both `model` and `app`, matching SEP-1865.
`McpUiToolMeta::app_only` creates a tool callable by an App on the same server
connection but hidden from the model by the host. `model_only` prevents
App-originated calls. Cross-server App calls remain a host policy violation.

Visibility metadata is not server-side authorization. Apply normal Tower
authentication, authorization, rate limits, and audit middleware to every
tool, including App-only tools.
"####]
