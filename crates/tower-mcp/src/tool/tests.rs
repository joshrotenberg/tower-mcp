//! Unit tests for [`Tool`](super::Tool), its builder, and the task types.
//!
//! Moved out in #1256. A sibling module rather than an integration
//! test because these reach private items.

use super::*;
use crate::extract::{Context, Json, RawArgs, State};
use crate::protocol::Content;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    name: String,
}

#[tokio::test]
async fn test_builder_tool() {
    let tool = ToolBuilder::new("greet")
        .description("Greet someone")
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    assert_eq!(tool.name, "greet");
    assert_eq!(tool.description.as_deref(), Some("Greet someone"));

    let result = tool.call(serde_json::json!({"name": "World"})).await;

    assert!(!result.is_error);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn test_mrtr_builder_preserves_input_required_outcome() {
    let tool = ToolBuilder::new("continue")
        .mrtr_handler::<NoParams, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("signed-state"),
            ))
        })
        .build();

    let outcome = tool.call_outcome(serde_json::json!({})).await.unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("signed-state")
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_builder_composes_guards_and_layers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let rounds = Arc::new(AtomicUsize::new(0));
    let observed = rounds.clone();
    let tool = ToolBuilder::new("guarded_continue")
        .mrtr_handler::<NoParams, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("continue"),
            ))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(1)))
        .guard(move |_request| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .build();

    for _ in 0..2 {
        assert!(
            tool.call_outcome(serde_json::json!({}))
                .await
                .unwrap()
                .as_input_required()
                .is_some()
        );
    }
    assert_eq!(rounds.load(Ordering::SeqCst), 2);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn built_mrtr_tool_accepts_a_guard() {
    let tool = ToolBuilder::new("denied_continue")
        .mrtr_handler::<NoParams, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("unreachable"),
            ))
        })
        .build()
        .with_guard(|_request| Err("MRTR access denied".to_string()));

    let outcome = tool.call_outcome(serde_json::json!({})).await.unwrap();
    let result = outcome
        .as_complete()
        .expect("guard rejection is a complete tool error");
    assert!(result.is_error);
    assert_eq!(result.first_text(), Some("MRTR access denied"));
}

#[tokio::test]
async fn test_raw_handler() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::json(args))
        })
        .build();

    let result = tool.call(serde_json::json!({"foo": "bar"})).await;

    assert!(!result.is_error);
}

#[test]
fn test_invalid_tool_name_empty() {
    let err = ToolBuilder::try_new("").err().expect("should fail");
    assert!(err.to_string().contains("cannot be empty"));
}

#[test]
fn test_invalid_tool_name_too_long() {
    let long_name = "a".repeat(65);
    let err = ToolBuilder::try_new(long_name).err().expect("should fail");
    assert!(err.to_string().contains("exceeds maximum"));
}

#[test]
fn test_invalid_tool_name_bad_chars() {
    let err = ToolBuilder::try_new("my tool!").err().expect("should fail");
    assert!(err.to_string().contains("invalid character"));
}

#[test]
#[should_panic(expected = "cannot be empty")]
fn test_new_panics_on_empty_name() {
    ToolBuilder::new("");
}

#[test]
#[should_panic(expected = "exceeds maximum")]
fn test_new_panics_on_too_long_name() {
    ToolBuilder::new("a".repeat(65));
}

#[test]
#[should_panic(expected = "invalid character")]
fn test_new_panics_on_invalid_chars() {
    ToolBuilder::new("my tool!");
}

#[test]
fn test_valid_tool_names() {
    // All valid characters per SEP-986
    let names = [
        "my_tool",
        "my-tool",
        "my.tool",
        "my/tool",
        "user-profile/update",
        "MyTool123",
        "a",
        &"a".repeat(64),
    ];
    for name in names {
        assert!(
            ToolBuilder::try_new(name).is_ok(),
            "Expected '{}' to be valid",
            name
        );
    }
}

#[tokio::test]
async fn test_context_aware_handler() {
    use crate::context::notification_channel;
    use crate::protocol::{ProgressToken, RequestId};

    #[derive(Debug, Deserialize, JsonSchema)]
    struct ProcessInput {
        count: i32,
    }

    let tool = ToolBuilder::new("process")
        .description("Process with context")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<ProcessInput>| async move {
                // Simulate progress reporting
                for i in 0..input.count {
                    if ctx.is_cancelled() {
                        return Ok(CallToolResult::error("Cancelled"));
                    }
                    ctx.report_progress(i as f64, Some(input.count as f64), None)
                        .await;
                }
                Ok(CallToolResult::text(format!(
                    "Processed {} items",
                    input.count
                )))
            },
        )
        .build();

    assert_eq!(tool.name, "process");

    // Test with a context that has progress token and notification sender
    let (tx, mut rx) = notification_channel(10);
    let ctx = RequestContext::new(RequestId::Number(1))
        .with_progress_token(ProgressToken::Number(42))
        .with_notification_sender(tx);

    let result = tool
        .call_with_context(ctx, serde_json::json!({"count": 3}))
        .await;

    assert!(!result.is_error);

    // Check that progress notifications were sent
    let mut progress_count = 0;
    while rx.try_recv().is_ok() {
        progress_count += 1;
    }
    assert_eq!(progress_count, 3);
}

