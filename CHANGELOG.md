# Changelog

All notable changes to this project will be documented in this file.

## [unreleased]

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
