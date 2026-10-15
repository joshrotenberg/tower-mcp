//! Unit tests for tool schema validation and `output_schema_for`.
//!
//! A sibling module rather than an integration test because the live-handler
//! cases reach private items.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

// The fields exist to shape the schema; the handlers never read them.
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct Person {
    #[schemars(range(min = 1, max = 120))]
    age: u32,
    #[schemars(length(min = 2))]
    name: String,
    #[schemars(regex(pattern = "^[a-z]+$"))]
    handle: String,
}

fn person() -> Value {
    json!({ "age": 30, "name": "Ada", "handle": "ada" })
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

/// A typed tool over `Person` and the number of times its handler ran.
fn person_tool() -> (Tool, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let tool = ToolBuilder::new("person")
        .handler(move |_input: Person| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(CallToolResult::text("ok"))
            }
        })
        .build();
    (tool, calls)
}

#[derive(Serialize, JsonSchema)]
struct Summary {
    count: u32,
    label: String,
}

#[tokio::test]
async fn output_schema_for_matches_the_schemars_schema() {
    let tool = ToolBuilder::new("summary")
        .output_schema_for::<Summary>()
        .handler(|()| async { Ok(CallToolResult::text("ok")) })
        .build();

    let expected = serde_json::to_value(schemars::schema_for!(Summary)).unwrap();
    let schema = tool.definition().output_schema.expect("output schema");
    assert_eq!(schema, expected);
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["count"].is_object());
}

#[tokio::test]
async fn output_schema_for_replaces_an_earlier_schema() {
    let tool = ToolBuilder::new("summary")
        .output_schema(json!({ "type": "object" }))
        .output_schema_for::<Summary>()
        .handler(|()| async { Ok(CallToolResult::text("ok")) })
        .build();

    let schema = tool.definition().output_schema.expect("output schema");
    assert_eq!(
        schema,
        serde_json::to_value(schemars::schema_for!(Summary)).unwrap()
    );
}

/// The switches are available across feature configurations.
#[tokio::test]
async fn opt_out_methods_build_with_or_without_the_feature() {
    let tool = ToolBuilder::new("either")
        .skip_input_validation()
        .skip_output_validation()
        .handler(|_input: Person| async { Ok(CallToolResult::text("ok")) })
        .build();
    let result = tool.call(with(person(), "age", json!(500))).await;
    assert!(!result.is_error);
}

/// Without the feature a constraint serde does not enforce reaches the handler.
#[cfg(not(feature = "schema-validation"))]
#[tokio::test]
async fn without_the_feature_arguments_are_not_schema_checked() {
    let (tool, calls) = person_tool();
    let result = tool.call(with(person(), "age", json!(500))).await;
    assert!(!result.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "schema-validation")]
mod enabled {
    use super::*;
    use crate::extract::RawArgs;

