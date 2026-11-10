//! URL elicitation on the 2026-07-28 lifecycle (#1624): the final shape has no
//! `elicitationId`. Frames are read off the wire and validated against the
//! vendored final schema.

#![cfg(feature = "stateless")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
use tower_mcp::client::{ChannelTransport, ClientHandler, ClientTransport, McpClient};
use tower_mcp::protocol::{
    ClientCapabilities, ElicitAction, ElicitFinalUrlParams, ElicitRequestParams, ElicitResult,
    ElicitUrlParams, ElicitationCapability, ElicitationUrlCapability, InputRequest, InputRequests,
    InputRequiredResult, InputResponse, RequestOutcome, TaskStatus, TaskSupportMode,
};
use tower_mcp::{CallToolResult, JsonRpcError, McpRouter, ProtocolSupport, RequestContext};
use tower_mcp::{Tool, ToolBuilder};

const TIMEOUT: Duration = Duration::from_secs(5);
const SIGN_IN_URL: &str = "https://example.com/oauth/start";

fn final_params() -> ElicitFinalUrlParams {
    ElicitFinalUrlParams::new("Sign in", SIGN_IN_URL)
}

fn requests(entries: Vec<(&str, ElicitRequestParams)>) -> InputRequests {
    entries
        .into_iter()
        .map(|(key, params)| (key.to_string(), InputRequest::Elicit(params)))
        .collect()
}

/// A tool that asks for the given inputs until the client answers `signin`.
fn signin_tool(asks: InputRequests, tasks: bool) -> Tool {
    let builder = ToolBuilder::new("signin");
    let builder = if tasks {
        builder.task_support(TaskSupportMode::Optional)
    } else {
        builder
    };
    builder
        .mrtr_handler(move |ctx: RequestContext, _args: Value| {
            let asks = asks.clone();
            async move {
                if ctx
                    .input_responses()
                    .is_some_and(|responses| responses.contains_key("signin"))
                {
                    return Ok(RequestOutcome::Complete(CallToolResult::text("signed in")));
                }
                Ok(RequestOutcome::InputRequired(
                    InputRequiredResult::with_requests(asks),
                ))
            }
        })
        .build()
}

fn final_schema_validator(def: &str) -> jsonschema::Validator {
    let text =
        std::fs::read_to_string("../../tools/wire-schema-check/schemas/2026-07-28/schema.json")
            .unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    jsonschema::validator_for(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/$defs/{def}"),
        "$defs": schema["$defs"],
    }))
    .unwrap()
}

/// Send one `tools/call` on the wire and return the raw response frame.
async fn raw_call(router: McpRouter, capabilities: Value) -> Value {
    let mut transport = ChannelTransport::new(router);
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "signin",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "raw", "version": "1.0.0"},
                "io.modelcontextprotocol/clientCapabilities": capabilities,
            }
        }
    });
    tokio::time::timeout(TIMEOUT, async {
        transport.send(&request.to_string()).await.unwrap();
        loop {
            let line = transport.recv().await.unwrap().expect("transport closed");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id") == Some(&json!(1)) {
                return frame;
            }
        }
    })
    .await
    .expect("no response frame")
}

#[tokio::test]
async fn final_server_emits_schema_valid_url_elicitation_without_the_id() {
    let legacy = ElicitUrlParams {
        mode: None,
        elicitation_id: "legacy".to_string(),
        message: "Sign in".to_string(),
        url: SIGN_IN_URL.to_string(),
        meta: None,
    };
    let router = McpRouter::new().tool(signin_tool(
        requests(vec![
            ("final", ElicitRequestParams::FinalUrl(final_params())),
            ("legacy", ElicitRequestParams::Url(legacy)),
        ]),
        false,
    ));

    let frame = raw_call(router, json!({"elicitation": {"url": {}}})).await;
    let result = &frame["result"];
    assert_eq!(result["resultType"], "input_required", "{frame}");

    let errors: Vec<String> = final_schema_validator("InputRequiredResult")
        .iter_errors(result)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}: {result}");

    let request_validator = final_schema_validator("ElicitRequest");
    for key in ["final", "legacy"] {
        let request = &result["inputRequests"][key];
        assert!(request_validator.is_valid(request), "{request}");
        assert_eq!(
            request,
            &json!({
                "method": "elicitation/create",
                "params": {"mode": "url", "message": "Sign in", "url": SIGN_IN_URL}
            })
        );
    }
}

