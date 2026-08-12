//! Error semantics do not depend on registration style (#1280).
//!
//! Before this fix, a fixed `Resource` handler's `Err` was flattened into
//! successful resource content, while a `ResourceTemplate` handler's `Err`
//! propagated as a JSON-RPC error: same request shape, different outcome
//! depending on which kind of resource matched. A `Prompt` had the same
//! asymmetry along a different axis: unlayered, an `Err` propagated;
//! layered, the same `Err` became a successful `prompts/get` whose
//! assistant message carried the error text.
//!
//! Both had one cause: `ResourceCatchError` and `PromptCatchError` had
//! nowhere to put an `Err` except the response, so it became content. The
//! fix makes the boxed service's success value a `Result<_, JsonRpcError>`,
//! converts the handler's own error to a structured `JsonRpcError` before
//! any layer runs, and sanitizes only a genuine middleware failure to
//! `-32603`. These tests exercise that through the router, the way a real
//! client sees it.

use std::collections::HashMap;
use std::time::Duration;

use tower::Layer;
use tower::timeout::TimeoutLayer;
use tower_service::Service;

use tower_mcp::client::{ChannelTransport, McpClient};
use tower_mcp::error::{Error, ErrorCode, JsonRpcError};
use tower_mcp::protocol::ReadResourceResult;
use tower_mcp::resource::ResourceRequest;
use tower_mcp::{GetPromptResult, McpRouter, PromptBuilder, ResourceBuilder, ResourceTemplate};

async fn connected_client(router: McpRouter) -> McpClient {
    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client
        .initialize("test-client", "1.0.0")
        .await
        .expect("initialize");
    client
}

/// Unwrap a `tower_mcp::Result` that is expected to be a structured
/// JSON-RPC error, and hand back the error itself so a test can assert on
/// its code and message directly rather than string-matching a rendered
/// `Display`.
fn expect_json_rpc_error<T: std::fmt::Debug>(result: tower_mcp::Result<T>) -> JsonRpcError {
    match result {
        Ok(value) => panic!("expected a JSON-RPC error, got Ok({value:?})"),
        Err(Error::JsonRpc(err)) => err,
        Err(other) => panic!("expected Error::JsonRpc, got {other:?}"),
    }
}

// =============================================================================
// 1. A fixed resource and a template agree on error shape.
// =============================================================================

#[tokio::test]
async fn a_fixed_resource_and_a_template_agree_on_error_shape() {
    let resource = ResourceBuilder::new("boom://fixed")
        .name("fixed")
        .handler(|| async { Err(Error::internal("boom")) })
        .build();
    let template = ResourceTemplate::builder("boom://template/{id}")
        .name("template")
        .handler(|_uri: String, _vars: HashMap<String, String>| async move {
            Err(Error::internal("boom"))
        });

    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource)
        .resource_template(template);
    let client = connected_client(router).await;

    let fixed_err = expect_json_rpc_error(client.read_resource("boom://fixed").await);
    let template_err = expect_json_rpc_error(client.read_resource("boom://template/42").await);

    assert_eq!(fixed_err.code, template_err.code);
    assert!(fixed_err.message.contains("boom"), "{}", fixed_err.message);
    assert!(
        template_err.message.contains("boom"),
        "{}",
        template_err.message
    );
}

// =============================================================================
// 2. An unlayered prompt and a layered prompt agree on error shape.
// =============================================================================

#[tokio::test]
async fn an_unlayered_and_a_layered_prompt_agree_on_error_shape() {
    let unlayered = PromptBuilder::new("unlayered")
        .handler(|_: HashMap<String, String>| async { Err(Error::internal("boom")) })
        .build();
    // A generous timeout that never fires: any difference between this and
    // the unlayered prompt comes from layering itself, not from an
    // unrelated timeout.
    let layered = PromptBuilder::new("layered")
        .handler(|_: HashMap<String, String>| async { Err(Error::internal("boom")) })
        .layer(TimeoutLayer::new(Duration::from_secs(30)));

    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .prompt(unlayered)
        .prompt(layered);
    let client = connected_client(router).await;

    let unlayered_err = expect_json_rpc_error(client.get_prompt("unlayered", None).await);
    let layered_err = expect_json_rpc_error(client.get_prompt("layered", None).await);

    assert_eq!(unlayered_err.code, layered_err.code);
    assert!(
        unlayered_err.message.contains("boom"),
        "{}",
        unlayered_err.message
    );
    assert!(
        layered_err.message.contains("boom"),
        "{}",
        layered_err.message
    );
}