#[tokio::test]
async fn test_context_aware_handler_cancellation() {
    use crate::protocol::RequestId;
    use std::sync::atomic::{AtomicI32, Ordering};

    #[derive(Debug, Deserialize, JsonSchema)]
    struct LongRunningInput {
        iterations: i32,
    }

    let iterations_completed = Arc::new(AtomicI32::new(0));
    let iterations_ref = iterations_completed.clone();

    let tool = ToolBuilder::new("long_running")
        .description("Long running task")
        .extractor_handler(
            (),
            move |ctx: Context, Json(input): Json<LongRunningInput>| {
                let completed = iterations_ref.clone();
                async move {
                    for i in 0..input.iterations {
                        if ctx.is_cancelled() {
                            return Ok(CallToolResult::error("Cancelled"));
                        }
                        completed.fetch_add(1, Ordering::SeqCst);
                        // Simulate work
                        tokio::task::yield_now().await;
                        // Cancel after iteration 2
                        if i == 2 {
                            ctx.cancellation_token().cancel();
                        }
                    }
                    Ok(CallToolResult::text("Done"))
                }
            },
        )
        .build();

    let ctx = RequestContext::new(RequestId::Number(1));

    let result = tool
        .call_with_context(ctx, serde_json::json!({"iterations": 10}))
        .await;

    // Should have been cancelled after 3 iterations (0, 1, 2)
    // The next iteration (3) checks cancellation and returns
    assert!(result.is_error);
    assert_eq!(iterations_completed.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_tool_builder_with_enhanced_fields() {
    let output_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "greeting": {"type": "string"}
        }
    });

    let tool = ToolBuilder::new("greet")
        .title("Greeting Tool")
        .description("Greet someone")
        .output_schema(output_schema.clone())
        .icon("https://example.com/icon.png")
        .icon_with_meta(
            "https://example.com/icon-large.png",
            Some("image/png".to_string()),
            Some(vec!["96x96".to_string()]),
        )
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    assert_eq!(tool.name, "greet");
    assert_eq!(tool.title.as_deref(), Some("Greeting Tool"));
    assert_eq!(tool.description.as_deref(), Some("Greet someone"));
    assert_eq!(tool.output_schema, Some(output_schema));
    assert!(tool.icons.is_some());
    assert_eq!(tool.icons.as_ref().unwrap().len(), 2);

    // Test definition includes new fields
    let def = tool.definition();
    assert_eq!(def.title.as_deref(), Some("Greeting Tool"));
    assert!(def.output_schema.is_some());
    assert!(def.icons.is_some());
}

#[tokio::test]
async fn test_handler_with_state() {
    let shared = Arc::new("shared-state".to_string());

    let tool = ToolBuilder::new("stateful")
        .description("Uses shared state")
        .extractor_handler(
            shared,
            |State(state): State<Arc<String>>, Json(input): Json<GreetInput>| async move {
                Ok(CallToolResult::text(format!(
                    "{}: Hello, {}!",
                    state, input.name
                )))
            },
        )
        .build();

    let result = tool.call(serde_json::json!({"name": "World"})).await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn test_handler_with_state_and_context() {
    use crate::protocol::RequestId;

    let shared = Arc::new(42_i32);

    let tool =
        ToolBuilder::new("stateful_ctx")
            .description("Uses state and context")
            .extractor_handler(
                shared,
                |State(state): State<Arc<i32>>,
                 _ctx: Context,
                 Json(input): Json<GreetInput>| async move {
                    Ok(CallToolResult::text(format!(
                        "{}: Hello, {}!",
                        state, input.name
                    )))
                },
            )
            .build();

    let ctx = RequestContext::new(RequestId::Number(1));
    let result = tool
        .call_with_context(ctx, serde_json::json!({"name": "World"}))
        .await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn test_handler_no_params() {
    let tool = ToolBuilder::new("no_params")
        .description("Takes no parameters")
        .extractor_handler((), |Json(_): Json<NoParams>| async {
            Ok(CallToolResult::text("no params result"))
        })
        .build();

    assert_eq!(tool.name, "no_params");

    // Should work with empty args
    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);

    // Should also work with unexpected args (ignored)
    let result = tool.call(serde_json::json!({"unexpected": "value"})).await;
    assert!(!result.is_error);

    // Check input schema includes type: object
    let schema = tool.definition().input_schema;
    assert_eq!(schema.get("type").unwrap().as_str().unwrap(), "object");
}

#[tokio::test]
async fn test_handler_with_state_no_params() {
    let shared = Arc::new("shared_value".to_string());

    let tool = ToolBuilder::new("with_state_no_params")
        .description("Takes no parameters but has state")
        .extractor_handler(
            shared,
            |State(state): State<Arc<String>>, Json(_): Json<NoParams>| async move {
                Ok(CallToolResult::text(format!("state: {}", state)))
            },
        )
        .build();

    assert_eq!(tool.name, "with_state_no_params");

    // Should work with empty args
    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "state: shared_value");

    // Check input schema includes type: object
    let schema = tool.definition().input_schema;
    assert_eq!(schema.get("type").unwrap().as_str().unwrap(), "object");
}

#[tokio::test]
async fn test_handler_no_params_with_context() {
    let tool = ToolBuilder::new("no_params_with_context")
        .description("Takes no parameters but has context")
        .extractor_handler((), |_ctx: Context, Json(_): Json<NoParams>| async move {
            Ok(CallToolResult::text("context available"))
        })
        .build();

    assert_eq!(tool.name, "no_params_with_context");

    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "context available");
}

