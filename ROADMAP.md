# Roadmap

## Current Status

- **Version**: 0.14.x
- **Spec version**: 2025-11-25 by default (plus the released 2026-07-28 protocol as an experimental implementation behind `protocol-2026-07-28`)
- **Server conformance (2025-11-25)**: 39/39
- **Client conformance (2025-11-25)**: 311/311 (empty baseline)
- **Server conformance (2026-07-28)**: 114/114 (empty baseline)
- **Client conformance (2026-07-28)**: 399/399 (empty baseline)
- **MSRV**: 1.90 (Rust 2024 edition)

## SDK Tier Assessment

tower-mcp targets Tier 2 per the [MCP SDK Tiering System](https://modelcontextprotocol.io/community/sdk-tiers).

| # | Requirement | Status |
|---|-------------|--------|
| 1 | Server conformance >= 80% | 100% (39/39) |
| 2 | Client conformance >= 80% | 311/311 stable; 399/399 final |
| 3 | Issue triage within 1 month | Active |
| 4 | P0 resolution within 2 weeks | 0 open |
| 5 | Stable release >= 1.0.0 | 0.14.x -- pre-1.0 |
| 6 | Spec tracking within 6 months | Current (2026-07-28) |
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

The released 2026-07-28 revision is implemented behind the experimental
`protocol-2026-07-28` feature with an exact per-instance `ProtocolSupport`
allow-list. The legacy 2025-era implementation remains the default.

Implemented final-revision areas include:

- Stateless `server/discover` lifecycle and per-request metadata
- Multi Round-Trip Requests (MRTR) and result discriminators
- `subscriptions/listen`, cache hints, and strict MCP HTTP headers
- Final client lifecycle, subscriptions, authorization conformance, and schema recovery
- Reusable OAuth registration, bounded scope escalation, and issuer-keyed credential persistence
- Compile-time/runtime protocol selection across HTTP, STDIO, WebSocket, Unix-over-HTTP, and JSON-RPC services
- Extension-key validation, explicit runtime negotiation, and safe withholding of incomplete Tasks advertisement
- Feature-gated typed MCP Apps resources, tool linkage, visibility, CSP constraints, and text fallback

### In Progress

| SEP | Title | Status | Issue |
|-----|-------|--------|-------|
| 2575/2567 | Final stateless/sessionless MCP lifecycle | Implemented (experimental, `protocol-2026-07-28`) | #929, #1059 |
| 2322 | Multi Round-Trip Requests (MRTR) | Implemented; policy/composition follow-ups remain | #950 |
| 2549 | Cache hints and response caching | Implemented | #1047, #1053 |
| 2243 | Standard/custom HTTP headers | Implemented | #1049, #1051 |
| 2133/1865 | Extensions and MCP Apps | Implemented; upstream conformance scenarios remain | #1060 |

### Monitoring

Open SEPs are tracked automatically via `.github/workflows/sep-sync.yml` and labeled `spec-tracking` in issues.

## Future Directions

- **1.0.0 stable release**: API freeze and stability guarantees
- **Promotion of 2026-07-28**: close the remaining transport, MRTR, Tasks, and extension gates in #929.
- **Tasks extension**: complete input-required task state, notifications, expiry, and client update APIs (#951).
- **MCP Apps interoperability**: track and adopt upstream extension conformance scenarios as they land (#1060).
- **SEP-1763 interceptors**: tower middleware maps naturally to this proposal.
