//! Tests for the `host_mediated_input` example: the end-to-end paths on both
//! lifecycles, then the bridge's boundaries (rejection, deadlines, caps,
//! cancellation) including a scripted 2025-11-25 server that can cancel its
//! own requests.

use super::*;

use std::sync::Mutex as StdMutex;

use tokio::time::timeout;
use tower_mcp::{ClientTransport, Tool};

/// Upper bound on any single wait, so a regression fails instead of hanging.
const STEP: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(150);

fn bridge(deadline: Duration) -> (HostBridge, HostInbox) {
    HostBridge::new(8, 32, deadline)
}

fn form_question() -> Question {
    let form = approval_form("Approve?");
    Question::Form {
        message: form.message,
        schema: form.requested_schema,
    }
}

fn approve() -> Answer {
    Answer::FormAccept(HashMap::from([(
        "approver".to_string(),
        ElicitFieldValue::String("ada".to_string()),
    )]))
}

fn sampled() -> Answer {
    Answer::Sampled(Box::new(stub_model(&summary_request("x"))))
}

async fn next_event(inbox: &mut HostInbox) -> HostEvent {
    timeout(STEP, inbox.recv())
        .await
        .expect("no host event in time")
        .expect("bridge closed")
}

async fn next_ask(inbox: &mut HostInbox) -> HostRequest {
    match next_event(inbox).await {
        HostEvent::Ask(request) => request,
        other => panic!("expected a question, got {other:?}"),
    }
}

async fn expect_withdrawn(inbox: &mut HostInbox, id: InputId, reason: Unanswered) {
    match next_event(inbox).await {
        HostEvent::Withdrawn {
            id: got,
            reason: why,
        } => assert_eq!((got, why), (id, reason)),
        other => panic!("expected a withdrawal, got {other:?}"),
    }
}

async fn stable(deadline: Duration) -> (McpClient, HostInbox) {
    let (bridge, inbox) = bridge(deadline);
    (
        connect_stable(stable_router(), bridge).await.unwrap(),
        inbox,
    )
}

async fn final_client(router: McpRouter) -> (McpClient, HostBridge, HostInbox) {
    let (bridge, inbox) = bridge(LONG);
    (connect_final(router).await.unwrap(), bridge, inbox)
}

/// Call `tool` on a stable client while the host answers its one question.
async fn call_answering(
    client: &McpClient,
    inbox: &mut HostInbox,
    tool: &str,
    arguments: Value,
    answer: Answer,
) -> (Question, String) {
    let (result, question) = timeout(STEP, async {
        tokio::join!(client.call_tool(tool, arguments), async {
            let request = next_ask(inbox).await;
            inbox.submit(request.id, answer).unwrap();
            request.question
        })
    })
    .await
    .expect("call did not finish");
    (question, result.unwrap().all_text())
}

// --- 2025-11-25 end to end -------------------------------------------------