#[tokio::test]
async fn test_handler_with_state_and_context_no_params() {
    let shared = Arc::new("shared".to_string());

    let tool = ToolBuilder::new("state_context_no_params")
        .description("Has state and context, no params")
        .extractor_handler(
            shared,
            |State(state): State<Arc<String>>, _ctx: Context, Json(_): Json<NoParams>| async move {
                Ok(CallToolResult::text(format!("state: {}", state)))
            },
        )
        .build();

    assert_eq!(tool.name, "state_context_no_params");

    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "state: shared");
}

#[tokio::test]
async fn test_raw_handler_with_state() {
    let prefix = Arc::new("prefix:".to_string());

    let tool = ToolBuilder::new("raw_with_state")
        .description("Raw handler with state")
        .extractor_handler(
            prefix,
            |State(state): State<Arc<String>>, RawArgs(args): RawArgs| async move {
                Ok(CallToolResult::text(format!("{} {}", state, args)))
            },
        )
        .build();

    assert_eq!(tool.name, "raw_with_state");

    let result = tool.call(serde_json::json!({"key": "value"})).await;
    assert!(!result.is_error);
    assert!(result.first_text().unwrap().starts_with("prefix:"));
}

#[tokio::test]
async fn test_raw_handler_with_state_and_context() {
    let prefix = Arc::new("prefix:".to_string());

    let tool = ToolBuilder::new("raw_state_context")
        .description("Raw handler with state and context")
        .extractor_handler(
            prefix,
            |State(state): State<Arc<String>>, _ctx: Context, RawArgs(args): RawArgs| async move {
                Ok(CallToolResult::text(format!("{} {}", state, args)))
            },
        )
        .build();

    assert_eq!(tool.name, "raw_state_context");

    let result = tool.call(serde_json::json!({"key": "value"})).await;
    assert!(!result.is_error);
    assert!(result.first_text().unwrap().starts_with("prefix:"));
}

#[tokio::test]
async fn test_tool_with_timeout_layer() {
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct SlowInput {
        delay_ms: u64,
    }

    // Create a tool with a short timeout
    let tool = ToolBuilder::new("slow_tool")
        .description("A slow tool")
        .handler(|input: SlowInput| async move {
            tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
            Ok(CallToolResult::text("completed"))
        })
        .layer(TimeoutLayer::new(Duration::from_millis(50)))
        .build();

    // Fast call should succeed
    let result = tool.call(serde_json::json!({"delay_ms": 10})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "completed");

    // Slow call should timeout and return an error result
    let result = tool.call(serde_json::json!({"delay_ms": 200})).await;
    assert!(result.is_error);
    // Tower's timeout error message is "request timed out"
    let msg = result.first_text().unwrap().to_lowercase();
    assert!(
        msg.contains("timed out") || msg.contains("timeout") || msg.contains("elapsed"),
        "Expected timeout error, got: {}",
        msg
    );
}

