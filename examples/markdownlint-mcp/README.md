# markdownlint-mcp

An MCP server for markdown linting, powered by [mdbook-lint](https://github.com/joshrotenberg/mdbook-lint).

This example demonstrates building a full-featured MCP server with tower-mcp, including tools, resources, resource templates, and prompts.

## Features

- **66 lint rules** from mdbook-lint (standard markdown + mdBook-specific)
- **Automatic fixes** for many common issues
- **Multiple input sources**: direct content, local files, or URLs
- **Rule browsing** via resources

## Requirements

- Rust 1.88+ (required by mdbook-lint)

## Running

### Stdio transport (for Claude Desktop, etc.)

```bash
cargo run -p markdownlint-mcp -- --transport stdio
```

### HTTP transport

```bash
cargo run -p markdownlint-mcp -- --transport http --port 3002
```

## Claude Desktop Configuration

Add to your Claude Desktop config (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "markdownlint": {
      "command": "cargo",
      "args": ["run", "-p", "markdownlint-mcp", "--", "--transport", "stdio"],
      "cwd": "/path/to/tower-mcp"
    }
  }
}
```

Or if you've built the release binary:

```json
{
  "mcpServers": {
    "markdownlint": {
      "command": "/path/to/markdownlint-mcp",
      "args": ["--transport", "stdio"]
    }
  }
}
```

## Tools

### lint_content

Lint markdown content directly.

```json
{
  "content": "# Hello\n\n\n\nToo many blank lines",
  "filename": "test.md"
}
```

### lint_file

Lint a local markdown file.

```json
{
  "path": "/path/to/document.md"
}
```

### lint_url

Fetch and lint markdown from a URL.

```json
{
  "url": "https://raw.githubusercontent.com/user/repo/main/README.md"
}
```

### list_rules

List all available lint rules with descriptions. No parameters required.

### explain_rule

Get detailed information about a specific rule.

```json
{
  "rule_id": "MD001"
}
```

### fix_content

Apply automatic fixes to markdown content where possible.

```json
{
  "content": "# Hello  \n\nTrailing spaces above",
  "filename": "test.md"
}
```

Returns the fixed content along with any remaining violations that couldn't be auto-fixed.

## Resources

### rules://overview

JSON overview of all available lint rules, including:
- Total rule count
- Rule IDs, names, descriptions
- Whether each rule supports auto-fix

### rules://{rule_id}

Resource template for individual rule details. Example: `rules://MD001`

## Prompts

### fix_suggestions

Generate human-readable suggestions for fixing lint violations. Pass the JSON output from `lint_content` or `lint_file` as the `violations_json` argument.

## Example Workflow

1. **Lint a file**: Use `lint_file` to check a markdown document
2. **Review violations**: The result includes rule IDs, line numbers, and messages
3. **Auto-fix**: Use `fix_content` to apply automatic fixes
4. **Get help**: Use `explain_rule` to understand specific violations
5. **Manual fixes**: Use the `fix_suggestions` prompt for guidance on remaining issues

## Rule Categories

- **MD001-MD059**: Standard markdown rules (headings, lists, whitespace, links, code, emphasis)
- **MDBOOK001-MDBOOK007**: mdBook-specific rules (code block languages, SUMMARY.md structure, internal links)

## License

MIT OR Apache-2.0
