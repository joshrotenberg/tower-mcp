# Changelog

All notable changes to this project will be documented in this file.

## [0.21.1] - 2026-08-08



## [0.21.0] - 2026-08-07

### Bug Fixes

- **types:** Give JsonRpcError a Display impl ([#1234](https://github.com/joshrotenberg/tower-mcp/pull/1234))

  Breaking: `JsonRpcError` now renders as `message (code N)` rather than the
  derived debug form, and `Error::JsonRpc` renders the inner error directly.
  Code matching on those message strings needs updating.

### Documentation

- Correct three claims that contradict the code ([#1235](https://github.com/joshrotenberg/tower-mcp/pull/1235))



## [0.20.1] - 2026-08-05

### Bug Fixes

- **types:** Omit absent optional params instead of sending null ([#1216](https://github.com/joshrotenberg/tower-mcp/pull/1216))
- **types:** Drop nonconforming _meta keys instead of failing the value ([#1217](https://github.com/joshrotenberg/tower-mcp/pull/1217))



## [0.20.0] - 2026-08-05

### Bug Fixes

- **types:** Preserve elicitation form field order ([#1203](https://github.com/joshrotenberg/tower-mcp/pull/1203))



## [0.19.0] - 2026-08-05

### Bug Fixes

- **types:** Dispatch PrimitiveSchemaDefinition on the declared type ([#1193](https://github.com/joshrotenberg/tower-mcp/pull/1193))



## [0.18.2] - 2026-08-04

### Features

- **protocol:** Enable 2026-07-28 by default on feature-compiled clients ([#1183](https://github.com/joshrotenberg/tower-mcp/pull/1183))
- **transport:** Route subscriptions/listen through the middleware boundary ([#1185](https://github.com/joshrotenberg/tower-mcp/pull/1185))



## [0.18.1] - 2026-08-04



## [0.18.0] - 2026-08-03



## [0.17.2] - 2026-08-02



## [0.17.1] - 2026-08-01

### Features

- **types:** Add JSON-RPC structural inspection ([#1130](https://github.com/joshrotenberg/tower-mcp/pull/1130))
- **types:** Add exact-revision MCP inspection ([#1132](https://github.com/joshrotenberg/tower-mcp/pull/1132))
- **runtime:** Enforce exact MCP inspection profiles ([#1133](https://github.com/joshrotenberg/tower-mcp/pull/1133))



## [0.17.0] - 2026-08-01

### Documentation

- **oauth:** Add end-to-end authorization guide ([#1125](https://github.com/joshrotenberg/tower-mcp/pull/1125))
- Complete public API rustdoc ([#1126](https://github.com/joshrotenberg/tower-mcp/pull/1126))
- Add task-oriented usage guides ([#1127](https://github.com/joshrotenberg/tower-mcp/pull/1127))
- Publish guides in rustdoc ([#1128](https://github.com/joshrotenberg/tower-mcp/pull/1128))

### Miscellaneous Tasks

- **conformance:** Align stable suites on current harness ([#1105](https://github.com/joshrotenberg/tower-mcp/pull/1105))



## [0.16.1] - 2026-07-31

### Miscellaneous Tasks

- Prepare release hygiene ([#1098](https://github.com/joshrotenberg/tower-mcp/pull/1098))

### Testing

- Harden untrusted-input boundaries with fuzzing ([#1096](https://github.com/joshrotenberg/tower-mcp/pull/1096))



## [0.16.0] - 2026-07-31

### Bug Fixes

- **tasks:** Make final task creation server-directed ([#1095](https://github.com/joshrotenberg/tower-mcp/pull/1095))

### Documentation

- **tasks:** Document the task authorization model ([#1090](https://github.com/joshrotenberg/tower-mcp/pull/1090))

### Features

- **tasks:** Dispatch the final task methods ([#1088](https://github.com/joshrotenberg/tower-mcp/pull/1088))
- **tasks:** Push notifications/tasks on state transitions ([#1092](https://github.com/joshrotenberg/tower-mcp/pull/1092))



## [0.15.0] - 2026-07-30

### Documentation

- Reconcile versions, example count, and conformance figures ([#1030](https://github.com/joshrotenberg/tower-mcp/pull/1030))
- Note the 2026-07-28 removal of elicitation-complete correlation ([#1042](https://github.com/joshrotenberg/tower-mcp/pull/1042))
- Correct smaller RC-to-final doc drift (#1037 part D) ([#1043](https://github.com/joshrotenberg/tower-mcp/pull/1043))

### Features

- **types:** 2026-07-28 model surface (resultType, MRTR, subscriptions, meta split) ([#1016](https://github.com/joshrotenberg/tower-mcp/pull/1016))
- **types:** Align DiscoverResult with the final 2026-07-28 schema ([#1040](https://github.com/joshrotenberg/tower-mcp/pull/1040))
- **protocol:** Add compile-time and runtime version policy ([#1046](https://github.com/joshrotenberg/tower-mcp/pull/1046))
- **server:** Implement final HTTP lifecycle ([#1048](https://github.com/joshrotenberg/tower-mcp/pull/1048))
- Implement final-protocol MRTR server support ([#1050](https://github.com/joshrotenberg/tower-mcp/pull/1050))
- **client:** Support final MCP lifecycle ([#1051](https://github.com/joshrotenberg/tower-mcp/pull/1051))
- **client:** Complete OAuth conformance ([#1052](https://github.com/joshrotenberg/tower-mcp/pull/1052))
- **client:** Honor final response cache hints ([#1053](https://github.com/joshrotenberg/tower-mcp/pull/1053))
- **client:** Support final protocol subscriptions ([#1056](https://github.com/joshrotenberg/tower-mcp/pull/1056))
- Validate MCP metadata and extension keys ([#1057](https://github.com/joshrotenberg/tower-mcp/pull/1057))
- **subscriptions:** Complete graceful server close ([#1064](https://github.com/joshrotenberg/tower-mcp/pull/1064))
- **mrtr:** Compose handler middleware ([#1065](https://github.com/joshrotenberg/tower-mcp/pull/1065))
- **client:** Add bounded OAuth scope escalation ([#1067](https://github.com/joshrotenberg/tower-mcp/pull/1067))
- **apps:** Add typed MCP Apps server support ([#1070](https://github.com/joshrotenberg/tower-mcp/pull/1070))
- **protocol:** Stabilize 2026 version naming ([#1071](https://github.com/joshrotenberg/tower-mcp/pull/1071))
- **tasks:** Add exact SEP-2663 final wire types ([#1074](https://github.com/joshrotenberg/tower-mcp/pull/1074))

### Miscellaneous Tasks

- Add a draft-protocol client conformance job ([#1034](https://github.com/joshrotenberg/tower-mcp/pull/1034))
- **conformance:** Re-baseline draft suite against harness 0.2.0-alpha.10 ([#1039](https://github.com/joshrotenberg/tower-mcp/pull/1039))

### Testing

- Property tests for wire and state-machine boundaries ([#1035](https://github.com/joshrotenberg/tower-mcp/pull/1035))



## [0.14.0] - 2026-07-24



## [0.13.1] - 2026-07-23

### Miscellaneous Tasks

- Update Cargo.toml dependencies



## [0.13.0] - 2026-07-23

### Bug Fixes

- **stateless:** Realign 2026-07-28 wire constants with the current draft schema ([#954](https://github.com/joshrotenberg/tower-mcp/pull/954))
- Draft-conformance quick wins (caching hints, sep-2164 uri, progress delivery) ([#960](https://github.com/joshrotenberg/tower-mcp/pull/960))
- **transport:** Size limits, stateless disconnect cancellation, response-id leniency ([#963](https://github.com/joshrotenberg/tower-mcp/pull/963))

### Features

- **client:** Task-augmented tool calls and a tasks API on McpClient ([#965](https://github.com/joshrotenberg/tower-mcp/pull/965))

### Miscellaneous Tasks

- **conformance:** Run the official 2026-07-28 draft conformance suite ([#958](https://github.com/joshrotenberg/tower-mcp/pull/958))



## [0.12.3] - 2026-07-23

### Bug Fixes

- **extract:** Re-export schemars and add version-skew diagnostics ([#937](https://github.com/joshrotenberg/tower-mcp/pull/937))



## [0.12.2] - 2026-07-06

### Documentation

- Bump README install version to 0.12 and redirect root CHANGELOG ([#930](https://github.com/joshrotenberg/tower-mcp/pull/930))



## [0.12.1] - 2026-06-12



## [0.12.0] - 2026-06-03



## [0.11.0] - 2026-06-02

### Bug Fixes

- **jsonrpc:** Serialize null id on error responses per spec ([#803](https://github.com/joshrotenberg/tower-mcp/pull/803))
- **types:** Emit null requestId on cancelled notification when unset ([#811](https://github.com/joshrotenberg/tower-mcp/pull/811))
- **types:** Reconcile McpErrorCode against SEP-2575 canonical assignments ([#838](https://github.com/joshrotenberg/tower-mcp/pull/838))
- **types:** Use InvalidParams (-32602) for resource-not-found per SEP-2164 ([#841](https://github.com/joshrotenberg/tower-mcp/pull/841))

### Documentation

- Position conformance numbers prominently per SEP-2484 ([#840](https://github.com/joshrotenberg/tower-mcp/pull/840))
- **examples:** Add axum embedding guide example ([#842](https://github.com/joshrotenberg/tower-mcp/pull/842))
- **tower-mcp-types:** Add crate-level doc, update description ([#823](https://github.com/joshrotenberg/tower-mcp/pull/823)) ([#851](https://github.com/joshrotenberg/tower-mcp/pull/851))
- Update lib.rs and README for 2026-07-28 stateless protocol (#858, #871) ([#886](https://github.com/joshrotenberg/tower-mcp/pull/886))
- Module-level docs for stateless transport, context, and types (#860, #864, #867, #874) ([#889](https://github.com/joshrotenberg/tower-mcp/pull/889))
- Update README version strings to 0.11 ([#897](https://github.com/joshrotenberg/tower-mcp/pull/897))

### Features

- **types,router:** Wire server/discover RPC end-to-end per SEP-2575 ([#829](https://github.com/joshrotenberg/tower-mcp/pull/829))
- **types:** TTL on list results (SEP-2549) + deprecation metadata (SEP-2577/2596) ([#826](https://github.com/joshrotenberg/tower-mcp/pull/826))
- **http:** Return spec-shape UnsupportedProtocolVersion per SEP-2575 ([#839](https://github.com/joshrotenberg/tower-mcp/pull/839))
- **tasks:** Repackage as io.modelcontextprotocol/tasks extension per SEP-2663 ([#846](https://github.com/joshrotenberg/tower-mcp/pull/846))
- **http:** SEP-2243 HTTP standardization headers ([#845](https://github.com/joshrotenberg/tower-mcp/pull/845))
- **http:** Add messages/listen SSE streaming endpoint (#814 chunk 4) ([#852](https://github.com/joshrotenberg/tower-mcp/pull/852))

### Testing

- **types:** Add wire-format assertion helpers for JSON-RPC ([#808](https://github.com/joshrotenberg/tower-mcp/pull/808))



## [0.10.1] - 2026-05-15

### Features

- **context:** Typed helpers for server-originated tasks/* (SEP-1686) ([#793](https://github.com/joshrotenberg/tower-mcp/pull/793))

### Miscellaneous Tasks

- Verify tower-mcp-types compiles for wasm32-unknown-unknown ([#784](https://github.com/joshrotenberg/tower-mcp/pull/784))



## [0.10.0] - 2026-03-26

### Documentation

- Fix remaining 0.8 version reference in README ([#759](https://github.com/joshrotenberg/tower-mcp/pull/759))

### Features

- HTTP client session expiry detection and automatic recovery ([#764](https://github.com/joshrotenberg/tower-mcp/pull/764))



## [0.9.2] - 2026-03-25

### Documentation

- Update all top-level docs for v0.9 and example reorganization ([#756](https://github.com/joshrotenberg/tower-mcp/pull/756))

### Features

- WebSocket spec compliance, stateless mode, and resource/prompt context ([#751](https://github.com/joshrotenberg/tower-mcp/pull/751))



## [0.9.1] - 2026-03-19



## [0.9.0] - 2026-03-18



## [0.8.8] - 2026-03-17

### Features

- Derive Serialize/Deserialize on RouterResponse and inner types ([#735](https://github.com/joshrotenberg/tower-mcp/pull/735))



## [0.8.7] - 2026-03-16



## [0.8.6] - 2026-03-16

### Documentation

- Update README, lib.rs, and AGENTS.md for accuracy and completeness ([#726](https://github.com/joshrotenberg/tower-mcp/pull/726))



## [0.8.5] - 2026-03-16



## [0.8.4] - 2026-03-09



## [0.8.3] - 2026-03-07



## [0.8.2] - 2026-03-07

### Documentation

- Pre-release documentation fixes ([#621](https://github.com/joshrotenberg/tower-mcp/pull/621))

### Features

- MCP proxy for multi-server aggregation ([#600](https://github.com/joshrotenberg/tower-mcp/pull/600))
- Optional proc macros for tools, prompts, and resources ([#613](https://github.com/joshrotenberg/tower-mcp/pull/613))
- Dynamic resource and resource template registries ([#616](https://github.com/joshrotenberg/tower-mcp/pull/616))



## [0.8.1] - 2026-03-06



## [0.8.0] - 2026-03-06

### Documentation

- Add doc examples for all 15 documentation coverage gaps ([#541](https://github.com/joshrotenberg/tower-mcp/pull/541)) ([#556](https://github.com/joshrotenberg/tower-mcp/pull/556))

### Refactor

- Move to standard workspace layout with crates/ directory ([#552](https://github.com/joshrotenberg/tower-mcp/pull/552))