#[tokio::test]
async fn test_tool_with_concurrency_limit_layer() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tower::limit::ConcurrencyLimitLayer;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct WorkInput {
        id: u32,
    }

    let max_concurrent = Arc::new(AtomicU32::new(0));
    let current_concurrent = Arc::new(AtomicU32::new(0));
    let max_ref = max_concurrent.clone();
    let current_ref = current_concurrent.clone();

    // Create a tool with concurrency limit of 2
    let tool = ToolBuilder::new("concurrent_tool")
        .description("A concurrent tool")
        .handler(move |input: WorkInput| {
            let max = max_ref.clone();
            let current = current_ref.clone();
            async move {
                // Track concurrency
                let prev = current.fetch_add(1, Ordering::SeqCst);
                max.fetch_max(prev + 1, Ordering::SeqCst);

                // Simulate work
                tokio::time::sleep(Duration::from_millis(50)).await;

                current.fetch_sub(1, Ordering::SeqCst);
                Ok(CallToolResult::text(format!("completed {}", input.id)))
            }
        })
        .layer(ConcurrencyLimitLayer::new(2))
        .build();

    // Launch 4 concurrent calls
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let t = tool.call(serde_json::json!({"id": i}));
            tokio::spawn(t)
        })
        .collect();

    for handle in handles {
        let result = handle.await.unwrap();
        assert!(!result.is_error);
    }

    // Max concurrent should not exceed 2
    assert!(max_concurrent.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn test_tool_with_multiple_layers() {
    use std::time::Duration;
    use tower::limit::ConcurrencyLimitLayer;
    use tower::timeout::TimeoutLayer;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct Input {
        value: String,
    }

    // Create a tool with multiple layers stacked
    let tool = ToolBuilder::new("multi_layer_tool")
        .description("Tool with multiple layers")
        .handler(|input: Input| async move {
            Ok(CallToolResult::text(format!("processed: {}", input.value)))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .layer(ConcurrencyLimitLayer::new(10))
        .build();

    let result = tool.call(serde_json::json!({"value": "test"})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "processed: test");
}

#[test]
fn test_tool_catch_error_clone() {
    // ToolCatchError should be Clone when inner is Clone
    // Use a simple tool that we can clone
    let tool = ToolBuilder::new("test")
        .description("test")
        .extractor_handler((), |RawArgs(_args): RawArgs| async {
            Ok(CallToolResult::text("ok"))
        })
        .build();
    // The tool contains a BoxToolService which is cloneable
    let _clone = tool.call(serde_json::json!({}));
}

#[test]
fn test_tool_catch_error_debug() {
    // ToolCatchError implements Debug when inner implements Debug
    // Since our internal services don't require Debug, just verify
    // that ToolCatchError has a Debug impl for appropriate types
    #[derive(Debug, Clone)]
    struct DebugService;

    impl Service<ToolRequest> for DebugService {
        type Response = CallToolResult;
        type Error = crate::error::Error;
        type Future = Pin<
            Box<
                dyn Future<Output = std::result::Result<CallToolResult, crate::error::Error>>
                    + Send,
            >,
        >;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: ToolRequest) -> Self::Future {
            Box::pin(async { Ok(CallToolResult::text("ok")) })
        }
    }

    let catch_error = ToolCatchError::new(DebugService);
    let debug = format!("{:?}", catch_error);
    assert!(debug.contains("ToolCatchError"));
}

#[test]
fn test_tool_request_new() {
    use crate::protocol::RequestId;

    let ctx = RequestContext::new(RequestId::Number(42));
    let args = serde_json::json!({"key": "value"});
    let req = ToolRequest::new(ctx.clone(), args.clone());

    assert_eq!(req.args, args);
}

#[test]
fn test_no_params_schema() {
    // NoParams should produce a schema with type: "object"
    let schema = schemars::schema_for!(NoParams);
    let schema_value = serde_json::to_value(&schema).unwrap();
    assert_eq!(
        schema_value.get("type").and_then(|v| v.as_str()),
        Some("object"),
        "NoParams should generate type: object schema"
    );
}

#[test]
fn test_no_params_deserialize() {
    // NoParams should deserialize from various inputs
    let from_empty_object: NoParams = serde_json::from_str("{}").unwrap();
    assert_eq!(from_empty_object, NoParams);

    let from_null: NoParams = serde_json::from_str("null").unwrap();
    assert_eq!(from_null, NoParams);

    // Should also accept objects with unexpected fields (ignored)
    let from_object_with_fields: NoParams =
        serde_json::from_str(r#"{"unexpected": "value"}"#).unwrap();
    assert_eq!(from_object_with_fields, NoParams);
}

#[tokio::test]
async fn test_no_params_type_in_handler() {
    // NoParams can be used as a handler input type
    let tool = ToolBuilder::new("status")
        .description("Get status")
        .handler(|_input: NoParams| async move { Ok(CallToolResult::text("OK")) })
        .build();

    // Check schema has type: object (not type: null like () would produce)
    let schema = tool.definition().input_schema;
    assert_eq!(
        schema.get("type").and_then(|v| v.as_str()),
        Some("object"),
        "NoParams handler should produce type: object schema"
    );

    // Should work with empty input
    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn test_serde_json_value_handler_has_type_object() {
    // serde_json::Value generates a schema without "type" via schemars.
    // We must ensure "type": "object" is added for MCP compliance.
    let tool = ToolBuilder::new("any_input")
        .description("Accepts any input")
        .handler(|_input: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let schema = tool.definition().input_schema;
    assert_eq!(
        schema.get("type").and_then(|v| v.as_str()),
        Some("object"),
        "serde_json::Value handler should produce schema with type: object"
    );
}

#[tokio::test]
async fn test_tool_with_name_prefix() {
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Input {
        value: String,
    }

    let tool = ToolBuilder::new("query")
        .description("Query something")
        .title("Query Tool")
        .handler(|input: Input| async move { Ok(CallToolResult::text(&input.value)) })
        .build();

    // Create prefixed version
    let prefixed = tool.with_name_prefix("db");

    // Check name is prefixed
    assert_eq!(prefixed.name, "db.query");

    // Check other fields are preserved
    assert_eq!(prefixed.description.as_deref(), Some("Query something"));
    assert_eq!(prefixed.title.as_deref(), Some("Query Tool"));

    // Check the tool still works
    let result = prefixed
        .call(serde_json::json!({"value": "test input"}))
        .await;
    assert!(!result.is_error);
    match &result.content[0] {
        Content::Text { text, .. } => assert_eq!(text, "test input"),
        _ => panic!("Expected text content"),
    }
}

#[tokio::test]
async fn test_tool_with_name_prefix_multiple_levels() {
    let tool = ToolBuilder::new("action")
        .description("Do something")
        .handler(|_: NoParams| async move { Ok(CallToolResult::text("done")) })
        .build();

    // Apply multiple prefixes
    let prefixed = tool.with_name_prefix("level1");
    assert_eq!(prefixed.name, "level1.action");

    let double_prefixed = prefixed.with_name_prefix("level0");
    assert_eq!(double_prefixed.name, "level0.level1.action");
}

// =============================================================================
// no_params_handler tests
// =============================================================================

#[tokio::test]
async fn test_no_params_handler_basic() {
    let tool = ToolBuilder::new("get_status")
        .description("Get current status")
        .no_params_handler(|| async { Ok(CallToolResult::text("OK")) })
        .build();

    assert_eq!(tool.name, "get_status");
    assert_eq!(tool.description.as_deref(), Some("Get current status"));

    // Should work with empty args
    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "OK");

    // Should also work with null args
    let result = tool.call(serde_json::json!(null)).await;
    assert!(!result.is_error);

    // Check input schema has type: object
    let schema = tool.definition().input_schema;
    assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
}

#[tokio::test]
async fn test_no_params_handler_with_captured_state() {
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter_ref = counter.clone();

    let tool = ToolBuilder::new("increment")
        .description("Increment counter")
        .no_params_handler(move || {
            let c = counter_ref.clone();
            async move {
                let prev = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CallToolResult::text(format!("Incremented from {}", prev)))
            }
        })
        .build();

    // Call multiple times
    let _ = tool.call(serde_json::json!({})).await;
    let _ = tool.call(serde_json::json!({})).await;
    let result = tool.call(serde_json::json!({})).await;

    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "Incremented from 2");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_no_params_handler_with_layer() {
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let tool = ToolBuilder::new("slow_status")
        .description("Slow status check")
        .no_params_handler(|| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(CallToolResult::text("done"))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(1)))
        .build();

    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "done");
}

#[tokio::test]
async fn test_no_params_handler_timeout() {
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let tool = ToolBuilder::new("very_slow_status")
        .description("Very slow status check")
        .no_params_handler(|| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(CallToolResult::text("done"))
        })
        .layer(TimeoutLayer::new(Duration::from_millis(50)))
        .build();

    let result = tool.call(serde_json::json!({})).await;
    assert!(result.is_error);
    let msg = result.first_text().unwrap().to_lowercase();
    assert!(
        msg.contains("timed out") || msg.contains("timeout") || msg.contains("elapsed"),
        "Expected timeout error, got: {}",
        msg
    );
}

