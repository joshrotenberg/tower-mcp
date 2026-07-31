# Official SDK interoperability

This harness smoke-tests tower-mcp against the released official MCP SDKs:

- TypeScript SDK `2.0.0`
- Python SDK `2.0.0`
- MCP `2025-11-25` (session-based `initialize`)
- MCP `2026-07-28` (stateless `server/discover`)

The eight-leg matrix runs both directions for both protocols:

| Client | Server | 2025-11-25 | 2026-07-28 |
| --- | --- | :---: | :---: |
| tower-mcp | TypeScript SDK | yes | yes |
| TypeScript SDK | tower-mcp | yes | yes |
| tower-mcp | Python SDK | yes | yes |
| Python SDK | tower-mcp | yes | yes |

Every leg checks negotiation, `tools/list`, `tools/call`, `resources/list`,
`resources/read`, `prompts/list`, `prompts/get`, and clean client shutdown. The
fixtures are deliberately small smoke tests; the conformance suites remain the
source of exhaustive protocol coverage.

## Run

Install Rust, Node.js 20 or newer, Python 3.12 or newer, and `uv`, then run:

```bash
python3 tools/sdk-interop/run.py
```

The runner installs the exact npm lockfile, syncs the exact uv lockfile, builds
the Rust fixture, reserves separate loopback ports, and always terminates child
servers. Server logs are printed when a leg fails.

The npm fixture overrides the Node adapter's transitive `@hono/node-server`
dependency to `2.0.12`. The official SDK's declared 1.x range currently
resolves to a version affected by a Windows static-file path-traversal advisory;
the patched adapter API is covered by this matrix and `npm audit` is clean.

## Updating SDK versions

Change the exact package versions in `typescript/package.json` and
`python/pyproject.toml`, regenerate both lockfiles, then run the full matrix.
Do not replace the pins with ranges: a release-readiness check should test a
known SDK pair and make dependency updates reviewable.
