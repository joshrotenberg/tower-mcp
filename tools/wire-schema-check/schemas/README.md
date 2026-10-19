# Vendored MCP schemas

Published JSON schemas used by `wire-schema-check`, copied unmodified from
`schema/<version>/schema.json` in
[modelcontextprotocol/modelcontextprotocol](https://github.com/modelcontextprotocol/modelcontextprotocol).

| Version      | Dialect    | Last upstream commit touching the file       |
|--------------|------------|----------------------------------------------|
| `2025-06-18` | draft-07   | `2ebb8060268e177b22598b403d6ba6764bb899ac`   |
| `2025-11-25` | 2020-12    | `c4c367f9f58296a7053f5c78a52fd02bfbb56a49`   |
| `2026-07-28` | 2020-12    | `271ecc9accafdd9b83a3c869fa67c22953b2af80`   |

Fetched at upstream `main` commit `1155ca460e5097019f32769b38dcba194432e2fb`
(2026-10-18), so that SHA reproduces all three files:

```sh
SHA=1155ca460e5097019f32769b38dcba194432e2fb
for v in 2025-06-18 2025-11-25 2026-07-28; do
  gh api "repos/modelcontextprotocol/modelcontextprotocol/contents/schema/$v/schema.json?ref=$SHA" \
    -H "Accept: application/vnd.github.raw" > "$v/schema.json"
done
```

A schema bump is a reviewed change: re-fetch at a new SHA, update this table,
run the conformance suites, and baseline or fix any new failure.

The `2025-06-18` file keeps its definitions under `definitions`; the other two
use `$defs`. `wire-schema-check` handles both.

`crates/tower-mcp/tests/fixtures/mcp-schema-2025-11-25.json` is a separate,
older copy of the `2025-11-25` schema used by the type-level tests in
`crates/tower-mcp/tests/schema_validation.rs`. It differs from the file here in the `NumberSchema` and `Task` definitions.