    async fn rejected(tool: &Tool, args: Value, calls: &AtomicUsize) -> String {
        let before = calls.load(Ordering::SeqCst);
        let result = tool.call(args).await;
        assert!(result.is_error, "expected an error result");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "the handler must not run for rejected arguments"
        );
        result.first_text().expect("error text").to_string()
    }

    #[tokio::test]
    async fn valid_arguments_reach_the_handler() {
        let (tool, calls) = person_tool();
        let result = tool.call(person()).await;
        assert!(!result.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn out_of_range_arguments_are_rejected_with_their_location() {
        let (tool, calls) = person_tool();

        let text = rejected(&tool, with(person(), "age", json!(200)), &calls).await;
        assert!(text.contains("/age"), "got: {text}");
        assert!(text.contains("maximum"), "got: {text}");
        assert!(text.contains("200"), "got: {text}");

        let text = rejected(&tool, with(person(), "age", json!(0)), &calls).await;
        assert!(text.contains("/age"), "got: {text}");
        assert!(text.contains("minimum"), "got: {text}");
    }

    #[tokio::test]
    async fn pattern_min_length_and_type_violations_are_rejected() {
        let (tool, calls) = person_tool();

        let text = rejected(&tool, with(person(), "handle", json!("Not Ok")), &calls).await;
        assert!(text.contains("/handle"), "got: {text}");
        assert!(text.contains("pattern"), "got: {text}");

        let text = rejected(&tool, with(person(), "name", json!("A")), &calls).await;
        assert!(text.contains("/name"), "got: {text}");
        assert!(text.contains("minLength"), "got: {text}");

        let text = rejected(&tool, with(person(), "age", json!("old")), &calls).await;
        assert!(text.contains("/age"), "got: {text}");
        assert!(text.contains("type"), "got: {text}");
    }

    #[tokio::test]
    async fn a_missing_required_property_is_reported_at_the_root() {
        let (tool, calls) = person_tool();
        let text = rejected(&tool, json!({ "age": 30 }), &calls).await;
        assert!(text.contains("(root)"), "got: {text}");
        assert!(text.contains("required"), "got: {text}");
        assert!(text.contains("name"), "got: {text}");
    }

    #[tokio::test]
    async fn omitted_arguments_are_checked_as_an_empty_object() {
        // `arguments` is optional in `tools/call` and arrives as null.
        let (tool, calls) = person_tool();
        let text = rejected(&tool, Value::Null, &calls).await;
        assert!(text.contains("required"), "got: {text}");

        // A tool that takes nothing is not rejected for having nothing.
        let none = ToolBuilder::new("none")
            .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
            .build();
        assert!(!none.call(Value::Null).await.is_error);
        let unit = ToolBuilder::new("unit")
            .handler(|()| async { Ok(CallToolResult::text("ok")) })
            .build();
        assert!(!unit.call(Value::Null).await.is_error);
    }

    #[tokio::test]
    async fn format_is_enforced() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let tool = ToolBuilder::new("contact")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "email": { "type": "string", "format": "email" },
                    "id": { "type": "string", "format": "uuid" },
                    "at": { "type": "string", "format": "date-time" }
                }
            }))
            .extractor_handler((), move |RawArgs(_): RawArgs| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(CallToolResult::text("ok"))
                }
            })
            .build();

        let text = rejected(&tool, json!({ "email": "nope" }), &calls).await;
        assert!(text.contains("/email"), "got: {text}");
        assert!(text.contains("format"), "got: {text}");
        rejected(&tool, json!({ "id": "1234" }), &calls).await;
        rejected(&tool, json!({ "at": "yesterday" }), &calls).await;

        let ok = tool
            .call(json!({
                "email": "ada@example.com",
                "id": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                "at": "2026-09-29T12:00:00Z"
            }))
            .await;
        assert!(!ok.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn raw_args_tools_with_an_explicit_schema_are_validated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let tool = ToolBuilder::new("query")
            .input_schema(json!({
                "type": "object",
                "properties": { "limit": { "type": "integer", "maximum": 10 } },
                "required": ["limit"]
            }))
            .extractor_handler((), move |RawArgs(args): RawArgs| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(CallToolResult::json(args))
                }
            })
            .build();

        let text = rejected(&tool, json!({ "limit": 50 }), &calls).await;
        assert!(text.contains("/limit"), "got: {text}");
        assert!(text.contains("maximum"), "got: {text}");
        rejected(&tool, json!({}), &calls).await;

        assert!(!tool.call(json!({ "limit": 5 })).await.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_input_schema_override_on_a_typed_handler_is_the_one_checked() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let tool = ToolBuilder::new("override")
            .input_schema(json!({
                "type": "object",
                "properties": { "name": { "type": "string", "maxLength": 3 } }
            }))
            .handler(move |_input: Person| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(CallToolResult::text("ok"))
                }
            })
            .build();

        let text = rejected(&tool, with(person(), "name", json!("Lovelace")), &calls).await;
        assert!(text.contains("/name"), "got: {text}");
        assert!(text.contains("maxLength"), "got: {text}");
    }

    struct Bounded;

    #[derive(Deserialize, JsonSchema)]
    struct BoundedInput {
        #[schemars(range(min = 0, max = 9))]
        digit: u32,
    }

    impl McpTool for Bounded {
        const NAME: &'static str = "bounded";
        const DESCRIPTION: &'static str = "a digit";
        type Input = BoundedInput;
        type Output = Value;

        async fn call(&self, input: Self::Input) -> Result<Self::Output> {
            Ok(json!({ "digit": input.digit }))
        }
    }

    #[tokio::test]
    async fn mcp_tool_implementations_are_validated() {
        let tool = Bounded.into_tool();
        let text = tool.call(json!({ "digit": 12 })).await;
        assert!(text.is_error);
        assert!(text.first_text().unwrap().contains("/digit"));
        assert!(!tool.call(json!({ "digit": 4 })).await.is_error);
    }

    #[tokio::test]
    async fn layered_and_guarded_tools_are_validated() {
        let (calls, layered) = {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let tool = ToolBuilder::new("layered")
                .handler(move |_input: Person| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(CallToolResult::text("ok"))
                    }
                })
                .guard(|_req: &ToolRequest| Ok(()))
                .build();
            (calls, tool)
        };
        rejected(&layered, with(person(), "age", json!(0)), &calls).await;
        assert!(!layered.call(person()).await.is_error);

        // A guard added to the built tool runs first, then validation.
        let (tool, calls) = person_tool();
        let guarded = tool.with_guard(|_req: &ToolRequest| Ok(()));
        rejected(&guarded, with(person(), "age", json!(0)), &calls).await;
        assert!(!guarded.call(person()).await.is_error);
    }

    #[tokio::test]
    async fn the_list_of_violations_is_capped() {
        let required: Vec<String> = (0..12).map(|n| format!("field{n}")).collect();
        let tool = ToolBuilder::new("wide")
            .input_schema(json!({ "type": "object", "required": required }))
            .extractor_handler((), |RawArgs(_): RawArgs| async {
                Ok(CallToolResult::text("ok"))
            })
            .build();

        let text = tool.call(json!({})).await.first_text().unwrap().to_string();
        assert_eq!(text.matches("- at ").count(), 5, "got: {text}");
        assert!(text.contains("and more violations"), "got: {text}");
    }

    #[tokio::test]
    async fn a_large_offending_value_does_not_bloat_the_message() {
        let (tool, calls) = person_tool();
        let huge = "X".repeat(50_000);
        let text = rejected(&tool, with(person(), "handle", json!(huge)), &calls).await;
        assert!(text.len() < 1_000, "message was {} bytes", text.len());
        assert!(text.contains("/handle"));
    }

    fn output_tool(result: Value) -> Tool {
        ToolBuilder::new("counted")
            .output_schema(json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "minimum": 0 } },
                "required": ["count"]
            }))
            .handler(move |_input: NoParams| {
                let result = result.clone();
                async move { Ok(CallToolResult::json(result)) }
            })
            .build()
    }

    #[tokio::test]
    async fn a_conforming_structured_result_passes_unchanged() {
        let tool = output_tool(json!({ "count": 3 }));
        let result = tool.call(json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!({ "count": 3 })));
    }

    #[tokio::test]
    async fn a_nonconforming_structured_result_becomes_an_error() {
        let tool = output_tool(json!({ "count": -1 }));
        let result = tool.call(json!({})).await;
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        let text = result.first_text().unwrap();
        assert!(text.contains("/count"), "got: {text}");
        assert!(text.contains("minimum"), "got: {text}");

        let text = output_tool(json!({ "count": "many" }))
            .call(json!({}))
            .await
            .first_text()
            .unwrap()
            .to_string();
        assert!(text.contains("/count"), "got: {text}");
        assert!(text.contains("type"), "got: {text}");
    }

    #[tokio::test]
    async fn without_an_output_schema_nothing_is_checked() {
        let tool = ToolBuilder::new("free")
            .handler(|_input: NoParams| async {
                Ok(CallToolResult::json(json!({ "anything": [1, "a"] })))
            })
            .build();
        let result = tool.call(json!({})).await;
        assert!(!result.is_error);
        assert!(result.structured_content.is_some());
    }

    #[tokio::test]
    async fn a_derived_output_schema_is_enforced() {
        let tool = ToolBuilder::new("summary")
            .output_schema_for::<Summary>()
            .handler(|_input: NoParams| async { Ok(CallToolResult::json(json!({ "count": "x" }))) })
            .build();
        let result = tool.call(json!({})).await;
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("/count"));
    }

    #[tokio::test]
    async fn a_handler_error_result_is_not_checked_against_the_output_schema() {
        let tool = ToolBuilder::new("failing")
            .output_schema_for::<Summary>()
            .handler(|_input: NoParams| async { Ok(CallToolResult::error("upstream unavailable")) })
            .build();
        let result = tool.call(json!({})).await;
        assert!(result.is_error);
        assert_eq!(result.first_text(), Some("upstream unavailable"));
    }

    #[tokio::test]
    async fn opting_out_disables_only_the_named_check() {
        let bad_input = with(person(), "age", json!(500));

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let no_input = ToolBuilder::new("no_input")
            .skip_input_validation()
            .handler(move |_input: Person| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(CallToolResult::text("ok"))
                }
            })
            .build();
        assert!(!no_input.call(bad_input.clone()).await.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Skipping input validation leaves output validation on.
        let bad_output = ToolBuilder::new("no_input_bad_output")
            .skip_input_validation()
            .output_schema_for::<Summary>()
            .handler(|_input: Person| async { Ok(CallToolResult::json(json!({ "count": "x" }))) })
            .build();
        assert!(bad_output.call(bad_input.clone()).await.is_error);

        let no_output = ToolBuilder::new("no_output")
            .skip_output_validation()
            .output_schema_for::<Summary>()
            .handler(|_input: Person| async { Ok(CallToolResult::json(json!({ "count": "x" }))) })
            .build();
        assert!(!no_output.call(person()).await.is_error);
        // Skipping output validation leaves input validation on.
        assert!(no_output.call(bad_input).await.is_error);

        let neither = ToolBuilder::new("neither")
            .skip_input_validation()
            .skip_output_validation()
            .output_schema_for::<Summary>()
            .handler(|_input: Person| async { Ok(CallToolResult::json(json!({ "count": "x" }))) })
            .build();
        assert!(
            !neither
                .call(with(person(), "age", json!(500)))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn a_schema_that_does_not_compile_is_left_unchecked() {
        let schema = json!({ "type": "object", "required": "not-a-list" });
        assert!(
            super::super::engine::ToolSchemas::compile("broken", Some(&schema), None).is_none(),
            "the schema is meant to be one that does not compile"
        );
        let tool = ToolBuilder::new("broken")
            .input_schema(schema)
            .extractor_handler((), |RawArgs(_): RawArgs| async {
                Ok(CallToolResult::text("ok"))
            })
            .build();
        // Building did not panic, and the tool still answers.
        assert!(!tool.call(json!({ "anything": true })).await.is_error);
    }

    #[test]
    fn remote_and_file_references_are_not_resolved() {
        for uri in ["https://example.com/schema.json", "file:///etc/schema.json"] {
            let schema = json!({
                "type": "object",
                "properties": { "a": { "$ref": uri } }
            });
            assert!(
                super::super::engine::ToolSchemas::compile("remote", Some(&schema), None).is_none(),
                "{uri} must not be fetched"
            );
        }
    }

    fn live_person_tool() -> Tool {
        ToolBuilder::new("live")
            .output_schema_for::<Summary>()
            .live_task_handler(|_task: TaskContext, input: Person| async move {
                let structured = if input.name == "bad-output" {
                    json!({ "count": "x" })
                } else {
                    json!({ "count": 1, "label": "ok" })
                };
                Ok(TaskOutcome::Completed(CallToolResult::json(structured)))
            })
            .build()
    }

    async fn run_live(tool: &Tool, args: Value) -> CallToolResult {
        let handler = tool.live_handler.clone().expect("live handler");
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
        match handler
            .call(ctx, TaskContext::new("task-1".to_string()), args)
            .await
            .expect("live handler result")
        {
            TaskOutcome::Completed(result) => result,
            other => panic!("expected a completed outcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_task_handlers_are_validated() {
        let tool = live_person_tool();

        let result = run_live(&tool, with(person(), "age", json!(500))).await;
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("/age"));

        let result = run_live(&tool, with(person(), "name", json!("bad-output"))).await;
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("/count"));

        let result = run_live(&tool, person()).await;
        assert!(!result.is_error);
        assert_eq!(
            result.structured_content,
            Some(json!({ "count": 1, "label": "ok" }))
        );
    }

    #[tokio::test]
    async fn a_live_tool_with_a_fallback_validates_both_paths() {
        let tool = ToolBuilder::new("live_fallback")
            .live_task_handler(|_task: TaskContext, _input: Person| async {
                Ok(TaskOutcome::Completed(CallToolResult::text("live")))
            })
            .fallback_handler(|_input: Person| async { Ok(CallToolResult::text("sync")) })
            .build();

        let result = tool.call(with(person(), "age", json!(500))).await;
        assert!(result.is_error);
        assert_eq!(tool.call(person()).await.first_text(), Some("sync"));
        assert!(
            run_live(&tool, with(person(), "age", json!(500)))
                .await
                .is_error
        );
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn mrtr_tools_are_validated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let tool = ToolBuilder::new("mrtr")
            .output_schema_for::<Summary>()
            .mrtr_handler(move |_ctx: RequestContext, input: Person| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let structured = if input.name == "bad-output" {
                        json!({ "count": "x" })
                    } else {
                        json!({ "count": 1, "label": "ok" })
                    };
                    Ok(RequestOutcome::Complete(CallToolResult::json(structured)))
                }
            })
            .build();

        let complete = |outcome: RequestOutcome<CallToolResult>| match outcome {
            RequestOutcome::Complete(result) => result,
            RequestOutcome::InputRequired(_) => panic!("unexpected input request"),
        };

        let result = complete(
            tool.call_outcome(with(person(), "age", json!(500)))
                .await
                .unwrap(),
        );
        assert!(result.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let result = complete(
            tool.call_outcome(with(person(), "name", json!("bad-output")))
                .await
                .unwrap(),
        );
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("/count"));

        let result = complete(tool.call_outcome(person()).await.unwrap());
        assert!(!result.is_error);
    }
}

