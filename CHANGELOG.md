# Changelog

All notable changes to this project will be documented in this file.

## [unreleased]

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