#[tokio::test]
async fn form_accept_decline_and_cancel_reach_the_server() {
    let (client, mut inbox) = stable(LONG).await;
    let prod = || json!({"environment": "prod"});

    let (question, text) = call_answering(&client, &mut inbox, "deploy", prod(), approve()).await;
    assert_eq!(text, "deployed to prod");
    let Question::Form { schema, .. } = question else {
        panic!("expected a form, got {question:?}");
    };
    assert_eq!(schema.required, ["approver"]);

    let (_, text) = call_answering(&client, &mut inbox, "deploy", prod(), Answer::Decline).await;
    assert_eq!(text, "not deployed to prod: decline");
    let (_, text) = call_answering(&client, &mut inbox, "deploy", prod(), Answer::Cancel).await;
    assert_eq!(text, "not deployed to prod: cancel");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn url_acceptance_is_an_explicit_host_assertion() {
    let (client, mut inbox) = stable(LONG).await;
    let (result, ()) = timeout(STEP, async {
        tokio::join!(client.call_tool("sign_in", json!({})), async {
            let request = next_ask(&mut inbox).await;
            let Question::Url { elicitation_id, .. } = &request.question else {
                panic!("expected a URL question, got {:?}", request.question);
            };
            assert_eq!(elicitation_id.as_deref(), Some("sign-in-1"));
            // A form submission is not URL acceptance; the question stays
            // pending until the host states what the user did.
            assert_eq!(
                inbox.submit(request.id, approve()),
                Err(SubmitError::WrongKind)
            );
            inbox.submit(request.id, Answer::UrlAcceptedByUser).unwrap();
        })
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().all_text(), "URL flow confirmed");

    let (_, text) =
        call_answering(&client, &mut inbox, "sign_in", json!({}), Answer::Decline).await;
    assert_eq!(text, "URL flow not confirmed: decline");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn sampling_is_answered_by_the_host_without_a_provider() {
    let (client, mut inbox) = stable(LONG).await;
    let text = || json!({"text": "Tower composes services."});
    let (question, result) =
        call_answering(&client, &mut inbox, "summarize", text(), sampled()).await;
    assert_eq!(question.kind(), "sampling");
    assert_eq!(result, "summary received");
    let (_, result) =
        call_answering(&client, &mut inbox, "summarize", text(), Answer::Decline).await;
    assert_eq!(result, "summary unavailable");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_questions_are_answered_out_of_order() {
    let (client, mut inbox) = stable(LONG).await;
    let host = async {
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(next_ask(&mut inbox).await);
        }
        for request in held.into_iter().rev() {
            let answer = match request.question {
                Question::Form { .. } => approve(),
                Question::Url { .. } => Answer::Decline,
                Question::Sample { .. } => sampled(),
            };
            inbox.submit(request.id, answer).unwrap();
        }
    };
    let (deploy, sign_in, summary, ()) = timeout(STEP, async {
        tokio::join!(
            client.call_tool("deploy", json!({"environment": "qa"})),
            client.call_tool("sign_in", json!({})),
            client.call_tool("summarize", json!({"text": "t"})),
            host,
        )
    })
    .await
    .unwrap();
    assert_eq!(deploy.unwrap().all_text(), "deployed to qa");
    assert_eq!(
        sign_in.unwrap().all_text(),
        "URL flow not confirmed: decline"
    );
    assert_eq!(summary.unwrap().all_text(), "summary received");
    client.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_expired_question_answers_cancel_and_rejects_the_late_answer() {
    let (client, mut inbox) = stable(SHORT).await;
    let (result, request) = timeout(STEP, async {
        tokio::join!(
            client.call_tool("deploy", json!({"environment": "prod"})),
            next_ask(&mut inbox),
        )
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().all_text(), "not deployed to prod: cancel");
    expect_withdrawn(&mut inbox, request.id, Unanswered::Expired).await;
    assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_inbox_releases_pending_and_later_questions() {
    let (client, mut inbox) = stable(LONG).await;
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            client.call_tool("deploy", json!({"environment": "prod"})),
            async move {
                next_ask(&mut inbox).await;
                drop(inbox);
            },
        )
    })
    .await
    .expect("a vanished host must not hold the call until the deadline");
    assert_eq!(result.unwrap().all_text(), "not deployed to prod: cancel");
    let later = timeout(STEP, client.call_tool("summarize", json!({"text": "t"})))
        .await
        .unwrap();
    assert_eq!(later.unwrap().all_text(), "summary unavailable");
    client.shutdown().await.unwrap();
}

// --- the bridge's boundaries ------------------------------------------------

#[tokio::test]
async fn unknown_duplicate_and_mismatched_answers_are_rejected() {
    let (bridge, mut inbox) = bridge(LONG);
    let (other, mut other_inbox) = super::tests::bridge(LONG);
    let asking = tokio::spawn({
        let bridge = bridge.clone();
        async move { bridge.ask(form_question(), Stop::never()).await }
    });
    let foreign = tokio::spawn(async move { other.ask(form_question(), Stop::never()).await });
    let request = next_ask(&mut inbox).await;
    let foreign_request = next_ask(&mut other_inbox).await;

    // An id from another bridge, and one this bridge never issued.
    assert_eq!(
        inbox.submit(foreign_request.id, Answer::Decline),
        Err(SubmitError::Unknown)
    );
    let never_issued = InputId {
        seq: 9_999,
        ..request.id
    };
    assert_eq!(
        inbox.submit(never_issued, Answer::Decline),
        Err(SubmitError::Unknown)
    );

    // Mismatched answers leave the question pending.
    for wrong in [Answer::UrlAcceptedByUser, sampled()] {
        assert_eq!(inbox.submit(request.id, wrong), Err(SubmitError::WrongKind));
    }

    inbox.submit(request.id, approve()).unwrap();
    assert!(matches!(
        asking.await.unwrap(),
        Ok(Answer::FormAccept(content)) if content.contains_key("approver")
    ));
    assert_eq!(
        inbox.submit(request.id, Answer::Decline),
        Err(SubmitError::Duplicate)
    );

    other_inbox
        .submit(foreign_request.id, Answer::Cancel)
        .unwrap();
    assert!(matches!(foreign.await.unwrap(), Ok(Answer::Cancel)));
}

/// The asker is a future this test holds and does not poll, so it cannot
/// wake between the deadline and the submission.
#[tokio::test(start_paused = true)]
async fn an_answer_after_the_deadline_is_stale_before_the_asker_wakes() {
    let (bridge, mut inbox) = bridge(SHORT);
    let ask = bridge.ask(form_question(), Stop::never());
    tokio::pin!(ask);
    assert!(futures::poll!(&mut ask).is_pending());
    let request = next_ask(&mut inbox).await;

    tokio::time::advance(SHORT).await;
    assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
    assert_eq!(ask.await.unwrap_err(), Unanswered::Expired);
    expect_withdrawn(&mut inbox, request.id, Unanswered::Expired).await;
}

#[tokio::test]
async fn an_answer_after_stop_is_stale_before_the_asker_wakes() {
    let (bridge, mut inbox) = bridge(LONG);
    let flag = StopFlag::default();
    let (check, waiter) = (flag.clone(), flag.clone());
    let stop = Stop::new(move || check.is_set(), async move { waiter.wait().await });
    let ask = bridge.ask(form_question(), stop);
    tokio::pin!(ask);
    assert!(futures::poll!(&mut ask).is_pending());
    let request = next_ask(&mut inbox).await;

    flag.set();
    assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
    assert_eq!(ask.await.unwrap_err(), Unanswered::Cancelled);
    expect_withdrawn(&mut inbox, request.id, Unanswered::Cancelled).await;
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_holds_questions_back_until_their_deadline() {
    let (bridge, mut inbox) = HostBridge::new(1, 32, SHORT);
    // Nobody reads the inbox. The first question takes the only slot; the
    // second waits for space and never gets it.
    let (first, second) = tokio::join!(
        bridge.ask(form_question(), Stop::never()),
        bridge.ask(form_question(), Stop::never()),
    );
    assert_eq!(first.unwrap_err(), Unanswered::Expired);
    assert_eq!(second.unwrap_err(), Unanswered::Expired);

    // Both producers are done, so the queue holds everything it ever will.
    let Ok(HostEvent::Ask(delivered)) = inbox.events.try_recv() else {
        panic!("the first question should be queued");
    };
    assert!(
        inbox.events.try_recv().is_err(),
        "the held-back question, and the withdrawal that found the queue full, never arrive"
    );
    assert_eq!(
        inbox.submit(delivered.id, Answer::Decline),
        Err(SubmitError::Stale)
    );
}

#[tokio::test]
async fn pending_questions_are_capped_before_anything_is_queued() {
    let (bridge, mut inbox) = HostBridge::new(8, 2, LONG);
    let spawn_ask = || {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.ask(form_question(), Stop::never()).await })
    };
    // Each question is received before the next is spawned, so ids and
    // handles pair up.
    let first = spawn_ask();
    let first_request = next_ask(&mut inbox).await;
    let second = spawn_ask();
    let second_request = next_ask(&mut inbox).await;

    // At the cap, the third is refused at once and never reaches the host,
    // even though the queue has room.
    let third = timeout(STEP, bridge.ask(form_question(), Stop::never())).await;
    assert_eq!(third.unwrap().unwrap_err(), Unanswered::Busy);
    assert!(inbox.events.try_recv().is_err());

    // Answering one frees a place.
    inbox.submit(first_request.id, Answer::Decline).unwrap();
    let first = timeout(STEP, first).await.unwrap().unwrap();
    assert!(matches!(first, Ok(Answer::Decline)));
    let fourth = spawn_ask();
    let fourth_request = next_ask(&mut inbox).await;
    for (handle, request) in [(second, second_request), (fourth, fourth_request)] {
        inbox.submit(request.id, Answer::Cancel).unwrap();
        let answer = timeout(STEP, handle).await.unwrap().unwrap();
        assert!(matches!(answer, Ok(Answer::Cancel)));
    }
}

#[tokio::test]
async fn dropping_an_ask_withdraws_its_question() {
    let (bridge, mut inbox) = bridge(LONG);
    let asking = tokio::spawn({
        let bridge = bridge.clone();
        async move { bridge.ask(form_question(), Stop::never()).await }
    });
    let request = next_ask(&mut inbox).await;
    asking.abort();
    expect_withdrawn(&mut inbox, request.id, Unanswered::Cancelled).await;
    assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
}

// --- server cancellation and shutdown (scripted 2025-11-25 server) --------

/// A server played by the test, so it can cancel its own elicitation, which
/// no in-repo server does.
struct ScriptedTransport {
    inbound: mpsc::Receiver<String>,
    inject: mpsc::Sender<String>,
    outbound: mpsc::UnboundedSender<Value>,
    open: bool,
}

struct ScriptedServer {
    inject: mpsc::Sender<String>,
    /// Closes when the client drops the transport, so draining it after
    /// `shutdown()` yields every frame the client ever sent.
    outbound: mpsc::UnboundedReceiver<Value>,
}

#[async_trait]
impl ClientTransport for ScriptedTransport {
    async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
        let frame: Value = serde_json::from_str(message)
            .map_err(|error| tower_mcp::Error::Transport(error.to_string()))?;
        if frame["method"] == "initialize" {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": {"name": "scripted", "version": "0.0.0"}
                }
            });
            let _ = self.inject.send(reply.to_string()).await;
        }
        let _ = self.outbound.send(frame);
        Ok(())
    }

    async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
        Ok(self.inbound.recv().await)
    }

    fn is_connected(&self) -> bool {
        self.open
    }

    async fn close(&mut self) -> tower_mcp::Result<()> {
        self.open = false;
        Ok(())
    }
}

