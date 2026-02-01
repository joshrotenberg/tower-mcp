//! tower-mcp-guide - Meta MCP server for exploring tower-mcp examples
//!
//! This server provides prompts that guide users through the available
//! tower-mcp example servers.

use std::collections::HashMap;
use tower_mcp::protocol::{Content, GetPromptResult, PromptMessage, PromptRole};
use tower_mcp::{McpRouter, Prompt, PromptBuilder, StdioTransport};

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=info".parse()?),
        )
        .init();

    let router = McpRouter::new()
        .server_info("tower-mcp-guide", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Meta server with prompts for exploring tower-mcp example servers. \
             Use the 'explore_examples' prompt to see what's available.",
        )
        .prompt(build_explore_examples_prompt())
        .prompt(build_test_server_prompt());

    StdioTransport::new(router).run().await?;
    Ok(())
}

fn build_explore_examples_prompt() -> Prompt {
    PromptBuilder::new("explore_examples")
        .description("Guide to all tower-mcp example MCP servers")
        .handler(|_args: HashMap<String, String>| async move {
            Ok(GetPromptResult {
                description: Some("Guide to tower-mcp example servers".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: EXPLORE_EXAMPLES_TEXT.to_string(),
                        annotations: None,
                    },
                }],
            })
        })
}

fn build_test_server_prompt() -> Prompt {
    PromptBuilder::new("test_server")
        .description("Test a specific tower-mcp example server")
        .required_arg(
            "server",
            "Server name: crates, markdownlint, weather, or conformance",
        )
        .handler(|args: HashMap<String, String>| async move {
            let server = args.get("server").map(|s| s.as_str()).unwrap_or("crates");
            let text = match server {
                "crates" | "crates-mcp" | "crates-mcp-local" => CRATES_TEST_TEXT,
                "markdownlint" | "markdownlint-mcp" => MARKDOWNLINT_TEST_TEXT,
                "weather" => WEATHER_TEST_TEXT,
                "conformance" | "conformance-server" => CONFORMANCE_TEST_TEXT,
                _ => "Unknown server. Try: crates, markdownlint, weather, or conformance",
            };
            Ok(GetPromptResult {
                description: Some(format!("Test guide for {} server", server)),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: text.to_string(),
                        annotations: None,
                    },
                }],
            })
        })
}

const EXPLORE_EXAMPLES_TEXT: &str = r#"# tower-mcp Example Servers

This repo includes several MCP servers you can explore. Here's what's available:

## 1. crates-mcp-local / crates-mcp-remote
Query the Rust crate registry (crates.io).

**Tools:**
- `search_crates` - Search for crates by name/keywords
- `get_crate_info` - Get detailed crate information
- `get_crate_versions` - Version history
- `get_dependencies` - Crate dependencies
- `get_reverse_dependencies` - Who depends on this crate
- `get_downloads` - Download statistics
- `get_owners` - Crate maintainers
- `get_summary` - crates.io global stats
- `get_crate_authors` - Package authors
- `get_user` - User profile info

**Try:** "Search for async runtime crates" or "What are tokio's dependencies?"

## 2. markdownlint-mcp
Lint markdown files with 66 rules from mdbook-lint.

**Tools:**
- `lint_content` - Lint markdown text directly
- `lint_file` - Lint a local file
- `lint_url` - Fetch and lint from URL
- `list_rules` - Show all 66 lint rules
- `explain_rule` - Details about a specific rule
- `fix_content` - Auto-fix violations

**Resources:**
- `rules://overview` - All rules as JSON
- `rules://{rule_id}` - Individual rule details

**Try:** "Lint this README for issues" or "What does rule MD001 check for?"

## 3. weather
Weather forecasts using the National Weather Service API.

**Tools:**
- `get_forecast` - Get weather forecast for coordinates
- `get_alerts` - Get weather alerts for a state

**Try:** "What's the weather in San Francisco?" or "Any weather alerts in California?"

## 4. conformance
Full MCP protocol conformance server (passes 39/39 official tests).

**Tools:** echo, add, longRunningOperation, sampleLLM, getTinyImage
**Resources:** Static and dynamic test resources
**Prompts:** Various test prompts including `exercise_conformance`

**Try:** Use the `exercise_conformance` prompt to test all features!

---

Which server would you like to explore? I can help you test any of them.
"#;

const CRATES_TEST_TEXT: &str = r#"Let's test the crates-mcp server! Try these:

1. **Search:** Find crates related to "async http client"
2. **Info:** Get details about the `tokio` crate
3. **Dependencies:** What does `axum` depend on?
4. **Reverse deps:** What crates depend on `tower`?
5. **Stats:** Get the crates.io summary statistics

Go ahead and try these queries using the crates-mcp tools!"#;

const MARKDOWNLINT_TEST_TEXT: &str = r#"Let's test the markdownlint-mcp server! Try these:

1. **Lint some text:**
   ```markdown
   # Hello



   Too many blank lines above!
   ## Skipped heading level
   ```

2. **List rules:** Get all 66 available lint rules

3. **Explain a rule:** What does MD001 (heading-increment) check for?

4. **Fix content:** Auto-fix the violations in the text above

5. **Read resource:** Get the rules://overview resource

Go ahead and try these using the markdownlint-mcp tools!"#;

const WEATHER_TEST_TEXT: &str = r#"Let's test the weather server! Try these:

1. **Forecast:** What's the weather forecast for:
   - San Francisco (37.7749, -122.4194)
   - New York City (40.7128, -74.0060)
   - Seattle (47.6062, -122.3321)

2. **Alerts:** Check for weather alerts in:
   - California (CA)
   - Florida (FL)
   - Texas (TX)

Go ahead and try these using the weather tools!"#;

const CONFORMANCE_TEST_TEXT: &str = r#"Let's test the conformance server! This server implements all MCP spec features.

**Quick tests:**
1. `echo` - Echo back "Hello MCP!"
2. `add` - Add 17 + 25

**Advanced tests:**
3. `longRunningOperation` - Watch for progress notifications
4. `sampleLLM` - This will request sampling from you (meta!)
5. `getTinyImage` - Returns a small PNG image

**Resources:**
6. Read `test://static/resource`
7. Try template `test://dynamic/resource/hello`

**Prompts:**
8. Get `test_simple_prompt`
9. Get `test_prompt_with_arguments` with arg1="foo" and arg2="bar"

Or just use the `exercise_conformance` prompt to run through everything!

Go ahead and try these using the conformance tools!"#;