#[tokio::test]
async fn test_no_params_handler_with_multiple_layers() {
    use std::time::Duration;
    use tower::limit::ConcurrencyLimitLayer;
    use tower::timeout::TimeoutLayer;

    let tool = ToolBuilder::new("multi_layer_status")
        .description("Status with multiple layers")
        .no_params_handler(|| async { Ok(CallToolResult::text("status ok")) })
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .layer(ConcurrencyLimitLayer::new(10))
        .build();

    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "status ok");
}

// =========================================================================
// Guard tests
// =========================================================================

#[tokio::test]
async fn test_guard_allows_request() {
    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct DeleteInput {
        id: String,
        confirm: bool,
    }

    let tool = ToolBuilder::new("delete")
        .description("Delete a record")
        .handler(|input: DeleteInput| async move {
            Ok(CallToolResult::text(format!("deleted {}", input.id)))
        })
        .guard(|req: &ToolRequest| {
            let confirm = req
                .args
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !confirm {
                return Err("Must set confirm=true to delete".to_string());
            }
            Ok(())
        })
        .build();

    let result = tool
        .call(serde_json::json!({"id": "abc", "confirm": true}))
        .await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "deleted abc");
}

#[tokio::test]
async fn test_guard_rejects_request() {
    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct DeleteInput2 {
        id: String,
        confirm: bool,
    }

    let tool = ToolBuilder::new("delete2")
        .description("Delete a record")
        .handler(|input: DeleteInput2| async move {
            Ok(CallToolResult::text(format!("deleted {}", input.id)))
        })
        .guard(|req: &ToolRequest| {
            let confirm = req
                .args
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !confirm {
                return Err("Must set confirm=true to delete".to_string());
            }
            Ok(())
        })
        .build();

    let result = tool
        .call(serde_json::json!({"id": "abc", "confirm": false}))
        .await;
    assert!(result.is_error);
    assert!(
        result
            .first_text()
            .unwrap()
            .contains("Must set confirm=true")
    );
}

#[tokio::test]
async fn test_guard_with_layer() {
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let tool = ToolBuilder::new("guarded_timeout")
        .description("Guarded with timeout")
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .guard(|_req: &ToolRequest| Ok(()))
        .build();

    let result = tool.call(serde_json::json!({"name": "World"})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "Hello, World!");
}

#[tokio::test]
async fn test_guard_on_no_params_handler() {
    let allowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let allowed_clone = allowed.clone();

    let tool = ToolBuilder::new("status")
        .description("Get status")
        .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
        .guard(move |_req: &ToolRequest| {
            if allowed_clone.load(std::sync::atomic::Ordering::Relaxed) {
                Ok(())
            } else {
                Err("Access denied".to_string())
            }
        })
        .build();

    // Allowed
    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "ok");

    // Denied
    allowed.store(false, std::sync::atomic::Ordering::Relaxed);
    let result = tool.call(serde_json::json!({})).await;
    assert!(result.is_error);
    assert!(result.first_text().unwrap().contains("Access denied"));
}

#[tokio::test]
async fn test_guard_on_no_params_handler_with_layer() {
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let tool = ToolBuilder::new("status_layered")
        .description("Get status with layers")
        .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .guard(|_req: &ToolRequest| Ok(()))
        .build();

    let result = tool.call(serde_json::json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "ok");
}