impl ScriptedServer {
    async fn send(&self, frame: Value) {
        self.inject.send(frame.to_string()).await.unwrap();
    }

    async fn elicit(&self, id: i64) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": format!("request {id}"),
                "requestedSchema": {"type": "object", "properties": {}}
            }
        }))
        .await;
    }

    async fn cancel(&self, id: i64) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id}
        }))
        .await;
    }

    /// The client's reply to `id`, recording every frame seen on the way.
    async fn reply_to(&mut self, id: i64, seen: &mut Vec<Value>) -> Value {
        timeout(STEP, async {
            loop {
                let frame = self.outbound.recv().await.expect("client gone");
                seen.push(frame.clone());
                if frame["id"] == json!(id) && frame.get("method").is_none() {
                    return frame;
                }
            }
        })
        .await
        .expect("no reply in time")
    }

    /// Everything else the client sent, up to the transport being dropped.
    async fn drain(&mut self, seen: &mut Vec<Value>) {
        while let Some(frame) = timeout(STEP, self.outbound.recv()).await.unwrap() {
            seen.push(frame);
        }
    }
}

/// [`HostHandler`], plus a signal each time a handler call has returned.
struct Recording {
    inner: HostHandler,
    returned: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl ClientHandler for Recording {
    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        let message = match &params {
            ElicitRequestParams::Form(form) => form.message.clone(),
            _ => String::new(),
        };
        let result = self.inner.handle_elicit(params).await;
        let _ = self.returned.send(message);
        result
    }
}

