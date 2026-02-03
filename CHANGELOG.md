# Changelog

All notable changes to this project will be documented in this file.

## [unreleased]

## [0.3.2] - 2026-02-03

### Features

- Add McpTracingLayer for structured request logging ([#315](https://github.com/joshrotenberg/tower-mcp/pull/315)) ([#316](https://github.com/joshrotenberg/tower-mcp/pull/316))
- **crates-mcp:** Add McpTracingLayer for request logging ([#319](https://github.com/joshrotenberg/tower-mcp/pull/319))



## [0.3.1] - 2026-02-03

### Documentation

- Update README and AGENTS.md for 0.3.0 ([#312](https://github.com/joshrotenberg/tower-mcp/pull/312))



## [0.3.0] - 2026-02-02

### Bug Fixes

- **markdownlint-mcp:** Wrap list_rules result in object for MCP compatibility ([#287](https://github.com/joshrotenberg/tower-mcp/pull/287))
- **markdownlint-mcp:** Implement fix_content to properly apply fixes ([#289](https://github.com/joshrotenberg/tower-mcp/pull/289))
- **examples:** Add .build() calls to resource handlers ([#300](https://github.com/joshrotenberg/tower-mcp/pull/300))

### Documentation

- Add middleware placement guide to lib.rs ([#297](https://github.com/joshrotenberg/tower-mcp/pull/297))

### Features

- API consistency audit for 0.3.0 ([#278](https://github.com/joshrotenberg/tower-mcp/pull/278))
- **examples:** Add markdownlint-mcp example ([#285](https://github.com/joshrotenberg/tower-mcp/pull/285))
- **examples:** Add codegen-mcp server ([#290](https://github.com/joshrotenberg/tower-mcp/pull/290))
- **codegen-mcp:** Add example comments and README generation ([#291](https://github.com/joshrotenberg/tower-mcp/pull/291))
- Per-tool middleware via .layer() on ToolBuilder ([#293](https://github.com/joshrotenberg/tower-mcp/pull/293))
- Per-resource middleware via .layer() on ResourceBuilder ([#298](https://github.com/joshrotenberg/tower-mcp/pull/298))
- Per-prompt middleware via .layer() on PromptBuilder ([#299](https://github.com/joshrotenberg/tower-mcp/pull/299))
- **router:** Add merge() and nest() for router composition ([#305](https://github.com/joshrotenberg/tower-mcp/pull/305))
- **extract:** Add axum-inspired extractor pattern for tool handlers ([#306](https://github.com/joshrotenberg/tower-mcp/pull/306))
- Simplify handler API with unified extractor pattern ([#308](https://github.com/joshrotenberg/tower-mcp/pull/308))
- Add router-level state and Extension<T> extractor ([#309](https://github.com/joshrotenberg/tower-mcp/pull/309))
- Typed rejection handling for extractors ([#303](https://github.com/joshrotenberg/tower-mcp/pull/303)) ([#310](https://github.com/joshrotenberg/tower-mcp/pull/310))

### Miscellaneous Tasks

- Add example MCP servers and tour guide ([#286](https://github.com/joshrotenberg/tower-mcp/pull/286))
- **ci:** Add examples build check ([#311](https://github.com/joshrotenberg/tower-mcp/pull/311))



## [0.2.3] - 2026-01-30

### Bug Fixes

- Improve deploy verification reliability ([#271](https://github.com/joshrotenberg/tower-mcp/pull/271))
- Add required-features for capability_filtering example ([#274](https://github.com/joshrotenberg/tower-mcp/pull/274))

### Documentation

- Add from_serialize reference to CallToolResult::json docs ([#276](https://github.com/joshrotenberg/tower-mcp/pull/276))

### Features

- **tool:** Add handler_no_params_with_state for stateful parameterless tools ([#275](https://github.com/joshrotenberg/tower-mcp/pull/275))



## [0.2.2] - 2026-01-29

### Bug Fixes

- Restructure JS template to fix YAML parsing ([#180](https://github.com/joshrotenberg/tower-mcp/pull/180))
- Update SEP status label when issue is final ([#259](https://github.com/joshrotenberg/tower-mcp/pull/259))

### Documentation

- Add resource/prompt examples, badges, SEP tracking ([#255](https://github.com/joshrotenberg/tower-mcp/pull/255))
- Add capability filtering example ([#267](https://github.com/joshrotenberg/tower-mcp/pull/267))

### Features

- **http:** Add SSE event IDs and stream resumption (SEP-1699) ([#176](https://github.com/joshrotenberg/tower-mcp/pull/176))
- Add automated SEP tracking workflow ([#179](https://github.com/joshrotenberg/tower-mcp/pull/179))
- **examples:** Add external API authentication patterns ([#260](https://github.com/joshrotenberg/tower-mcp/pull/260))
- **filter:** Add session-specific capability filtering ([#264](https://github.com/joshrotenberg/tower-mcp/pull/264))
- **filter:** Add session-specific resource filtering ([#265](https://github.com/joshrotenberg/tower-mcp/pull/265))
- **filter:** Add session-specific prompt filtering ([#266](https://github.com/joshrotenberg/tower-mcp/pull/266))
- **session:** Add type-safe extensions to SessionState ([#269](https://github.com/joshrotenberg/tower-mcp/pull/269))

### Refactor

- Extract SEP sync script to separate file ([#254](https://github.com/joshrotenberg/tower-mcp/pull/254))



## [0.2.1] - 2026-01-29

### Bug Fixes

- Correct Dockerfile path in fly.toml with build context ([#165](https://github.com/joshrotenberg/tower-mcp/pull/165))
- Dockerfile path should be relative to fly.toml location ([#166](https://github.com/joshrotenberg/tower-mcp/pull/166))
- Use latest stable Rust in Dockerfile for let-chains support ([#167](https://github.com/joshrotenberg/tower-mcp/pull/167))
- Add initialized notification to deploy verification ([#171](https://github.com/joshrotenberg/tower-mcp/pull/171))
- **crates-mcp:** Add --minimal flag to workaround Claude Code MCP issues ([#174](https://github.com/joshrotenberg/tower-mcp/pull/174))

### Documentation

- Update version references to 0.2 ([#163](https://github.com/joshrotenberg/tower-mcp/pull/163))
- Add live demo section to README and bump rate limits ([#168](https://github.com/joshrotenberg/tower-mcp/pull/168))

### Features

- Add HTTP transport and Fly.io deployment for crates-mcp demo ([#164](https://github.com/joshrotenberg/tower-mcp/pull/164))
- **http:** Add /health endpoint for simple health checks ([#173](https://github.com/joshrotenberg/tower-mcp/pull/173))
- **crates-mcp:** Add comprehensive MCP feature showcase ([#170](https://github.com/joshrotenberg/tower-mcp/pull/170))
- **tool:** Add NoParams type for parameterless tools ([#175](https://github.com/joshrotenberg/tower-mcp/pull/175))

### Miscellaneous Tasks

- Expand CI testing matrix with platforms and Rust versions ([#161](https://github.com/joshrotenberg/tower-mcp/pull/161))
- Add deployment verification step to deploy workflow ([#169](https://github.com/joshrotenberg/tower-mcp/pull/169))



## [0.2.0] - 2026-01-29

### Documentation

- Add badges and update install instructions ([#103](https://github.com/joshrotenberg/tower-mcp/pull/103))
- Add spec section links to README compliance matrix ([#122](https://github.com/joshrotenberg/tower-mcp/pull/122))
- Use handler_with_state in crates-mcp example ([#139](https://github.com/joshrotenberg/tower-mcp/pull/139))
- Add async tasks and more ToolBuilder examples to README ([#156](https://github.com/joshrotenberg/tower-mcp/pull/156))
- Show server protocol version response in http_server example ([#157](https://github.com/joshrotenberg/tower-mcp/pull/157))

### Features

- Add JsonRpcLayer for ServiceBuilder composition ([#116](https://github.com/joshrotenberg/tower-mcp/pull/116))
- Implement tower::Service for AuthService ([#117](https://github.com/joshrotenberg/tower-mcp/pull/117))
- Add .layer() to HTTP and WebSocket transports ([#118](https://github.com/joshrotenberg/tower-mcp/pull/118))
- Add BoxError type alias ([#119](https://github.com/joshrotenberg/tower-mcp/pull/119))
- Add full feature flag ([#120](https://github.com/joshrotenberg/tower-mcp/pull/120))
- Add tower middleware composition examples ([#121](https://github.com/joshrotenberg/tower-mcp/pull/121))
- Add MCP conformance test suite ([#123](https://github.com/joshrotenberg/tower-mcp/pull/123))
- Add OAuth 2.1 resource server support ([#129](https://github.com/joshrotenberg/tower-mcp/pull/129))
- Add MCP test harness with TestClient ([#124](https://github.com/joshrotenberg/tower-mcp/pull/124)) ([#130](https://github.com/joshrotenberg/tower-mcp/pull/130))
- Add ScopeEnforcementLayer for per-operation OAuth scope checks ([#127](https://github.com/joshrotenberg/tower-mcp/pull/127)) ([#132](https://github.com/joshrotenberg/tower-mcp/pull/132))
- API ergonomics quick wins (#133-#137) ([#138](https://github.com/joshrotenberg/tower-mcp/pull/138))
- OAuth follow-ups -- WebSocket well-known endpoint and JWKS fetching (#126, #128) ([#140](https://github.com/joshrotenberg/tower-mcp/pull/140))
- Bump MCP protocol version to 2025-11-25 ([#142](https://github.com/joshrotenberg/tower-mcp/pull/142))
- Add weather server example ([#158](https://github.com/joshrotenberg/tower-mcp/pull/158))

### Miscellaneous Tasks

- Release v0.1.0 ([#101](https://github.com/joshrotenberg/tower-mcp/pull/101))
- Remove unused tokio-tungstenite direct dependency ([#104](https://github.com/joshrotenberg/tower-mcp/pull/104))
- Bump jsonwebtoken to v10 and reqwest to v0.13 ([#141](https://github.com/joshrotenberg/tower-mcp/pull/141))
- 0.2.0 release readiness ([#159](https://github.com/joshrotenberg/tower-mcp/pull/159))



## [0.1.0] - 2026-01-28

### Bug Fixes

- Address code review findings ([#44](https://github.com/joshrotenberg/tower-mcp/pull/44))
- Use MCP-reserved error codes for resource errors ([#54](https://github.com/joshrotenberg/tower-mcp/pull/54))

### Documentation

- Expand CLAUDE.md with project context and motivation
- Update README with current implementation status ([#67](https://github.com/joshrotenberg/tower-mcp/pull/67))
- Update README with current implementation status ([#71](https://github.com/joshrotenberg/tower-mcp/pull/71))
- Add documentation to key protocol types ([#100](https://github.com/joshrotenberg/tower-mcp/pull/100))

### Features

- Initial tower-mcp implementation
- MCP spec compliance and documentation
- Add tool name validation per MCP spec
- Add batch request support per JSON-RPC 2.0 spec
- Add stdio transport, session enforcement, and integration tests
- Improve error handling and add doc tests
- Add resources and prompts support
- Add Streamable HTTP transport ([#19](https://github.com/joshrotenberg/tower-mcp/pull/19))
- Add progress and cancellation support ([#20](https://github.com/joshrotenberg/tower-mcp/pull/20))
- Add logging notifications support ([#41](https://github.com/joshrotenberg/tower-mcp/pull/41))
- Add task lifecycle management ([#42](https://github.com/joshrotenberg/tower-mcp/pull/42))
- Add resource templates support ([#39](https://github.com/joshrotenberg/tower-mcp/pull/39))
- Add resource subscriptions support ([#43](https://github.com/joshrotenberg/tower-mcp/pull/43))
- Add logging/setLevel support for VS Code compatibility ([#55](https://github.com/joshrotenberg/tower-mcp/pull/55))
- Add session TTL and cleanup to HTTP transport ([#58](https://github.com/joshrotenberg/tower-mcp/pull/58))
- Add WebSocket and child process transports ([#60](https://github.com/joshrotenberg/tower-mcp/pull/60))
- Add MCP client support with stdio transport ([#62](https://github.com/joshrotenberg/tower-mcp/pull/62))
- Add elicitation support for user input requests ([#63](https://github.com/joshrotenberg/tower-mcp/pull/63))
- Add auth module with API key and bearer token helpers ([#64](https://github.com/joshrotenberg/tower-mcp/pull/64))
- Improve session reconnection with JSON-RPC error codes ([#65](https://github.com/joshrotenberg/tower-mcp/pull/65))
- Add roots/discovery support ([#68](https://github.com/joshrotenberg/tower-mcp/pull/68))
- Add completion and sampling support ([#69](https://github.com/joshrotenberg/tower-mcp/pull/69))
- Add sampling runtime for stdio transport ([#72](https://github.com/joshrotenberg/tower-mcp/pull/72))
- Add sampling support for WebSocket transport ([#75](https://github.com/joshrotenberg/tower-mcp/pull/75))
- Add sampling support for HTTP transport ([#76](https://github.com/joshrotenberg/tower-mcp/pull/76))
- Add MCP 2025-11-25 spec compliance enhancements ([#81](https://github.com/joshrotenberg/tower-mcp/pull/81))
- Add comprehensive crates-mcp example ([#87](https://github.com/joshrotenberg/tower-mcp/pull/87))
- Add DX improvements for result types and middleware support ([#94](https://github.com/joshrotenberg/tower-mcp/pull/94))
- Improve test coverage and documentation ([#95](https://github.com/joshrotenberg/tower-mcp/pull/95))
- Implement completion/complete handler ([#96](https://github.com/joshrotenberg/tower-mcp/pull/96))
- Add elicitation runtime support ([#97](https://github.com/joshrotenberg/tower-mcp/pull/97))
- Add client CLI example ([#98](https://github.com/joshrotenberg/tower-mcp/pull/98))

### Miscellaneous Tasks

- Add CI workflow and release-plz automation
- Prepare for crates.io publishing ([#99](https://github.com/joshrotenberg/tower-mcp/pull/99))

### Refactor

- Extract JsonRpcService into dedicated module ([#57](https://github.com/joshrotenberg/tower-mcp/pull/57))
- Deduplicate StdioTransport and SyncStdioTransport code ([#59](https://github.com/joshrotenberg/tower-mcp/pull/59))

### Testing

- Update integration tests for MCP error codes ([#56](https://github.com/joshrotenberg/tower-mcp/pull/56))



### Features

- Initial tower-mcp implementation
- Tool builder pattern with type-safe handlers
- McpTool trait for complex tool implementations
- McpRouter with Tower Service implementation
- JsonRpcService for protocol framing
- Session state management and enforcement
- Batch request support per JSON-RPC 2.0 spec
- Tool name validation per MCP spec
- Stdio transport for CLI usage
- Protocol version negotiation
- Tool annotations (readOnlyHint, destructiveHint, idempotentHint, openWorldHint)
- Content annotations (audience, priority)

### Documentation

- Comprehensive CLAUDE.md with project context
- README with usage examples
- Basic example demonstrating tool creation
- Stdio server example for MCP client testing