fn shape_tool(result: CallToolResult) -> Tool {
    ToolBuilder::new("snapshots")
        .handler(move |()| {
            let result = result.clone();
            async move { Ok(result) }
        })
        .build()
}

fn assert_shape_error(result: &CallToolResult, kind: &str) {
    assert!(result.is_error);
    assert!(result.structured_content.is_none());
    assert_serialized_eq(
        result.first_text().unwrap(),
        format!(
            "Tool 'snapshots' returned structuredContent of type {kind}; MCP 2025-11-25 and earlier require a JSON object. Wrap a list with CallToolResult::from_list or return an object."
        ),
    );
}

#[tokio::test]
async fn an_array_structured_content_is_an_error_result_on_a_2025_request() {
    assert_shape_error(
        &shape_tool(CallToolResult::json(json!([1, 2])))
            .call(Value::Null)
            .await,
        "array",
    );
}

#[tokio::test]
async fn a_scalar_structured_content_is_an_error_result_on_a_2025_request() {
    for (value, kind) in [
        (json!("value"), "string"),
        (json!(42), "number"),
        (json!(true), "boolean"),
    ] {
        assert_shape_error(
            &shape_tool(CallToolResult::json(value))
                .call(Value::Null)
                .await,
            kind,
        );
    }
}

#[tokio::test]
async fn a_null_structured_content_is_an_error_result_on_a_2025_request() {
    assert_shape_error(
        &shape_tool(CallToolResult::json(Value::Null))
            .call(Value::Null)
            .await,
        "null",
    );
}