struct Scripted {
    client: McpClient,
    inbox: HostInbox,
    server: ScriptedServer,
    returned: mpsc::UnboundedReceiver<String>,
}

async fn scripted() -> Scripted {
    let (bridge, inbox) = bridge(LONG);
    let (inject, inbound) = mpsc::channel(32);
    let (outbound_tx, outbound) = mpsc::unbounded_channel();
    let transport = ScriptedTransport {
        inbound,
        inject: inject.clone(),
        outbound: outbound_tx,
        open: true,
    };
    let (returned_tx, returned) = mpsc::unbounded_channel();
    let handler = Recording {
        inner: HostHandler::new(bridge),
        returned: returned_tx,
    };
    let client = McpClient::builder()
        .with_capabilities(host_capabilities())
        .connect(transport, handler)
        .await
        .unwrap();
    client.initialize("scripted", "1.0.0").await.unwrap();
    Scripted {
        client,
        inbox,
        server: ScriptedServer { inject, outbound },
        returned,
    }
}

fn message_of(request: &HostRequest) -> &str {
    match &request.question {
        Question::Form { message, .. } => message,
        other => panic!("expected a form, got {other:?}"),
    }
}

#[tokio::test]
async fn a_late_answer_after_server_cancellation_produces_no_reply() {
    let Scripted {
        client,
        mut inbox,
        mut server,
        mut returned,
    } = scripted().await;
    server.elicit(7).await;
    server.elicit(8).await;
    let mut ids = HashMap::new();
    for _ in 0..2 {
        let request = next_ask(&mut inbox).await;
        ids.insert(message_of(&request).to_string(), request.id);
    }
    let (cancelled, kept) = (ids["request 7"], ids["request 8"]);

    server.cancel(7).await;
    expect_withdrawn(&mut inbox, cancelled, Unanswered::Cancelled).await;
    // The host answers late, and the bridge refuses it.
    assert_eq!(
        inbox.submit(cancelled, Answer::Decline),
        Err(SubmitError::Stale)
    );
    // Handler 7 has returned an ordinary `cancel` result, which the client
    // must not send. ORDERING ASSUMPTION: after `Recording` signals, handler
    // 7's task hands that outcome to the client loop without yielding, so it
    // is queued before handler 8 can return. The loop takes outcomes in
    // order, so reply 8 on the wire then means outcome 7 was already
    // processed. If that handoff ever yields, this test can pass without
    // proving suppression; the library offers no hook to synchronize on it.
    let first = timeout(STEP, returned.recv()).await.unwrap();
    assert_eq!(first.as_deref(), Some("request 7"));

    inbox.submit(kept, Answer::Decline).unwrap();
    let mut seen = Vec::new();
    let reply = server.reply_to(8, &mut seen).await;
    assert_eq!(reply["result"]["action"], json!("decline"));

    client.shutdown().await.unwrap();
    server.drain(&mut seen).await;
    let replies_to_7: Vec<_> = seen
        .iter()
        .filter(|frame| frame["id"] == json!(7) && frame.get("method").is_none())
        .collect();
    assert!(
        replies_to_7.is_empty(),
        "a cancelled request was answered: {replies_to_7:?}"
    );
}

