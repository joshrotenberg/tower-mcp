//! Skill-to-prompt example -- load Agent Skills as MCP prompts.
//!
//! Demonstrates loading markdown skill files and registering them as dynamic
//! MCP prompts at runtime. This pattern lets servers ship domain expertise
//! alongside their tools without any special library support -- just
//! `PromptBuilder` + `DynamicPromptRegistry`.
//!
//! The skill files follow the [Agent Skills](https://agentskills.io/specification)
//! format: a `SKILL.md` with YAML frontmatter (`name`, `description`) and
//! markdown body content.
//!
//! Run with: `cargo run --example skill_prompts --features dynamic-tools`
//!
//! Then try:
//!   - `prompts/list` to see the loaded skills
//!   - `prompts/get` with `name: "code-review"` to get the full skill content

use std::path::Path;

use tower_mcp::context::notification_channel;
use tower_mcp::{
    DynamicPromptRegistry, GetPromptResult, JsonRpcRequest, JsonRpcService, McpRouter,
    PromptBuilder,
};

/// A parsed skill from a SKILL.md file.
struct Skill {
    name: String,
    description: String,
    body: String,
}

/// Parse a SKILL.md file with YAML frontmatter.
///
/// Expected format:
/// ```text
/// ---
/// name: skill-name
/// description: What this skill does
/// ---
///
/// Markdown body content...
/// ```
fn parse_skill(content: &str) -> Option<Skill> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }

    // Find the closing frontmatter delimiter
    let after_first = &content[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];
    let body = after_first[end + 3..].trim().to_string();

    // Simple YAML parsing -- just extract name and description
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().to_string());
        }
    }

    Some(Skill {
        name: name?,
        description: description?,
        body,
    })
}

/// Load all skills from a directory.
///
/// Each subdirectory should contain a `SKILL.md` file.
fn load_skills(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Failed to read skills directory {}: {e}", dir.display());
            return skills;
        }
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        let skill_file = entry.path().join("SKILL.md");
        if let Ok(content) = std::fs::read_to_string(&skill_file) {
            match parse_skill(&content) {
                Some(skill) => {
                    println!("  Loaded skill: {} -- {}", skill.name, skill.description);
                    skills.push(skill);
                }
                None => {
                    eprintln!("  Skipped {}: invalid frontmatter", skill_file.display());
                }
            }
        }
    }

    skills
}

/// Register parsed skills as dynamic prompts.
fn register_skills(registry: &DynamicPromptRegistry, skills: Vec<Skill>) {
    for skill in skills {
        let body = skill.body.clone();
        let description = skill.description.clone();

        let prompt = PromptBuilder::new(&skill.name)
            .description(&skill.description)
            .handler(move |_args| {
                let body = body.clone();
                let description = description.clone();
                async move {
                    Ok(GetPromptResult::user_message_with_description(
                        body,
                        description,
                    ))
                }
            })
            .build();

        registry.register(prompt);
    }
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Set up router with dynamic prompts
    let (router, prompt_registry) = McpRouter::new()
        .server_info("skill-prompts-example", "0.1.0")
        .with_dynamic_prompts();

    // Load skills from the examples/skills directory
    let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/skills");

    println!("Loading skills from {}...", skills_dir.display());
    let skills = load_skills(&skills_dir);
    println!("Loaded {} skills.\n", skills.len());

    // Register skills as prompts
    register_skills(&prompt_registry, skills);

    // Wire up notifications and create service
    let (tx, mut rx) = notification_channel(256);
    let router = router.with_notification_sender(tx);
    let mut service = JsonRpcService::new(router);

    println!("=== Skill Prompts Example ===\n");

    // 1. Initialize
    println!("1. Initialize:");
    let resp = service
        .call_single(
            JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "example-client", "version": "1.0.0" }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 2. List prompts -- should show the loaded skills
    println!("2. List prompts:");
    let resp = service
        .call_single(JsonRpcRequest::new(2, "prompts/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 3. Get a specific skill prompt
    println!("3. Get 'code-review' prompt:");
    let resp = service
        .call_single(
            JsonRpcRequest::new(3, "prompts/get").with_params(serde_json::json!({
                "name": "code-review"
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 4. Dynamically add a new skill at runtime
    println!("4. Register a new skill at runtime:");
    let prompt = PromptBuilder::new("quick-debug")
        .description("Quick debugging checklist")
        .user_message("1. Check the error message\n2. Read the stack trace\n3. Reproduce locally\n4. Add logging\n5. Check recent changes");
    prompt_registry.register(prompt);
    println!("   Registered 'quick-debug'\n");

    // 5. List prompts again -- should include the new one
    println!("5. List prompts (after adding 'quick-debug'):");
    let resp = service
        .call_single(JsonRpcRequest::new(5, "prompts/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 6. Remove a skill
    println!("6. Unregister 'rust-error-handling':");
    prompt_registry.unregister("rust-error-handling");
    println!("   Done.\n");

    // 7. Final prompt list
    println!("7. List prompts (final):");
    let resp = service
        .call_single(JsonRpcRequest::new(7, "prompts/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 8. Drain notifications
    println!("=== Notifications received ===");
    let mut count = 0;
    while let Ok(notification) = rx.try_recv() {
        count += 1;
        println!("   [{count}] {notification:?}");
    }
    if count == 0 {
        println!("   (none)");
    }

    Ok(())
}