#[tokio::test]
async fn an_object_structured_content_passes_unchanged() {
    let expected = CallToolResult::json(json!({"items": [1, 2]}));
    assert_serialized_eq(
        shape_tool(expected.clone()).call(Value::Null).await,
        expected,
    );
}

#[tokio::test]
async fn a_result_without_structured_content_is_not_checked() {
    let expected = CallToolResult::text("ok");
    assert_serialized_eq(
        shape_tool(expected.clone()).call(Value::Null).await,
        expected,
    );
}

#[tokio::test]
async fn an_error_result_is_not_shape_checked() {
    let expected = CallToolResult {
        is_error: true,
        ..CallToolResult::json(json!([1, 2]))
    };
    assert_serialized_eq(
        shape_tool(expected.clone()).call(Value::Null).await,
        expected,
    );
}

#[tokio::test]
async fn a_hand_built_result_is_shape_checked() {
    let result = CallToolResult {
        structured_content: Some(json!([1, 2])),
        ..CallToolResult::text("list")
    };
    assert_shape_error(&shape_tool(result).call(Value::Null).await, "array");
}

#[tokio::test]
async fn an_mcp_tool_whose_output_serializes_to_an_array_is_an_error_result() {
    struct Snapshots;
    impl McpTool for Snapshots {
        const NAME: &'static str = "snapshots";
        const DESCRIPTION: &'static str = "List snapshots";
        type Input = NoParams;
        type Output = Vec<u32>;
        async fn call(&self, _input: Self::Input) -> Result<Self::Output> {
            Ok(vec![1, 2])
        }
    }
    assert_shape_error(&Snapshots.into_tool().call(json!({})).await, "array");
}