#[tokio::test]
async fn client_shutdown_withdraws_pending_questions_and_closes_the_bridge() {
    let Scripted {
        client,
        mut inbox,
        server,
        ..
    } = scripted().await;
    server.elicit(5).await;
    let request = next_ask(&mut inbox).await;

    timeout(STEP, client.shutdown()).await.unwrap().unwrap();
    expect_withdrawn(&mut inbox, request.id, Unanswered::Cancelled).await;
    assert_eq!(
        inbox.submit(request.id, Answer::Decline),
        Err(SubmitError::Stale)
    );
    // With the client gone, the last bridge handle goes too.
    assert!(timeout(STEP, inbox.recv()).await.unwrap().is_none());
}

// --- 2026-07-28 -------------------------------------------------------------

/// Retries once with only `requestState`, then asks for approval.
fn warm_up_tool() -> Tool {
    ToolBuilder::new("warm_up")
        .mrtr_handler(|ctx: RequestContext, _input: Value| async move {
            match ctx.request_state() {
                None => Ok(RequestOutcome::input_required(
                    InputRequiredResult::new().with_request_state("warming"),
                )),
                Some("warming") => Ok(RequestOutcome::input_required(
                    input_required(vec![("approval", form_request("Ready?"))])
                        .with_request_state("asked"),
                )),
                Some(_) => {
                    let answer = elicit_answer(&ctx, "approval").expect("answered");
                    Ok(done(format!("warm: {}", action_word(answer.action))))
                }
            }
        })
        .build()
}

