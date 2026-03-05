# Versioning Policy

## Semantic Versioning

tower-mcp follows [Semantic Versioning 2.0.0](https://semver.org/). While below 1.0.0, minor version bumps may include breaking changes.

## Workspace Versioning

The workspace maintains synchronized versions:

- `tower-mcp` and `tower-mcp-types` share the same version number
- Both crates are released together

## Minimum Supported Rust Version (MSRV)

- Current MSRV: **1.90** (Rust 2024 edition)
- MSRV bumps are treated as minor version changes (not patch)
- MSRV is tested in CI on every push

## MCP Specification Tracking

- tower-mcp tracks the [Model Context Protocol specification](https://spec.modelcontextprotocol.io/)
- Current spec version: **2025-11-25**
- Spec version changes that require breaking API changes will bump the minor version (pre-1.0) or major version (post-1.0)

## Feature Flags

Feature flags may be added in minor versions. Existing feature flag behavior will not change in patch versions. See `Cargo.toml` for the full list of available features.

## Breaking Changes

Breaking changes are:

- Communicated in commit messages using `feat!:` or `fix!:` prefixes
- Documented in the auto-generated CHANGELOG
- Batched into minor version releases when possible
