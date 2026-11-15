//! End-to-end tests for the Skills extension (SEP-2640): a router serving a
//! [`SkillSet`], driven by [`McpClient`] over an in-process channel and by raw
//! JSON-RPC over the HTTP transport on the 2026-07-28 path.

#![cfg(feature = "skills")]

use tower_mcp::client::{ChannelTransport, McpClient};
use tower_mcp::protocol::skills::{DIRECTORY_MIME_TYPE, SKILLS_EXTENSION_ID};
use tower_mcp::skills::{SkillBuilder, SkillSet, sha256_digest};
use tower_mcp::{CallToolResult, Error, McpRouter, ToolBuilder};

const PDF_SKILL_MD: &str = "---\nname: pdf-processing\ndescription: Extract, fill, and assemble PDF documents\nlicense: Apache-2.0\n---\n\n# PDF processing\n\nChoose the matching template from `templates/`.\n";
const REFUNDS_SKILL_MD: &str =
    "---\nname: refunds\ndescription: Process customer refund requests per company policy\n---\n";
const INVOICE: &str = "# Invoice\n\nCustomer:\nAmount:\n";

fn skill_set() -> SkillSet {
    SkillSet::new()
        .skill(
            SkillBuilder::new("pdf-processing")
                .skill_md(PDF_SKILL_MD)
                .file("templates/invoice.md", INVOICE)
                .file("templates/regional/eu.md", "EU\n")
                .file("scripts/blob.bin", vec![0u8, 159, 146, 150]),
        )
        .unwrap()
        .skill(
            SkillBuilder::new("acme/billing/refunds")
                .skill_md(REFUNDS_SKILL_MD)
                .unlisted(),
        )
        .unwrap()
        .directory_read(true)
}

async fn client_for(router: McpRouter) -> McpClient {
    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .unwrap();
    client.initialize("skills-test", "1.0.0").await.unwrap();
    client
}

fn router() -> McpRouter {
    McpRouter::new()
        .server_info("skills-test-server", "1.0.0")
        .skills(skill_set())
}

fn error_code<T: std::fmt::Debug>(result: Result<T, Error>) -> i32 {
    match result {
        Err(Error::JsonRpc(e)) => e.code,
        other => panic!("expected a JSON-RPC error, got {other:?}"),
    }
}

#[tokio::test]
async fn capabilities_declare_extension_and_resources() {
    let client = client_for(router()).await;
    let info = client.server_info().await.unwrap();
    let caps = info.capabilities;
    assert!(
        caps.resources.is_some(),
        "skills require the resources capability"
    );
    let extensions = caps.extensions.expect("extensions declared");
    assert_eq!(
        extensions[SKILLS_EXTENSION_ID],
        serde_json::json!({ "directoryRead": true })
    );
}

#[tokio::test]
async fn list_returns_listed_skills_only() {
    let client = client_for(router()).await;
    let listed = client.list_skills().await.unwrap();
    assert_eq!(listed.skills.len(), 1, "the unlisted skill stays out");
    let skill = &listed.skills[0];
    assert_eq!(skill.uri, "skill://pdf-processing/SKILL.md");
    assert_eq!(skill.frontmatter["license"], "Apache-2.0");
    let files = skill.resources.files().unwrap();
    assert_eq!(files.len(), 4);
    let invoice = skill
        .resources
        .get("skill://pdf-processing/templates/invoice.md")
        .unwrap();
    assert_eq!(invoice.size, 29);
    assert_eq!(invoice.digest, sha256_digest(INVOICE.as_bytes()));
    // SEP-2640 requires both cache hints on every result.
    assert_eq!(listed.ttl_ms, Some(0));
    assert!(listed.cache_scope.is_some());
}

