//! Skills extension (SEP-2640) client scenarios.
//!
//! The harness plays the server and grades which requests arrive:
//!
//! - `sep-2640-client-no-prefetch`: list skills and stop, reading no file.
//! - `sep-2640-client-verify-{digest,size,frontmatter}`: load the first
//!   listed skill (read and verify its `SKILL.md`), then read and verify its
//!   first supporting file. The harness tampers with `SKILL.md`, so a
//!   verifying client fails at the first read and never asks for the
//!   supporting file. That failure is the pass, which is why this exits with
//!   an error in those scenarios.

use anyhow::{Context, Result, bail};
use tower_mcp::client::McpClient;
use tower_mcp::protocol::skills::SKILLS_EXTENSION_ID;

use crate::core_scenarios::connect_activated;
use crate::handlers;

/// Whether the server declared the Skills extension, on either lifecycle.
async fn declares_skills(client: &McpClient) -> bool {
    let declared = |extensions: Option<&std::collections::HashMap<String, serde_json::Value>>| {
        extensions.is_some_and(|e| e.contains_key(SKILLS_EXTENSION_ID))
    };
    if let Some(discovered) = client.discovery().await
        && declared(discovered.capabilities.extensions.as_ref())
    {
        return true;
    }
    client
        .server_info()
        .await
        .is_some_and(|info| declared(info.capabilities.extensions.as_ref()))
}

/// `sep-2640-client-no-prefetch`: enumerate skills without fetching any file.
pub async fn no_prefetch(server_url: &str) -> Result<()> {
    let client = connect_activated(server_url, || handlers::BasicHandler, |b| b).await?;
    if !declares_skills(&client).await {
        bail!("server did not declare {SKILLS_EXTENSION_ID}");
    }
    let skills = client.list_all_skills().await?;
    tracing::info!(count = skills.len(), "Listed skills without loading any");
    client.shutdown().await?;
    Ok(())
}

/// `sep-2640-client-verify-*`: load a skill and read a supporting file, with
/// every read verified against the listed entry.
pub async fn load_and_verify(server_url: &str) -> Result<()> {
    let client = connect_activated(server_url, || handlers::BasicHandler, |b| b).await?;
    if !declares_skills(&client).await {
        bail!("server did not declare {SKILLS_EXTENSION_ID}");
    }
    let skills = client.list_all_skills().await?;
    let skill = skills.first().context("server listed no skills")?;

    let manifest = client
        .read_verified_skill_file(skill, &skill.uri)
        .await
        .context("SKILL.md failed verification; not loading the skill")?;
    tracing::info!(uri = %skill.uri, bytes = manifest.len(), "Loaded verified SKILL.md");

    if let Some(supporting) = skill
        .resources
        .files()
        .and_then(|files| files.iter().find(|f| f.uri != skill.uri))
    {
        let bytes = client
            .read_verified_skill_file(skill, &supporting.uri)
            .await
            .with_context(|| format!("{} failed verification", supporting.uri))?;
        tracing::info!(uri = %supporting.uri, bytes = bytes.len(), "Read verified supporting file");
    }

    client.shutdown().await?;
    Ok(())
}
