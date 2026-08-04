# Changelog

All notable changes to this project will be documented in this file.

## [0.1.9] - 2026-08-04

### Bug Fixes

- **mcp-repl:** Subscribe to final surface changes ([#1159](https://github.com/joshrotenberg/tower-mcp/pull/1159))

### Features

- **mcp-repl:** Define strict scripting contracts ([#1162](https://github.com/joshrotenberg/tower-mcp/pull/1162))
- **mcp-repl:** Import standard MCP server configs ([#1164](https://github.com/joshrotenberg/tower-mcp/pull/1164))
- **mcp-repl:** Validate schema snapshots ([#1165](https://github.com/joshrotenberg/tower-mcp/pull/1165))
- **mcp-repl:** Add secure OAuth profiles ([#1166](https://github.com/joshrotenberg/tower-mcp/pull/1166))

### Miscellaneous Tasks

- **mcp-repl:** Prepare standalone lifecycle ([#1169](https://github.com/joshrotenberg/tower-mcp/pull/1169))

### Refactor

- **mcp-repl:** Split library core from binary ([#1167](https://github.com/joshrotenberg/tower-mcp/pull/1167))

### Testing

- **mcp-repl:** Add binary transport E2E coverage ([#1160](https://github.com/joshrotenberg/tower-mcp/pull/1160))



## [0.1.8] - 2026-08-03

### Miscellaneous Tasks

- Updated the following local packages: tower-mcp



## [0.1.7] - 2026-08-02

### Bug Fixes

- **mcp-repl:** Preserve quoted tool arguments ([#1138](https://github.com/joshrotenberg/tower-mcp/pull/1138))
- **mcp-repl:** Redraw around child stderr ([#1139](https://github.com/joshrotenberg/tower-mcp/pull/1139))

### Features

- **mcp-repl:** Surface task status transitions ([#1140](https://github.com/joshrotenberg/tower-mcp/pull/1140))



## [0.1.6] - 2026-08-01

### Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.1.5] - 2026-07-31

### Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.1.4] - 2026-07-30

### Features

- **mcp-repl:** Server profiles via config file ([#1017](https://github.com/joshrotenberg/tower-mcp/pull/1017))
- **mcp-repl:** Wire tracing and last-exchange inspection ([#1020](https://github.com/joshrotenberg/tower-mcp/pull/1020))
- **mcp-repl:** Respond to sampling/create requests ([#1023](https://github.com/joshrotenberg/tower-mcp/pull/1023))
- **mcp-repl:** Command aliases ([#1022](https://github.com/joshrotenberg/tower-mcp/pull/1022))
- **mcp-repl:** Auto-reconnect and session resurrection ([#1018](https://github.com/joshrotenberg/tower-mcp/pull/1018))
- **mcp-repl:** Find command and did-you-mean for unknown commands ([#1021](https://github.com/joshrotenberg/tower-mcp/pull/1021))
- **mcp-repl:** Subscribe to resource updates ([#1025](https://github.com/joshrotenberg/tower-mcp/pull/1025))
- **mcp-repl:** Bench command for tool latency sampling ([#1024](https://github.com/joshrotenberg/tower-mcp/pull/1024))
- **mcp-repl:** Output capture and pipe filtering ([#1033](https://github.com/joshrotenberg/tower-mcp/pull/1033))
- **repl:** Select stable or final protocol lifecycle ([#1055](https://github.com/joshrotenberg/tower-mcp/pull/1055))



## [0.1.3] - 2026-07-24

### Bug Fixes

- **mcp-repl:** Don't blame multi-instance for every not-initialized startup ([#1004](https://github.com/joshrotenberg/tower-mcp/pull/1004))

### Features

- **mcp-repl:** Fetch the startup surface concurrently ([#992](https://github.com/joshrotenberg/tower-mcp/pull/992))
- **mcp-repl:** Auth flags and per-call latency annotations ([#998](https://github.com/joshrotenberg/tower-mcp/pull/998))
- **mcp-repl:** List the tools at startup ([#1000](https://github.com/joshrotenberg/tower-mcp/pull/1000))
- **mcp-repl:** One-shot execution mode and persistent history ([#1003](https://github.com/joshrotenberg/tower-mcp/pull/1003))



## [0.1.2] - 2026-07-24

### Features

- **mcp-repl:** Info replays the full startup banner; resources/templates cross-hint ([#989](https://github.com/joshrotenberg/tower-mcp/pull/989))



## [0.1.1] - 2026-07-24

### Bug Fixes

- **mcp-repl:** Retry surface fetch when the server reports not-initialized ([#987](https://github.com/joshrotenberg/tower-mcp/pull/987))



## [0.1.0] - 2026-07-23

### Documentation

- **mcp-repl:** Live cratesio-mcp example and related tools ([#982](https://github.com/joshrotenberg/tower-mcp/pull/982))

### Features

- **examples:** Mcp-repl, an interactive MCP client REPL ([#966](https://github.com/joshrotenberg/tower-mcp/pull/966))
- **mcp-repl:** Reedline, colored output, describe, template completion ([#981](https://github.com/joshrotenberg/tower-mcp/pull/981))

### Miscellaneous Tasks

- **mcp-repl:** Prepare for crates.io publishing ([#980](https://github.com/joshrotenberg/tower-mcp/pull/980))