#[tokio::test]
async fn test_guard_on_extractor_handler() {
    use std::sync::Arc;

    #[derive(Clone)]
    struct AppState {
        prefix: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct QueryInput {
        query: String,
    }

    let state = Arc::new(AppState {
        prefix: "db".to_string(),
    });

    let tool = ToolBuilder::new("search")
        .description("Search")
        .extractor_handler(
            state,
            |State(app): State<Arc<AppState>>, Json(input): Json<QueryInput>| async move {
                Ok(CallToolResult::text(format!(
                    "{}: {}",
                    app.prefix, input.query
                )))
            },
        )
        .guard(|req: &ToolRequest| {
            let query = req.args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if query.is_empty() {
                return Err("Query cannot be empty".to_string());
            }
            Ok(())
        })
        .build();

    // Valid query
    let result = tool.call(serde_json::json!({"query": "hello"})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "db: hello");

    // Empty query rejected by guard
    let result = tool.call(serde_json::json!({"query": ""})).await;
    assert!(result.is_error);
    assert!(
        result
            .first_text()
            .unwrap()
            .contains("Query cannot be empty")
    );
}

#[tokio::test]
async fn test_guard_on_extractor_handler_with_layer() {
    use std::sync::Arc;
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    #[derive(Clone)]
    struct AppState2 {
        prefix: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct QueryInput2 {
        query: String,
    }

    let state = Arc::new(AppState2 {
        prefix: "db".to_string(),
    });

    let tool = ToolBuilder::new("search2")
        .description("Search with layer and guard")
        .extractor_handler(
            state,
            |State(app): State<Arc<AppState2>>, Json(input): Json<QueryInput2>| async move {
                Ok(CallToolResult::text(format!(
                    "{}: {}",
                    app.prefix, input.query
                )))
            },
        )
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .guard(|_req: &ToolRequest| Ok(()))
        .build();

    let result = tool.call(serde_json::json!({"query": "hello"})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "db: hello");
}

#[tokio::test]
async fn test_tool_with_guard_post_build() {
    let tool = ToolBuilder::new("admin_action")
        .description("Admin action")
        .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("done")) })
        .build();

    // Apply guard after building
    let guarded = tool.with_guard(|req: &ToolRequest| {
        let name = req.args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name == "admin" {
            Ok(())
        } else {
            Err("Only admin allowed".to_string())
        }
    });

    // Admin passes
    let result = guarded.call(serde_json::json!({"name": "admin"})).await;
    assert!(!result.is_error);

    // Non-admin blocked
    let result = guarded.call(serde_json::json!({"name": "user"})).await;
    assert!(result.is_error);
    assert!(result.first_text().unwrap().contains("Only admin allowed"));
}

#[tokio::test]
async fn test_with_guard_preserves_tool_metadata() {
    let tool = ToolBuilder::new("my_tool")
        .description("A tool")
        .title("My Tool")
        .read_only()
        .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("done")) })
        .build();

    let guarded = tool.with_guard(|_req: &ToolRequest| Ok(()));

    assert_eq!(guarded.name, "my_tool");
    assert_eq!(guarded.description.as_deref(), Some("A tool"));
    assert_eq!(guarded.title.as_deref(), Some("My Tool"));
    assert!(guarded.annotations.is_some());
}

#[tokio::test]
async fn test_guard_group_pattern() {
    // Demonstrate applying the same guard to multiple tools (per-group pattern)
    let require_auth = |req: &ToolRequest| {
        let token = req
            .args
            .get("_token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if token == "valid" {
            Ok(())
        } else {
            Err("Authentication required".to_string())
        }
    };

    let tool1 = ToolBuilder::new("action1")
        .description("Action 1")
        .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("action1")) })
        .build();
    let tool2 = ToolBuilder::new("action2")
        .description("Action 2")
        .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("action2")) })
        .build();

    // Apply same guard to both
    let guarded1 = tool1.with_guard(require_auth);
    let guarded2 = tool2.with_guard(require_auth);

    // Without auth
    let r1 = guarded1
        .call(serde_json::json!({"name": "test", "_token": "invalid"}))
        .await;
    let r2 = guarded2
        .call(serde_json::json!({"name": "test", "_token": "invalid"}))
        .await;
    assert!(r1.is_error);
    assert!(r2.is_error);

    // With auth
    let r1 = guarded1
        .call(serde_json::json!({"name": "test", "_token": "valid"}))
        .await;
    let r2 = guarded2
        .call(serde_json::json!({"name": "test", "_token": "valid"}))
        .await;
    assert!(!r1.is_error);
    assert!(!r2.is_error);
}

#[tokio::test]
async fn test_input_validation_returns_tool_error() {
    // Per SEP-1303: input validation errors should be returned as
    // CallToolResult with isError=true, not as protocol errors.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct StrictInput {
        name: String,
        count: u32,
    }

    let tool = ToolBuilder::new("strict_tool")
        .description("requires specific input")
        .handler(|input: StrictInput| async move {
            Ok(CallToolResult::text(format!(
                "{}: {}",
                input.name, input.count
            )))
        })
        .build();

    // Valid input works
    let result = tool
        .call(serde_json::json!({"name": "test", "count": 5}))
        .await;
    assert!(!result.is_error);

    // Missing required field returns isError, not protocol error
    let result = tool.call(serde_json::json!({"name": "test"})).await;
    assert!(result.is_error);
    let text = result.first_text().unwrap();
    assert!(text.contains("Invalid input"), "got: {text}");

    // Wrong type returns isError, not protocol error
    let result = tool
        .call(serde_json::json!({"name": "test", "count": "not_a_number"}))
        .await;
    assert!(result.is_error);
    let text = result.first_text().unwrap();
    assert!(text.contains("Invalid input"), "got: {text}");
}