// =============================================================================
// 3. A structured error survives a handler intact, resource and prompt.
// =============================================================================

#[tokio::test]
async fn a_structured_error_survives_from_a_resource_handler() {
    let resource = ResourceBuilder::new("structured://resource")
        .name("structured")
        .handler(|| async { Err(Error::invalid_params("bad shape")) })
        .build();
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource);
    let client = connected_client(router).await;

    let error = expect_json_rpc_error(client.read_resource("structured://resource").await);
    assert_eq!(error.code, ErrorCode::InvalidParams.code());
    assert!(error.message.contains("bad shape"), "{}", error.message);
}

#[tokio::test]
async fn a_structured_error_survives_from_a_prompt_handler() {
    let prompt = PromptBuilder::new("structured")
        .handler(|_: HashMap<String, String>| async { Err(Error::invalid_params("bad shape")) })
        .build();
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .prompt(prompt);
    let client = connected_client(router).await;

    let error = expect_json_rpc_error(client.get_prompt("structured", None).await);
    assert_eq!(error.code, ErrorCode::InvalidParams.code());
    assert!(error.message.contains("bad shape"), "{}", error.message);
}

// =============================================================================
// 4. An opaque middleware failure is sanitized, and its Debug never leaks.
// =============================================================================

/// An error deliberately shaped so its `Display` (what a sanitized JSON-RPC
/// message may show) and its `Debug` (what only a log should show) differ.
/// Only the `Display` text may ever reach a client.
struct SecretLeakError;

impl std::fmt::Debug for SecretLeakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SecretLeakError {{ internal_token: \"sk-supersecret\" }}"
        )
    }
}

impl std::fmt::Display for SecretLeakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream dependency unavailable")
    }
}

impl std::error::Error for SecretLeakError {}

/// A layer that always fails with [`SecretLeakError`], regardless of what
/// the wrapped resource handler would have done. This is unambiguously a
/// middleware failure, not a handler failure, since the handler never runs.
#[derive(Clone)]
struct AlwaysFailResourceLayer;

impl<S> Layer<S> for AlwaysFailResourceLayer {
    type Service = AlwaysFailResourceService;
    fn layer(&self, _inner: S) -> Self::Service {
        AlwaysFailResourceService
    }
}

#[derive(Clone)]
struct AlwaysFailResourceService;

impl Service<ResourceRequest> for AlwaysFailResourceService {
    type Response = std::result::Result<ReadResourceResult, JsonRpcError>;
    type Error = SecretLeakError;
    type Future = std::future::Ready<std::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: ResourceRequest) -> Self::Future {
        std::future::ready(Err(SecretLeakError))
    }
}

/// The prompt-side equivalent of [`AlwaysFailResourceLayer`].
#[derive(Clone)]
struct AlwaysFailPromptLayer;

impl<S> Layer<S> for AlwaysFailPromptLayer {
    type Service = AlwaysFailPromptService;
    fn layer(&self, _inner: S) -> Self::Service {
        AlwaysFailPromptService
    }
}

#[derive(Clone)]
struct AlwaysFailPromptService;

impl Service<tower_mcp::prompt::PromptRequest> for AlwaysFailPromptService {
    type Response = std::result::Result<GetPromptResult, JsonRpcError>;
    type Error = SecretLeakError;
    type Future = std::future::Ready<std::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: tower_mcp::prompt::PromptRequest) -> Self::Future {
        std::future::ready(Err(SecretLeakError))
    }
}