#[tokio::test]
async fn get_answers_for_unlisted_skills_and_rejects_unknown() {
    let client = client_for(router()).await;
    let got = client
        .get_skill("skill://acme/billing/refunds/SKILL.md")
        .await
        .unwrap();
    assert_eq!(got.skill.name(), Some("refunds"));
    assert!(got.ttl_ms.is_some() && got.cache_scope.is_some());

    let code = error_code(
        client
            .get_skill("skill://acme/billing/chargebacks/SKILL.md")
            .await,
    );
    assert_eq!(code, -32602);
}

#[tokio::test]
async fn files_are_resources_and_verify() {
    let client = client_for(router()).await;
    let resources = client.list_all_resources().await.unwrap();
    let manifest = resources
        .iter()
        .find(|r| r.uri == "skill://pdf-processing/SKILL.md")
        .expect("SKILL.md is listed as a resource");
    assert_eq!(manifest.mime_type.as_deref(), Some("text/markdown"));
    assert_eq!(manifest.name, "pdf-processing");
    assert_eq!(
        manifest.description.as_deref(),
        Some("Extract, fill, and assemble PDF documents")
    );

    let skill = client
        .get_skill("skill://pdf-processing/SKILL.md")
        .await
        .unwrap()
        .skill;
    let md = client
        .read_verified_skill_file(&skill, &skill.uri)
        .await
        .unwrap();
    assert_eq!(md, PDF_SKILL_MD.as_bytes());
    let blob = client
        .read_verified_skill_file(&skill, "skill://pdf-processing/scripts/blob.bin")
        .await
        .unwrap();
    assert_eq!(blob, vec![0u8, 159, 146, 150]);

    // A file outside the held entry is refused before any read.
    let mut stale = skill.clone();
    if let tower_mcp::protocol::skills::SkillResources::Files(files) = &mut stale.resources {
        files.retain(|f| !f.uri.ends_with("invoice.md"));
    }
    assert!(
        client
            .read_verified_skill_file(&stale, "skill://pdf-processing/templates/invoice.md")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn directory_read_lists_direct_children() {
    let client = client_for(router()).await;
    let root = client
        .read_resource_directory("skill://pdf-processing", None)
        .await
        .unwrap();
    let names: Vec<_> = root
        .resources
        .iter()
        .map(|r| (r.name.as_str(), r.mime_type.as_deref()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("pdf-processing", Some("text/markdown")),
            ("scripts", Some(DIRECTORY_MIME_TYPE)),
            ("templates", Some(DIRECTORY_MIME_TYPE)),
        ]
    );

    let templates = client
        .read_resource_directory("skill://pdf-processing/templates", None)
        .await
        .unwrap();
    assert_eq!(templates.resources.len(), 2);
    assert!(templates.ttl_ms.is_some() && templates.cache_scope.is_some());

    let code = error_code(
        client
            .read_resource_directory("skill://pdf-processing/SKILL.md", None)
            .await,
    );
    assert_eq!(code, -32602);
    let code = error_code(
        client
            .read_resource_directory("skill://nowhere", None)
            .await,
    );
    assert_eq!(code, -32602);
}

#[tokio::test]
async fn directory_read_is_method_not_found_unless_enabled() {
    let skills = skill_set().directory_read(false);
    let client = client_for(McpRouter::new().skills(skills)).await;
    let info = client.server_info().await.unwrap();
    assert_eq!(
        info.capabilities.extensions.unwrap()[SKILLS_EXTENSION_ID],
        serde_json::json!({})
    );
    let code = error_code(
        client
            .read_resource_directory("skill://pdf-processing", None)
            .await,
    );
    assert_eq!(code, -32601);
}

#[tokio::test]
async fn skills_methods_are_method_not_found_without_a_catalog() {
    let echo = ToolBuilder::new("echo")
        .description("Echo")
        .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
        .build();
    let client = client_for(McpRouter::new().tool(echo)).await;
    assert_eq!(error_code(client.list_skills().await), -32601);
    assert_eq!(
        error_code(client.get_skill("skill://a/SKILL.md").await),
        -32601
    );
}

#[tokio::test]
async fn disabling_the_manifest_hides_the_skill() {
    let router = router();
    router.disable_resource("skill://pdf-processing/SKILL.md");
    let client = client_for(router).await;
    assert!(client.list_skills().await.unwrap().skills.is_empty());
    assert_eq!(
        error_code(client.get_skill("skill://pdf-processing/SKILL.md").await),
        -32602
    );
}

#[tokio::test]
async fn listing_paginates_without_splitting_entries() {
    let more = SkillSet::new()
        .skill(SkillBuilder::new("alpha").skill_md("---\nname: alpha\ndescription: First\n---\n"))
        .unwrap()
        .skill(SkillBuilder::new("beta").skill_md("---\nname: beta\ndescription: Second\n---\n"))
        .unwrap();
    let client = client_for(router().skills(more).page_size(1)).await;
    let first = client.list_skills().await.unwrap();
    assert_eq!(first.skills.len(), 1);
    assert!(first.next_cursor.is_some());
    let all = client.list_all_skills().await.unwrap();
    let uris: Vec<_> = all.iter().map(|s| s.uri.as_str()).collect();
    assert_eq!(
        uris,
        vec![
            "skill://alpha/SKILL.md",
            "skill://beta/SKILL.md",
            "skill://pdf-processing/SKILL.md",
        ]
    );
}

#[tokio::test]
async fn merging_routers_carries_skills() {
    let other = McpRouter::new().skills(
        SkillSet::new()
            .skill(
                SkillBuilder::new("alpha").skill_md("---\nname: alpha\ndescription: First\n---\n"),
            )
            .unwrap(),
    );
    let client = client_for(router().merge(other)).await;
    assert_eq!(client.list_all_skills().await.unwrap().len(), 2);
    assert!(
        client
            .read_resource_directory("skill://alpha", None)
            .await
            .is_ok(),
        "directoryRead from the first catalog survives the merge"
    );
}

#[cfg(all(feature = "http", feature = "stateless"))]
mod final_protocol {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use tower_mcp::HttpTransport;
    use tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;

    async fn post(method: &str, params: serde_json::Value) -> serde_json::Value {
        let app = HttpTransport::new(router())
            .disable_origin_validation()
            .into_router();
        let mut params = params;
        params["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28,
            "io.modelcontextprotocol/clientInfo": { "name": "t", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        });
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Protocol-Version", PROTOCOL_VERSION_2026_07_28)
            .header("Mcp-Method", method)
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)))
    }

    #[tokio::test]
    async fn discover_declares_the_extension() {
        let resp = post("server/discover", serde_json::json!({})).await;
        let caps = &resp["result"]["capabilities"];
        assert_eq!(
            caps["extensions"][SKILLS_EXTENSION_ID],
            serde_json::json!({ "directoryRead": true }),
            "{resp}"
        );
        assert!(caps["resources"].is_object(), "{resp}");
    }

    #[tokio::test]
    async fn results_carry_result_type_and_cache_hints() {
        for (method, params) in [
            ("skills/list", serde_json::json!({})),
            (
                "skills/get",
                serde_json::json!({ "uri": "skill://pdf-processing/SKILL.md" }),
            ),
            (
                "resources/directory/read",
                serde_json::json!({ "uri": "skill://pdf-processing/templates" }),
            ),
        ] {
            let resp = post(method, params).await;
            let result = &resp["result"];
            assert_eq!(result["resultType"], "complete", "{method}: {resp}");
            assert!(result["ttlMs"].is_u64(), "{method}: {resp}");
            assert!(result["cacheScope"].is_string(), "{method}: {resp}");
        }
    }

    #[tokio::test]
    async fn get_has_no_cursor_and_unknown_is_invalid_params() {
        let resp = post(
            "skills/get",
            serde_json::json!({ "uri": "skill://pdf-processing/SKILL.md" }),
        )
        .await;
        assert!(resp["result"].get("nextCursor").is_none());
        let resp = post(
            "skills/get",
            serde_json::json!({ "uri": "skill://nope/SKILL.md" }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
    }
}