#[tokio::test]
async fn test_input_schema_override_with_raw_args() {
    // With a RawArgs handler there is no typed input struct, so the
    // builder normally falls back to `{ "type": "object" }`. The
    // `input_schema` setter must let users declare a richer schema.
    let custom = serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "minLength": 1 }
        },
        "required": ["query"]
    });

    let tool = ToolBuilder::new("query")
        .description("Query with a custom schema")
        .input_schema(custom.clone())
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::json(args))
        })
        .build();

    let schema = tool.definition().input_schema;
    assert_eq!(schema, custom);

    // The handler still executes against the raw args.
    let result = tool.call(serde_json::json!({"query": "hello"})).await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn test_input_schema_override_wins_over_typed_handler() {
    // When both `.input_schema(...)` and a typed `.handler(|x: Foo|)` are
    // provided, the explicit schema must win over the schemars-generated
    // one.
    let custom = serde_json::json!({
        "type": "object",
        "title": "GreetOverride",
        "properties": {
            "name": { "type": "string", "minLength": 1, "maxLength": 64 }
        },
        "required": ["name"],
        "additionalProperties": false
    });

    let tool = ToolBuilder::new("greet")
        .description("Greet someone with a hand-tuned schema")
        .input_schema(custom.clone())
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    let schema = tool.definition().input_schema;
    assert_eq!(schema, custom);
    // Confirm the schemars-generated `GreetInput` schema did not leak in.
    assert_eq!(schema["title"], "GreetOverride");

    // Handler still dispatches via the typed deserialization.
    let result = tool.call(serde_json::json!({"name": "World"})).await;
    assert!(!result.is_error);
}

#[tokio::test]
async fn test_input_schema_override_preserves_2020_12_constructs() {
    // Schemars cannot express `oneOf` in property positions directly;
    // overriding the schema must keep those advanced constructs intact.
    let custom = serde_json::json!({
        "type": "object",
        "properties": {
            "filter": {
                "oneOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": { "field": { "type": "string" } },
                        "required": ["field"]
                    }
                ]
            }
        },
        "required": ["filter"]
    });

    let tool = ToolBuilder::new("filter_tool")
        .description("Demonstrates oneOf preservation")
        .input_schema(custom.clone())
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::json(args))
        })
        .build();

    let schema = tool.definition().input_schema;
    assert_eq!(schema, custom);
    let one_of = schema["properties"]["filter"]["oneOf"]
        .as_array()
        .expect("oneOf must survive as an array");
    assert_eq!(one_of.len(), 2);
    assert_eq!(one_of[0]["type"], "string");
    assert_eq!(one_of[1]["type"], "object");
}

#[tokio::test]
async fn test_input_schema_override_adds_type_object_if_missing() {
    // `ensure_object_schema` must still run against the user-supplied
    // schema, so MCP-spec `type: "object"` is added when omitted.
    let custom_no_type = serde_json::json!({
        "properties": {
            "x": { "type": "number" }
        }
    });

    let tool = ToolBuilder::new("typeless")
        .description("Schema missing top-level type")
        .input_schema(custom_no_type)
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::json(args))
        })
        .build();

    let schema = tool.definition().input_schema;
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["x"].is_object());
}

// =============================================================================
// Clone / prefix / guard vs. handler kind (#1340)
// =============================================================================
//
// #1298 found that `Tool::clone` dropped `live_handler`, and that
// `Tool::with_guard` panicked reaching for an absent service on a live-only
// tool. Both are fixed, but nothing had crossed every construction and
// transformation method against every handler kind systematically. These pin
// that matrix at the unit level, calling the handler after the
// transformation rather than checking that a field is non-`None` -- a field
// check would have passed even while #1298's bug was live, since the field
// itself was untouched; only the wrong one was cloned. The combinations that
// need a live task to actually run end-to-end through a router (clone and
// prefix together on a live-plus-fallback tool, a guard rejecting both of a
// tool's paths) live in `tests/live_tasks.rs`, where the router and task
// store are already wired up; this module covers the handler kinds that
// don't need that machinery, plus the live handler kinds invoked directly
// through the crate-private `LiveToolHandler` trait so a test here does not
// have to stand up a task store just to prove a field survived.

#[tokio::test]
async fn plain_tool_clone_still_runs() {
    let tool = ToolBuilder::new("greet")
        .description("Greet someone")
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    let cloned = tool.clone();
    let result = cloned.call(serde_json::json!({"name": "World"})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "Hello, World!");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_tool_clone_still_runs() {
    let tool = ToolBuilder::new("continue")
        .mrtr_handler::<NoParams, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                crate::protocol::InputRequiredResult::new().with_request_state("signed-state"),
            ))
        })
        .build();

    let cloned = tool.clone();
    let outcome = cloned.call_outcome(serde_json::json!({})).await.unwrap();
    assert_eq!(
        outcome
            .as_input_required()
            .and_then(|result| result.request_state.as_deref()),
        Some("signed-state"),
        "a dropped mrtr_handler would fall through to the absent `service` \
         and panic on `.expect(...)` in `call_outcome_with_context` instead"
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_tool_with_name_prefix_still_runs() {
    let tool = ToolBuilder::new("continue")
        .mrtr_handler::<NoParams, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::Complete(CallToolResult::text("done")))
        })
        .build();

    let prefixed = tool.with_name_prefix("ns");
    assert_eq!(prefixed.name, "ns.continue");

    let outcome = prefixed.call_outcome(serde_json::json!({})).await.unwrap();
    let result = outcome
        .as_complete()
        .expect("mrtr_handler must survive the prefix");
    assert_eq!(result.first_text().unwrap(), "done");
}

