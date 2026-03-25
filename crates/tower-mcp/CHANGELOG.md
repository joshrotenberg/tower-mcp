# Changelog

All notable changes to this project will be documented in this file.

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


