//! Unit tests for [`Resource`](super::Resource), [`ResourceTemplate`](super::ResourceTemplate), and their builders.

use super::*;
use std::time::Duration;
use tower::timeout::TimeoutLayer;

#[tokio::test]
async fn test_builder_resource() {
    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test File")
        .description("A test file")
        .text("Hello, World!");

    assert_eq!(resource.uri, "file:///test.txt");
    assert_eq!(resource.name, "Test File");
    assert_eq!(resource.description.as_deref(), Some("A test file"));

    let result = resource.read().await;
    assert_eq!(result.contents.len(), 1);
    assert_eq!(result.contents[0].text.as_deref(), Some("Hello, World!"));
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn test_mrtr_builder_preserves_input_required_outcome() {
    let resource = ResourceBuilder::new("test://continue")
        .mrtr_handler(|_ctx| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("signed-state"),
            ))
        })
        .build();

    let outcome = resource
        .read_outcome_with_context(RequestContext::new(crate::protocol::RequestId::Number(1)))
        .await
        .unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("signed-state")
    );
}

// =========================================================================
// Clone vs. handler kind (#1340)
// =========================================================================
//
// `Resource::clone` (see the hand-written `impl Clone for Resource` above)
// is exercised incidentally by `read()`/`read_with_context()`, which clone
// `self` before dispatching -- so a plain-handler resource's clone was
// already covered by every test that calls `.read()`. `read_outcome_with_context`,
// which the router actually calls, does not clone `self`, only the handler
// behind it, so nothing here had exercised `Resource::clone` for an MRTR
// handler at all. `Tool::clone` dropping a field (#1298) is the reason
// this gets a dedicated, named test rather than staying implicit.

#[tokio::test]
async fn resource_clone_carries_the_plain_handler() {
    let resource = ResourceBuilder::new("memory://cloneable")
        .name("Cloneable")
        .handler(|| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "memory://cloneable".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("original".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .build();

    let cloned = resource.clone();
    let result = cloned.read().await;
    assert_eq!(result.contents[0].text.as_deref(), Some("original"));
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn resource_clone_carries_the_mrtr_handler() {
    let resource = ResourceBuilder::new("test://cloneable-continue")
        .mrtr_handler(|_ctx| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("cloned-state"),
            ))
        })
        .build();

    let cloned = resource.clone();
    let outcome = cloned
        .read_outcome_with_context(RequestContext::new(crate::protocol::RequestId::Number(1)))
        .await
        .unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("cloned-state"),
        "a dropped mrtr_handler after clone would fall through to the \
         absent `service` and panic on `.expect(...)` instead of \
         returning this"
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_resource_composes_middleware() {
    let resource = ResourceBuilder::new("test://layered-continue")
        .mrtr_handler(|_ctx| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("layered-state"),
            ))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(1)))
        .build();

    let outcome = resource
        .read_outcome_with_context(RequestContext::new(crate::protocol::RequestId::Number(2)))
        .await
        .unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("layered-state")
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn test_mrtr_template_preserves_context_and_input_required_outcome() {
    let template = ResourceTemplateBuilder::new("test://items/{id}").mrtr_handler(
        |ctx, uri, variables| async move {
            assert_eq!(ctx.request_state(), Some("prior-state"));
            assert_eq!(uri, "test://items/42");
            assert_eq!(variables.get("id").map(String::as_str), Some("42"));
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("next-state"),
            ))
        },
    );
    let variables = template.match_uri("test://items/42").unwrap();
    let mut ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
    ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
        None,
        Some("prior-state".into()),
    ));

    let outcome = template
        .read_outcome_with_context(ctx, "test://items/42", variables)
        .await
        .unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("next-state")
    );
}

#[tokio::test]
async fn test_json_resource() {
    let resource = ResourceBuilder::new("file:///config.json")
        .name("Config")
        .json(serde_json::json!({"key": "value"}));

    assert_eq!(resource.mime_type.as_deref(), Some("application/json"));

    let result = resource.read().await;
    assert!(result.contents[0].text.as_ref().unwrap().contains("key"));
}