#[tokio::test]
async fn skip_output_validation_also_skips_the_shape_check() {
    let expected = CallToolResult::json(json!([1, 2]));
    let tool = ToolBuilder::new("snapshots")
        .skip_output_validation()
        .handler(|()| async { Ok(CallToolResult::json(json!([1, 2]))) })
        .build();
    assert_serialized_eq(tool.call(Value::Null).await, expected);
}

#[cfg(not(feature = "schema-validation"))]
#[tokio::test]
async fn the_shape_check_runs_without_the_schema_validation_feature() {
    assert_shape_error(
        &shape_tool(CallToolResult::json(json!([1, 2])))
            .call(Value::Null)
            .await,
        "array",
    );
}

#[tokio::test]
async fn a_tool_with_an_input_schema_but_no_output_schema_is_still_shape_checked() {
    let tool = ToolBuilder::new("snapshots")
        .input_schema(json!({"type": "object"}))
        .handler(|_args: Value| async { Ok(CallToolResult::json(json!([1, 2]))) })
        .build();
    assert_shape_error(&tool.call(json!({})).await, "array");
}

#[tokio::test]
async fn a_tool_without_compilable_schemas_is_still_shape_checked() {
    let tool = ToolBuilder::new("snapshots")
        .skip_input_validation()
        .output_schema(json!({"type": "invalid"}))
        .handler(|()| async { Ok(CallToolResult::json(json!([1, 2]))) })
        .build();
    assert_shape_error(&tool.call(Value::Null).await, "array");
}

