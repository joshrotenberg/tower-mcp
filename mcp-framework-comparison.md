# MCP framework comparison: `rmcp`, `tower-mcp`, and Anubis

<!-- markdownlint-disable MD013 -->

**Primary Rust comparison:** [`modelcontextprotocol/rust-sdk`](rust-sdk)
(`rmcp`) and [`joshrotenberg/tower-mcp`](tower-mcp)  
**Focused cross-language comparison:** [`zoedsoupe/anubis-mcp`](anubis-mcp)
versus [`joshrotenberg/tower-mcp`](tower-mcp)  
**Snapshot:** 2026-07-27  
**Compared commits:** `rust-sdk` `125dbfd5`; `tower-mcp` `0b7bc1c3`;
`anubis-mcp` `2451bb7b`

## Executive summary

The two Rust projects are credible, actively maintained MCP implementations.
For the stable `2025-11-25` protocol, both passed the applicable required
checks in the official conformance harness. They are not, however,
interchangeable:

- **`rmcp` is the safer default.** It has the strongest protocol coverage,
  substantially better support for the imminent `2026-07-28` protocol, broader
  client and OAuth behavior, cross-language interoperability tests, a stable
  release line, and the governance advantages of the official SDK.
- **`tower-mcp` is the more idiomatic application framework for teams already
  invested in Tower/Axum.** Its `Service`/`Layer` model, extractors, router
  composition, dynamic registries, proxy support, resilience middleware,
  public test client, and deployment-oriented state abstractions are genuine
  advantages—not just alternative syntax.
- **The principal `tower-mcp` risk is protocol lag at the release boundary.**
  Its stable server is excellent, but its draft server passed 66 and failed 36
  required checks in an unbaselined run. Its draft client passed 171, failed
  42, and emitted four recommendations-level warnings. The implementation has
  many of the new types, but not all of the required behavior.
- **Raw test volume slightly favors `tower-mcp`; test-system breadth favors
  `rmcp`.** `tower-mcp` has more test functions and a clean all-workspace,
  all-targets, all-features run. `rmcp` has much more integration-test code,
  a coverage job, cross-language tests, public API/SemVer checks, and complete
  stable and draft conformance results.
- **Documentation quality is close but different.** `rmcp` has the more
  complete protocol-facing guide. `tower-mcp` has denser source rustdoc and
  better architecture, contribution, and versioning material. Both have stale
  status documents; `tower-mcp` has more user-visible version drift.
- **Anubis is a credible Elixir alternative, but its strengths are
  architectural rather than proven protocol leadership.** It is a stable
  `1.10.0` release with an OTP-native supervision model, Phoenix/Plug
  integration, first-party Redis persistence, BEAM-cluster session routing,
  and excellent HexDocs-oriented material. It implements a broad stable
  surface, but has no full official conformance fixture or CI job. Targeted
  official probes found working initialization, ping, and multi-stream SSE,
  plus one required DNS-rebinding-protection failure in the built-in
  Streamable HTTP plug.

### Recommendation

Choose **`rmcp`** unless Tower-native composition is a first-order requirement.
Choose **`tower-mcp`** for a server-centric system where middleware,
application-state extraction, routing, runtime registration, resilience, or
proxying matter more than immediate support for every `2026-07-28` behavior.
Do not currently choose `tower-mcp` for a new draft-protocol client or for a
deployment that must claim complete draft conformance.

Between Anubis and `tower-mcp`, choose **Anubis** when the application is
already on the BEAM and supervision, process isolation, Phoenix integration,
or Redis-backed cross-node sessions are decisive. Choose **`tower-mcp`** when
officially demonstrated stable conformance, Tower middleware, typed
extractors, proxy/aggregation, client OAuth, or early `2026-07-28` support
matters more.

## Rust comparison at a glance

| Area | `rmcp` | `tower-mcp` | Edge |
| --- | --- | --- | --- |
| Stable protocol | Complete in tested client/server suites | Complete required checks; four client SHOULD warnings | Slight `rmcp` |
| `2026-07-28` draft | Complete in tested client/server suites | Partial, especially stateless lifecycle and MRTR | `rmcp` |
| Server ergonomics | Handler traits, peers, concise macros | Tower `Service`/`Layer`, extractors, nested routers | `tower-mcp` |
| Client breadth | Mature generic client, broad auth and transports | Capable stable client; draft client incomplete | `rmcp` |
| Middleware/composition | Hooks and Tower compatibility | First-class per-router/per-handler Tower layers | `tower-mcp` |
| Dynamic runtime behavior | Dynamic tools and handler routing | Dynamic tools, prompts, resources, templates, capability filters | `tower-mcp` |
| Authentication | Comprehensive OAuth client/server implementation | Strong JWT/JWKS and common OAuth flows; narrower draft support | `rmcp` |
| Testing | Broader integration, interop, conformance, coverage CI | More local test functions; excellent `TestClient` | `rmcp` overall |
| Documentation | More protocol feature coverage and snippets | Better rustdoc density and project-process docs | Split |
| Release maturity | Stable v2 line; v3 currently beta | v0.14, explicitly pre-1.0 | `rmcp` |
| Governance | Official, multi-contributor SDK | Predominantly single-maintainer project | `rmcp` |
| Small reusable types layer | No standalone runtime-free types crate | `tower-mcp-types`, no async runtime required | `tower-mcp` |
| License | Apache-2.0 | MIT or Apache-2.0 | `tower-mcp` flexibility |

## Scope and protocol context

