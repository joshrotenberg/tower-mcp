# Roadmap

This document tracks tower-mcp's path from Tier 3 to Tier 2 (and eventually Tier 1) per the [MCP SDK Tiering System](https://modelcontextprotocol.io/community/sdk-tiers).

## Current Status

- **Tier**: 3
- **Spec version**: 2025-11-25
- **Server conformance**: 30/30 (100%)
- **Client conformance**: Not yet tested (no conformance client)

## Tier 2 Requirements

| # | Requirement | Status | Tracking |
|---|-------------|--------|----------|
| 1 | Server conformance >= 80% | 100% (30/30) | Done |
| 2 | Client conformance >= 80% | No client yet | #538 |
| 3 | Issue triage within 1 month | 97% within 2BD | Done |
| 4 | P0 resolution within 2 weeks | 0 open | Done |
| 5 | Stable release >= 1.0.0 | 0.7.0 | #539 |
| 6 | Spec tracking within 6 months | 64-day gap | #540 |
| 7 | Documentation coverage | Not assessed | #541 |
| 8 | Dependency update policy | dependabot.yml | Done |
| 9 | Roadmap | This file | Done |

## Tier 2 Blockers

### Conformance client (#538)

Build a conformance client binary that exercises all 20 scored client scenarios (4 core + 16 auth). This overlaps with the planned CLI client (#172) and should share infrastructure.

### Stable release (#539)

Resolve before 1.0.0:
- Workspace layout migration (#526)
- Public API audit (ensure nothing leaks that shouldn't)
- Feature flag naming finalized
- Error type stability review

### Documentation gaps (#541)

The tier assessment evaluates 48 features across tools, resources, prompts, sampling, elicitation, roots, logging, completions, transports, and protocol mechanics. Audit against this list and fill gaps.

## Spec Tracking

### Accepted SEPs (implemented)

tower-mcp tracks the 2025-11-25 spec. All features from accepted SEPs in that version are implemented, including:
- Streamable HTTP transport
- WebSocket transport
- OAuth 2.1 resource server support (with JWKS)
- Elicitation (form mode, enum inference, defaults)
- Tool annotations (readOnlyHint, idempotentHint, etc.)
- DNS rebinding protection
- Session management (mcp-session-id)

### Draft SEPs (monitoring)

| SEP | Title | Impact | Issue |
|-----|-------|--------|-------|
| 2357 | `application/mcp+json` media type | Low | #528 |
| 2356 | File input support | Medium | #529 |
| 2339 | Task continuity | Medium | #530 |
| 2342 | Memory interchange format | Low | #531 |
| 2350/2351/2352 | OAuth clarifications | Medium | #532 |
| 2322 | Multi round-trip requests | High | #533 |
| 2317 | Content negotiation | Low | #534 |
| 2325 | SSH transport | Low | #535 |

### Existing spec feature tracking

| SEP | Title | Status | Issue |
|-----|-------|--------|-------|
| 1442 | Stateless mode | Draft | #263 |
| 2127 | Server cards / .well-known | Draft | #155 |

## Tier 1 Path

Tier 1 requirements (beyond Tier 2) are not yet fully published but are expected to include:
- 95%+ conformance pass rate (server and client)
- Comprehensive documentation (all 48 features)
- Published versioning policy (VERSIONING.md)
- Published security policy (SECURITY.md)
- Active maintenance with rapid spec tracking