#[cfg(feature = "stateless")]
fn final_context() -> RequestContext {
    let mut ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
    ctx.extensions_mut()
        .insert(crate::stateless::StatelessRequestMeta {
            protocol_version: Some(crate::protocol::PROTOCOL_VERSION_2026_07_28.to_string()),
            ..Default::default()
        });
    ctx
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_2026_07_28_request_keeps_an_array_structured_content() {
    let expected = CallToolResult::json(json!([1, 2]));
    assert_serialized_eq(
        shape_tool(expected.clone())
            .call_with_context(final_context(), Value::Null)
            .await,
        expected,
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_2026_07_28_request_keeps_a_scalar_structured_content() {
    for value in [json!("value"), json!(42), json!(true), Value::Null] {
        let expected = CallToolResult::json(value);
        assert_serialized_eq(
            shape_tool(expected.clone())
                .call_with_context(final_context(), Value::Null)
                .await,
            expected,
        );
    }
}

fn live_shape_tool() -> Tool {
    ToolBuilder::new("snapshots")
        .live_task_handler(|_task: TaskContext, ()| async {
            Ok(TaskOutcome::Completed(CallToolResult::json(json!([1, 2]))))
        })
        .build()
}

async fn call_live_shape_tool(ctx: RequestContext) -> CallToolResult {
    let handler = live_shape_tool().live_handler.unwrap();
    let TaskOutcome::Completed(result) = handler
        .call(ctx, TaskContext::new("task".to_string()), Value::Null)
        .await
        .unwrap()
    else {
        panic!("expected completion")
    };
    result
}

#[tokio::test]
async fn live_task_handlers_are_shape_checked() {
    let ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
    assert_shape_error(&call_live_shape_tool(ctx).await, "array");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_2026_07_28_live_task_keeps_an_array_structured_content() {
    assert_serialized_eq(
        call_live_shape_tool(final_context()).await,
        CallToolResult::json(json!([1, 2])),
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_replayed_final_request_keeps_arrays_without_transport_metadata_or_dropping_logs() {
    use crate::context::{ServerNotification, notification_channel};
    use crate::protocol::{LogLevel, LoggingMessageParams, RequestId};

    let (tx, mut rx) = notification_channel(1);
    let mut ctx = RequestContext::new(RequestId::Number(1)).with_notification_sender(tx);
    ctx.extensions_mut()
        .insert(crate::router::ReplayedFinalRequest);

    assert!(
        ctx.extension::<crate::stateless::StatelessRequestMeta>()
            .is_none()
    );
    assert!(ctx.per_request_meta().is_none());
    assert!(crate::router::is_final_protocol_request(ctx.extensions()));
    ctx.send_log(LoggingMessageParams::new(
        LogLevel::Debug,
        json!("replayed"),
    ));
    let ServerNotification::LogMessage(params) = rx.try_recv().expect("replay log delivered")
    else {
        panic!("expected log notification")
    };
    assert_eq!(params.level, LogLevel::Debug);
    assert_eq!(params.data, json!("replayed"));

    let expected = CallToolResult::json(json!([1, 2]));
    assert_serialized_eq(
        shape_tool(expected.clone())
            .call_with_context(ctx, Value::Null)
            .await,
        expected,
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_handlers_are_shape_checked() {
    let tool = ToolBuilder::new("snapshots")
        .mrtr_handler(|_ctx: RequestContext, ()| async {
            Ok(RequestOutcome::Complete(CallToolResult::json(json!([
                1, 2
            ]))))
        })
        .build();
    let RequestOutcome::Complete(result) = tool.call_outcome(Value::Null).await.unwrap() else {
        panic!("expected completion")
    };
    assert_shape_error(&result, "array");
    let RequestOutcome::Complete(result) = tool
        .call_outcome_with_context(final_context(), Value::Null)
        .await
        .unwrap()
    else {
        panic!("expected completion")
    };
    assert_serialized_eq(result, CallToolResult::json(json!([1, 2])));
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn an_mrtr_input_required_outcome_passes_through() {
    let expected = crate::protocol::InputRequiredResult::new().with_request_state("next");
    let tool = ToolBuilder::new("snapshots")
        .mrtr_handler(|_ctx: RequestContext, ()| async {
            Ok(RequestOutcome::InputRequired(
                crate::protocol::InputRequiredResult::new().with_request_state("next"),
            ))
        })
        .build();
    assert_serialized_eq(
        tool.call_outcome(Value::Null).await.unwrap(),
        RequestOutcome::<CallToolResult>::InputRequired(expected),
    );
    assert_serialized_eq(
        tool.mrtr_handler.unwrap().input_schema(),
        ensure_object_schema(serde_json::to_value(schemars::schema_for!(())).unwrap()),
    );
}

#[cfg(feature = "schema-validation")]
fn object_output_schema_tool() -> Tool {
    ToolBuilder::new("snapshots")
        .output_schema(json!({"type": "object"}))
        .handler(|()| async { Ok(CallToolResult::json(json!([1, 2]))) })
        .build()
}

#[cfg(feature = "schema-validation")]
#[tokio::test]
async fn the_shape_check_runs_before_the_output_schema_check() {
    assert_shape_error(
        &object_output_schema_tool().call(Value::Null).await,
        "array",
    );
}

#[cfg(all(feature = "schema-validation", feature = "stateless"))]
#[tokio::test]
async fn a_2026_07_28_request_still_checks_the_output_schema() {
    let result = object_output_schema_tool()
        .call_with_context(final_context(), Value::Null)
        .await;
    assert!(result.is_error);
    assert!(
        result
            .first_text()
            .unwrap()
            .contains("does not match its output schema")
    );
}

fn assert_serialized_eq(actual: impl Serialize, expected: impl Serialize) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}