The current stable MCP specification is
[`2025-11-25`](https://modelcontextprotocol.io/specification/2025-11-25).
The repository heads are also implementing the
[`2026-07-28` draft](https://modelcontextprotocol.io/specification/draft),
whose final release was scheduled for the day after this snapshot. The draft
is a substantial change rather than a small additive revision: among other
things it introduces a stateless core, discovery in place of a mandatory
initialize session, per-request metadata, HTTP request/response headers,
response caching, multi-round-trip requests (MRTR), and a finalized tasks
extension. See the MCP project's
[release-candidate notice](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/).

This timing matters. `tower-mcp` is highly conformant to the stable protocol,
while `rmcp` is already highly conformant to both the stable and next
protocols. A comparison that only runs the stable suite misses the largest
present-day difference.

Anubis advertises stable protocol versions through `2025-11-25` but does not
yet implement the `2026-07-28` protocol. Its focused comparison below is
therefore primarily against `tower-mcp`'s stable behavior. Draft support is
still recorded because it is a meaningful selection criterion at this
snapshot.

## Protocol adherence

### Live conformance results

I built each repository's supplied conformance client and server and ran pinned
versions of the official
[`modelcontextprotocol/conformance`](https://github.com/modelcontextprotocol/conformance)
harness. The draft runs below were performed **without expected-failure
baselines**, so gaps were not converted into passes.

| Protocol / role | `rmcp` | `tower-mcp` |
| --- | ---: | ---: |
| Stable `2025-11-25` server, active suite | **40 passed, 0 failed** | **39 passed, 0 failed** |
| Stable `2025-11-25` client | **189 passed, 0 failed, 0 warnings** | **215 passed, 0 failed, 4 warnings** |
| Draft `2026-07-28` server | **114 passed, 0 failed, 0 warnings** | **66 passed, 36 failed, 4 warnings** |
| Draft `2026-07-28` client | **376 passed, 0 failed, 0 warnings** | **171 passed, 42 failed, 4 warnings** |

Raw check totals differ because the harness runs behavior-dependent checks
against the capabilities and flows each fixture exposes. The meaningful
comparison is the absence or presence of failures and warnings, not whether one
successful fixture caused more assertions to execute.

Supplementary stable-server probes also found no required-check failure:

- Both passed all four JSON Schema 2020-12 checks.
- The SSE polling probe completed against both. The harness emitted two formal
  checks for `rmcp`, while its `tower-mcp` run recorded the successful
  `text/event-stream` response as informational output with zero scored checks.

The four `tower-mcp` stable-client warnings are SHOULD-level rather than
required failures. They concern CIMD/scopes metadata and scope step-up
behavior, so stable compliance remains strong, but `rmcp` is cleaner in this
run.

### Draft gaps in `tower-mcp`

The unbaselined draft failures align with
[`conformance-baseline-draft.yml`](tower-mcp/conformance-baseline-draft.yml)
and current code:

- The stateless-server scenario passed 4 of 28 checks. Missing behavior clusters
  around complete per-request `_meta` validation, HTTP status mapping,
  capability gating, and subscription-listen acknowledgement/filter semantics.
- Custom-header handling passed 7 of 9 checks; annotated-argument header
  validation remains incomplete.
- Most MRTR behavioral scenarios fail. The wire types exist, but the complete
  request/resume/association/task behavior is not yet implemented.
- The draft client still follows legacy initialization assumptions in several
  paths and omits required draft protocol/version header behavior.

By contrast, `rmcp` contains explicit initialize/discover/automatic lifecycle
modes and passed the draft client and server suites. Its implementation
includes request-state protection, request metadata, standard and custom
headers, caching, subscriptions/listen, MRTR, and the finalized tasks
extension.

`tower-mcp` correctly distinguishes the draft constant as upcoming rather than
including it in its stable supported-version list. `rmcp` supports the draft
version explicitly while retaining `2025-11-25` as its default/latest stable
selection. In both projects, users should opt into the draft deliberately.

### Conformance caveats

These results are strong interoperability evidence, but they are not a formal
certification:

- The harness tests each project's supplied fixture, not every public API
  combination.
- A fixture can omit an optional capability and therefore avoid related checks.
- Expected-failure baselines are useful for regression control but can obscure
  absolute status. This is why the decisive draft runs were unbaselined.
- Extensions such as tasks are not necessarily included in every SDK tier
  calculation.

The MCP project's [SDK tier criteria](https://modelcontextprotocol.io/community/sdk-tiers)
also include documentation, stable releases, versioning, roadmaps, and
dependency policy—not only conformance percentage. `rmcp` clears the technical
conformance threshold in these runs, but its own roadmap still identifies
documentation and governance-policy work. `tower-mcp` cannot meet the
stable-release requirement while it remains pre-1.0.

## Features and architecture

Legend: **Yes** = first-class implementation; **Partial** = types or a useful
subset exist, but important behavior is incomplete; **No first-class support**
= it may be constructible by an application but is not a framework feature.

### Protocol and capability surface

| Capability | `rmcp` | `tower-mcp` | Notes |
| --- | --- | --- | --- |
| Tools, prompts, resources, templates | Yes | Yes | Both cover core server primitives |
| Roots, sampling, elicitation | Yes | Yes | Form and URL elicitation represented |
| Logging, completion, progress, cancellation | Yes | Yes | Stable behavior is broad in both |
| Pagination and list-changed notifications | Yes | Yes | Both |
| Structured tool output | Yes | Yes | Both |
| Text/image/audio/embedded-resource content | Yes | Yes | Both |
| Resource links and annotations | Yes | Yes | Both |
| JSON Schema 2020-12 | Yes | Yes | Both passed supplemental conformance |
| Stable tasks APIs | Yes | Yes | `tower-mcp` has a notably usable pluggable task store |
| Draft stateless/discovery lifecycle | Yes | Partial | Largest current server gap in `tower-mcp` |
| Draft per-request metadata | Yes | Partial | Types/tests exist in `tower-mcp`; full validation does not |
| Draft headers | Yes | Partial | Custom annotated-argument validation incomplete in `tower-mcp` |
| Draft response caching | Yes | Partial | Wire/TTL support exists in `tower-mcp`; client behavior is narrower |
| Draft subscriptions/listen | Yes | Partial | `tower-mcp` exposes APIs but misses conformance semantics |
| Draft MRTR | Yes | Partial | `tower-mcp` has types; behavioral scenarios largely fail |
| Finalized tasks extension | Yes | Partial | `tower-mcp` has substantial task machinery but incomplete draft composition |

### Programming model

#### `rmcp`

The official SDK is centered on `ServerHandler`/`ClientHandler` traits, a
bidirectional `Peer`, and generic transport-backed services. Its macros make a
small tool server very concise, and the same peer abstraction makes
server-initiated requests natural. This maps closely to the protocol and keeps
examples easy to compare with specification concepts.

Strengths:

- Concise `#[tool]`, prompt, and routing macros.
- Symmetric client/server peer model.
- Generic transport traits, including async reader/writer composition.
- Protocol additions generally appear quickly and coherently.
- Rich auth and HTTP implementation.

Tradeoffs:

- Macro expansion and generic service internals can be harder to debug.
- Application middleware and per-endpoint extraction are less central to the
  design than in `tower-mcp`.
- The crate is broad; there is no independent, runtime-free types package.

Useful starting points are the
[`rmcp` README](rust-sdk/crates/rmcp/README.md),
[`ServerHandler`](rust-sdk/crates/rmcp/src/handler/server.rs), and
[`service`](rust-sdk/crates/rmcp/src/service.rs) implementations.

#### `tower-mcp`

`tower-mcp` treats MCP as an application-routing problem. `McpService` and
`McpRouter` fit Tower's `Service`/`Layer` vocabulary; handlers use Axum-like
extractors for state, extensions, context, and JSON. Routers can be merged or
nested and can apply global or per-handler layers.

Strengths:

- Familiar model for Tower/Axum users.
- Excellent middleware reuse: tracing, auditing, authorization, rate limiting,
  circuit breaking, bulkheads, and custom layers.
- Runtime registries for tools, prompts, resources, and templates.
- Capability filtering/guards and per-route policy.
- Strong application-testing API through `TestClient`.
- Proxy/aggregation support and explicit deployment/state abstractions.

Tradeoffs:

- More concepts and usually more setup than the macro-first official SDK.
- Some central modules are very large, notably the router and HTTP transport;
  that concentration raises review and maintenance cost.
- Protocol behavior can lag behind the already-present public types.

Useful starting points are the
[`tower-mcp` README](tower-mcp/README.md),
[`router`](tower-mcp/crates/tower-mcp/src/router.rs), and
[`testing`](tower-mcp/crates/tower-mcp/src/testing.rs) modules.

### Distinctive framework features

| Feature | `rmcp` | `tower-mcp` |
| --- | --- | --- |
| Tower-native handler middleware | Compatible at service boundaries | First-class, including per-handler layers |
| Axum-style state/context extractors | No | Yes |
| Router nesting/merging | Handler/tool routing | Full application-style router composition |
| Dynamic registries | Strong dynamic tool routing | Tools, prompts, resources, and templates |
| MCP backend proxy/aggregation | No first-class subsystem | Yes |
| Resilience primitives | Compose externally | Optional circuit breaker, limiter, bulkhead layers |
| Public in-process test client | No equivalent high-level helper | Yes |
| Runtime-free types crate | No | Yes, `tower-mcp-types` |
| Compatibility comparison tool | Cross-language integration tests | `rmcp-compat` wire comparison utility |
| Full interactive client example | Several focused clients | Substantial `mcp-repl` application |

### Transports and deployment

Both cover stdio, Streamable HTTP, child-process use, in-memory/channel-style
testing, and Unix-domain-socket scenarios. Both expose session/event-state
abstractions suitable for more than a single in-memory process.

Notable differences:

- `rmcp` has especially generic transport composition and dedicated
  worker/WASI examples.
- `tower-mcp` offers a first-class WebSocket transport. WebSocket is useful,
  but it is not one of the standard MCP transports and should not be counted as
  extra protocol conformance.
- `tower-mcp`'s session stores, event stores, router state, and deployment
  examples present horizontal-scaling concerns more directly.
- `tower-mcp-types` is independently useful for WASM or data-model consumers;
  the complete `tower-mcp` runtime is not equivalently runtime-free.

### Authentication and security

`rmcp` has the broader and more thoroughly exercised OAuth implementation:
discovery/metadata, dynamic and client-ID metadata registration, authorization
code and refresh flows, scope escalation, client credentials, and
`private_key_jwt`, plus extensive issuer, redirect, and SSRF-oriented tests.

`tower-mcp` has strong server-application building blocks: JWT/JWKS validation,
scope layers, authorization middleware, audit logging, and common OAuth client
flows including PKCE and client credentials. It is quite practical for stable
resource-server use, but the conformance warnings and draft failures show that
metadata and step-up behavior are not yet as complete.

Both repositories run dependency-audit automation. `rmcp` additionally runs
CodeQL. This review did not substitute those workflows with a claim that either
tree is vulnerability-free.

## Testing and quality engineering

### Quantitative inventory

These static counts are directional, not coverage percentages. Generated test
cases, doctests, parametrization, and differing workspace layouts make a
one-to-one score invalid.

| Metric | `rmcp` repository | `tower-mcp` repository |
| --- | ---: | ---: |
| Rust test attributes in crate trees | 919 | 1,144 |
| Integration-test Rust LOC under crate `tests/` | 20,632 | 8,375 |
| Registered Cargo examples | 33 | 31 |
| Example Rust files / LOC | 51 / 6,902 | 54 / 17,928 |
| Core production-ish Rust LOC | 44,781 | 57,746 |
| Enumerated core test entries in tested feature matrix | 1,006 | 1,142 |
| Whole-workspace test entries available in clean all-feature run | See caveat below | 1,225 |

Neither repository contains a fuzzing or property-test program. Both would
benefit from property tests around JSON-RPC envelope classification, URI and
header validation, pagination cursors, capability negotiation, and lifecycle
state transitions.

### Local test results

- `tower-mcp`: `cargo test --workspace --all-targets --all-features` passed.
- `rmcp`: its CI-equivalent core matrix passed, including all features except
  the special `local` feature used to gate environment-dependent behavior.
  Several tests need localhost sockets or subprocesses, so the final run was
  performed outside the restricted filesystem/network sandbox.
- `rmcp`: `cargo test --workspace --all-features` does **not** compile as a
  single feature-unified mega-build. The `local` feature becomes unified across
  examples and conflicts with cfg-gated Streamable HTTP exports; a Unix
  example also produces a non-`Send` future in that configuration. The
  repository's supported CI builds examples individually, where they pass.
  This is a feature-composability rough edge, not evidence that normal `rmcp`
  configurations fail.

### CI and suite design

`rmcp` has the stronger overall quality gate:

- formatting, clippy, MSRV, normal and feature-matrix tests;
- example builds and tests;
- official client and server conformance;
- cross-language integration against JavaScript and Python MCP stacks;
- `cargo llvm-cov`;
- dependency audit and CodeQL;
- SemVer/public-API checks.

`tower-mcp` also has a serious CI system:

- stable and beta toolchains on Linux, macOS, and Windows;
- MSRV validation;
- all-target/all-feature tests;
- WASM validation for the types crate;
- rustdoc with warnings denied;
- examples, formatting, clippy, and dependency audit;
- stable server/client and draft-server conformance jobs.

Two improvements would materially strengthen `tower-mcp`:

1. Add the draft **client** conformance job; it is presently the least visible
   large gap.
2. Add line/branch coverage publication or a threshold, particularly for
   transport failure and lifecycle-state paths.

`rmcp` already runs `cargo llvm-cov`, but its workflow does not publish or
enforce a coverage percentage. `tower-mcp` has no line-coverage job. Therefore
this report intentionally gives **no numeric line-coverage score**.

### Test-suite character

`rmcp` invests more in protocol, transport, HTTP, OAuth, and external
interoperability paths. Its integration-test tree is more than twice as large,
and its official-schema snapshots help detect model drift.

`tower-mcp` invests heavily in unit-level framework behavior: router
composition, extractors, layers, stores, tasks, proxying, dynamic features, and
failure handling. Its public `TestClient` and test fixture types are unusually
good framework ergonomics. It also validates many types against a vendored
official `2025-11-25` schema.

## Documentation quality

### Inventory

| Metric | `rmcp` | `tower-mcp` |
| --- | ---: | ---: |
| Markdown files / lines | 17 / 3,977 | 14 / 2,574 |
| Rust fenced examples in Markdown | 54 | 19 |
| Rustdoc comment lines in core crate trees | 5,263 | 11,900 |
| Example Rust LOC | 6,902 | 17,928 |

### `rmcp` documentation

The root [`README`](rust-sdk/README.md) is the better protocol-facing guide. It
walks through tools, resources, prompts, sampling, roots, logging, completion,
subscriptions, caching, headers, stateless operation, MRTR, tasks, and OAuth,
with many copyable snippets. Dedicated macro and OAuth material plus 51
example source files make feature discovery relatively easy.

Weaknesses:

- The core crate's own README is very short; guidance is dispersed across the
  workspace root and examples.
- There is no project-level `CONTRIBUTING.md` or `VERSIONING.md` in this
  checkout.
- [`ROADMAP.md`](rust-sdk/ROADMAP.md) is already stale at this fast-moving
  boundary: it records draft conformance gaps that the live run no longer has.
- That roadmap's own feature-documentation audit marks only 26 of 48 feature
  areas fully documented, with 14 undocumented and seven partial. The large
  README is strong, but not exhaustive.

Assessment: **very good user-facing protocol documentation, incomplete formal
project and API-documentation coverage.**

### `tower-mcp` documentation

The root [`README`](tower-mcp/README.md) gives an excellent conceptual
introduction to routers, handlers, extractors, middleware, testing, proxying,
and deployment. Source rustdoc is much denser than `rmcp`'s and is checked with
warnings denied. The project also has clear
[`CONTRIBUTING`](tower-mcp/CONTRIBUTING.md),
[`VERSIONING`](tower-mcp/VERSIONING.md), and
[`SECURITY`](tower-mcp/SECURITY.md) documents. Its examples contain far more
application code, including a substantial
[`mcp-repl`](tower-mcp/examples/mcp-repl/README.md).

Weaknesses:

- README installation snippets still use `0.12` while the workspace is `0.14`.
- [`ROADMAP.md`](tower-mcp/ROADMAP.md) still describes the `0.9.x` era and an
  older conformance total.
- Conformance totals differ among the README, roadmap, example notes, and
  baselines.
- The README says there are 24 focused examples, while 31 are registered across
  the workspace.
- There are fewer short, copyable feature snippets; much knowledge lives in
  source rustdoc or full examples.

Assessment: **excellent framework/architecture documentation and source
rustdoc, with material release/status drift.**

## Maintenance, maturity, and dependency profile

| Attribute | `rmcp` | `tower-mcp` |
| --- | --- | --- |
| Checkout version | `3.0.0-beta.2` | `0.14.0` |
| Mature release line | Yes, v2 stable tags | No, pre-1.0 |
| MSRV | Rust 1.88 | Rust 1.90 |
| Edition | 2024 | 2024 |
| License | Apache-2.0 | MIT or Apache-2.0 |
| Commits in prior 30 / 90 days | 76 / 128 | 70 / 137 |
| Maintainer shape in recent history | Multiple active organization contributors | One dominant maintainer plus automation |
| Default features | Server, macros, base64 | Empty; user opts into capabilities |

Both projects are moving very quickly. `rmcp`'s multi-contributor official
governance, stable v2 line, and ecosystem role lower continuity and adoption
risk. Its v3 API is nevertheless beta at this snapshot.

`tower-mcp`'s pace and scope are impressive, but v0.x versioning and maintainer
concentration are real production risks. Its
[`VERSIONING.md`](tower-mcp/VERSIONING.md) correctly warns that pre-1.0 minor
versions may contain breaking changes.

Approximate unique dependency nodes for this target, measured with
`cargo tree`, were:

| Configuration | Unique nodes |
| --- | ---: |
| `rmcp`, no default features | 37 |
| `rmcp`, default features | 55 |
| `rmcp`, all features | 179 |
| `tower-mcp`, no/default features | 62 |
| `tower-mcp`, all features | 180 |
| `tower-mcp-types` alone | 16 |

These figures are target- and feature-dependent. The meaningful result is that
fully enabled footprints are similar; `rmcp` starts lighter, while
`tower-mcp` offers the smallest standalone types-only option.

## Focused comparison: Anubis MCP versus `tower-mcp`

This section treats the language difference as an architectural choice rather
than trying to score Elixir syntax against Rust syntax. The useful question is
which framework supplies the MCP behavior, deployment model, and operational
features an application needs.

### Focused verdict

Anubis is a serious framework, not a thin protocol wrapper. Its strongest
advantages are native BEAM supervision, process isolation, Phoenix/Plug
embedding, session-scoped runtime components, first-party Redis persistence,
and cross-node live-session routing through `:pg`. It is also on a stable 1.x
release line.

`tower-mcp` is the stronger choice on protocol confidence and composable
application behavior. Its complete stable server/client fixtures pass the
official harness, and it has first-class Tower layers, typed extractors,
router composition, resilience policies, a public in-process test client,
proxy/aggregation, broader OAuth client behavior, and partial support for the
next protocol.

The practical choice is therefore:

- choose **Anubis** for an Elixir/Phoenix system where OTP supervision and
  distributed session continuity are part of the architecture;
- choose **`tower-mcp`** when the application needs the best demonstrated
  stable interoperability, rich per-handler policy, backend aggregation, or a
  path toward `2026-07-28`;
- do not presently describe Anubis as fully `2025-11-25` conformant. Its core
  behavior is well implemented and heavily tested, but the repository lacks a
  complete conformance fixture, and a targeted transport probe found one
  required security failure.

### General feature comparison

| Area | Anubis MCP | `tower-mcp` | Practical edge |
| --- | --- | --- | --- |
| Primary model | OTP processes, supervisors, behaviours, Plug/Phoenix | Tower `Service`/`Layer`, routers, typed extractors | Ecosystem-dependent |
| Client and server | Both | Both | Split |
| Core MCP primitives | Tools, prompts, resources, templates | Tools, prompts, resources, templates | Even |
| Runtime registration | Tools, prompts, resources, and templates on a session frame | Dynamic router registries for all four component types | Different scope |
| Schema approach | Peri DSL, runtime validation, advertised-output validation in client | Rust types plus Serde/Schemars, with raw JSON escape hatches | Different tradeoff |
| Process supervision | Native OTP supervisors and per-session GenServers | Tokio tasks and Tower service lifecycle | Anubis |
| Distributed live routing | Built-in `:pg` registry across connected BEAM nodes | Application-supplied deployment/routing | Anubis |
| Durable session backend | First-party optional Redis adapter with TTL | Pluggable `SessionStore`; in-repository implementation is in-memory | Anubis out of box |
| Event resumption | Pluggable event store, bounded in-memory store, Last-Event-ID replay | Pluggable event store and HTTP resumption | Even in abstraction; Anubis has stronger cluster recipe |
| Durable tasks | Pluggable server task store and TTL; incomplete method surface | Pluggable client/server task machinery | `tower-mcp` overall |
| Middleware and policy | Plug pipeline and application callbacks | Global and per-handler Tower layers | `tower-mcp` |
| Built-in resilience | OTP restart/fault isolation | Circuit breaker, rate limiter, bulkhead, timeout/retry layers | Split |
| Router composition | Server/component modules | Merge, nest, guard, filter, and layer routers | `tower-mcp` |
| Backend proxy/aggregation | No first-class subsystem | First-class multi-backend proxy and namespace routing | `tower-mcp` |
| Observability | Telemetry events and structured Logger integration | `tracing` plus audit/observability middleware | Split |
| High-level test helper | ExUnit support modules and normal Plug testing | Public `TestClient` and fixture types | `tower-mcp` |
| Release status | `1.10.0`, stable 1.x tags | `0.14.0`, documented pre-1.0 contract | Anubis |
| License | LGPL-3.0 | MIT or Apache-2.0 | `tower-mcp` is more permissive |

Anubis's dynamic registration is notably session-local: callbacks can add
components to an [`Anubis.Server.Frame`](anubis-mcp/lib/anubis/server/frame.ex)
for that live client. `tower-mcp`'s registries are more naturally part of a
shared application router. Neither is universally better; per-client catalogs
favor Anubis, while shared hot registration and layered policy favor
`tower-mcp`.

The reliability models are also complementary rather than equivalent. OTP
gives Anubis supervised failure domains and restart semantics without an
additional framework abstraction. `tower-mcp` gives the application explicit
request-path policies—rate limits, circuit breaking, bulkheads, tracing, and
authorization—that can be attached globally or to one handler.

### Stable protocol surface

| Capability | Anubis MCP | `tower-mcp` |
| --- | --- | --- |
| Advertised stable versions | `2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25` | `2025-03-26`, `2025-11-25` |
| Tools/resources/prompts/templates | Yes | Yes |
| Pagination and list-changed notifications | Yes | Yes |
| Resource subscriptions | Yes | Yes |
| Logging, completion, progress, cancellation, ping | Yes | Yes |
| Structured tool output and content variants | Yes | Yes |
| Roots | Yes | Yes |
| Classic sampling | Yes | Yes |
| Sampling with tools and `toolChoice` | No first-class support found | Yes |
| Form elicitation | Yes | Yes |
| URL elicitation | No first-class support found | Yes |
| Expanded enum/multi-select elicitation schemas | Partial older subset | Yes |
| Icons metadata | No first-class support found | Yes |
| Stable tasks | Partial server-side implementation | Client and server support |
| Next protocol, `2026-07-28` | No | Partial |

Anubis declares more backward-version breadth. Its
[`Protocol.Registry`](anubis-mcp/lib/anubis/protocol/registry.ex) has four
version modules, while `tower-mcp` deliberately concentrates on two stable
versions and the next draft. The tradeoff is that Anubis's
[`V2025_11_25`](anubis-mcp/lib/anubis/protocol/v2025_11_25.ex) module mostly
inherits `2025-06-18` and adds tasks. Source inspection found no corresponding
implementation for several other additions called out in the official
[`2025-11-25` changelog](https://modelcontextprotocol.io/specification/2025-11-25/changelog):
URL-mode elicitation, tool-enabled sampling, and icons.

Anubis's tasks implementation is useful but not complete. It supports
task-augmented `tools/call`, get/result/cancel/status operations, TTLs, and a
pluggable store. `tasks/list` intentionally returns method-not-found because
the implementation lacks a safe authorization-context binding, and there is
no comparable high-level client task API. Tasks remain experimental in the
stable specification, so this limitation should not be confused with a core
MCP conformance percentage.

### Conformance evidence

`tower-mcp` supplies purpose-built client and server fixtures and runs official
conformance in CI. Anubis currently supplies neither. To get evidence without
mistaking absent harness-specific fixture components for SDK defects, I ran
only fixture-independent server scenarios against a temporary minimal Anubis
Streamable HTTP server. The server and probe code were removed afterward.

| Official harness `0.1.16`, protocol `2025-11-25` | Anubis MCP | `tower-mcp` |
| --- | ---: | ---: |
| Complete stable server fixture | Not available | **39 passed, 0 failed** |
| Complete stable client fixture | Not available | **215 passed, 0 failed, 4 SHOULD warnings** |
| Server initialize probe | **1 passed, 0 failed** | Covered by complete fixture |
| Ping probe | **1 passed, 0 failed** | Covered by complete fixture |
| Concurrent SSE streams probe | **2 passed, 0 failed** | Covered by complete fixture |
| DNS rebinding protection probe | **1 passed, 1 required failure** | Passed in complete fixture |
| Default SSE polling probe | 0 scored failures, 2 SHOULD warnings | No required failure |

These Anubis rows are **not** a conformance score. They demonstrate that the
initialization, request, and concurrent Streamable HTTP paths interoperate with
the official harness, while exposing one concrete security defect:
the built-in Streamable HTTP plug returned HTTP 200 for an invalid
`Host`/`Origin` request. The stable transport specification requires servers
to validate `Origin` when present and return 403 for an invalid value; see
[Security Warning: DNS Rebinding Attacks](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#security-warning-dns-rebinding-attacks).
A host application can put its own validating Plug in front, but Anubis's
default transport did not satisfy the required check.

The two polling warnings concerned the recommended SSE priming event and
`retry` field. Anubis has configurable event-store/resumption machinery and
tests for priming and retry behavior, so this result describes the minimal
default configuration—not a claim that the framework cannot implement those
recommendations.

Source review identified two more transport areas that deserve full harness
coverage:

- The Streamable HTTP plug does not appear to reject an unsupported
  `MCP-Protocol-Version` header. The stable specification requires a 400
  response for an invalid or unsupported supplied version.
- Its recovery-oriented session path can create or reinitialize state where a
  strict interpretation would reject a missing or invalid session. The spec
  says that after a server terminates a session it must return 404 for that
  session ID, and recommends 400 when a session-requiring server receives no
  session ID.

Those are inspection findings, not scored failures from this targeted run.
They should be converted into executable conformance cases before making a
stronger claim. Overall, `tower-mcp` has materially stronger protocol evidence:
it proves a complete stable client and server surface, whereas Anubis proves a
well-tested implementation plus a small set of official transport scenarios.

### Transports and authentication

| Feature | Anubis MCP | `tower-mcp` |
| --- | --- | --- |
| stdio server/client | Yes | Yes |
| Streamable HTTP server/client | Yes | Yes |
| Legacy HTTP+SSE server/client | Yes | Yes |
| Child-process client | Yes | Yes |
| WebSocket | Client transport | Server transport |
| Unix-domain socket | No first-class transport | Yes |
| In-process/channel transport | OTP process/message composition | First-class channel transport |
| Server bearer validation | Yes | Yes |
| JWT/JWKS | Yes | Yes |
| Opaque-token introspection | Built-in RFC 7662 validator | Custom validator required |
| Protected resource metadata | Yes | Yes |
| Component/handler scopes | Per-component scopes | Tower auth/scope layers |
| OAuth client flows | Explicitly out of scope | Client credentials and authorization-code/PKCE flows |

Both frameworks cover the standard transports. Their WebSocket offerings are
opposite halves of a non-standard extension: Anubis exposes a client
transport, while `tower-mcp` exposes a full server transport. It should not be
counted as extra MCP conformance in either case.

Anubis is a capable OAuth resource server. Its
[`Authorization`](anubis-mcp/lib/anubis/server/authorization.ex) subsystem
supports protected-resource metadata, custom validators, JWT/JWKS validation
with caching, per-component scopes, and RFC 7662 introspection for opaque
tokens. `tower-mcp` has the broader end-to-end story because it also implements
common client acquisition, discovery, refresh, and PKCE flows. Anubis has the
more convenient built-in opaque-token adapter.

### Distributed operation

This is Anubis's clearest non-language-specific advantage. Its server sessions
are supervised processes; an optional
[`Redis session store`](anubis-mcp/lib/anubis/server/session/store/redis.ex)
persists serialized state with TTL handling, and the
[`Registry.PG`](anubis-mcp/lib/anubis/server/registry/pg.ex) adapter locates
live session processes across a connected BEAM cluster. Its Streamable HTTP
[`EventStore`](anubis-mcp/lib/anubis/server/transport/streamable_http/event_store.ex)
supports Last-Event-ID resumption and can be replaced with a durable adapter.
Together, these provide a concrete multi-node recovery story in the framework
itself.

The supplied Redis store still assumes a single writer per session: its update
path is a GET/merge/SETEX sequence with last-write-wins semantics, not a Redis
transaction or compare-and-swap. Cross-node routing helps preserve that
invariant during normal operation, but split-brain and concurrent-recovery
behavior remain application/deployment concerns.

`tower-mcp` has well-designed `SessionStore`, `EventStore`, and `TaskStore`
traits plus deployment documentation, but the repository's supplied stores
are in-memory. A production team can attach Redis, a database, or another
backend; it must build or select that adapter. Conversely, `tower-mcp` is
better when one process needs to compose and govern multiple upstream MCP
servers: Anubis has no equivalent to its proxy/aggregation subsystem.

### Tests, CI, and documentation

The local Anubis run passed **975 tests**—35 doctests and 940 ExUnit tests—when
the 11 integration-tagged cases were enabled with Redis. Its static inventory
contains 937 `test` declarations and about 17,546 lines under `test/`, versus
about 20,871 Elixir lines under `lib/`. These counts show unusually substantial
test investment, but they are not directly comparable with Rust's generated
test entries or a line-coverage percentage.

Anubis CI is strong on language-level correctness:

- Elixir 1.18, 1.19.5, and 1.20.2 across OTP 27/28 combinations;
- formatting and strict Credo;
- Dialyzer;
- the full ExUnit suite with Redis.

Its largest QA omissions relative to `tower-mcp` are official conformance,
coverage publication/enforcement, external interoperability tests, and a
multi-OS matrix. `tower-mcp` passed its 1,225-entry all-workspace test run and
adds stable/draft conformance, Rust toolchain/MSRV checks, documentation
validation, WASM types checks, examples, and Linux/macOS/Windows coverage.
Neither repository publishes a numeric line/branch coverage result, so this
review assigns no percentage.

Anubis documentation is excellent once the reader moves beyond its concise
root README. Its ExDoc package includes dedicated guides for
[server construction](anubis-mcp/pages/building-a-server.md),
[clients](anubis-mcp/pages/building-a-client.md),
[transports](anubis-mcp/pages/transports.md),
[authorization](anubis-mcp/pages/authorization.md),
[testing](anubis-mcp/pages/testing.md), and
[recipes](anubis-mcp/pages/recipes.md), plus a cheatsheet and substantial API
docs. The inventory found about 3,232 Markdown/cheat-sheet lines, 370 `@doc`
annotations, and 309 `@spec` annotations. Its current release number and
installation material are consistent.

Compared with `tower-mcp`, Anubis has fresher release-facing guides and a more
cohesive generated documentation package. `tower-mcp` has richer source-level
architecture material, substantially larger examples, and better
documentation of composition, proxying, versioning, and project policy, but
also the version/status drift described earlier.

### Maturity and project risk

| Attribute | Anubis MCP | `tower-mcp` |
| --- | --- | --- |
| Checkout version | `1.10.0` | `0.14.0` |
| Language baseline | Elixir `~> 1.18` | Rust 1.90, edition 2024 |
| Release line | Post-1.0 | Documented pre-1.0 |
| Commits in prior 30 / 90 days | 38 / 81 | 70 / 137 |
| Total commits at snapshot | 344 | 447 |
| Maintainer shape | One dominant maintainer plus smaller contributions/automation | One dominant maintainer plus automation |
| License | LGPL-3.0 | MIT or Apache-2.0 |

Both projects are active and both carry maintainer-concentration risk. Anubis's
post-1.0 release line lowers expected API-churn risk relative to `tower-mcp`;
it does not remove continuity risk. `tower-mcp` is changing faster and has a
documented pre-1.0 breakage policy. Raw commit totals are context, not a
quality score. Its dual permissive license is easier for many distribution
models, while Anubis's LGPL terms deserve project-specific review. This report
is not legal advice.

### Potential Anubis conformance contribution

A full upstream conformance integration looks like a worthwhile, bounded
contribution, but it should be designed separately from this review. The most
useful shape would be:

1. a server fixture exposing the harness's expected tools, prompts, resources,
   subscriptions, sampling, elicitation, and task behaviors;
2. a client fixture/driver for the official client scenarios;
3. a pinned `2025-11-25` CI job that reports unbaselined required failures;
4. focused regression tests for Origin validation, protocol-version headers,
   and missing/terminated-session status codes.

That would turn the current ambiguity into reproducible evidence and prevent
future protocol drift. The exploratory server used for this report was
intentionally deleted rather than presented as contribution-quality code. A
clean fork is available for this work; scope, upstream appetite, and whether
to fix the known transport issue before or alongside the fixture should be
discussed before creating the branch.

## Decision guide

### Prefer `rmcp` when

- latest protocol adherence and interoperability are primary;
- the application needs both a full client and server;
- the `2026-07-28` stateless lifecycle, MRTR, caching, headers, subscriptions,
  or tasks extension must work now;
- OAuth breadth and security edge cases matter;
- a stable release line, official governance, or broad ecosystem compatibility
  is required;
- concise macro-based servers are preferred.

### Prefer `tower-mcp` when

- the project is primarily an MCP server embedded in a Tower/Axum stack;
- existing `Layer`s, extractors, state, authorization, rate limiting, tracing,
  or resilience policies should apply naturally;
- runtime registration and capability filtering are central;
- one server must proxy or aggregate multiple MCP backends;
- a runtime-free MCP types crate is useful;
- stable `2025-11-25` support is sufficient while draft work catches up.

### Prefer Anubis MCP when

- the application is written in Elixir or embedded in Phoenix/Plug;
- OTP supervision, fault isolation, and per-session processes are valuable;
- live sessions must be routed across a BEAM cluster;
- a supplied Redis session store and concrete cross-node recovery model are
  preferable to implementing storage adapters;
- session-specific dynamic tools, prompts, resources, or templates are useful;
- a post-1.0 release line is more important than verified full-suite conformance
  or early next-protocol support.

### Priorities that would most improve `tower-mcp`

1. Complete draft lifecycle, per-request metadata/header validation,
   subscriptions/listen semantics, and MRTR behavior.
2. Put draft-client conformance in CI and publish unbaselined summaries beside
   baseline-controlled regressions.
3. Reconcile all version, example-count, roadmap, and conformance numbers.
4. Publish a line/branch coverage report and add property/fuzz testing for wire
   and state-machine boundaries.
5. Move toward a 1.0 contract and broaden the maintainer/reviewer base.

### Priorities that would most improve `rmcp`

1. Finish the feature-documentation gaps recorded in its roadmap and expand the
   crate-level guide.
2. Add explicit contribution, versioning, and dependency-policy documents.
3. Publish coverage results or enforce a threshold rather than only running
   coverage tooling.
4. Make the entire workspace robust to Cargo feature unification, or document
   why the all-workspace/all-features combination is unsupported.
5. Keep roadmap conformance status generated from current CI results to avoid
   rapid drift.

### Priorities that would most improve Anubis MCP

1. Add complete official client/server conformance fixtures and run the stable
   suite in CI.
2. Validate `Origin` in the built-in HTTP plugs and add regression coverage for
   DNS rebinding.
3. Complete the non-task `2025-11-25` changes: URL elicitation, tool-enabled
   sampling, icons, and expanded enum/multi-select elicitation schemas.
4. Enforce protocol-version and missing/terminated-session HTTP behavior
   explicitly.
5. Complete task listing/client APIs, then add external interoperability and
   published line/branch coverage.

## Bottom line

For a general recommendation, **`rmcp` wins** on the criteria with the highest
interoperability and production-risk impact: protocol adherence, client
completeness, authentication breadth, release maturity, and governance.

That conclusion should not obscure `tower-mcp`'s strongest result: it is not a
less complete clone of the official SDK. It is a thoughtful Tower-native
framework with composition and operational features the official SDK does not
match. For a stable-protocol, server-heavy Rust application, that programming
model can outweigh the official SDK's advantages. The current dividing line is
clear: **choose `rmcp` for protocol leadership; choose `tower-mcp` for
Tower-native application architecture, while accepting draft and pre-1.0
risk.**

Anubis adds a third, coherent option rather than changing that Rust conclusion:
**choose Anubis for OTP-native, distributed Elixir architecture.** Relative to
`tower-mcp`, it offers a more mature release line and stronger supplied
multi-node persistence/routing, but gives up Tower's middleware and proxy
model, client OAuth breadth, next-protocol work, and—most importantly—the
confidence of a complete official conformance run.

## Methodology and reproducibility notes

The review combined:

- repository manifests, public API/source inspection, feature flags, examples,
  docs, changelogs/roadmaps, git history, and CI workflows;
- static inventories using `rg`, `find`, `wc`, `git`, and `cargo metadata/tree`;
- supported local Cargo test matrices;
- the full Anubis ExUnit suite with its Redis integration cases;
- official conformance harness versions `0.1.16` for `2025-11-25` and
  `0.2.0-alpha.9` for `2026-07-28`;
- separate server and client runs with explicit `--spec-version` filters;
- supplementary stable-server JSON Schema and SSE polling suites;
- fixture-independent Anubis initialization, ping, concurrent-SSE,
  DNS-rebinding, and polling probes using the stable harness.

All numerical findings are tied to the three commits at the top of this report.
The Anubis probes are deliberately reported separately from complete
conformance fixtures and should not be aggregated into a percentage.
Fast-moving draft implementations and conformance packages can change these
results quickly, so future comparisons should pin repository commits and
harness versions as done here.
