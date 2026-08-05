#![doc = r####"
# Protocol-version guide

tower-mcp separates three questions that are easy to conflate:

1. Which wire types does the library know?
2. Which protocol implementations were compiled into `tower-mcp`?
3. Which compiled versions may this client or server use at runtime?

This guide explains the crate policy and APIs. The MCP specification remains
authoritative for protocol behavior:

- [2026-07-28 versioning and compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)
- [2026-07-28 key changes](https://modelcontextprotocol.io/specification/2026-07-28/changelog)
- [2026-07-28 discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)
- [2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)

## Current support policy

| Wire revision | Status in tower-mcp 0.19 | Compile-time switch | Lifecycle |
|---|---|---|---|
| `2026-07-28` | released, enabled when compiled | `protocol-2026-07-28` | sessionless, per-request metadata, optional `server/discover` first |
| `2025-11-25` | stable default | always available | `initialize` session lifecycle |
| `2025-03-26` | backward compatibility | always available | `initialize` session lifecycle |

The default Cargo build is intentionally conservative. A release of the MCP
specification does not silently move existing applications to a different
lifecycle. Enabling `full` compiles the final implementation because `full`
includes `protocol-2026-07-28`, but client runtime selection remains stable by
default.

The former `stateless` Cargo feature is a compatibility alias. New manifests
should use `protocol-2026-07-28`. Likewise,
`EXPERIMENTAL_PROTOCOL_VERSION` and `UPCOMING_PROTOCOL_VERSION` are deprecated
aliases of `PROTOCOL_VERSION_2026_07_28`; use the canonical constant in new
code.

## Types, compile-time support, and runtime support

### Wire types

`tower-mcp-types` exposes every protocol type the release knows without
pulling in Tower, Tokio, or an HTTP runtime. Its known-version constants answer
schema and serialization questions, not whether a `tower-mcp` transport can
execute that lifecycle.

### Compiled implementations

`COMPILED_PROTOCOL_VERSIONS` reports the implementations present in the
current `tower-mcp` build, in preference order. `is_protocol_version_compiled`
answers the same question for one string.

Without the final feature:

```text
["2025-11-25", "2025-03-26"]
```

With `protocol-2026-07-28`:

```text
["2026-07-28", "2025-11-25", "2025-03-26"]
```

### Runtime allowlists

`ProtocolSupport` is an exact, ordered allowlist for one client or transport.
Construction rejects an empty list, duplicates, and versions that were not
compiled.

```rust
use tower_mcp::{BoxError, ProtocolSupport};

fn main() -> Result<(), BoxError> {
    let stable = ProtocolSupport::stable();
    assert!(stable.contains("2025-11-25"));

    let final_only = ProtocolSupport::try_new(["2026-07-28"])?;
    assert_eq!(final_only.preferred(), "2026-07-28");
    Ok(())
}
```

`ProtocolSupport::compiled()` enables the whole compiled set.
`ProtocolSupport::stable()` narrows a build to the session protocols even
when `protocol-2026-07-28` was compiled. `ProtocolSupport::default()` is the
compiled set, for clients and servers alike.

Two older constants have narrower meaning:

- `LATEST_PROTOCOL_VERSION` is the preferred session default
  (`2025-11-25`), not the newest published date.
- `SUPPORTED_PROTOCOL_VERSIONS` is the session-negotiable set, the versions
  `initialize` can produce. `2026-07-28` removed `initialize`, so it is
  never in this list regardless of features.

Use `COMPILED_PROTOCOL_VERSIONS` and `ProtocolSupport` for deployment policy.

## Configure a server

An HTTP server enables every compiled implementation by default. If the final
feature is not compiled, that is the stable set. If it is compiled, the same
endpoint is dual-era unless you narrow it.

The server snippets require the `http` feature. Final-only and dual-era
snippets also require `protocol-2026-07-28`.

### Stable-only server

```rust
use tower_mcp::{HttpTransport, McpRouter, ProtocolSupport};

fn main() {
    let transport = HttpTransport::new(McpRouter::new())
        .protocol_support(ProtocolSupport::stable());
    drop(transport);
}
```

### Final-only server

```rust
use tower_mcp::{BoxError, HttpTransport, McpRouter, ProtocolSupport};

fn main() -> Result<(), BoxError> {
    let support = ProtocolSupport::try_new(["2026-07-28"])?;
    let transport = HttpTransport::new(McpRouter::new())
        .protocol_support(support);
    drop(transport);
    Ok(())
}
```

### Explicit dual-era server

```rust
use tower_mcp::{BoxError, HttpTransport, McpRouter, ProtocolSupport};

fn main() -> Result<(), BoxError> {
    let support = ProtocolSupport::try_new([
        "2026-07-28",
        "2025-11-25",
        "2025-03-26",
    ])?;
    let transport = HttpTransport::new(McpRouter::new())
        .protocol_support(support);
    drop(transport);
    Ok(())
}
```

The order is advertised as preference order. `protocol_versions(...)` is a
convenience method that constructs the same validated policy on the transport.

On HTTP, a dual-era transport selects behavior from the incoming request:

- a final request declares `2026-07-28`, includes required per-request
  metadata, and has no session ID;
- an `initialize` request starts a legacy lifecycle and subsequent requests
  use its negotiated session/version rules.

Both may be served concurrently at one URL. The final path is automatic when
the feature and runtime policy allow it; do not call the historical
`stateless(StatelessConfig)` API to activate it.

## Configure a client

Clients default to every compiled implementation, matching servers. The entry
point still selects the era: `initialize` starts a legacy session and
`discover` starts the final lifecycle, so compiling a feature never changes an
application's first wire request; it only removes the extra configuration step
before `discover`.

### Stable client

```rust,ignore
let client = McpClient::connect(transport).await?;
client.initialize("my-client", "1.0.0").await?;
```

`initialize` sends the legacy handshake and the required
`notifications/initialized` notification. HTTP stores the negotiated protocol
version and session ID for later requests.

### Final client

```rust,ignore
let client = McpClient::connect(transport).await?;
client.discover("my-client", "1.0.0").await?;
```

With `protocol-2026-07-28` compiled, no configuration is needed. To *refuse*
the session protocols entirely, narrow the client instead:

```rust,ignore
let support = ProtocolSupport::try_new(["2026-07-28"])?;
let client = McpClient::builder()
    .protocol_support(support)
    .connect_simple(transport)
    .await?;
client.discover("my-client", "1.0.0").await?;
```

`discover` starts the modern path and records the selected version. Every
later request carries the version, client identity, and capabilities in
`_meta`; HTTP adds the required protocol and method headers. Do not call
`initialize` on the same client.

`discover` retries one recognized `UnsupportedProtocolVersionError` with a
mutually supported modern version. It does not automatically convert a modern
client into a legacy client. An application that must connect to unknown-era
servers should own that policy: probe according to the spec, create a fresh
client/transport for fallback, and call `initialize` only after classifying the
server as legacy. Reusing a partially probed connection risks mixing lifecycle
state.

See the [client guide](crate::guides::client) for complete setup and request patterns.

## Behavioral differences applications must account for

The final revision is not just a newer version string:

| Area | 2025-11-25 and earlier | 2026-07-28 |
|---|---|---|
| Startup | `initialize` / `notifications/initialized` | no handshake; `server/discover` is available |
| Identity/capabilities | negotiated once | carried on every request |
| HTTP sessions | optional/required session IDs | protocol-level sessions removed |
| General server push | legacy GET/POST SSE streams | explicit `subscriptions/listen` POST |
| Stream replay | optional SSE IDs and `Last-Event-ID` | removed |
| Server input requests | bidirectional requests | MRTR `input_required` result and retry |
| Cache hints | application-defined | `ttlMs` and `cacheScope` on cacheable results |
| Tasks | legacy experimental shapes | negotiated `io.modelcontextprotocol/tasks` extension |
| `ping` / `logging/setLevel` | core methods | removed from final core |

tower-mcp version-gates these paths. It also keeps deprecated legacy features
available where their protocol revision requires them. Do not branch on server
display name or version; use negotiated protocol and capabilities.

Top-level JSON-RPC arrays need a finer distinction than “legacy.” Request
batches are accepted only for the exact `2025-03-26` revision. Batching was
removed in `2025-06-18`, so `2025-11-25` and `2026-07-28` reject arrays before
dispatch. `JsonRpcService` records the legacy revision returned by
`initialize`; HTTP uses the session's negotiated revision, while final
requests use their per-request metadata. A multi-version `ProtocolSupport`
allowlist is never treated as enough context to guess batch policy.

## Interoperability policy

For broad interoperability:

- deploy a dual-era server when both existing and final clients must connect;
- leave clients stable unless the target server is known to support the final
  lifecycle or the application implements era probing;
- advertise extensions only when the corresponding application behavior is
  enabled (`with_tasks`, `with_mcp_apps`, or a validated extension declaration);
- treat absence of a capability as unsupported;
- use `sse_responses(true)` only for legacy clients that incorrectly require
  SSE-wrapped synchronous responses;
- test the exact feature/runtime combinations you ship, not only `--all-features`.

The official conformance suites run against tower-mcp's stable and final client
and server paths on every pull request. The repository also tests wire
compatibility with rmcp and the official TypeScript and Python SDKs. Counts
change as upstream scenarios are added; use the current CI result as the source
of truth.

Useful local matrix checks:

```bash
# Stable default library
cargo check -p tower-mcp --no-default-features

# Stable HTTP server/client
cargo check -p tower-mcp --no-default-features \
  --features "http,http-client"

# Dual-era HTTP server/client
cargo check -p tower-mcp --no-default-features \
  --features "http,http-client,protocol-2026-07-28"

# Everything shipped by the workspace
cargo test --workspace --all-targets --all-features
```

## Upgrade policy for applications

Treat a protocol upgrade as an application change, even when the crate update
is semver-compatible:

1. Compile the new implementation in CI without changing runtime selection.
2. Run the relevant conformance and integration matrix.
3. Enable the new version on a canary server with an explicit
   `ProtocolSupport` allowlist.
4. Confirm proxy headers, authentication resource binding, caching, and stream
   behavior for the new lifecycle.
5. Opt clients in deliberately and call the lifecycle method for that era.
6. Keep or remove legacy support according to observed client traffic and your
   published compatibility policy.

Avoid using status-bearing aliases such as “upcoming” in configuration or
persisted data. Store the exact wire date (`2026-07-28`) and use canonical
constants in code. If an unsupported date reaches a runtime allowlist,
`ProtocolSupport::try_new` fails at startup rather than advertising a path the
binary cannot execute.

## Runnable examples

```bash
# Final server/discover response shape
cargo run --example server_discover \
  --features "http,protocol-2026-07-28"

# Full final client/server lifecycle
cargo run --example stateless_http_client \
  --features "http,http-client,protocol-2026-07-28"

# Final Tasks extension
cargo run --example tasks --features protocol-2026-07-28
```

The [examples index](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/README.md)
also links the stable HTTP and conformance implementations.
"####]
