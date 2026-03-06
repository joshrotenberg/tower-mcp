# Changelog

All notable changes to this project will be documented in this file.

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