/// A live handler has nothing reachable through `Tool::call`/`call_outcome`
/// (#1329 tracks `call_outcome_with_context` panicking for a live-only tool
/// called that way), so these invoke `LiveToolHandler::call` directly. Both
/// the trait and the field it lives behind are crate-private, and this
/// module is a sibling of `tool.rs` rather than an integration test
/// specifically so it can reach them (see the module doc comment).
fn live_ctx() -> RequestContext {
    RequestContext::new(crate::protocol::RequestId::Number(0))
}

#[tokio::test]
async fn live_only_tool_clone_carries_the_live_handler() {
    let tool = ToolBuilder::new("run")
        .description("Completes immediately")
        .live_task_handler(|_task: TaskContext, _input: NoParams| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ran")))
        })
        .build();

    let cloned = tool.clone();
    let handler = cloned
        .live_handler
        .as_ref()
        .expect("clone must carry the live handler");
    let outcome = handler
        .call(
            live_ctx(),
            TaskContext::new("t1".to_string()),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    match outcome {
        TaskOutcome::Completed(result) => assert_eq!(result.first_text().unwrap(), "ran"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn live_only_tool_with_name_prefix_carries_the_live_handler() {
    let tool = ToolBuilder::new("run")
        .description("Completes immediately")
        .live_task_handler(|_task: TaskContext, _input: NoParams| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ran")))
        })
        .build();

    let prefixed = tool.with_name_prefix("ns");
    assert_eq!(prefixed.name, "ns.run");

    let handler = prefixed
        .live_handler
        .as_ref()
        .expect("with_name_prefix must carry the live handler");
    let outcome = handler
        .call(
            live_ctx(),
            TaskContext::new("t1".to_string()),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    match outcome {
        TaskOutcome::Completed(result) => assert_eq!(result.first_text().unwrap(), "ran"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// #1328 landed a `Tool` that carries a live handler and an MRTR fallback at
/// the same time, the day before this matrix was written. Its clone,
/// prefix, and guard paths had no dedicated coverage yet beyond the routing
/// behavior in `tests/live_tasks.rs::multi_tool`.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn live_plus_mrtr_fallback_clone_carries_both_handlers() {
    let tool = ToolBuilder::new("multi")
        .description("Live plus MRTR fallback")
        .live_task_handler(|_task: TaskContext, _input: NoParams| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("live")))
        })
        .fallback_mrtr_handler(|_ctx: RequestContext, _input: NoParams| async move {
            Ok(RequestOutcome::Complete(CallToolResult::text("fallback")))
        })
        .build();

    let cloned = tool.clone();

    let outcome = cloned.call_outcome(serde_json::json!({})).await.unwrap();
    let result = outcome
        .as_complete()
        .expect("mrtr fallback must survive the clone");
    assert_eq!(result.first_text().unwrap(), "fallback");

    let handler = cloned
        .live_handler
        .as_ref()
        .expect("clone must carry the live handler alongside the fallback");
    let outcome = handler
        .call(
            live_ctx(),
            TaskContext::new("t1".to_string()),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    match outcome {
        TaskOutcome::Completed(result) => assert_eq!(result.first_text().unwrap(), "live"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn live_plus_mrtr_fallback_with_name_prefix_carries_both_handlers() {
    let tool = ToolBuilder::new("multi")
        .description("Live plus MRTR fallback")
        .live_task_handler(|_task: TaskContext, _input: NoParams| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("live")))
        })
        .fallback_mrtr_handler(|_ctx: RequestContext, _input: NoParams| async move {
            Ok(RequestOutcome::Complete(CallToolResult::text("fallback")))
        })
        .build();

    let prefixed = tool.with_name_prefix("ns");
    assert_eq!(prefixed.name, "ns.multi");

    let outcome = prefixed.call_outcome(serde_json::json!({})).await.unwrap();
    let result = outcome
        .as_complete()
        .expect("mrtr fallback must survive the prefix");
    assert_eq!(result.first_text().unwrap(), "fallback");

    let handler = prefixed
        .live_handler
        .as_ref()
        .expect("with_name_prefix must carry the live handler alongside the fallback");
    let outcome = handler
        .call(
            live_ctx(),
            TaskContext::new("t1".to_string()),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    match outcome {
        TaskOutcome::Completed(result) => assert_eq!(result.first_text().unwrap(), "live"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn live_plus_mrtr_fallback_with_guard_rejects_both_paths() {
    let tool = ToolBuilder::new("multi")
        .description("Live plus MRTR fallback")
        .live_task_handler(|_task: TaskContext, _input: NoParams| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text(
                "should not run (live)",
            )))
        })
        .fallback_mrtr_handler(|_ctx: RequestContext, _input: NoParams| async move {
            Ok(RequestOutcome::Complete(CallToolResult::text(
                "should not run (fallback)",
            )))
        })
        .build()
        .with_guard(|_req: &ToolRequest| Err("nope".to_string()));

    let outcome = tool.call_outcome(serde_json::json!({})).await.unwrap();
    let result = outcome
        .as_complete()
        .expect("guard rejection is a complete tool error");
    assert!(result.is_error);
    assert_eq!(result.first_text().unwrap(), "nope");

    let handler = tool
        .live_handler
        .as_ref()
        .expect("with_guard must not drop the live handler");
    let outcome = handler
        .call(
            live_ctx(),
            TaskContext::new("t1".to_string()),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    match outcome {
        TaskOutcome::Completed(result) => {
            assert!(result.is_error);
            assert_eq!(result.first_text().unwrap(), "nope");
        }
        other => panic!("expected a rejected-but-terminal Completed, got {other:?}"),
    }
}
