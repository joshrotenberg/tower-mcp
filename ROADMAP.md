# Roadmap

## Current Status

- **Version**: 0.20.x
- **Spec version**: 2025-11-25 by default, plus the released 2026-07-28 protocol as an opt-in implementation behind `protocol-2026-07-28`
- **Server conformance (2025-11-25)**: 48/48 (empty baseline)
- **Client conformance (2025-11-25)**: 235/235 (empty baseline)
- **Server conformance (2026-07-28)**: 114/114 (empty baseline)
- **Client conformance (2026-07-28)**: 399/399 (empty baseline)
- **MSRV**: 1.90 (Rust 2024 edition)

## SDK Tier Assessment

tower-mcp targets Tier 2 per the [MCP SDK Tiering System](https://modelcontextprotocol.io/community/sdk-tiers).

| # | Requirement | Status |
|---|-------------|--------|
| 1 | Server conformance >= 80% | 100% (48/48 stable; 114/114 final) |
| 2 | Client conformance >= 80% | 235/235 stable; 399/399 final |
| 3 | Issue triage within 1 month | Active |
| 4 | P0 resolution within 2 weeks | 0 open |
| 5 | Stable release >= 1.0.0 | 0.20.x -- pre-1.0 |
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

The released 2026-07-28 revision is implemented behind the opt-in
`protocol-2026-07-28` feature with an exact per-instance `ProtocolSupport`
allow-list. `PROTOCOL_VERSION_2026_07_28` is the canonical wire-version
constant; the former `EXPERIMENTAL_PROTOCOL_VERSION` and
`UPCOMING_PROTOCOL_VERSION` names are deprecated compatibility aliases. The
legacy 2025-era implementation remains the non-breaking default.

Implemented final-revision areas include:

- Stateless `server/discover` lifecycle and per-request metadata
- Multi Round-Trip Requests (MRTR) and result discriminators
- `subscriptions/listen`, cache hints, and strict MCP HTTP headers
- Final client lifecycle, subscriptions, authorization conformance, and schema recovery
- Reusable OAuth registration, bounded scope escalation, and issuer-keyed credential persistence
- Compile-time/runtime protocol selection across HTTP, STDIO, WebSocket, Unix-over-HTTP, and JSON-RPC services
- Extension-key validation, explicit runtime negotiation, and final Tasks extension advertisement
- Feature-gated typed MCP Apps resources, tool linkage, visibility, CSP constraints, and text fallback
- Final Tasks extension lifecycle, ownership, input requests, notifications, expiry, and client update APIs

### Recently Completed and Monitored

| SEP | Title | Status | Issue |
|-----|-------|--------|-------|
| 2575/2567 | Final stateless/sessionless MCP lifecycle | Implemented (opt-in, `protocol-2026-07-28`) | #929, #1059 |
| 2322 | Multi Round-Trip Requests (MRTR) | Implemented; policy/composition follow-ups remain | #950 |
| 2549 | Cache hints and response caching | Implemented | #1047, #1053 |
| 2243 | Standard/custom HTTP headers | Implemented | #1049, #1051 |
| 2133/1865 | Extensions and MCP Apps | Implemented; monitoring upstream conformance coverage | #1060 |

### Monitoring

Open SEPs are read from the [`SEP` label](https://github.com/modelcontextprotocol/modelcontextprotocol/labels/SEP) upstream. Issues are filed here only for SEPs this crate has committed to implementing.

## Future Directions

- **1.0.0 stable release**: API freeze and stability guarantees
- **Default protocol transition**: keep 2025-11-25 as the non-breaking default while exposing released 2026-07-28 through explicit compile-time and runtime selection (#929).
- **Extension interoperability**: adopt upstream Tasks and MCP Apps conformance scenarios as they land.
- **SEP-1763 interceptors**: tower middleware maps naturally to this proposal.