#[tokio::test]
async fn final_url_elicitation_requires_the_url_capability() {
    let router = McpRouter::new().tool(signin_tool(
        requests(vec![(
            "signin",
            ElicitRequestParams::FinalUrl(final_params()),
        )]),
        false,
    ));

    let frame = raw_call(router, json!({"elicitation": {"form": {}}})).await;
    assert_eq!(frame["error"]["code"], -32021, "{frame}");
    assert!(
        frame["error"]["data"]["requiredCapabilities"]["elicitation"]["url"].is_object(),
        "{frame}"
    );
}

#[derive(Clone, Default)]
struct RecordingHandler {
    seen: Arc<Mutex<Vec<ElicitRequestParams>>>,
}

#[async_trait]
impl ClientHandler for RecordingHandler {
    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        self.seen.lock().unwrap().push(params);
        Ok(accept())
    }
}

fn accept() -> ElicitResult {
    ElicitResult {
        action: ElicitAction::Accept,
        content: None,
        meta: None,
    }
}

fn url_capable() -> ClientCapabilities {
    ClientCapabilities {
        elicitation: Some(ElicitationCapability {
            url: Some(ElicitationUrlCapability::default()),
            ..ElicitationCapability::default()
        }),
        ..ClientCapabilities::default()
    }
}

#[tokio::test]
async fn client_fulfils_an_id_less_url_elicitation_from_a_final_server() {
    let router = McpRouter::new().tool(signin_tool(
        requests(vec![(
            "signin",
            ElicitRequestParams::FinalUrl(final_params()),
        )]),
        false,
    ));
    let handler = RecordingHandler::default();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_capabilities(url_capable())
        .connect(ChannelTransport::new(router), handler.clone())
        .await
        .unwrap();
    client.discover("final-url", "1.0.0").await.unwrap();

    let result = tokio::time::timeout(TIMEOUT, client.call_tool("signin", json!({})))
        .await
        .expect("call timed out")
        .unwrap();
    assert_eq!(result.all_text(), "signed in");

    let seen = handler.seen.lock().unwrap();
    let [ElicitRequestParams::FinalUrl(params)] = seen.as_slice() else {
        panic!("expected one final URL elicitation, got {seen:?}");
    };
    assert_eq!(params.url, SIGN_IN_URL);
}

#[tokio::test]
async fn task_input_path_carries_an_id_less_url_elicitation() {
    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .task_store(store.clone())
        .tool(signin_tool(
            requests(vec![(
                "signin",
                ElicitRequestParams::FinalUrl(final_params()),
            )]),
            true,
        ))
        .with_tasks();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_capabilities(url_capable())
        .with_tasks()
        .connect_simple(ChannelTransport::new(router))
        .await
        .unwrap();
    client.discover("final-url", "1.0.0").await.unwrap();

    let outcome = client
        .call_tool_once_task_aware("signin", json!({}), None, None)
        .await
        .unwrap();
    let tower_mcp::client::TaskAwareCallToolOutcome::Task(created) = outcome else {
        panic!("expected a task");
    };
    let id = created.task.metadata.task_id.clone();

    tokio::time::timeout(TIMEOUT, async {
        loop {
            let task = store.get_task(&id).await.unwrap().unwrap();
            assert_ne!(task.status, TaskStatus::Failed, "{task:?}");
            if task.status == TaskStatus::InputRequired {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task never asked for input");

    let detailed = serde_json::to_value(client.task_get_detailed(&id).await.unwrap()).unwrap();
    assert_eq!(
        detailed["inputRequests"]["signin"]["params"],
        json!({"mode": "url", "message": "Sign in", "url": SIGN_IN_URL}),
        "{detailed}"
    );

    client
        .task_update(
            &id,
            [("signin".to_string(), InputResponse::Elicit(accept()))]
                .into_iter()
                .collect(),
        )
        .await
        .unwrap();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let task = store.get_task(&id).await.unwrap().unwrap();
            if task.status == TaskStatus::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task never completed");
}