/// Asks for approval on every attempt.
fn insatiable_tool() -> Tool {
    ToolBuilder::new("insatiable")
        .mrtr_handler(|_ctx: RequestContext, _input: Value| async move {
            Ok(ask_client(vec![("approval", form_request("Again?"))]))
        })
        .build()
}

/// A task that asks for a completion.
fn sampled_task_tool() -> Tool {
    ToolBuilder::new("sampled_task")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler(|ctx: RequestContext, _input: Value| async move {
            if let Some(result) = sample_answer(&ctx, "summary") {
                return Ok(done(summarized(Some(&result))));
            }
            Ok(ask_client(vec![(
                "summary",
                InputRequest::CreateMessage(summary_request("t")),
            )]))
        })
        .build()
}

fn test_router() -> McpRouter {
    final_router()
        .tool(warm_up_tool())
        .tool(insatiable_tool())
        .tool(sampled_task_tool())
}

#[tokio::test]
async fn final_input_required_round_trips_through_the_host() {
    let (client, bridge, mut inbox) = final_client(final_router()).await;
    let lifetime = StopFlag::default();
    let host = async {
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(next_ask(&mut inbox).await);
        }
        for request in held.into_iter().rev() {
            let answer = match &request.question {
                Question::Form { .. } => approve(),
                Question::Url { elicitation_id, .. } => {
                    assert_eq!(*elicitation_id, None, "a 2026-07-28 URL has no id");
                    Answer::Cancel
                }
                Question::Sample { .. } => sampled(),
            };
            inbox.submit(request.id, answer).unwrap();
        }
    };
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            call_tool_via_host(&client, &bridge, &lifetime, "onboard", json!({})),
            host
        )
    })
    .await
    .unwrap();
    assert_eq!(
        result.unwrap().all_text(),
        "onboard: approval=accept, URL flow not confirmed: cancel, summary received"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn state_only_retries_work_and_the_round_limit_is_checked_before_asking() {
    let (client, bridge, mut inbox) = final_client(test_router()).await;
    let lifetime = StopFlag::default();

    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            call_tool_via_host(&client, &bridge, &lifetime, "warm_up", json!({})),
            async {
                let request = next_ask(&mut inbox).await;
                inbox.submit(request.id, approve()).unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().all_text(), "warm: accept");

    let mut asked = 0;
    let result = timeout(STEP, async {
        let driver = call_tool_via_host(&client, &bridge, &lifetime, "insatiable", json!({}));
        tokio::pin!(driver);
        loop {
            tokio::select! {
                result = &mut driver => break result,
                Some(HostEvent::Ask(request)) = inbox.recv() => {
                    asked += 1;
                    inbox.submit(request.id, approve()).unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("attempts"));
    // The last attempt's request is never put to the host.
    assert_eq!(asked, MAX_INPUT_ROUNDS - 1);
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn ending_the_lifetime_withdraws_pending_host_questions() {
    let (client, bridge, mut inbox) = final_client(test_router()).await;
    let lifetime = StopFlag::default();
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            call_tool_via_host(&client, &bridge, &lifetime, "insatiable", json!({})),
            async {
                let request = next_ask(&mut inbox).await;
                lifetime.set();
                // Refused at once, before the driver has even woken.
                assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
                expect_withdrawn(&mut inbox, request.id, Unanswered::Cancelled).await;
            }
        )
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("lifetime"));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn final_task_input_is_answered_through_tasks_update() {
    let (client, bridge, mut inbox) = final_client(final_router()).await;
    let lifetime = StopFlag::default();
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            run_task_via_host(
                &client,
                &bridge,
                &lifetime,
                "nightly_report",
                json!({}),
                |_| {}
            ),
            async {
                let request = next_ask(&mut inbox).await;
                assert_eq!(request.question.kind(), "form");
                inbox.submit(request.id, approve()).unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().all_text(), "report published");
    client.shutdown().await.unwrap();
}

fn abandoned(error: &BoxError) -> &TaskAbandoned {
    error
        .downcast_ref::<TaskAbandoned>()
        .unwrap_or_else(|| panic!("expected TaskAbandoned, got {error}"))
}

#[tokio::test]
async fn a_refused_sampling_request_cancels_that_task() {
    let (client, bridge, mut inbox) = final_client(test_router()).await;
    let lifetime = StopFlag::default();
    let created = StdMutex::new(None);
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            run_task_via_host(
                &client,
                &bridge,
                &lifetime,
                "sampled_task",
                json!({}),
                |id| {
                    *created.lock().unwrap() = Some(id.to_string());
                }
            ),
            async {
                let request = next_ask(&mut inbox).await;
                assert_eq!(request.question.kind(), "sampling");
                inbox.submit(request.id, Answer::Decline).unwrap();
            }
        )
    })
    .await
    .unwrap();
    let error = result.unwrap_err();
    let abandoned = abandoned(&error);
    let created = created.lock().unwrap().clone().expect("task created");
    assert_eq!(abandoned.task_id, created);
    assert!(abandoned.cause.contains("declined"), "{}", abandoned.cause);
    assert_eq!(abandoned.status_after_cancel, Ok(TaskStatus::Cancelled));
    let task = client.task_get_detailed(&created).await.unwrap();
    assert_eq!(task.task.status(), TaskStatus::Cancelled);
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_task_that_ends_elsewhere_withdraws_its_question() {
    let (client, bridge, mut inbox) = final_client(final_router()).await;
    let lifetime = StopFlag::default();
    let created = StdMutex::new(None);
    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            run_task_via_host(
                &client,
                &bridge,
                &lifetime,
                "nightly_report",
                json!({}),
                |id| {
                    *created.lock().unwrap() = Some(id.to_string());
                }
            ),
            async {
                let request = next_ask(&mut inbox).await;
                let id = created.lock().unwrap().clone().expect("task created");
                client.task_cancel(&id, None).await.unwrap();
                expect_withdrawn(&mut inbox, request.id, Unanswered::Cancelled).await;
                assert_eq!(inbox.submit(request.id, approve()), Err(SubmitError::Stale));
            }
        )
    })
    .await
    .unwrap();
    let error = result.unwrap_err();
    let abandoned = abandoned(&error);
    assert!(
        abandoned.cause.contains("ended elsewhere"),
        "{}",
        abandoned.cause
    );
    assert_eq!(abandoned.status_after_cancel, Ok(TaskStatus::Cancelled));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lifetime_ended_beforehand_stops_before_any_request() {
    let (client, bridge, mut inbox) = final_client(test_router()).await;
    let lifetime = StopFlag::default();
    lifetime.set();
    let call = call_tool_via_host(&client, &bridge, &lifetime, "insatiable", json!({})).await;
    assert!(call.unwrap_err().to_string().contains("lifetime"));
    let mut created = false;
    let task = run_task_via_host(
        &client,
        &bridge,
        &lifetime,
        "nightly_report",
        json!({}),
        |_| {
            created = true;
        },
    )
    .await;
    assert!(task.unwrap_err().to_string().contains("lifetime"));
    assert!(!created, "no task is created once the lifetime has ended");
    assert!(inbox.events.try_recv().is_err());
    client.shutdown().await.unwrap();
}

/// [`ChannelTransport`] that records the method of every frame the client
/// sends.
struct Counting {
    inner: ChannelTransport,
    methods: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl ClientTransport for Counting {
    async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
        if let Ok(frame) = serde_json::from_str::<Value>(message)
            && let Some(method) = frame["method"].as_str()
        {
            self.methods.lock().unwrap().push(method.to_string());
        }
        self.inner.send(message).await
    }

    async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
        self.inner.recv().await
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    async fn close(&mut self) -> tower_mcp::Result<()> {
        self.inner.close().await
    }
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_sampling_request_ends_the_call_without_a_continuation() {
    let methods = Arc::new(StdMutex::new(Vec::new()));
    let transport = Counting {
        inner: ChannelTransport::new(final_router()),
        methods: Arc::clone(&methods),
    };
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_capabilities(host_capabilities())
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("timeout-test", "1.0.0").await.unwrap();
    let (bridge, mut inbox) = bridge(SHORT);
    let lifetime = StopFlag::default();

    let (result, ()) = timeout(STEP, async {
        tokio::join!(
            call_tool_via_host(&client, &bridge, &lifetime, "onboard", json!({})),
            async {
                let mut held = HashMap::new();
                for _ in 0..3 {
                    let request = next_ask(&mut inbox).await;
                    held.insert(request.question.kind(), request.id);
                }
                // Only the form is answered. The URL and sampling questions
                // share the deadline, so both are withdrawn when it passes.
                inbox.submit(held["form"], approve()).unwrap();
                let mut withdrawn = HashMap::new();
                for _ in 0..2 {
                    match next_event(&mut inbox).await {
                        HostEvent::Withdrawn { id, reason } => withdrawn.insert(id, reason),
                        other => panic!("expected a withdrawal, got {other:?}"),
                    };
                }
                assert_eq!(withdrawn.get(&held["sampling"]), Some(&Unanswered::Expired));
                assert!(withdrawn.contains_key(&held["url"]), "{withdrawn:?}");
            }
        )
    })
    .await
    .unwrap();
    let error = result.unwrap_err().to_string();
    assert!(error.contains("in time"), "{error}");
    let calls = methods
        .lock()
        .unwrap()
        .iter()
        .filter(|m| *m == "tools/call")
        .count();
    assert_eq!(
        calls, 1,
        "no continuation after the sampling request timed out"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_batch_larger_than_the_cap_is_refused_before_any_question() {
    let client = connect_final(final_router()).await.unwrap();
    let (bridge, mut inbox) = HostBridge::new(8, 2, LONG);
    let lifetime = StopFlag::default();
    // `onboard` asks for three inputs at once.
    let result = timeout(
        STEP,
        call_tool_via_host(&client, &bridge, &lifetime, "onboard", json!({})),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("limit"));
    assert!(inbox.events.try_recv().is_err());
    client.shutdown().await.unwrap();
}
