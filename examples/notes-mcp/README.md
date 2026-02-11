# notes-mcp

A Redis-backed customer notes MCP server — a lightweight CRM context system for AI agents.

This example demonstrates tower-mcp with a persistent storage backend using Redis 8's built-in JSON and Search capabilities (RedisJSON + RediSearch). The core idea: give an AI agent a searchable, persistent memory for customer relationships so it can recall context, track history, and prepare for interactions.

## Why Redis for MCP context?

Most MCP examples use ephemeral or read-only data sources. A persistent, searchable store changes what an agent can do:

- **Deterministic recall** — Full-text search and tag filters mean the agent retrieves exactly the right notes, not whatever fits in a context window. Searching `@tags:{renewal}` returns every renewal-related note across all customers, every time.
- **Accumulating context** — Each `add_note` call builds the knowledge base. Over time, the agent's ability to prepare briefings and spot patterns improves because it has more data to search, not because the model changed.
- **Structured filtering** — RediSearch TAG fields (tier, note type, customer ID) give the agent precise, composable filters. "All enterprise meeting notes tagged security" is a single query, not an LLM summarization task.
- **Separation of storage and reasoning** — The agent reasons over search results, not raw data. Redis handles indexing, sorting, and filtering; the model handles synthesis and judgment.

## Features

- **8 Tools**:
  - `search_customers` — Full-text search across name, company, and role with optional tier filter
  - `search_notes` — Full-text content search with filters for customer, note type, and tags
  - `list_customers` — List all customers with their note counts
  - `get_customer` — Complete customer profile with all their notes, sorted by date
  - `add_note` — Create timestamped, tagged notes attached to a customer
  - `update_customer` — Update customer profile fields (name, company, email, role, tier)
  - `update_note` — Update note content, type, or tags
  - `delete_note` — Permanently delete a note by ID

- **1 Resource Template + 1 Static Resource**:
  - `notes://customers/{id}` — Customer profile and notes as a readable resource
  - `notes://recent` — The 10 most recent notes across all customers

- **2 Prompts**:
  - `prep_meeting` — Guided meeting preparation workflow that chains tool calls to build a briefing
  - `account_review` — Tier-level portfolio review with themes and action items

- **Seed Data** — 5 customers and 18 notes with realistic narrative arcs:
  - Enterprise renewal negotiation (Meridian Systems)
  - Startup evaluation and conversion (LaunchPad AI)
  - SMB self-service success story (CraftBrew Supply Co)
  - Enterprise compliance journey (Titan Financial Group)
  - Startup technical deep-dive (NexGen Robotics)

## Prerequisites

- Redis 8.0+ (JSON and Search are built-in as of Redis 8)

## Quick Start

```bash
# Start Redis
docker compose -f examples/notes-mcp/docker-compose.yml up -d

# Build and run with seed data
cargo run -p notes-mcp -- --seed
```

## CLI Options

```
Options:
  -t, --transport <TRANSPORT>    Transport to use [default: stdio]
      --redis-url <URL>          Redis connection URL [default: redis://127.0.0.1:6379]
      --seed                     Seed the database with sample data
  -l, --log-level <LEVEL>       Log level [default: info]
  -h, --help                     Print help
```

## MCP Client Configuration

### Claude Code

The project's `.mcp.json` includes an entry that seeds automatically:

```json
{
  "mcpServers": {
    "notes-mcp": {
      "command": "cargo",
      "args": ["run", "-p", "notes-mcp", "--", "--seed"],
      "env": {
        "RUST_LOG": "notes_mcp=info"
      }
    }
  }
}
```

### Claude Desktop

Add to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "notes": {
      "command": "/path/to/tower-mcp/target/release/notes-mcp",
      "args": ["--seed"]
    }
  }
}
```

## Example Usage

Once connected, try:

1. **Search for a customer:**
   > "Find enterprise customers"

2. **Get full context on a customer:**
   > "Show me everything about Sarah Chen"

3. **Search notes by topic:**
   > "Find all notes about security and compliance"

4. **Add a note after a call:**
   > "Add a meeting note for Marcus at LaunchPad AI: discussed migration timeline, targeting Q1 launch"

5. **List all customers:**
   > "List all customers and their note counts"

6. **Update a customer:**
   > "Update Sarah Chen's role to VP of Engineering"

7. **Prepare for a meeting:**
   > Use the `prep_meeting` prompt with customer name "David Park" and focus "compliance"

8. **Review a tier:**
   > Use the `account_review` prompt with tier "enterprise" and focus "renewals"

## How It Works

### Data Model

**Customers** are stored as JSON documents at `customer:{id}` keys and indexed for full-text search on name, company, and role, with TAG fields for email and tier.

**Notes** are stored at `note:{id}` keys with full-text search on content, and TAG fields for customer ID, note type, tags, and creation date (sortable).

### Redis Commands Used

| Operation | Redis Commands |
|-----------|---------------|
| Search customers/notes | `FT.SEARCH` with full-text queries and TAG filters |
| List all customers | `FT.SEARCH idx:customers *` + per-customer note count query |
| Get a customer | `JSON.GET customer:{id} $` |
| Add a note | `JSON.GET` (verify customer) + `JSON.SET note:{uuid} $` |
| Update customer/note | `JSON.GET` (verify exists) + `JSON.SET` per changed field |
| Delete a note | `JSON.GET` (verify exists) + `DEL note:{id}` |
| Create indexes | `FT.CREATE` with JSON schema mappings |
| Seed data | `FT.DROPINDEX` + `FT.CREATE` + `JSON.SET` per document |

### Search Query Examples

The tools build RediSearch queries dynamically:

```
# Full-text search
sarah

# Multi-word search (OR-joined automatically)
(case|study|marketing)

# Text + tier filter
(sarah) @tier:{enterprise}

# Notes by customer with tag filter (hyphens escaped)
@customerId:{c1} @tags:{case\-study}

# All meeting notes
@noteType:{meeting}
```

## Project Structure

```
src/
├── main.rs              # CLI, router setup, transport
├── state.rs             # AppState, data models, FT.SEARCH parser, query helpers
├── seed.rs              # Index creation + sample data
├── tools/
│   ├── mod.rs
│   ├── search_customers.rs
│   ├── search_notes.rs
│   ├── list_customers.rs
│   ├── add_note.rs
│   ├── get_customer.rs
│   ├── update_customer.rs
│   ├── update_note.rs
│   └── delete_note.rs
├── resources/
│   ├── mod.rs
│   ├── customer.rs      # notes://customers/{id} template
│   └── recent_notes.rs  # notes://recent static resource
└── prompts/
    ├── mod.rs
    ├── prep_meeting.rs
    └── account_review.rs
```

## License

MIT OR Apache-2.0