#[tokio::test]
async fn test_handler_resource() {
    let resource = ResourceBuilder::new("memory://counter")
        .name("Counter")
        .handler(|| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "memory://counter".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("42".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .build();

    let result = resource.read().await;
    assert_eq!(result.contents[0].text.as_deref(), Some("42"));
}

#[tokio::test]
async fn test_handler_resource_with_layer() {
    let resource = ResourceBuilder::new("file:///with-timeout.txt")
        .name("Resource with Timeout")
        .handler(|| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "file:///with-timeout.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("content".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build();

    let result = resource.read().await;
    assert_eq!(result.contents[0].text.as_deref(), Some("content"));
}

#[tokio::test]
async fn test_handler_resource_with_timeout_error() {
    let resource = ResourceBuilder::new("file:///slow.txt")
        .name("Slow Resource")
        .handler(|| async {
            // Sleep much longer than timeout to ensure timeout fires reliably in CI
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "file:///slow.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("content".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .layer(TimeoutLayer::new(Duration::from_millis(50)))
        .build();

    let result = resource.read().await;
    // read() still renders an error as content, since its signature has
    // no room for a Result. The timeout is a genuine middleware
    // failure (#1280), so it is sanitized to -32603 rather than
    // leaking the raw middleware error.
    assert!(result.contents[0].text.as_ref().unwrap().contains("-32603"));
}

#[tokio::test]
async fn test_context_aware_handler() {
    let resource = ResourceBuilder::new("file:///ctx.txt")
        .name("Context Resource")
        .handler_with_context(|_ctx: RequestContext| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "file:///ctx.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("context aware".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .build();

    let result = resource.read().await;
    assert_eq!(result.contents[0].text.as_deref(), Some("context aware"));
}

#[tokio::test]
async fn test_context_aware_handler_with_layer() {
    let resource = ResourceBuilder::new("file:///ctx-layer.txt")
        .name("Context Resource with Layer")
        .handler_with_context(|_ctx: RequestContext| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "file:///ctx-layer.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("context with layer".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        })
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build();

    let result = resource.read().await;
    assert_eq!(
        result.contents[0].text.as_deref(),
        Some("context with layer")
    );
}

#[tokio::test]
async fn test_trait_resource() {
    struct TestResource;

    impl McpResource for TestResource {
        const URI: &'static str = "test://resource";
        const NAME: &'static str = "Test";
        const DESCRIPTION: Option<&'static str> = Some("A test resource");
        const MIME_TYPE: Option<&'static str> = Some("text/plain");

        async fn read(&self) -> Result<ReadResourceResult> {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: Self::URI.to_string(),
                    mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
                    text: Some("test content".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        }
    }

    let resource = TestResource.into_resource();
    assert_eq!(resource.uri, "test://resource");
    assert_eq!(resource.name, "Test");

    let result = resource.read().await;
    assert_eq!(result.contents[0].text.as_deref(), Some("test content"));
}

#[test]
fn test_resource_definition() {
    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test")
        .description("Description")
        .mime_type("text/plain")
        .text("content");

    let def = resource.definition();
    assert_eq!(def.uri, "file:///test.txt");
    assert_eq!(def.name, "Test");
    assert_eq!(def.description.as_deref(), Some("Description"));
    assert_eq!(def.mime_type.as_deref(), Some("text/plain"));
}

#[test]
fn test_resource_request_new() {
    let ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
    let req = ResourceRequest::new(ctx, "file:///test.txt".to_string());
    assert_eq!(req.uri, "file:///test.txt");
}

#[test]
fn test_resource_catch_error_clone() {
    let handler = FnHandler {
        handler: || async {
            Ok::<_, Error>(ReadResourceResult {
                contents: vec![],
                meta: None,
                ..Default::default()
            })
        },
    };
    let service = ResourceHandlerService::new(handler);
    let catch_error = ResourceCatchError::new(service);
    let _clone = catch_error.clone();
}

#[test]
fn test_resource_catch_error_debug() {
    let handler = FnHandler {
        handler: || async {
            Ok::<_, Error>(ReadResourceResult {
                contents: vec![],
                meta: None,
                ..Default::default()
            })
        },
    };
    let service = ResourceHandlerService::new(handler);
    let catch_error = ResourceCatchError::new(service);
    let debug = format!("{:?}", catch_error);
    assert!(debug.contains("ResourceCatchError"));
}

// =========================================================================
// Resource Template Tests
// =========================================================================

#[test]
fn test_compile_uri_template_simple() {
    let CompiledTemplate {
        pattern: regex,
        variables: vars,
        ..
    } = compile_uri_template("file:///{path}").unwrap();
    assert_eq!(vars, vec!["path"]);
    assert!(regex.is_match("file:///README.md"));
    assert!(!regex.is_match("file:///foo/bar")); // no slashes in simple expansion
}

#[test]
fn test_compile_uri_template_multiple_vars() {
    let CompiledTemplate {
        pattern: regex,
        variables: vars,
        ..
    } = compile_uri_template("api://v1/{resource}/{id}").unwrap();
    assert_eq!(vars, vec!["resource", "id"]);
    assert!(regex.is_match("api://v1/users/123"));
    assert!(regex.is_match("api://v1/posts/abc"));
    assert!(!regex.is_match("api://v1/users")); // missing id
}

#[test]
fn test_compile_uri_template_reserved_expansion() {
    let CompiledTemplate {
        pattern: regex,
        variables: vars,
        ..
    } = compile_uri_template("file:///{+path}").unwrap();
    assert_eq!(vars, vec!["path"]);
    assert!(regex.is_match("file:///README.md"));
    assert!(regex.is_match("file:///foo/bar/baz.txt")); // slashes allowed
}

#[test]
fn test_compile_uri_template_special_chars() {
    let CompiledTemplate {
        pattern: regex,
        variables: vars,
        ..
    } = compile_uri_template("http://example.com/api?query={q}").unwrap();
    assert_eq!(vars, vec!["q"]);
    assert!(regex.is_match("http://example.com/api?query=hello"));
}

#[test]
fn test_resource_template_match_uri() {
    let template = ResourceTemplateBuilder::new("db://users/{id}")
        .name("User Records")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: Some(format!("User {}", vars.get("id").unwrap())),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    // Test matching
    let vars = template.match_uri("db://users/123").unwrap();
    assert_eq!(vars.get("id"), Some(&"123".to_string()));

    // Test non-matching
    assert!(template.match_uri("db://posts/123").is_none());
    assert!(template.match_uri("db://users").is_none());
}

#[test]
fn test_resource_template_match_multiple_vars() {
    let template = ResourceTemplateBuilder::new("api://{version}/{resource}/{id}")
        .name("API Resources")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let vars = template.match_uri("api://v2/users/abc-123").unwrap();
    assert_eq!(vars.get("version"), Some(&"v2".to_string()));
    assert_eq!(vars.get("resource"), Some(&"users".to_string()));
    assert_eq!(vars.get("id"), Some(&"abc-123".to_string()));
}

#[tokio::test]
async fn test_resource_template_read() {
    let template = ResourceTemplateBuilder::new("file:///{path}")
        .name("Files")
        .mime_type("text/plain")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let path = vars.get("path").unwrap().clone();
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("text/plain".to_string()),
                    text: Some(format!("Contents of {}", path)),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let vars = template.match_uri("file:///README.md").unwrap();
    let result = template.read("file:///README.md", vars).await.unwrap();

    assert_eq!(result.contents.len(), 1);
    assert_eq!(result.contents[0].uri, "file:///README.md");
    assert_eq!(
        result.contents[0].text.as_deref(),
        Some("Contents of README.md")
    );
}

#[test]
fn test_resource_template_definition() {
    let template = ResourceTemplateBuilder::new("db://records/{id}")
        .name("Database Records")
        .description("Access database records by ID")
        .mime_type("application/json")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let def = template.definition();
    assert_eq!(def.uri_template, "db://records/{id}");
    assert_eq!(def.name, "Database Records");
    assert_eq!(
        def.description.as_deref(),
        Some("Access database records by ID")
    );
    assert_eq!(def.mime_type.as_deref(), Some("application/json"));
}

#[test]
fn test_resource_template_reserved_path() {
    let template = ResourceTemplateBuilder::new("file:///{+path}")
        .name("Files with subpaths")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    // Reserved expansion should match slashes
    let vars = template.match_uri("file:///src/lib/utils.rs").unwrap();
    assert_eq!(vars.get("path"), Some(&"src/lib/utils.rs".to_string()));
}

#[test]
fn test_resource_annotations() {
    use crate::protocol::{ContentAnnotations, ContentRole};

    let annotations = ContentAnnotations {
        audience: Some(vec![ContentRole::User]),
        priority: Some(0.8),
        last_modified: None,
    };

    let resource = ResourceBuilder::new("file:///important.txt")
        .name("Important File")
        .annotations(annotations.clone())
        .text("content");

    let def = resource.definition();
    assert!(def.annotations.is_some());
    let ann = def.annotations.unwrap();
    assert_eq!(ann.priority, Some(0.8));
    assert_eq!(ann.audience.unwrap(), vec![ContentRole::User]);
}

#[test]
fn test_resource_template_annotations() {
    use crate::protocol::{ContentAnnotations, ContentRole};

    let annotations = ContentAnnotations {
        audience: Some(vec![ContentRole::Assistant]),
        priority: Some(0.5),
        last_modified: None,
    };

    let template = ResourceTemplateBuilder::new("db://users/{id}")
        .name("Users")
        .annotations(annotations)
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: Some("data".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let def = template.definition();
    assert!(def.annotations.is_some());
    let ann = def.annotations.unwrap();
    assert_eq!(ann.priority, Some(0.5));
    assert_eq!(ann.audience.unwrap(), vec![ContentRole::Assistant]);
}

#[test]
fn test_resource_no_annotations_by_default() {
    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test")
        .text("content");

    let def = resource.definition();
    assert!(def.annotations.is_none());
}

#[test]
fn test_try_handler_success() {
    let result = ResourceTemplateBuilder::new("db://users/{id}")
        .name("Users")
        .try_handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: Some("ok".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    assert!(result.is_ok());
    let template = result.unwrap();
    assert_eq!(template.uri_template, "db://users/{id}");
}

#[test]
fn test_compile_uri_template_returns_result() {
    // Valid templates should succeed
    assert!(compile_uri_template("file:///{path}").is_ok());
    assert!(compile_uri_template("api://v1/{resource}/{id}").is_ok());
    assert!(compile_uri_template("file:///{+path}").is_ok());
    assert!(compile_uri_template("no-vars").is_ok());
    assert!(compile_uri_template("").is_ok());
}
