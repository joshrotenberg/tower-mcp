# Changelog

All notable changes to this project will be documented in this file.

## [0.11.0] - 2026-06-02

### Bug Fixes

- **types:** Reconcile McpErrorCode against SEP-2575 canonical assignments ([#838](https://github.com/joshrotenberg/tower-mcp/pull/838))
- **types:** Use InvalidParams (-32602) for resource-not-found per SEP-2164 ([#841](https://github.com/joshrotenberg/tower-mcp/pull/841))

### Documentation

- Position conformance numbers prominently per SEP-2484 ([#840](https://github.com/joshrotenberg/tower-mcp/pull/840))
- **examples:** Add axum embedding guide example ([#842](https://github.com/joshrotenberg/tower-mcp/pull/842))
- Update lib.rs and README for 2026-07-28 stateless protocol (#858, #871) ([#886](https://github.com/joshrotenberg/tower-mcp/pull/886))
- Module-level docs for stateless transport, context, and types (#860, #864, #867, #874) ([#889](https://github.com/joshrotenberg/tower-mcp/pull/889))
- Update README version strings to 0.11 ([#897](https://github.com/joshrotenberg/tower-mcp/pull/897))

### Features

- **types,router:** Wire server/discover RPC end-to-end per SEP-2575 ([#829](https://github.com/joshrotenberg/tower-mcp/pull/829))
- **types:** TTL on list results (SEP-2549) + deprecation metadata (SEP-2577/2596) ([#826](https://github.com/joshrotenberg/tower-mcp/pull/826))
- **oauth-client:** Validate iss parameter on authorization callback per SEP-2468 ([#835](https://github.com/joshrotenberg/tower-mcp/pull/835))
- **tool:** Add input_schema(Value) setter on ToolBuilder ([#837](https://github.com/joshrotenberg/tower-mcp/pull/837))
- **http:** Return spec-shape UnsupportedProtocolVersion per SEP-2575 ([#839](https://github.com/joshrotenberg/tower-mcp/pull/839))
- **session-store:** Populate client_info and client_capabilities on initialize ([#843](https://github.com/joshrotenberg/tower-mcp/pull/843))
- **stateless:** Migrate StatelessRequestMeta to FINAL SEP-2575 keys ([#844](https://github.com/joshrotenberg/tower-mcp/pull/844))
- **http,context:** Thread SEP-2575 per-request _meta through RequestContext ([#847](https://github.com/joshrotenberg/tower-mcp/pull/847))
- **tasks:** Repackage as io.modelcontextprotocol/tasks extension per SEP-2663 ([#846](https://github.com/joshrotenberg/tower-mcp/pull/846))
- **http:** SEP-2243 HTTP standardization headers ([#845](https://github.com/joshrotenberg/tower-mcp/pull/845))
- **http:** Add messages/listen SSE streaming endpoint (#814 chunk 4) ([#852](https://github.com/joshrotenberg/tower-mcp/pull/852))
- **http:** Version-gated stateless mode for 2026-07-28 protocol (#814 chunk 5) ([#853](https://github.com/joshrotenberg/tower-mcp/pull/853))
- **examples:** Add server/discover walkthrough example ([#884](https://github.com/joshrotenberg/tower-mcp/pull/884)) ([#896](https://github.com/joshrotenberg/tower-mcp/pull/896))
- **examples:** Add stateless HTTP client example for 2026-07-28 protocol ([#881](https://github.com/joshrotenberg/tower-mcp/pull/881)) ([#894](https://github.com/joshrotenberg/tower-mcp/pull/894))

### Testing

- **types:** Add wire-format assertion helpers for JSON-RPC ([#808](https://github.com/joshrotenberg/tower-mcp/pull/808))
- Lock down JSON-RPC parse-error wire format for stdio and http ([#812](https://github.com/joshrotenberg/tower-mcp/pull/812))
- Audit full JSON Schema 2020-12 support per SEP-2106 ([#833](https://github.com/joshrotenberg/tower-mcp/pull/833))
- **stdio:** Expose run_with_streams to enable end-to-end loop tests ([#836](https://github.com/joshrotenberg/tower-mcp/pull/836))
- **http:** Add stateless transport coverage for tools/list, notifications, and missing Mcp-Method ([#890](https://github.com/joshrotenberg/tower-mcp/pull/890))
- **http:** Add messages/listen session-negotiated and id-echo tests (#861, #863) ([#892](https://github.com/joshrotenberg/tower-mcp/pull/892))
- Add server/discover versions, per-request meta, router dispatch, and UnsupportedProtocolVersion tests (#866, #869, #872, #875) ([#893](https://github.com/joshrotenberg/tower-mcp/pull/893))
- **conformance:** Add TTL, deprecation metadata, and tasks extension coverage (closes #873) ([#895](https://github.com/joshrotenberg/tower-mcp/pull/895))



## [0.10.1] - 2026-05-15

### Bug Fixes

- **client:** Use match guard for OAuth error_description fallback ([#794](https://github.com/joshrotenberg/tower-mcp/pull/794))
- **stdio:** Bidi transport closes on parse error; strip UTF-8 BOM ([#797](https://github.com/joshrotenberg/tower-mcp/pull/797))

### Documentation

- Production deployment guide ([#780](https://github.com/joshrotenberg/tower-mcp/pull/780))

### Features

- Unix Domain Socket transport ([#773](https://github.com/joshrotenberg/tower-mcp/pull/773))
- Pluggable SessionStore for horizontal scaling ([#778](https://github.com/joshrotenberg/tower-mcp/pull/778))
- **event-store:** Pluggable SSE event store for stream resumption ([#779](https://github.com/joshrotenberg/tower-mcp/pull/779))
- **session:** Restore unknown sessions from SessionStore + auto-reinitialize ([#782](https://github.com/joshrotenberg/tower-mcp/pull/782))
- **context:** Typed helpers for server-originated tasks/* (SEP-1686) ([#793](https://github.com/joshrotenberg/tower-mcp/pull/793))
- **router:** Reversible tool/resource/prompt disable/enable ([#792](https://github.com/joshrotenberg/tower-mcp/pull/792))
- **http:** Host header validation with :authority fallback and rejection logging ([#798](https://github.com/joshrotenberg/tower-mcp/pull/798))
- **http:** Allow external NotificationSender via with_notifications ([#801](https://github.com/joshrotenberg/tower-mcp/pull/801))

### Testing

- Close critical test coverage gaps ([#757](https://github.com/joshrotenberg/tower-mcp/pull/757)) ([#766](https://github.com/joshrotenberg/tower-mcp/pull/766))



## [0.10.0] - 2026-03-26

### Documentation

- Fix remaining 0.8 version reference in README ([#759](https://github.com/joshrotenberg/tower-mcp/pull/759))

### Features

- Default to optional sessions for HTTP transport ([#761](https://github.com/joshrotenberg/tower-mcp/pull/761))
- HTTP client session expiry detection and automatic recovery ([#764](https://github.com/joshrotenberg/tower-mcp/pull/764))
- OAuth 2.0 Authorization Code grant with PKCE for HTTP client ([#765](https://github.com/joshrotenberg/tower-mcp/pull/765))



## [0.9.2] - 2026-03-25

### Documentation

- Update all top-level docs for v0.9 and example reorganization ([#756](https://github.com/joshrotenberg/tower-mcp/pull/756))

### Features

- WebSocket spec compliance, stateless mode, and resource/prompt context ([#751](https://github.com/joshrotenberg/tower-mcp/pull/751))
- SEP-1442 stateless HTTP transport ([#753](https://github.com/joshrotenberg/tower-mcp/pull/753))

### Refactor

- Consolidate examples from 27 to 23 ([#755](https://github.com/joshrotenberg/tower-mcp/pull/755))

### Testing

- Close critical test coverage gaps ([#758](https://github.com/joshrotenberg/tower-mcp/pull/758))



## [0.9.1] - 2026-03-19

### Features

- Add optional_sessions() to HttpTransport for client compatibility ([#743](https://github.com/joshrotenberg/tower-mcp/pull/743))



## [0.9.0] - 2026-03-18

### Features

- Replace simple CircuitBreakerLayer with tower-resilience re-exports ([#740](https://github.com/joshrotenberg/tower-mcp/pull/740))



## [0.8.8] - 2026-03-17

### Features

- Derive Serialize/Deserialize on RouterResponse and inner types ([#735](https://github.com/joshrotenberg/tower-mcp/pull/735))
- Add list_sessions() and terminate_session() to SessionHandle ([#736](https://github.com/joshrotenberg/tower-mcp/pull/736))



## [0.8.7] - 2026-03-16

### Features

- Add McpProxy::remove_backend(), replace_backend(), backend_namespaces() ([#730](https://github.com/joshrotenberg/tower-mcp/pull/730))



## [0.8.6] - 2026-03-16

### Documentation

- Update README, lib.rs, and AGENTS.md for accuracy and completeness ([#726](https://github.com/joshrotenberg/tower-mcp/pull/726))
- Add missing examples and register all examples in Cargo.toml ([#728](https://github.com/joshrotenberg/tower-mcp/pull/728))



## [0.8.5] - 2026-03-16

### Features

- Add request rewrite helpers and response error checking ([#707](https://github.com/joshrotenberg/tower-mcp/pull/707))

### Testing

- Add Send+Sync assertions for McpProxy ([#705](https://github.com/joshrotenberg/tower-mcp/pull/705))



## [0.8.4] - 2026-03-09

### Features

- Add built-in tool call logging to McpRouter ([#700](https://github.com/joshrotenberg/tower-mcp/pull/700))



## [0.8.3] - 2026-03-07

### Features

- Add StdioClientTransport::spawn_command for custom Command config ([#624](https://github.com/joshrotenberg/tower-mcp/pull/624))
- Add HttpTransport::from_service() for generic service support ([#626](https://github.com/joshrotenberg/tower-mcp/pull/626))
- Expose session count via SessionHandle from HttpTransport ([#629](https://github.com/joshrotenberg/tower-mcp/pull/629))
- Add dynamic backend addition to McpProxy ([#630](https://github.com/joshrotenberg/tower-mcp/pull/630))



## [0.8.2] - 2026-03-07

### Bug Fixes

- Proxy improvements -- error visibility, poll_ready, instructions, docs ([#614](https://github.com/joshrotenberg/tower-mcp/pull/614))
- Make compile_uri_template fallible, add try_handler ([#619](https://github.com/joshrotenberg/tower-mcp/pull/619))

### Documentation

- Pre-release documentation fixes ([#621](https://github.com/joshrotenberg/tower-mcp/pull/621))

### Features

- MCP proxy for multi-server aggregation ([#600](https://github.com/joshrotenberg/tower-mcp/pull/600))
- Optional proc macros for tools, prompts, and resources ([#613](https://github.com/joshrotenberg/tower-mcp/pull/613))
- Dynamic prompt registry and skill-to-prompt example ([#615](https://github.com/joshrotenberg/tower-mcp/pull/615))
- Dynamic resource and resource template registries ([#616](https://github.com/joshrotenberg/tower-mcp/pull/616))
- Add AuditLayer middleware for structured audit logging ([#618](https://github.com/joshrotenberg/tower-mcp/pull/618))



## [0.8.1] - 2026-03-06

### Bug Fixes

- Use oneshot() to ensure poll_ready before call in JsonRpcService ([#598](https://github.com/joshrotenberg/tower-mcp/pull/598))

### Features

- Add client handler and sampling server examples ([#589](https://github.com/joshrotenberg/tower-mcp/pull/589))
- Add OAuth client example ([#590](https://github.com/joshrotenberg/tower-mcp/pull/590))



## [0.8.0] - 2026-03-06

### Bug Fixes

- Return input validation errors as tool results (SEP-1303) ([#575](https://github.com/joshrotenberg/tower-mcp/pull/575))
- Ensure tool input schemas always have "type": "object" ([#587](https://github.com/joshrotenberg/tower-mcp/pull/587))

### Documentation

- Add tower-mcp-types section to README ([#483](https://github.com/joshrotenberg/tower-mcp/pull/483))
- Restructure README to highlight key differentiators ([#485](https://github.com/joshrotenberg/tower-mcp/pull/485))
- Add .title() to examples and expand doc comment ([#522](https://github.com/joshrotenberg/tower-mcp/pull/522))
- Add missing doc comments for all public API items ([#586](https://github.com/joshrotenberg/tower-mcp/pull/586))

### Features

- Add MCP conformance client and fix HTTP client SSE handling ([#577](https://github.com/joshrotenberg/tower-mcp/pull/577))
- 100% MCP client conformance (265/265) ([#579](https://github.com/joshrotenberg/tower-mcp/pull/579))
- Inject tool annotations into request extensions for middleware ([#588](https://github.com/joshrotenberg/tower-mcp/pull/588))

### Refactor

- Move to standard workspace layout with crates/ directory ([#552](https://github.com/joshrotenberg/tower-mcp/pull/552))
- Hide internal types and tighten pub visibility ([#581](https://github.com/joshrotenberg/tower-mcp/pull/581))


