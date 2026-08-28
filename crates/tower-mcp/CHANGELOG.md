# Changelog

All notable changes to this project will be documented in this file.

## [0.22.3] - 2026-08-28

### Testing

- **tasks:** Pin declined and cancelled elicitation responses (closes #1426) ([#1434](https://github.com/joshrotenberg/tower-mcp/pull/1434))
- **stdio:** Isolate the frame-tracing tests into their own binary (closes #1435) ([#1436](https://github.com/joshrotenberg/tower-mcp/pull/1436))



## [0.22.2] - 2026-08-23

### Bug Fixes

- **http:** Authorize final task subscriptions ([#1406](https://github.com/joshrotenberg/tower-mcp/pull/1406))
- **http:** Bound final subscription streams ([#1408](https://github.com/joshrotenberg/tower-mcp/pull/1408))
- **router:** Authorize resource template access ([#1410](https://github.com/joshrotenberg/tower-mcp/pull/1410))
- **http:** Isolate legacy resource subscriptions ([#1411](https://github.com/joshrotenberg/tower-mcp/pull/1411))
- **oauth:** Bind validate adapter audience ([#1413](https://github.com/joshrotenberg/tower-mcp/pull/1413))
- **tracing:** Redact protocol frame payloads ([#1414](https://github.com/joshrotenberg/tower-mcp/pull/1414))
- **router:** Authorize completion references ([#1415](https://github.com/joshrotenberg/tower-mcp/pull/1415))
- **http:** Refresh live session expiry ([#1416](https://github.com/joshrotenberg/tower-mcp/pull/1416))

### Documentation

- **http:** Fix final curl examples ([#1417](https://github.com/joshrotenberg/tower-mcp/pull/1417))
- Add crates.io downloads badge ([#1424](https://github.com/joshrotenberg/tower-mcp/pull/1424))

### Features

- **tasks:** Construct task resume contexts ([#1412](https://github.com/joshrotenberg/tower-mcp/pull/1412))
- **stdio:** Layer bidirectional transport ([#1418](https://github.com/joshrotenberg/tower-mcp/pull/1418))
- **tasks:** Add task owner resolver ([#1419](https://github.com/joshrotenberg/tower-mcp/pull/1419))
- **tasks:** Add live execution lifecycle handle ([#1420](https://github.com/joshrotenberg/tower-mcp/pull/1420))
- **tasks:** Add configurable TTL and expiry cleanup ([#1421](https://github.com/joshrotenberg/tower-mcp/pull/1421))
- **tasks:** Bound in-memory task retention ([#1422](https://github.com/joshrotenberg/tower-mcp/pull/1422))

### Testing

- Eliminate task presence timing race ([#1423](https://github.com/joshrotenberg/tower-mcp/pull/1423))



## [0.22.1] - 2026-08-21

### Bug Fixes

- **transport:** Keep the request id on inspection error responses (closes #1372) ([#1373](https://github.com/joshrotenberg/tower-mcp/pull/1373))
- **client:** React to a 401 challenge so an expired token re-authenticates (closes #1370) ([#1374](https://github.com/joshrotenberg/tower-mcp/pull/1374))
- **tasks:** Restore TaskContext during task replay ([#1387](https://github.com/joshrotenberg/tower-mcp/pull/1387))

### Documentation

- Enforce missing_docs on the published crates (closes #1365) ([#1366](https://github.com/joshrotenberg/tower-mcp/pull/1366))
- Mark the features deprecated in 2026-07-28 (closes #1371) ([#1375](https://github.com/joshrotenberg/tower-mcp/pull/1375))

### Features

- **client:** Configure final per-request log level ([#1405](https://github.com/joshrotenberg/tower-mcp/pull/1405))

### Refactor

- **router:** Split impl McpRouter and the router-adjacent free items ([#1256](https://github.com/joshrotenberg/tower-mcp/pull/1256)) ([#1362](https://github.com/joshrotenberg/tower-mcp/pull/1362))
- **tool:** Split the service and task types out of tool.rs ([#1256](https://github.com/joshrotenberg/tower-mcp/pull/1256)) ([#1363](https://github.com/joshrotenberg/tower-mcp/pull/1363))

### Testing

- **http:** Cover the stateless SSE notification fallback ([#1367](https://github.com/joshrotenberg/tower-mcp/pull/1367)) ([#1369](https://github.com/joshrotenberg/tower-mcp/pull/1369))



## [0.22.0] - 2026-08-12

### Bug Fixes

- **tasks:** Only resume a task when an update actually answered something ([#1247](https://github.com/joshrotenberg/tower-mcp/pull/1247))
- **stdio:** Stop a saturated request limit from starving control traffic ([#1254](https://github.com/joshrotenberg/tower-mcp/pull/1254))
- **tasks:** Enforce input request key uniqueness over a task's lifetime ([#1248](https://github.com/joshrotenberg/tower-mcp/pull/1248))
- **stdio:** Keep inbound notification routing when layering or going silent ([#1257](https://github.com/joshrotenberg/tower-mcp/pull/1257))
- **extract:** Mention bridge_extension in the missing-extension message ([#1277](https://github.com/joshrotenberg/tower-mcp/pull/1277))
- **tool:** Re-export McpTool, McpResource, and McpPrompt from the crate root ([#1278](https://github.com/joshrotenberg/tower-mcp/pull/1278))
- **resource:** Stop percent decoding from panicking on multibyte input ([#1279](https://github.com/joshrotenberg/tower-mcp/pull/1279))
- **server:** Make resources.subscribe reflect configured and version support ([#1283](https://github.com/joshrotenberg/tower-mcp/pull/1283))
- **stdio:** Never answer a notification, and stop leaking a type name ([#1284](https://github.com/joshrotenberg/tower-mcp/pull/1284))
- **auth:** Match the authorization scheme case-insensitively ([#1289](https://github.com/joshrotenberg/tower-mcp/pull/1289))
- **stdio:** Keep the transport alive when stdin carries invalid UTF-8 ([#1292](https://github.com/joshrotenberg/tower-mcp/pull/1292))
- **tool:** Preserve live handlers through clone and prefix ([#1298](https://github.com/joshrotenberg/tower-mcp/pull/1298))
- **test:** Stop the version drift scan reading gitignored paths ([#1299](https://github.com/joshrotenberg/tower-mcp/pull/1299))
- **client:** Keep the stdio client alive when a server writes invalid UTF-8 ([#1300](https://github.com/joshrotenberg/tower-mcp/pull/1300))
- **tasks:** Make TaskStore cancellation tokens constructible ([#1302](https://github.com/joshrotenberg/tower-mcp/pull/1302))
- **tasks:** Close the live-task cancellation registration and outcome races ([#1304](https://github.com/joshrotenberg/tower-mcp/pull/1304))
- **tasks:** Cover live handlers with catch_panics and clean up unconditionally ([#1307](https://github.com/joshrotenberg/tower-mcp/pull/1307))
- **session:** Require an initialize request before initialized opens a session ([#1311](https://github.com/joshrotenberg/tower-mcp/pull/1311))
- **client:** Strip a UTF-8 BOM on the client receive paths ([#1314](https://github.com/joshrotenberg/tower-mcp/pull/1314))
- **resource,prompt:** Close two of the three public surface gaps ([#1315](https://github.com/joshrotenberg/tower-mcp/pull/1315))
- **router:** Key the in-flight registry by dispatch, not request id ([#1312](https://github.com/joshrotenberg/tower-mcp/pull/1312))
- **prompt:** Reject a prompts/get missing a required argument ([#1323](https://github.com/joshrotenberg/tower-mcp/pull/1323))
- Error semantics change depending on how a resource or prompt is registered (closes #1280) ([#1327](https://github.com/joshrotenberg/tower-mcp/pull/1327))
- **oauth:** Match Bearer scheme case-insensitively in resource-server middleware (closes #1337) ([#1342](https://github.com/joshrotenberg/tower-mcp/pull/1342))
- **http:** Loopback validation compares scheme and host by exact bytes (closes #1341) ([#1344](https://github.com/joshrotenberg/tower-mcp/pull/1344))
- **childproc:** Correlate responses by id, frame over bytes, strip BOM (closes #1334) ([#1343](https://github.com/joshrotenberg/tower-mcp/pull/1343))
- **websocket:** Apply the full frame classification to the bidirectional path (closes #1335) ([#1351](https://github.com/joshrotenberg/tower-mcp/pull/1351))
- **http:** Strip a BOM on the server body path and preserve the request id (closes #1336) ([#1352](https://github.com/joshrotenberg/tower-mcp/pull/1352))
- **tool:** Return an error for direct live-only calls ([#1332](https://github.com/joshrotenberg/tower-mcp/pull/1332))
- **http:** Reject a bracketed IPv6 host with a trailing suffix (closes #1350) ([#1355](https://github.com/joshrotenberg/tower-mcp/pull/1355))
- **tasks:** Harden task error boundaries ([#1348](https://github.com/joshrotenberg/tower-mcp/pull/1348))
- **transport:** Route transport error responses through the disclosure policy (closes #1354) ([#1360](https://github.com/joshrotenberg/tower-mcp/pull/1360))
- **oauth:** Match cache-control directives case-insensitively (closes #1358) ([#1361](https://github.com/joshrotenberg/tower-mcp/pull/1361))

### Documentation

- Trim the README to the essentials ([#1268](https://github.com/joshrotenberg/tower-mcp/pull/1268))
- Worked examples for resources and prompts ([#1273](https://github.com/joshrotenberg/tower-mcp/pull/1273))
- Worked examples for tasks, auth, and the client ([#1274](https://github.com/joshrotenberg/tower-mcp/pull/1274))

### Features

- **resource:** Support RFC 6570 query expansion in template matching ([#1259](https://github.com/joshrotenberg/tower-mcp/pull/1259))
- **stdio:** Add an observable, bounded shutdown phase ([#1260](https://github.com/joshrotenberg/tower-mcp/pull/1260))
- **tasks:** Live task execution that keeps the handler future alive ([#1288](https://github.com/joshrotenberg/tower-mcp/pull/1288))
- **transport:** Add serve_with_shutdown to the transports that own a listener ([#1291](https://github.com/joshrotenberg/tower-mcp/pull/1291))
- **tool:** Expose RequestContext to live task handlers ([#1308](https://github.com/joshrotenberg/tower-mcp/pull/1308))
- **tasks:** Owner-aware task presence distinguishing expired from missing ([#1310](https://github.com/joshrotenberg/tower-mcp/pull/1310))
- **unix:** Serve on a caller-owned listener ([#1313](https://github.com/joshrotenberg/tower-mcp/pull/1313))
- **router:** Add configurable panic disclosure policy ([#1309](https://github.com/joshrotenberg/tower-mcp/pull/1309))
- **tasks:** Split require_input into park_input and PendingInput::wait ([#1322](https://github.com/joshrotenberg/tower-mcp/pull/1322))
- **tasks:** Let one tool serve a live task plus a synchronous/MRTR fallback (closes #1246) ([#1328](https://github.com/joshrotenberg/tower-mcp/pull/1328))
- **router:** Per-capability advertisement builders (closes #1338) ([#1356](https://github.com/joshrotenberg/tower-mcp/pull/1356))

### Miscellaneous Tasks

- Run doctests, and gate the guides that need features ([#1287](https://github.com/joshrotenberg/tower-mcp/pull/1287))
- Catch version-reference drift across the workspace ([#1290](https://github.com/joshrotenberg/tower-mcp/pull/1290))
- Teach the version-drift scanner to fix what it finds (closes #1339) ([#1357](https://github.com/joshrotenberg/tower-mcp/pull/1357))

### Refactor

- **router:** Move the inline test module into its own file ([#1263](https://github.com/joshrotenberg/tower-mcp/pull/1263))
- Move the remaining inline test modules out of their sources ([#1318](https://github.com/joshrotenberg/tower-mcp/pull/1318))
- **router:** Extract the task operations into a child module ([#1319](https://github.com/joshrotenberg/tower-mcp/pull/1319))
- **router:** Extract logging, subscriptions, and notifications ([#1320](https://github.com/joshrotenberg/tower-mcp/pull/1320))
- **router:** Move the task helpers beside the methods that use them ([#1321](https://github.com/joshrotenberg/tower-mcp/pull/1321))
- **auth:** Remove AuthConfig ([#1324](https://github.com/joshrotenberg/tower-mcp/pull/1324))
- **resource,prompt:** Make the handler traits crate-private ([#1325](https://github.com/joshrotenberg/tower-mcp/pull/1325))
- **http:** Split transport/http.rs (phase 3 of #1256) ([#1326](https://github.com/joshrotenberg/tower-mcp/pull/1326))
- **framing:** Extract the JSON-RPC inbound frame classification predicate ([#1346](https://github.com/joshrotenberg/tower-mcp/pull/1346))
- **resource:** Move the inline test modules to siblings ([#1256](https://github.com/joshrotenberg/tower-mcp/pull/1256)) ([#1359](https://github.com/joshrotenberg/tower-mcp/pull/1359))

### Testing

- Assert core stdio behaviours across every configuration ([#1262](https://github.com/joshrotenberg/tower-mcp/pull/1262))
- Adversarial input pass for request handling ([#1266](https://github.com/joshrotenberg/tower-mcp/pull/1266))
- Keep examples on the public API surface ([#1267](https://github.com/joshrotenberg/tower-mcp/pull/1267))
- **examples:** Guard the public-API surface, and show a layer feeding a handler ([#1316](https://github.com/joshrotenberg/tower-mcp/pull/1316))
- **transport:** Assert notification routing across the stdio matrix ([#1317](https://github.com/joshrotenberg/tower-mcp/pull/1317))
- Wait on the task transition instead of a fixed sleep in subscriptions_listen ([#1333](https://github.com/joshrotenberg/tower-mcp/pull/1333))
- **tasks:** Cross Tool/Resource/Prompt construction methods against every handler kind (closes #1340) ([#1345](https://github.com/joshrotenberg/tower-mcp/pull/1345))



## [0.21.1] - 2026-08-08

### Features

- **http,ws:** Bridge server-supplied request extensions into RequestContext ([#1243](https://github.com/joshrotenberg/tower-mcp/pull/1243))



## [0.21.0] - 2026-08-07

### Bug Fixes

- Re-export notification_channel from the crate root ([#1241](https://github.com/joshrotenberg/tower-mcp/pull/1241))

### Documentation

- Correct three claims that contradict the code ([#1235](https://github.com/joshrotenberg/tower-mcp/pull/1235))

### Features

- **router:** Add opt-in panic containment for tool handlers ([#1236](https://github.com/joshrotenberg/tower-mcp/pull/1236))
- **stdio:** Handle requests concurrently ([#1238](https://github.com/joshrotenberg/tower-mcp/pull/1238))

  Requests on a stdio connection now run on their own tasks, so a slow tool
  no longer blocks the rest of the connection. Responses consequently arrive
  in completion order rather than request order. JSON-RPC pairs a response to
  its request by id, so this is within spec, but code that assumed responses
  came back positionally needs to match by id. `StdioTransport::max_concurrent_requests(1)`
  restores the previous strictly serial handling.
- **router:** Add try_merge and conflicts for router composition ([#1239](https://github.com/joshrotenberg/tower-mcp/pull/1239))



## [0.20.1] - 2026-08-05

### Features

- **client:** Add a WebSocket client transport ([#1222](https://github.com/joshrotenberg/tower-mcp/pull/1222))



## [0.20.0] - 2026-08-05

### Bug Fixes

- **tasks:** Fail honestly when a task-capable tool asks for input ([#1209](https://github.com/joshrotenberg/tower-mcp/pull/1209))

### Documentation

- **context:** Explain why the final lifecycle has no client requester ([#1206](https://github.com/joshrotenberg/tower-mcp/pull/1206))

### Features

- **client:** Cancel an in-flight request when its caller drops ([#1210](https://github.com/joshrotenberg/tower-mcp/pull/1210))
- **tasks:** Resume a task whose handler asked for input ([#1211](https://github.com/joshrotenberg/tower-mcp/pull/1211))



## [0.19.0] - 2026-08-05

### Bug Fixes

- **tasks:** Apply tasks/update inputResponses on the stable lifecycle ([#1192](https://github.com/joshrotenberg/tower-mcp/pull/1192))

### Features

- **channel:** Support server-initiated requests in-process ([#1194](https://github.com/joshrotenberg/tower-mcp/pull/1194))
- **client:** Let a client request progress notifications ([#1195](https://github.com/joshrotenberg/tower-mcp/pull/1195))



## [0.18.2] - 2026-08-04

### Bug Fixes

- **proxy:** Stack repeated backend_layer calls instead of replacing ([#1176](https://github.com/joshrotenberg/tower-mcp/pull/1176))
- **client:** Fail initialize when notifications/initialized is not delivered ([#1177](https://github.com/joshrotenberg/tower-mcp/pull/1177))

### Features

- **protocol:** Enable 2026-07-28 by default on feature-compiled clients ([#1183](https://github.com/joshrotenberg/tower-mcp/pull/1183))
- **channel:** Apply Tower layers to the in-process transport ([#1184](https://github.com/joshrotenberg/tower-mcp/pull/1184))
- **transport:** Route subscriptions/listen through the middleware boundary ([#1185](https://github.com/joshrotenberg/tower-mcp/pull/1185))
- **transport:** Observe the terminal half of subscriptions/listen streams ([#1186](https://github.com/joshrotenberg/tower-mcp/pull/1186))



## [0.18.1] - 2026-08-04

### Bug Fixes

- **client:** Separate HTTP-only request path ([#1171](https://github.com/joshrotenberg/tower-mcp/pull/1171))

### Features

- **mcp-repl:** Import standard MCP server configs ([#1164](https://github.com/joshrotenberg/tower-mcp/pull/1164))
- **mcp-repl:** Add secure OAuth profiles ([#1166](https://github.com/joshrotenberg/tower-mcp/pull/1166))



## [0.18.0] - 2026-08-03

### Features

- **prompts:** Initialize dynamic catalogs lazily ([#1146](https://github.com/joshrotenberg/tower-mcp/pull/1146))
- **channel:** Support final subscriptions ([#1145](https://github.com/joshrotenberg/tower-mcp/pull/1145))
- **tasks:** Prepare task state before execution ([#1144](https://github.com/joshrotenberg/tower-mcp/pull/1144))



## [0.17.2] - 2026-08-02

### Bug Fixes

- **mcp-repl:** Redraw around child stderr ([#1139](https://github.com/joshrotenberg/tower-mcp/pull/1139))

### Features

- **mcp-repl:** Surface task status transitions ([#1140](https://github.com/joshrotenberg/tower-mcp/pull/1140))



## [0.17.1] - 2026-08-01

### Features

- **types:** Add JSON-RPC structural inspection ([#1130](https://github.com/joshrotenberg/tower-mcp/pull/1130))
- **types:** Add exact-revision MCP inspection ([#1132](https://github.com/joshrotenberg/tower-mcp/pull/1132))
- **runtime:** Enforce exact MCP inspection profiles ([#1133](https://github.com/joshrotenberg/tower-mcp/pull/1133))



## [0.17.0] - 2026-08-01

### Bug Fixes

- **oauth:** Conform reusable client flows to final spec ([#1121](https://github.com/joshrotenberg/tower-mcp/pull/1121))
- **oauth:** Harden resource server setup ([#1122](https://github.com/joshrotenberg/tower-mcp/pull/1122))

### Documentation

- **oauth:** Add end-to-end authorization guide ([#1125](https://github.com/joshrotenberg/tower-mcp/pull/1125))
- Complete public API rustdoc ([#1126](https://github.com/joshrotenberg/tower-mcp/pull/1126))
- Add task-oriented usage guides ([#1127](https://github.com/joshrotenberg/tower-mcp/pull/1127))
- Publish guides in rustdoc ([#1128](https://github.com/joshrotenberg/tower-mcp/pull/1128))

### Features

- **oauth:** Add cohesive authorization flow ([#1124](https://github.com/joshrotenberg/tower-mcp/pull/1124))

### Miscellaneous Tasks

- **conformance:** Align stable suites on current harness ([#1105](https://github.com/joshrotenberg/tower-mcp/pull/1105))

### Testing

- **interop:** Cover final protocol against rmcp ([#1109](https://github.com/joshrotenberg/tower-mcp/pull/1109))



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

- **tasks:** Durable task state and input flow ([#1076](https://github.com/joshrotenberg/tower-mcp/pull/1076))
- **tasks:** Negotiate the final tasks extension ([#1078](https://github.com/joshrotenberg/tower-mcp/pull/1078))
- **tasks:** Dispatch the final task methods ([#1088](https://github.com/joshrotenberg/tower-mcp/pull/1088))
- **tasks:** Bind task operations to the authenticated principal ([#1089](https://github.com/joshrotenberg/tower-mcp/pull/1089))
- **client:** Add typed tasks/update API ([#1091](https://github.com/joshrotenberg/tower-mcp/pull/1091))
- **tasks:** Push notifications/tasks on state transitions ([#1092](https://github.com/joshrotenberg/tower-mcp/pull/1092))

### Testing

- Add coverage for the unix socket transport ([#1073](https://github.com/joshrotenberg/tower-mcp/pull/1073))
- Check each public feature combination ([#1094](https://github.com/joshrotenberg/tower-mcp/pull/1094))

### Build

- **deps:** Bump jsonwebtoken from 10.3.0 to 11.0.0 ([#1086](https://github.com/joshrotenberg/tower-mcp/pull/1086))



## [0.15.0] - 2026-07-30

### Bug Fixes

- **protocol:** Complete 2026 result envelopes ([#1047](https://github.com/joshrotenberg/tower-mcp/pull/1047))
- **http:** Enforce schema-defined MCP headers ([#1049](https://github.com/joshrotenberg/tower-mcp/pull/1049))
- **client:** Refresh stale tool schemas once ([#1054](https://github.com/joshrotenberg/tower-mcp/pull/1054))
- **tasks:** Withhold incomplete final advertisement ([#1058](https://github.com/joshrotenberg/tower-mcp/pull/1058))
- **http:** Associate client requests with originating POST ([#1063](https://github.com/joshrotenberg/tower-mcp/pull/1063))

### Documentation

- Reconcile versions, example count, and conformance figures ([#1030](https://github.com/joshrotenberg/tower-mcp/pull/1030))
- Note the 2026-07-28 removal of elicitation-complete correlation ([#1042](https://github.com/joshrotenberg/tower-mcp/pull/1042))
- Correct smaller RC-to-final doc drift (#1037 part D) ([#1043](https://github.com/joshrotenberg/tower-mcp/pull/1043))

### Features

- **types:** 2026-07-28 model surface (resultType, MRTR, subscriptions, meta split) ([#1016](https://github.com/joshrotenberg/tower-mcp/pull/1016))
- **mcp-repl:** Subscribe to resource updates ([#1025](https://github.com/joshrotenberg/tower-mcp/pull/1025))
- **types:** Align DiscoverResult with the final 2026-07-28 schema ([#1040](https://github.com/joshrotenberg/tower-mcp/pull/1040))
- **http:** Stamp server identity into _meta on 2026-07-28 responses ([#1041](https://github.com/joshrotenberg/tower-mcp/pull/1041))
- **protocol:** Add compile-time and runtime version policy ([#1046](https://github.com/joshrotenberg/tower-mcp/pull/1046))
- **server:** Implement final HTTP lifecycle ([#1048](https://github.com/joshrotenberg/tower-mcp/pull/1048))
- Implement final-protocol MRTR server support ([#1050](https://github.com/joshrotenberg/tower-mcp/pull/1050))
- **client:** Support final MCP lifecycle ([#1051](https://github.com/joshrotenberg/tower-mcp/pull/1051))
- **client:** Complete OAuth conformance ([#1052](https://github.com/joshrotenberg/tower-mcp/pull/1052))
- **client:** Honor final response cache hints ([#1053](https://github.com/joshrotenberg/tower-mcp/pull/1053))
- **client:** Support final protocol subscriptions ([#1056](https://github.com/joshrotenberg/tower-mcp/pull/1056))
- Validate MCP metadata and extension keys ([#1057](https://github.com/joshrotenberg/tower-mcp/pull/1057))
- **transports:** Support final protocol across JSON-RPC bindings ([#1059](https://github.com/joshrotenberg/tower-mcp/pull/1059))
- **stdio:** Support final subscriptions ([#1062](https://github.com/joshrotenberg/tower-mcp/pull/1062))
- **subscriptions:** Complete graceful server close ([#1064](https://github.com/joshrotenberg/tower-mcp/pull/1064))
- **mrtr:** Compose handler middleware ([#1065](https://github.com/joshrotenberg/tower-mcp/pull/1065))
- **client:** Add reusable OAuth registration policy ([#1066](https://github.com/joshrotenberg/tower-mcp/pull/1066))
- **client:** Add bounded OAuth scope escalation ([#1067](https://github.com/joshrotenberg/tower-mcp/pull/1067))
- **client:** Persist OAuth registrations by issuer ([#1068](https://github.com/joshrotenberg/tower-mcp/pull/1068))
- **extensions:** Add runtime negotiation ([#1069](https://github.com/joshrotenberg/tower-mcp/pull/1069))
- **apps:** Add typed MCP Apps server support ([#1070](https://github.com/joshrotenberg/tower-mcp/pull/1070))
- **protocol:** Stabilize 2026 version naming ([#1071](https://github.com/joshrotenberg/tower-mcp/pull/1071))
- **tasks:** Add exact SEP-2663 final wire types ([#1074](https://github.com/joshrotenberg/tower-mcp/pull/1074))

### Miscellaneous Tasks

- Add a draft-protocol client conformance job ([#1034](https://github.com/joshrotenberg/tower-mcp/pull/1034))
- **conformance:** Re-baseline draft suite against harness 0.2.0-alpha.10 ([#1039](https://github.com/joshrotenberg/tower-mcp/pull/1039))

### Testing

- Property tests for wire and state-machine boundaries ([#1035](https://github.com/joshrotenberg/tower-mcp/pull/1035))



## [0.14.0] - 2026-07-24

### Bug Fixes

- **client:** Wake the caller when a background HTTP POST fails ([#991](https://github.com/joshrotenberg/tower-mcp/pull/991))
- **client:** Bound the inline notification send so a stall cannot freeze the client ([#999](https://github.com/joshrotenberg/tower-mcp/pull/999))



## [0.13.1] - 2026-07-23

### Bug Fixes

- **client:** Await notification POSTs inline to preserve ordering ([#969](https://github.com/joshrotenberg/tower-mcp/pull/969))
- **client:** Report pre-session HTTP 404 as endpoint-not-found, not Session expired ([#984](https://github.com/joshrotenberg/tower-mcp/pull/984))

### Features

- **examples:** Mcp-repl, an interactive MCP client REPL ([#966](https://github.com/joshrotenberg/tower-mcp/pull/966))

### Testing

- **childproc:** Poll for exit instead of a fixed sleep ([#985](https://github.com/joshrotenberg/tower-mcp/pull/985))



## [0.13.0] - 2026-07-23

### Bug Fixes

- **stateless:** Realign 2026-07-28 wire constants with the current draft schema ([#954](https://github.com/joshrotenberg/tower-mcp/pull/954))
- Draft-conformance quick wins (caching hints, sep-2164 uri, progress delivery) ([#960](https://github.com/joshrotenberg/tower-mcp/pull/960))
- **transport:** Size limits, stateless disconnect cancellation, response-id leniency ([#963](https://github.com/joshrotenberg/tower-mcp/pull/963))

### Features

- **transport:** ChannelTransport notification delivery and concurrent requests ([#964](https://github.com/joshrotenberg/tower-mcp/pull/964))
- **client:** Task-augmented tool calls and a tasks API on McpClient ([#965](https://github.com/joshrotenberg/tower-mcp/pull/965))

### Miscellaneous Tasks

- **conformance:** Run the official 2026-07-28 draft conformance suite ([#958](https://github.com/joshrotenberg/tower-mcp/pull/958))

### Refactor

- **tasks:** Promote TaskStore to a pluggable trait ([#962](https://github.com/joshrotenberg/tower-mcp/pull/962))



## [0.12.3] - 2026-07-23

### Bug Fixes

- **extract:** Re-export schemars and add version-skew diagnostics ([#937](https://github.com/joshrotenberg/tower-mcp/pull/937))



## [0.12.2] - 2026-07-06

### Documentation

- Bump README install version to 0.12 and redirect root CHANGELOG ([#930](https://github.com/joshrotenberg/tower-mcp/pull/930))



## [0.12.1] - 2026-06-12

### Miscellaneous Tasks

- Update Cargo.toml dependencies



## [0.12.0] - 2026-06-03

### Features

- **http:** Add opt-in SSE response wrapping for rmcp compatibility (closes #900) ([#902](https://github.com/joshrotenberg/tower-mcp/pull/902))
- **http:** Enforce notifications/initialized before tool dispatch per MCP spec ([#901](https://github.com/joshrotenberg/tower-mcp/pull/901)) ([#903](https://github.com/joshrotenberg/tower-mcp/pull/903))



## [0.11.0] - 2026-06-02

### Bug Fixes

- **types:** Reconcile McpErrorCode against SEP-2575 canonical assignments ([#838](https://github.com/joshrotenberg/tower-mcp/pull/838))
- **types:** Use InvalidParams (-32602) for resource-not-found per SEP-2164 ([#841](https://github.com/joshrotenberg/tower-mcp/pull/841))

### Documentation

- Position conformance numbers prominently per SEP-2484 ([#840](https://github.com/joshrotenberg/tower-mcp/pull/840))
- **examples:** Add axum embedding guide example ([#842](https://github.com/joshrotenberg/tower-mcp/pull/842))
- Update lib.rs and README for 2026-07-28 stateless protocol (#858, #871) ([#886](https://github.com/joshrotenberg/tower-mcp/pull/886))
- Module-level docs for stateless transport, context, and types (#860, #864, #867, #874) ([#889](https://github.com/joshrotenberg/tower-mcp/pull/889))
- Update README version strings to 0.11 ([#897](https://github.com/joshrotenberg/tower-mcp/pull/897))

### Features

- **types,router:** Wire server/discover RPC end-to-end per SEP-2575 ([#829](https://github.com/joshrotenberg/tower-mcp/pull/829))
- **types:** TTL on list results (SEP-2549) + deprecation metadata (SEP-2577/2596) ([#826](https://github.com/joshrotenberg/tower-mcp/pull/826))
- **oauth-client:** Validate iss parameter on authorization callback per SEP-2468 ([#835](https://github.com/joshrotenberg/tower-mcp/pull/835))
- **tool:** Add input_schema(Value) setter on ToolBuilder ([#837](https://github.com/joshrotenberg/tower-mcp/pull/837))
- **http:** Return spec-shape UnsupportedProtocolVersion per SEP-2575 ([#839](https://github.com/joshrotenberg/tower-mcp/pull/839))
- **session-store:** Populate client_info and client_capabilities on initialize ([#843](https://github.com/joshrotenberg/tower-mcp/pull/843))
- **stateless:** Migrate StatelessRequestMeta to FINAL SEP-2575 keys ([#844](https://github.com/joshrotenberg/tower-mcp/pull/844))
- **http,context:** Thread SEP-2575 per-request _meta through RequestContext ([#847](https://github.com/joshrotenberg/tower-mcp/pull/847))
- **tasks:** Repackage as io.modelcontextprotocol/tasks extension per SEP-2663 ([#846](https://github.com/joshrotenberg/tower-mcp/pull/846))
- **http:** SEP-2243 HTTP standardization headers ([#845](https://github.com/joshrotenberg/tower-mcp/pull/845))
- **http:** Add messages/listen SSE streaming endpoint (#814 chunk 4) ([#852](https://github.com/joshrotenberg/tower-mcp/pull/852))
- **http:** Version-gated stateless mode for 2026-07-28 protocol (#814 chunk 5) ([#853](https://github.com/joshrotenberg/tower-mcp/pull/853))
- **examples:** Add server/discover walkthrough example ([#884](https://github.com/joshrotenberg/tower-mcp/pull/884)) ([#896](https://github.com/joshrotenberg/tower-mcp/pull/896))
- **examples:** Add stateless HTTP client example for 2026-07-28 protocol ([#881](https://github.com/joshrotenberg/tower-mcp/pull/881)) ([#894](https://github.com/joshrotenberg/tower-mcp/pull/894))

### Testing

- **types:** Add wire-format assertion helpers for JSON-RPC ([#808](https://github.com/joshrotenberg/tower-mcp/pull/808))
- Lock down JSON-RPC parse-error wire format for stdio and http ([#812](https://github.com/joshrotenberg/tower-mcp/pull/812))
- Audit full JSON Schema 2020-12 support per SEP-2106 ([#833](https://github.com/joshrotenberg/tower-mcp/pull/833))
- **stdio:** Expose run_with_streams to enable end-to-end loop tests ([#836](https://github.com/joshrotenberg/tower-mcp/pull/836))
- **http:** Add stateless transport coverage for tools/list, notifications, and missing Mcp-Method ([#890](https://github.com/joshrotenberg/tower-mcp/pull/890))
- **http:** Add messages/listen session-negotiated and id-echo tests (#861, #863) ([#892](https://github.com/joshrotenberg/tower-mcp/pull/892))
- Add server/discover versions, per-request meta, router dispatch, and UnsupportedProtocolVersion tests (#866, #869, #872, #875) ([#893](https://github.com/joshrotenberg/tower-mcp/pull/893))
- **conformance:** Add TTL, deprecation metadata, and tasks extension coverage (closes #873) ([#895](https://github.com/joshrotenberg/tower-mcp/pull/895))



## [0.10.1] - 2026-05-15

### Bug Fixes

- **client:** Use match guard for OAuth error_description fallback ([#794](https://github.com/joshrotenberg/tower-mcp/pull/794))
- **stdio:** Bidi transport closes on parse error; strip UTF-8 BOM ([#797](https://github.com/joshrotenberg/tower-mcp/pull/797))

### Documentation

- Production deployment guide ([#780](https://github.com/joshrotenberg/tower-mcp/pull/780))

### Features

- Unix Domain Socket transport ([#773](https://github.com/joshrotenberg/tower-mcp/pull/773))
- Pluggable SessionStore for horizontal scaling ([#778](https://github.com/joshrotenberg/tower-mcp/pull/778))
- **event-store:** Pluggable SSE event store for stream resumption ([#779](https://github.com/joshrotenberg/tower-mcp/pull/779))
- **session:** Restore unknown sessions from SessionStore + auto-reinitialize ([#782](https://github.com/joshrotenberg/tower-mcp/pull/782))
- **context:** Typed helpers for server-originated tasks/* (SEP-1686) ([#793](https://github.com/joshrotenberg/tower-mcp/pull/793))
- **router:** Reversible tool/resource/prompt disable/enable ([#792](https://github.com/joshrotenberg/tower-mcp/pull/792))
- **http:** Host header validation with :authority fallback and rejection logging ([#798](https://github.com/joshrotenberg/tower-mcp/pull/798))
- **http:** Allow external NotificationSender via with_notifications ([#801](https://github.com/joshrotenberg/tower-mcp/pull/801))

### Testing

- Close critical test coverage gaps ([#757](https://github.com/joshrotenberg/tower-mcp/pull/757)) ([#766](https://github.com/joshrotenberg/tower-mcp/pull/766))



## [0.10.0] - 2026-03-26

### Documentation

- Fix remaining 0.8 version reference in README ([#759](https://github.com/joshrotenberg/tower-mcp/pull/759))

### Features

- Default to optional sessions for HTTP transport ([#761](https://github.com/joshrotenberg/tower-mcp/pull/761))
- HTTP client session expiry detection and automatic recovery ([#764](https://github.com/joshrotenberg/tower-mcp/pull/764))
- OAuth 2.0 Authorization Code grant with PKCE for HTTP client ([#765](https://github.com/joshrotenberg/tower-mcp/pull/765))



## [0.9.2] - 2026-03-25

### Documentation

- Update all top-level docs for v0.9 and example reorganization ([#756](https://github.com/joshrotenberg/tower-mcp/pull/756))

### Features

- WebSocket spec compliance, stateless mode, and resource/prompt context ([#751](https://github.com/joshrotenberg/tower-mcp/pull/751))
- SEP-1442 stateless HTTP transport ([#753](https://github.com/joshrotenberg/tower-mcp/pull/753))

### Refactor

- Consolidate examples from 27 to 23 ([#755](https://github.com/joshrotenberg/tower-mcp/pull/755))

### Testing

- Close critical test coverage gaps ([#758](https://github.com/joshrotenberg/tower-mcp/pull/758))



## [0.9.1] - 2026-03-19

### Features

- Add optional_sessions() to HttpTransport for client compatibility ([#743](https://github.com/joshrotenberg/tower-mcp/pull/743))



## [0.9.0] - 2026-03-18

### Features

- Replace simple CircuitBreakerLayer with tower-resilience re-exports ([#740](https://github.com/joshrotenberg/tower-mcp/pull/740))



## [0.8.8] - 2026-03-17

### Features

- Derive Serialize/Deserialize on RouterResponse and inner types ([#735](https://github.com/joshrotenberg/tower-mcp/pull/735))
- Add list_sessions() and terminate_session() to SessionHandle ([#736](https://github.com/joshrotenberg/tower-mcp/pull/736))



## [0.8.7] - 2026-03-16

### Features

- Add McpProxy::remove_backend(), replace_backend(), backend_namespaces() ([#730](https://github.com/joshrotenberg/tower-mcp/pull/730))



## [0.8.6] - 2026-03-16

### Documentation

- Update README, lib.rs, and AGENTS.md for accuracy and completeness ([#726](https://github.com/joshrotenberg/tower-mcp/pull/726))
- Add missing examples and register all examples in Cargo.toml ([#728](https://github.com/joshrotenberg/tower-mcp/pull/728))



## [0.8.5] - 2026-03-16

### Features

- Add request rewrite helpers and response error checking ([#707](https://github.com/joshrotenberg/tower-mcp/pull/707))

### Testing

- Add Send+Sync assertions for McpProxy ([#705](https://github.com/joshrotenberg/tower-mcp/pull/705))



## [0.8.4] - 2026-03-09

### Features

- Add built-in tool call logging to McpRouter ([#700](https://github.com/joshrotenberg/tower-mcp/pull/700))



## [0.8.3] - 2026-03-07

### Features

- Add StdioClientTransport::spawn_command for custom Command config ([#624](https://github.com/joshrotenberg/tower-mcp/pull/624))
- Add HttpTransport::from_service() for generic service support ([#626](https://github.com/joshrotenberg/tower-mcp/pull/626))
- Expose session count via SessionHandle from HttpTransport ([#629](https://github.com/joshrotenberg/tower-mcp/pull/629))
- Add dynamic backend addition to McpProxy ([#630](https://github.com/joshrotenberg/tower-mcp/pull/630))



## [0.8.2] - 2026-03-07

### Bug Fixes

- Proxy improvements -- error visibility, poll_ready, instructions, docs ([#614](https://github.com/joshrotenberg/tower-mcp/pull/614))
- Make compile_uri_template fallible, add try_handler ([#619](https://github.com/joshrotenberg/tower-mcp/pull/619))

### Documentation

- Pre-release documentation fixes ([#621](https://github.com/joshrotenberg/tower-mcp/pull/621))

### Features

- MCP proxy for multi-server aggregation ([#600](https://github.com/joshrotenberg/tower-mcp/pull/600))
- Optional proc macros for tools, prompts, and resources ([#613](https://github.com/joshrotenberg/tower-mcp/pull/613))
- Dynamic prompt registry and skill-to-prompt example ([#615](https://github.com/joshrotenberg/tower-mcp/pull/615))
- Dynamic resource and resource template registries ([#616](https://github.com/joshrotenberg/tower-mcp/pull/616))
- Add AuditLayer middleware for structured audit logging ([#618](https://github.com/joshrotenberg/tower-mcp/pull/618))



## [0.8.1] - 2026-03-06

### Bug Fixes

- Use oneshot() to ensure poll_ready before call in JsonRpcService ([#598](https://github.com/joshrotenberg/tower-mcp/pull/598))

### Features

- Add client handler and sampling server examples ([#589](https://github.com/joshrotenberg/tower-mcp/pull/589))
- Add OAuth client example ([#590](https://github.com/joshrotenberg/tower-mcp/pull/590))



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