#[tokio::test]
async fn an_opaque_middleware_failure_on_a_resource_is_sanitized() {
    let resource = ResourceBuilder::new("opaque://resource")
        .name("opaque")
        .handler(|| async { Ok(ReadResourceResult::text("opaque://resource", "unused")) })
        .layer(AlwaysFailResourceLayer)
        .build();
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource);
    let client = connected_client(router).await;

    let error = expect_json_rpc_error(client.read_resource("opaque://resource").await);
    assert_eq!(error.code, ErrorCode::InternalError.code());
    assert!(
        error.message.contains("upstream dependency unavailable"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("sk-supersecret"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("internal_token"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn an_opaque_middleware_failure_on_a_prompt_is_sanitized() {
    let prompt = PromptBuilder::new("opaque")
        .handler(|_: HashMap<String, String>| async { Ok(GetPromptResult::user_message("unused")) })
        .layer(AlwaysFailPromptLayer);
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .prompt(prompt);
    let client = connected_client(router).await;

    let error = expect_json_rpc_error(client.get_prompt("opaque", None).await);
    assert_eq!(error.code, ErrorCode::InternalError.code());
    assert!(
        error.message.contains("upstream dependency unavailable"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("sk-supersecret"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("internal_token"),
        "{}",
        error.message
    );
}

// =============================================================================
// 5. Ok(...) content that merely mentions "error" is still a success.
// =============================================================================

#[tokio::test]
async fn ok_resource_content_mentioning_error_is_still_a_success() {
    let resource = ResourceBuilder::new("safe://resource")
        .name("safe")
        .handler(|| async {
            Ok(ReadResourceResult::text(
                "safe://resource",
                "an error occurred upstream, retry later",
            ))
        })
        .build();
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource);
    let client = connected_client(router).await;

    let result = client
        .read_resource("safe://resource")
        .await
        .expect("content that merely mentions \"error\" must not become a JSON-RPC error");
    assert_eq!(
        result.contents[0].text.as_deref(),
        Some("an error occurred upstream, retry later")
    );
}

#[tokio::test]
async fn ok_prompt_content_mentioning_error_is_still_a_success() {
    let prompt = PromptBuilder::new("safe")
        .handler(|_: HashMap<String, String>| async {
            Ok(GetPromptResult::user_message(
                "this describes an error condition",
            ))
        })
        .build();
    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .prompt(prompt);
    let client = connected_client(router).await;

    let result = client
        .get_prompt("safe", None)
        .await
        .expect("content that merely mentions \"error\" must not become a JSON-RPC error");
    assert_eq!(
        result.first_message_text(),
        Some("this describes an error condition")
    );
}

// =============================================================================
// 6. MRTR resource and prompt handlers, plain and layered.
// =============================================================================

#[cfg(feature = "stateless")]
#[tokio::test]
async fn mrtr_resource_and_prompt_preserve_handler_errors() {
    let resource = ResourceBuilder::new("mrtr://resource")
        .mrtr_handler(|_ctx| async move { Err(Error::internal("mrtr boom")) })
        .build();
    let prompt = PromptBuilder::new("mrtr")
        .mrtr_handler(|_ctx, _args| async move { Err(Error::internal("mrtr boom")) })
        .build();

    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource)
        .prompt(prompt);
    let client = connected_client(router).await;

    let resource_err = expect_json_rpc_error(client.read_resource("mrtr://resource").await);
    let prompt_err = expect_json_rpc_error(client.get_prompt("mrtr", None).await);

    assert!(
        resource_err.message.contains("mrtr boom"),
        "{}",
        resource_err.message
    );
    assert!(
        prompt_err.message.contains("mrtr boom"),
        "{}",
        prompt_err.message
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn layered_mrtr_resource_and_prompt_preserve_handler_errors() {
    // A generous timeout that never fires, same reasoning as test 2: any
    // observed difference from the unlayered MRTR case would come from
    // layering, not from an unrelated timeout.
    let resource = ResourceBuilder::new("mrtr-layered://resource")
        .mrtr_handler(|_ctx| async move { Err(Error::internal("mrtr boom")) })
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build();
    let prompt = PromptBuilder::new("mrtr-layered")
        .mrtr_handler(|_ctx, _args| async move { Err(Error::internal("mrtr boom")) })
        .layer(TimeoutLayer::new(Duration::from_secs(30)));

    let router = McpRouter::new()
        .server_info("error-semantics", "1.0.0")
        .resource(resource)
        .prompt(prompt);
    let client = connected_client(router).await;

    let resource_err = expect_json_rpc_error(client.read_resource("mrtr-layered://resource").await);
    let prompt_err = expect_json_rpc_error(client.get_prompt("mrtr-layered", None).await);

    assert!(
        resource_err.message.contains("mrtr boom"),
        "{}",
        resource_err.message
    );
    assert!(
        prompt_err.message.contains("mrtr boom"),
        "{}",
        prompt_err.message
    );
}
