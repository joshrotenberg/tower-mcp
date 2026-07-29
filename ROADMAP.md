# Roadmap

## Current Status

- **Version**: 0.14.x
- **Spec version**: 2025-11-25 (plus the 2026-07-28 draft behind the `stateless` feature)
- **Server conformance (2025-11-25)**: 39/39
- **Client conformance (2025-11-25)**: 264/266 (two offline-access gaps, tracked in #953)
- **Server conformance (2026-07-28)**: 67/101 (gaps baselined, tracked in #929)
- **MSRV**: 1.90 (Rust 2024 edition)

## SDK Tier Assessment

tower-mcp targets Tier 2 per the [MCP SDK Tiering System](https://modelcontextprotocol.io/community/sdk-tiers).

| # | Requirement | Status |
|---|-------------|--------|
| 1 | Server conformance >= 80% | 100% (39/39) |
| 2 | Client conformance >= 80% | 264/266 |
| 3 | Issue triage within 1 month | Active |
| 4 | P0 resolution within 2 weeks | 0 open |
| 5 | Stable release >= 1.0.0 | 0.14.x -- pre-1.0 |
| 6 | Spec tracking within 6 months | Current (2025-11-25) |
| 7 | Documentation coverage | In progress |
| 8 | Dependency update policy | dependabot.yml |
| 9 | Roadmap | This file |

### Remaining for Tier 2

- **1.0.0 release**: Public API audit, feature flag naming finalized, error type stability
- **Documentation**: Audit against tier assessment feature checklist

## Spec Tracking

### Implemented SEPs

All features from the 2025-11-25 spec are implemented, including:
- Streamable HTTP transport with session management
- WebSocket transport (SEP-1288 compliant)
- OAuth 2.1 resource server support (with JWKS)
- Elicitation (form and URL modes)
- Tool annotations
- Async tasks with lifecycle management
- SSE event IDs and stream resumption (SEP-1699)
- Sampling (all transports)
- Completion (autocomplete)

### In Progress

| SEP | Title | Status | Issue |
|-----|-------|--------|-------|
| 1442 | Stateless MCP mode | Implemented (experimental, `stateless` feature) | #748 |
| 1288 | WebSocket transport | Implemented (binary rejection, subprotocols, zombie prevention) | #745-#747 |

### Monitoring

Open SEPs are tracked automatically via `.github/workflows/sep-sync.yml` and labeled `spec-tracking` in issues.

## Future Directions

- **1.0.0 stable release**: API freeze and stability guarantees
- **SEP-1442 transport completion**: WebSocket stateless mode, per-request capabilities wiring
- **SEP-1763 interceptors**: tower middleware maps naturally to this proposal
- **Distributed session support**: sticky routing documentation, stateless mode as primary solution
