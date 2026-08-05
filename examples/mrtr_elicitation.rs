//! Asking the client for input on both protocol lifecycles.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example mrtr_elicitation --features protocol-2026-07-28
//! ```
//!
//! A server frequently needs something from the user mid-call: a confirmation,
//! a missing field, a choice. How it asks depends on the lifecycle, and the
//! two mechanisms are not interchangeable.
//!
//! On **2025-11-25** the server initiates a JSON-RPC request:
//! [`RequestContext::elicit_form`] sends `elicitation/create` and awaits the
//! answer inline. The handler blocks; one call, one result.
//!
//! On **2026-07-28** there are no server-initiated requests at all. The schema
//! keeps `ElicitRequest` only as a member of `InputRequest`, carried inside an
//! `InputRequiredResult`. So the handler *returns* the request instead of
//! sending it, the client fulfils it, and the client calls the tool again with
//! the answers attached. That is SEP-2322, Multi Round-Trip Requests.
//!
//! Calling `elicit_form` on a 2026-07-28 request therefore fails, and the
//! error says so and points here. The two shapes are below, and
//! `confirm_deploy` serves both eras from one handler.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::context::RequestContext;
use tower_mcp::protocol::{
    ElicitFormParams, ElicitFormSchema, ElicitRequestParams, InputRequest, InputRequests,
    InputRequiredResult, RequestOutcome,
};
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct DeployInput {
    /// Which environment to deploy to.
    environment: String,
}

/// The key the server issues its request under, and reads the answer back
/// from. It is the server's own identifier: the client echoes it verbatim.
const CONFIRMATION: &str = "confirmation";

fn confirmation_form() -> ElicitFormParams {
    ElicitFormParams {
        mode: None,
        message: "Confirm the deploy?".to_string(),
        // Field order is preserved and is presentation-significant: the client
        // renders the form in the order declared here.
        requested_schema: ElicitFormSchema::new().string_field(
            "decision",
            Some("Type 'yes' to proceed"),
            true,
        ),
        meta: None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let confirm = ToolBuilder::new("confirm_deploy")
        .description("Deploy after confirming with the user")
        .mrtr_handler(|ctx: RequestContext, input: DeployInput| async move {
            // Retry leg: the client fulfilled the request and called again
            // with the answers. Read them by the key we issued.
            if let Some(responses) = ctx.input_responses()
                && let Some(answer) = responses.get(CONFIRMATION)
            {
                let accepted = matches!(
                    answer,
                    tower_mcp::protocol::InputResponse::Elicit(result)
                        if result.action == tower_mcp::protocol::ElicitAction::Accept
                );
                return Ok(RequestOutcome::Complete(CallToolResult::text(
                    if accepted {
                        format!("deployed to {}", input.environment)
                    } else {
                        "deploy declined".to_string()
                    },
                )));
            }

            // First leg. A 2025-11-25 client can be asked inline, because that
            // lifecycle permits server-initiated requests. `can_elicit()`
            // reports whether this request has that route, so the handler does
            // not need to inspect the protocol version itself.
            if ctx.can_elicit() {
                let result = ctx.elicit_form(confirmation_form()).await?;
                let accepted = result.action == tower_mcp::protocol::ElicitAction::Accept;
                return Ok(RequestOutcome::Complete(CallToolResult::text(
                    if accepted {
                        format!("deployed to {}", input.environment)
                    } else {
                        "deploy declined".to_string()
                    },
                )));
            }

            // 2026-07-28: return the request rather than sending it. The
            // client fulfils every entry in `inputRequests` and retries the
            // original call with the answers in `inputResponses`.
            //
            // `requestState` is an opaque blob echoed back on the retry. It is
            // the handler's own resumption state, and the client must not
            // interpret it. Sign or encrypt anything sensitive: it is a round
            // trip through an untrusted peer.
            let mut requests: InputRequests = BTreeMap::new();
            requests.insert(
                CONFIRMATION.to_string(),
                InputRequest::Elicit(ElicitRequestParams::Form(confirmation_form())),
            );

            Ok(RequestOutcome::input_required(
                InputRequiredResult::with_requests(requests)
                    .with_request_state(format!("env={}", input.environment)),
            ))
        })
        .build();

    let router = McpRouter::new()
        .server_info("mrtr-elicitation-example", env!("CARGO_PKG_VERSION"))
        .tool(confirm);

    StdioTransport::new(router).run().await?;
    Ok(())
}

// # Choosing between the two
//
// A handler that only ever serves 2025-11-25 can keep calling `elicit_form`
// and ignore all of this. A handler that serves 2026-07-28 at all must have
// the input-required path, because the inline call cannot work there.
//
// Branch on `ctx.can_elicit()` (or `can_sample()`), not on the protocol
// version: those report whether *this* request has a route back to the client,
// which is the question the code actually cares about, and they stay correct
// if a transport declines to provide a requester for its own reasons.
//
// # Why the round trip is not just a slower inline call
//
// The client re-invokes the tool from the top. The handler gets a fresh
// context and must reconstruct whatever it had before asking, either by
// recomputing from the arguments (which the client resends unchanged) or by
// carrying it in `requestState`. Anything held only in a local variable across
// the `await` in the inline version is gone in the MRTR version.
//
// This is also why a single handler can issue several requests at once: fill
// `inputRequests` with every key it needs, and the client answers all of them
// before the retry, rather than one round trip per question.
