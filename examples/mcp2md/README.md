# mcp2md

`mcp2md` connects to an MCP server, discovers its public surface, and writes a
deterministic Markdown reference. It also reports documentation gaps without
calling tools, rendering prompts, or reading resources.

## Installation

```console
cargo install mcp2md
```

From this repository, replace `mcp2md` in the examples below with
`cargo run -p mcp2md --`.

## Usage

```console
# Spawn a stdio server.
mcp2md stdio -- cargo run --example getting_started

# Connect over Streamable HTTP.
mcp2md http http://127.0.0.1:3000/mcp

# Write the generated reference to a file.
mcp2md --output MCP.md http https://example.com/mcp
```

Stable MCP is the default. A server using the final 2026-07-28 lifecycle can
be inspected explicitly:

```console
mcp2md --protocol final http https://example.com/mcp
```

By default the document includes readable parameter tables, exact JSON
schemas, a canonical raw protocol inventory, and a documentation assessment.
Use `--compact` to omit the raw JSON or `--no-assessment` to omit the
assessment. HTTP bearer authentication uses `--bearer` or `MCP_BEARER`; custom
headers can be repeated with `--header 'Name: Value'`.

The assessment measures whether documentation is present, including nested
tool fields, not whether it is correct. Optional presentation metadata such as
titles, tool behavior annotations, output schemas, and resource MIME types is
reported separately and does not reduce the documentation score.

## CI and committed documentation

Generate and commit a reference once:

```console
mcp2md --output MCP.md stdio -- ./my-server
```

Then verify both freshness and documentation coverage in CI:

```console
mcp2md \
  --check MCP.md \
  --fail-under 90 \
  --assessment-output mcp-documentation.json \
  stdio -- ./my-server
```

`--check` exits unsuccessfully when the generated Markdown differs from the
committed file. `--fail-under` independently enforces a coverage percentage.
The JSON assessment includes server identity, negotiated protocol, category
coverage, optional metadata coverage, and every concrete documentation gap.

## Safety and scope

The generator performs the handshake and advertised list operations only. It
does not call tools, render prompts, read resources, or subscribe to updates.
Descriptions and metadata come from the inspected server and should be treated
as untrusted content when publishing the resulting document.
