//! Host-mediated elicitation and sampling.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example host_mediated_input --features protocol-2026-07-28
//! cargo test --example host_mediated_input --features protocol-2026-07-28
//! ```
//!
//! An agent host or GUI often owns the user interface and the model, while an
//! [`McpClient`] owns the connection. When a server asks for input (a form, a
//! URL flow, or an LLM completion), the client hands the question to the
//! host, waits while the host asks its user or model, and sends the answer
//! back for the request that asked. This example builds that bridge from
//! public tower-mcp APIs only. It embeds no inference provider, terminal UI
//! or host wire format; those belong to the host.
//!
//! # The pieces
//!
//! - [`HostBridge`] sends typed [`Question`]s to the host and waits for the
//!   matching [`Answer`]. [`HostInbox`] is the host's end: it receives
//!   [`HostEvent`]s and submits answers by [`InputId`]. Unknown, duplicate,
//!   stale and mismatched answers are refused with a [`SubmitError`].
//! - [`HostHandler`] implements [`ClientHandler`] for the 2025-11-25
//!   lifecycle, where the server sends `elicitation/create` and
//!   `sampling/createMessage` as JSON-RPC requests.
//! - [`call_tool_via_host`] and [`run_task_via_host`] drive the 2026-07-28
//!   lifecycle, where the server returns an `input_required` result (or a task
//!   in the `input_required` state) and the client retries with
//!   `inputResponses` (or sends `tasks/update`).
//!
//! # Lifecycles
//!
//! On **2025-11-25** the client runs each server-initiated request on its own
//! task with its own cancellation token, so several questions can be pending
//! at once without blocking the connection. [`HostHandler`] reads the token
//! through [`ClientHandler::current_cancellation_token`]. The token fires when
//! the server sends `notifications/cancelled` or the client is closed with
//! [`McpClient::shutdown`]; the question is then withdrawn from the host, and
//! the client sends no reply if the token fired before the client's last
//! check of it, which runs after the handler returns and before the reply is
//! handed over (#1483). Dropping the `McpClient` without
//! `shutdown()` aborts the client's message loop without cancelling tokens, so
//! questions then end at their deadline instead.
//!
//! On **2026-07-28**, automatic resolution in [`McpClient::call_tool`] and
//! related methods runs the [`ClientHandler`] off the message loop, with
//! concurrent keys and a shared cancellation token for the round. This
//! example drives the rounds explicitly to control how it sends each key to
//! the host and to watch for task termination while the host decides. The
//! drivers use [`McpClient::call_tool_once`],
//! [`McpClient::call_tool_once_task_aware`], [`McpClient::task_get_detailed`]
//! and [`McpClient::task_update`]. They send every entry of `inputRequests`
//! to the host at once and watch a [`StopFlag`] set by the client's owner,
//! the connection state, and (for tasks) whether the task has reached a
//! terminal status.
//!
//! # Identity, bounds and cleanup
//!
//! The JSON-RPC request id never leaves the client, which replies on the id it
//! dispatched. The host sees an [`InputId`] issued by the bridge; it includes
//! a per-bridge number, so an id from another session reads as unknown. On
//! 2026-07-28 the server's `inputRequests` key stays inside the driver and
//! `requestState` is echoed back unread.
//!
//! | Situation                       | Host sees                         | Server gets                                     |
//! |---------------------------------|-----------------------------------|-------------------------------------------------|
//! | Host answers in time            | (nothing more)                    | the answer                                      |
//! | Deadline passes                 | `Withdrawn(Expired)`              | elicitation `cancel`, sampling error            |
//! | Queue full until the deadline   | nothing                           | elicitation `cancel`, sampling error            |
//! | Too many questions pending      | nothing                           | elicitation `cancel`, sampling error            |
//! | Host drops its inbox            | n/a                               | elicitation `cancel`, sampling error            |
//! | Server cancels (2025-11-25)     | `Withdrawn(Cancelled)`            | no reply                                        |
//! | `shutdown()` (2025-11-25)       | `Withdrawn(Cancelled)`            | no reply                                        |
//! | Lifetime ends (2026-07-28)      | `Withdrawn(Cancelled)`            | nothing further; a task is cancelled, best effort |
//! | Connection closes (2026-07-28)  | `Withdrawn(Cancelled)` once polled | nothing further; task cancel is attempted       |
//! | Task ends elsewhere             | `Withdrawn(Cancelled)` once polled | usually no `tasks/update`; a late one can slip through |
//!
//! Disconnects and remote task termination are polled every
//! [`WATCH_INTERVAL`], so the host can still answer, and the answer be used,
//! between the change and the poll that notices it.
//!
//! [`HostInbox::submit`] decides under the same lock that withdrawal uses, and
//! it checks the question's deadline and stop signal itself, so an answer
//! submitted after either is `Stale` even if the waiting side has not woken
//! yet. `Ok` means the bridge used the answer. It does not mean the server
//! received it: a server cancellation that lands after the answer was taken
//! still suppresses the reply. `Withdrawn` events are best effort (dropped
//! when the queue is full); a host that misses one learns from `Stale`.
//!
//! # Trust
//!
//! Every string in a [`Question`] is written by the server. A host shows it to
//! its user as data and never treats it as an instruction to itself. The demo
//! prints ids, kinds and outcomes only: messages, field names, URLs (which can
//! carry one-time tokens), form values and model output stay inside the host's
//! workflow. [`Answer::UrlAcceptedByUser`] is the host's statement that its
//! user explicitly confirmed the URL flow. Showing, opening or rendering the
//! URL is not confirmation, and a question nobody decides ends as `cancel`.
//!
//! # Capabilities and transports
//!
//! The clients advertise `elicitation.form`, `elicitation.url` and `sampling`
//! (see [`host_capabilities`]); the 2026-07-28 client also declares the Tasks
//! extension. Advertise only what the host can answer. The bridge does not
//! depend on the transport, but the transport and the peer must support the
//! lifecycle in use: server-initiated requests for 2025-11-25, and
//! `input_required` results (plus the Tasks extension for tasks) for
//! 2026-07-28. The demo uses an in-process [`ChannelTransport`], so it needs
//! no network or child process.
//!
//! # Not here
//!
//! Event framing for a CLI, terminal prompts and default interaction modes are
//! host concerns (mcp-repl owns them for its CLI). The bridge stays in this
//! example rather than the library: it needs nothing beyond public APIs, and
//! its policies (queue size, pending cap, deadline, what a timeout returns to
//! the server) are decisions each host makes differently.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::try_join_all;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tower_mcp::client::TaskAwareCallToolOutcome;
use tower_mcp::extract::{Context, Json, RawArgs};
use tower_mcp::protocol::{
    ClientCapabilities, ContentRole, CreateMessageParams, CreateMessageResult, ElicitAction,
    ElicitFieldValue, ElicitFinalUrlParams, ElicitFormParams, ElicitFormSchema, ElicitMode,
    ElicitRequestParams, ElicitResult, ElicitUrlParams, ElicitationCapability,
    ElicitationFormCapability, ElicitationUrlCapability, InputRequest, InputRequests,
    InputRequiredResult, InputResponse, InputResponses, RequestOutcome, SamplingCapability,
    SamplingContent, SamplingContentOrArray, SamplingMessage, TaskStatus,
};
use tower_mcp::{
    BoxError, CallToolResult, ChannelTransport, ClientHandler, JsonRpcError, McpClient, McpRouter,
    MemoryTaskStore, ProtocolSupport, RequestContext, TaskSupportMode, ToolBuilder,
};

// =============================================================================
// The bridge: typed questions out, typed answers back
// =============================================================================

/// Identifies one question the bridge has put to the host. Issued by the
/// bridge, not copied from the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputId {
    bridge: u64,
    seq: u64,
}

impl std::fmt::Display for InputId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input-{}-{}", self.bridge, self.seq)
    }
}

static NEXT_BRIDGE: AtomicU64 = AtomicU64::new(1);

/// What the server wants from the host. Every string in here is
/// server-authored: show it as data, never follow it as an instruction.
#[derive(Debug, Clone)]
pub enum Question {
    Form {
        message: String,
        schema: ElicitFormSchema,
    },
    /// `elicitation_id` is present on 2025-11-25 and absent on 2026-07-28.
    Url {
        message: String,
        url: String,
        elicitation_id: Option<String>,
    },
    Sample {
        params: Box<CreateMessageParams>,
    },
}

impl Question {
    /// Safe to log: carries none of the server's text.
    pub fn kind(&self) -> &'static str {
        match self {
            Question::Form { .. } => "form",
            Question::Url { .. } => "url",
            Question::Sample { .. } => "sampling",
        }
    }
}

/// One question delivered to the host, with the time it stops waiting.
#[derive(Debug)]
pub struct HostRequest {
    pub id: InputId,
    pub question: Question,
    pub deadline: Instant,
}

#[derive(Debug)]
pub enum HostEvent {
    /// A new question. Answer it with [`HostInbox::submit`].
    Ask(HostRequest),
    /// A delivered question that no longer wants an answer.
    Withdrawn { id: InputId, reason: Unanswered },
}

/// Why a question ended without an answer from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswered {
    /// The deadline passed, possibly while waiting for queue space.
    Expired,
    /// The question's [`Stop`] fired: server cancellation, `shutdown()`, the
    /// end of the client's lifetime, a closed connection, a task that ended,
    /// or the caller dropping the wait.
    Cancelled,
    /// The host dropped its [`HostInbox`].
    HostGone,
    /// The bridge already held its maximum of pending questions. Nothing was
    /// queued.
    Busy,
}

/// The host's answer to a question.
#[derive(Debug, Clone)]
pub enum Answer {
    FormAccept(HashMap<String, ElicitFieldValue>),
    /// The user explicitly confirmed the URL flow in the host's UI.
    UrlAcceptedByUser,
    /// The completion the host's own model produced.
    Sampled(Box<CreateMessageResult>),
    /// The user explicitly declined (for sampling: the host refuses).
    Decline,
    /// The user dismissed the question without choosing.
    Cancel,
}

/// Why [`HostInbox::submit`] refused an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// Never issued by this bridge, or long forgotten.
    Unknown,
    /// Already answered.
    Duplicate,
    /// Past its deadline, stopped, or withdrawn.
    Stale,
    /// The answer does not fit the question, such as a URL confirmation for
    /// a form. The question stays pending. Checking form values against the
    /// schema is the host's job.
    WrongKind,
}

type StopCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Tells the bridge to stop waiting: a check `submit` can make under the
/// ledger lock, and a future `ask` can race.
pub struct Stop {
    is_set: StopCheck,
    wait: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Stop {
    pub fn new(
        is_set: impl Fn() -> bool + Send + Sync + 'static,
        wait: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            is_set: Arc::new(is_set),
            wait: Box::pin(wait),
        }
    }

    pub fn never() -> Self {
        Self::new(|| false, std::future::pending())
    }

    /// A shared check with no wakeup of its own, for questions whose caller
    /// already races the same condition and drops `ask` when it holds.
    fn check_only(is_set: StopCheck) -> Self {
        Self {
            is_set,
            wait: Box::pin(std::future::pending()),
        }
    }
}

/// A one-way flag: once set, it stays set. Cheap to clone.
#[derive(Clone)]
pub struct StopFlag(Arc<watch::Sender<bool>>);

impl Default for StopFlag {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl StopFlag {
    pub fn set(&self) {
        self.0.send_replace(true);
    }

    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn wait(&self) {
        let _ = self.0.subscribe().wait_for(|set| *set).await;
    }
}

/// Whether `answer` can answer a question of `kind` (see [`Question::kind`]).
fn fits(kind: &str, answer: &Answer) -> bool {
    matches!(
        (kind, answer),
        (_, Answer::Decline | Answer::Cancel)
            | ("form", Answer::FormAccept(_))
            | ("url", Answer::UrlAcceptedByUser)
            | ("sampling", Answer::Sampled(_))
    )
}

/// Finished ids remembered so a late or repeated answer reads as `Stale` or
/// `Duplicate`. Older ids read as `Unknown`; all are refused.
const RETIRED_MEMORY: usize = 256;

#[derive(Clone, Copy)]
enum Retired {
    Answered,
    Withdrawn,
}

struct Pending {
    kind: &'static str,
    reply: oneshot::Sender<Answer>,
    deadline: Instant,
    stopped: StopCheck,
}

#[derive(Default)]
struct Ledger {
    pending: HashMap<InputId, Pending>,
    retired: VecDeque<(InputId, Retired)>,
}

impl Ledger {
    fn retire(&mut self, id: InputId, how: Retired) {
        if self.retired.len() == RETIRED_MEMORY {
            self.retired.pop_front();
        }
        self.retired.push_back((id, how));
    }

    fn retired(&self, id: InputId) -> Option<Retired> {
        self.retired
            .iter()
            .find(|(retired, _)| *retired == id)
            .map(|(_, how)| *how)
    }
}

struct Shared {
    bridge: u64,
    next_seq: AtomicU64,
    ledger: Mutex<Ledger>,
}

impl Shared {
    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The client side of the bridge. Cheap to clone.
#[derive(Clone)]
pub struct HostBridge {
    shared: Arc<Shared>,
    events: mpsc::Sender<HostEvent>,
    max_pending: usize,
    deadline: Duration,
}

/// The host side of the bridge.
pub struct HostInbox {
    events: mpsc::Receiver<HostEvent>,
    shared: Arc<Shared>,
}

impl HostBridge {
    /// `queue`: undelivered events the channel holds. `max_pending`:
    /// questions the bridge holds at once, delivered or not, so a server that
    /// asks for many inputs cannot grow it without bound. `deadline`: how long
    /// each question waits.
    pub fn new(queue: usize, max_pending: usize, deadline: Duration) -> (HostBridge, HostInbox) {
        let shared = Arc::new(Shared {
            bridge: NEXT_BRIDGE.fetch_add(1, Ordering::Relaxed),
            next_seq: AtomicU64::new(1),
            ledger: Mutex::default(),
        });
        let (tx, rx) = mpsc::channel(queue);
        let bridge = HostBridge {
            shared: Arc::clone(&shared),
            events: tx,
            max_pending,
            deadline,
        };
        (bridge, HostInbox { events: rx, shared })
    }

    /// Ask the host `question` and wait for its answer, until `stop` fires,
    /// the deadline passes (time spent waiting for queue space included), or
    /// the host drops its inbox. Dropping the returned future withdraws the
    /// question.
    pub async fn ask(&self, question: Question, stop: Stop) -> Result<Answer, Unanswered> {
        let Stop { is_set, wait } = stop;
        let id = InputId {
            bridge: self.shared.bridge,
            seq: self.shared.next_seq.fetch_add(1, Ordering::Relaxed),
        };
        let deadline = Instant::now() + self.deadline;
        let (reply_tx, mut reply_rx) = oneshot::channel();
        {
            let mut ledger = self.shared.ledger();
            if is_set() {
                return Err(Unanswered::Cancelled);
            }
            if ledger.pending.len() >= self.max_pending {
                return Err(Unanswered::Busy);
            }
            let pending = Pending {
                kind: question.kind(),
                reply: reply_tx,
                deadline,
                stopped: is_set,
            };
            ledger.pending.insert(id, pending);
        }
        // Every way out from here, including being dropped, goes through this.
        let mut guard = Withdrawal {
            bridge: self,
            id,
            delivered: false,
            settled: false,
        };
        let event = HostEvent::Ask(HostRequest {
            id,
            question,
            deadline,
        });

        let outcome = tokio::select! {
            outcome = async {
                self.events.send(event).await.map_err(|_| Unanswered::HostGone)?;
                guard.delivered = true;
                tokio::select! {
                    answer = &mut reply_rx => answer.map_err(|_| Unanswered::HostGone),
                    () = self.events.closed() => Err(Unanswered::HostGone),
                }
            } => outcome,
            () = wait => Err(Unanswered::Cancelled),
            () = tokio::time::sleep_until(deadline) => Err(Unanswered::Expired),
        };
        guard.settle(outcome, &mut reply_rx)
    }
}

impl HostInbox {
    /// The next event, or `None` once every [`HostBridge`] clone is gone.
    pub async fn recv(&mut self) -> Option<HostEvent> {
        self.events.recv().await
    }

    /// Answer question `id`. A refused answer changes nothing; `Ok` means the
    /// bridge used this answer.
    pub fn submit(&self, id: InputId, answer: Answer) -> Result<(), SubmitError> {
        if id.bridge != self.shared.bridge {
            return Err(SubmitError::Unknown);
        }
        let mut ledger = self.shared.ledger();
        let Some(pending) = ledger.pending.get(&id) else {
            return Err(match ledger.retired(id) {
                Some(Retired::Answered) => SubmitError::Duplicate,
                Some(Retired::Withdrawn) => SubmitError::Stale,
                None => SubmitError::Unknown,
            });
        };
        // The waiting side may not have woken yet; its deadline and stop
        // signal still win.
        if Instant::now() >= pending.deadline || (pending.stopped)() {
            return Err(SubmitError::Stale);
        }
        if !fits(pending.kind, &answer) {
            return Err(SubmitError::WrongKind);
        }
        let pending = ledger.pending.remove(&id).expect("present above");
        ledger.retire(id, Retired::Answered);
        // Sent under the lock: the asker can only give up by taking it, so it
        // will find the answer waiting.
        let _ = pending.reply.send(answer);
        Ok(())
    }
}

/// Removes a question from the ledger when `ask` stops waiting for it.
struct Withdrawal<'a> {
    bridge: &'a HostBridge,
    id: InputId,
    delivered: bool,
    settled: bool,
}

impl Withdrawal<'_> {
    fn settle(
        mut self,
        outcome: Result<Answer, Unanswered>,
        reply_rx: &mut oneshot::Receiver<Answer>,
    ) -> Result<Answer, Unanswered> {
        self.settled = true;
        let reason = match outcome {
            Ok(answer) => return Ok(answer),
            Err(reason) => reason,
        };
        if self.withdraw(reason) {
            Err(reason)
        } else {
            // `submit` took the answer before the deadline or stop (it checks
            // both), so the answer stands.
            reply_rx.try_recv().map_err(|_| reason)
        }
    }

    /// Returns `false` if the host already answered.
    fn withdraw(&self, reason: Unanswered) -> bool {
        let mut ledger = self.bridge.shared.ledger();
        if ledger.pending.remove(&self.id).is_none() {
            return false;
        }
        ledger.retire(self.id, Retired::Withdrawn);
        drop(ledger);
        if self.delivered {
            let _ = self.bridge.events.try_send(HostEvent::Withdrawn {
                id: self.id,
                reason,
            });
        }
        true
    }
}

impl Drop for Withdrawal<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.withdraw(Unanswered::Cancelled);
        }
    }
}

// =============================================================================
// 2025-11-25: server-initiated requests through ClientHandler
// =============================================================================

fn elicit_question(params: ElicitRequestParams) -> Result<Question, JsonRpcError> {
    match params {
        ElicitRequestParams::Form(form) => Ok(Question::Form {
            message: form.message,
            schema: form.requested_schema,
        }),
        ElicitRequestParams::Url(url) => Ok(Question::Url {
            message: url.message,
            url: url.url,
            elicitation_id: Some(url.elicitation_id),
        }),
        ElicitRequestParams::FinalUrl(url) => Ok(Question::Url {
            message: url.message,
            url: url.url,
            elicitation_id: None,
        }),
        _ => Err(JsonRpcError::invalid_params("unsupported elicitation mode")),
    }
}

fn elicit_result(outcome: Result<Answer, Unanswered>) -> ElicitResult {
    match outcome {
        Ok(Answer::FormAccept(content)) => ElicitResult::accept(content),
        Ok(Answer::UrlAcceptedByUser) => ElicitResult {
            action: ElicitAction::Accept,
            content: None,
            meta: None,
        },
        Ok(Answer::Decline) => ElicitResult::decline(),
        // `cancel` is "dismissed without an explicit choice"; a deadline, a
        // busy or vanished host and a cancellation all look like that.
        Ok(Answer::Cancel | Answer::Sampled(_)) | Err(_) => ElicitResult::cancel(),
    }
}

fn sample_result(outcome: Result<Answer, Unanswered>) -> Result<CreateMessageResult, JsonRpcError> {
    match outcome {
        Ok(Answer::Sampled(result)) => Ok(*result),
        Ok(_) => Err(JsonRpcError::internal_error(
            "the host declined the sampling request",
        )),
        Err(Unanswered::Expired) => Err(JsonRpcError::request_timeout(
            "the host did not answer the sampling request in time",
        )),
        Err(_) => Err(JsonRpcError::internal_error(
            "the host is not answering sampling requests",
        )),
    }
}

/// Every server-initiated elicitation and sampling request becomes a question
/// to the host.
pub struct HostHandler {
    bridge: HostBridge,
}

impl HostHandler {
    pub fn new(bridge: HostBridge) -> Self {
        Self { bridge }
    }

    /// The client's cancellation for the request being handled. Call it from
    /// inside a handler method, where the client scopes the token.
    fn stop(&self) -> Stop {
        match self.current_cancellation_token() {
            Some(token) => {
                let check = token.clone();
                Stop::new(move || check.is_cancelled(), async move {
                    token.cancelled().await
                })
            }
            None => Stop::never(),
        }
    }
}

#[async_trait]
impl ClientHandler for HostHandler {
    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        let question = elicit_question(params)?;
        Ok(elicit_result(self.bridge.ask(question, self.stop()).await))
    }

    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        let question = Question::Sample {
            params: Box::new(params),
        };
        sample_result(self.bridge.ask(question, self.stop()).await)
    }
}

// =============================================================================
// 2026-07-28: input_required results and tasks
// =============================================================================

/// Most `tools/call` attempts per call, so at most one fewer host rounds.
const MAX_INPUT_ROUNDS: usize = 4;
/// How often the drivers check the connection and task status while waiting.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// How long a driver spends cancelling a task it gives up on.
const CANCEL_BUDGET: Duration = Duration::from_secs(2);
const LIFETIME_ENDED: &str = "the client's lifetime ended";

/// Refuse to start or continue once the owner has ended the client's lifetime
/// or the connection is gone.
fn ensure_alive(client: &McpClient, lifetime: &StopFlag) -> Result<(), BoxError> {
    if lifetime.is_set() {
        return Err(LIFETIME_ENDED.into());
    }
    if !client.is_connected() {
        return Err("the connection closed".into());
    }
    Ok(())
}

/// Run `rpc` unless the lifetime ends first, so a stalled request cannot hold
/// the driver away from its cleanup.
async fn unless_ended<T>(
    lifetime: &StopFlag,
    rpc: impl Future<Output = tower_mcp::Result<T>>,
) -> Result<T, BoxError> {
    tokio::select! {
        biased;
        () = lifetime.wait() => Err(LIFETIME_ENDED.into()),
        result = rpc => Ok(result?),
    }
}

/// Send every input request to the host at once, keyed as the server keyed
/// them. `stopped` is shared by all of them; the caller owns the wakeup.
///
/// A batch larger than the bridge's pending cap is refused before any
/// question is created. A refused or unanswered sampling request ends the
/// whole operation here, with no continuation sent: `InputResponse` has no
/// error variant, and a retry must answer every request it was given. (A task
/// would accept a partial `tasks/update` and keep the sampling request
/// outstanding, which is only useful to a host that means to resume later;
/// this one does not.) If one request fails, the others are dropped, which
/// withdraws them.
async fn answer_inputs(
    bridge: &HostBridge,
    requests: InputRequests,
    stopped: StopCheck,
) -> Result<InputResponses, BoxError> {
    if requests.len() > bridge.max_pending {
        return Err(format!(
            "{} input requests exceed the host's limit of {}",
            requests.len(),
            bridge.max_pending
        )
        .into());
    }
    let asks = requests.into_iter().map(|(key, request)| {
        let stop = Stop::check_only(Arc::clone(&stopped));
        async move {
            let response = match request {
                InputRequest::Elicit(params) => {
                    let question = elicit_question(params).map_err(tower_mcp::Error::JsonRpc)?;
                    InputResponse::Elicit(elicit_result(bridge.ask(question, stop).await))
                }
                InputRequest::CreateMessage(params) => {
                    let question = Question::Sample {
                        params: Box::new(params),
                    };
                    let result = sample_result(bridge.ask(question, stop).await);
                    InputResponse::CreateMessage(result.map_err(tower_mcp::Error::JsonRpc)?)
                }
                _ => return Err(BoxError::from("this host does not answer that input kind")),
            };
            Ok((key, response))
        }
    });
    Ok(try_join_all(asks).await?.into_iter().collect())
}

/// One host round, raced against the end of the client's lifetime, a closed
/// connection and, for a task, the task reaching a terminal status.
///
/// Residual race: disconnects and task termination are polled every
/// [`WATCH_INTERVAL`]. Between a change and the poll that notices it, the host
/// can still answer and the driver can still use that answer.
async fn answer_round(
    client: &McpClient,
    bridge: &HostBridge,
    lifetime: &StopFlag,
    requests: InputRequests,
    task_id: Option<&str>,
) -> Result<InputResponses, BoxError> {
    let round = StopFlag::default();
    let ended = async {
        let why = tokio::select! {
            () = lifetime.wait() => LIFETIME_ENDED,
            () = disconnected(client) => "the connection closed",
            why = task_ended(client, task_id) => why,
        };
        // Set before returning, so `submit` refuses answers from here on.
        round.set();
        why
    };
    let (round_set, lifetime_set) = (round.clone(), lifetime.clone());
    let stopped: StopCheck = Arc::new(move || round_set.is_set() || lifetime_set.is_set());
    tokio::select! {
        biased;
        why = ended => Err(why.into()),
        answers = answer_inputs(bridge, requests, stopped) => answers,
    }
}

async fn disconnected(client: &McpClient) {
    while client.is_connected() {
        tokio::time::sleep(WATCH_INTERVAL).await;
    }
}

async fn task_ended(client: &McpClient, task_id: Option<&str>) -> &'static str {
    let Some(task_id) = task_id else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        match client.task_get_detailed(task_id).await {
            Ok(task) if task.task.status().is_terminal() => return "the task ended elsewhere",
            Ok(_) => {}
            Err(_) => return "the task could not be read",
        }
    }
}

/// Call a tool on 2026-07-28, sending its input requests to the host and
/// retrying with the answers. Set `lifetime` to stop: it is checked before
/// every attempt, including retries that carry only `requestState`.
pub async fn call_tool_via_host(
    client: &McpClient,
    bridge: &HostBridge,
    lifetime: &StopFlag,
    name: &str,
    arguments: Value,
) -> Result<CallToolResult, BoxError> {
    let mut input_responses = None;
    let mut request_state = None;
    for attempt in 1..=MAX_INPUT_ROUNDS {
        ensure_alive(client, lifetime)?;
        let outcome = client
            .call_tool_once(
                name,
                arguments.clone(),
                input_responses.take(),
                request_state.take(),
            )
            .await?;
        let required = match outcome {
            RequestOutcome::Complete(result) => return Ok(result),
            RequestOutcome::InputRequired(required) => required,
        };
        required.validate()?;
        // Checked before asking the host, so no answers are collected for a
        // retry that will not be sent.
        if attempt == MAX_INPUT_ROUNDS {
            break;
        }
        // A result may carry only `requestState`: retry with no answers.
        input_responses = match required.input_requests {
            Some(requests) if !requests.is_empty() => {
                Some(answer_round(client, bridge, lifetime, requests, None).await?)
            }
            _ => None,
        };
        // Opaque server state: echoed back, never read.
        request_state = required.request_state;
    }
    Err(format!("the tool still wanted input after {MAX_INPUT_ROUNDS} attempts").into())
}

/// A task the driver gave up on and tried to cancel.
///
/// `Display` and `Debug` leave out the task id (task ids act as bearer tokens)
/// and the cause (which can carry server text). Read the fields directly.
pub struct TaskAbandoned {
    pub task_id: String,
    pub cause: String,
    /// One read of the task's status after the cancel request: a snapshot,
    /// which can still be `Working` if the server has not finished
    /// cancelling. `Err` if cancelling or reading failed or ran out of time.
    pub status_after_cancel: Result<TaskStatus, String>,
}

impl std::fmt::Display for TaskAbandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status_after_cancel {
            Ok(status) => write!(f, "task abandoned; status after cancel: {status:?}"),
            Err(_) => write!(f, "task abandoned; status after cancel unknown"),
        }
    }
}

impl std::fmt::Debug for TaskAbandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for TaskAbandoned {}

/// Call a tool the server runs as a task on 2026-07-28, answering the task's
/// input requests through the host.
///
/// The creation request is not raced against `lifetime`, so a created task's
/// id is always known; the flag is checked just before it. From creation
/// until this returns, the driver owns the task's cancellation: on any
/// failure, including `lifetime` ending during a later request, it cancels the
/// task (bounded by [`CANCEL_BUDGET`]) and returns [`TaskAbandoned`].
/// `on_created` hands the caller the id as soon as it exists. Stop the driver
/// by setting `lifetime`, not by dropping this future: a dropped future cannot
/// run async cleanup, so the task would be left for the caller to cancel or
/// for its TTL to reclaim. The 2026-07-28 `tasks/cancel` carries no reason.
pub async fn run_task_via_host(
    client: &McpClient,
    bridge: &HostBridge,
    lifetime: &StopFlag,
    name: &str,
    arguments: Value,
    on_created: impl FnOnce(&str),
) -> Result<CallToolResult, BoxError> {
    ensure_alive(client, lifetime)?;
    let outcome = client
        .call_tool_once_task_aware(name, arguments, None, None)
        .await?;
    let TaskAwareCallToolOutcome::Task(created) = outcome else {
        return Err("the tool did not create a task".into());
    };
    let task_id = created.task.metadata.task_id;
    on_created(&task_id);
    match drive_task(client, bridge, lifetime, &task_id).await {
        Ok(result) => Ok(result),
        Err(cause) => {
            let status_after_cancel = cancel_within_budget(client, &task_id).await;
            Err(Box::new(TaskAbandoned {
                task_id,
                cause: cause.to_string(),
                status_after_cancel,
            }))
        }
    }
}

async fn drive_task(
    client: &McpClient,
    bridge: &HostBridge,
    lifetime: &StopFlag,
    task_id: &str,
) -> Result<CallToolResult, BoxError> {
    loop {
        let task = unless_ended(lifetime, client.task_get_detailed(task_id)).await?;
        match task.task.status() {
            TaskStatus::Completed => {
                let result = task.task.result().cloned().ok_or("no task result")?;
                return Ok(serde_json::from_value(Value::Object(result))?);
            }
            TaskStatus::InputRequired => {
                // As in the built-in driver: an empty map would loop on empty
                // updates without progress.
                let requests = task
                    .task
                    .input_requests()
                    .filter(|requests| !requests.is_empty())
                    .cloned()
                    .ok_or("input_required task without inputRequests")?;
                let responses =
                    answer_round(client, bridge, lifetime, requests, Some(task_id)).await?;
                ensure_alive(client, lifetime)?;
                // Residual race: the task can still end between the watcher's
                // last status poll and this update. The server ignores keys it
                // no longer has outstanding, but the update still carries the
                // host's answers to it.
                unless_ended(lifetime, client.task_update(task_id, responses)).await?;
            }
            TaskStatus::Working => {
                let interval = task.task.metadata().poll_interval_ms.unwrap_or(50);
                let pause = Duration::from_millis(interval.clamp(10, 1_000));
                unless_ended(lifetime, async {
                    tokio::time::sleep(pause).await;
                    Ok(())
                })
                .await?;
            }
            _ => return Err("the task ended without a result".into()),
        }
    }
}

/// Best effort, bounded: send `tasks/cancel`, then read the status once.
async fn cancel_within_budget(client: &McpClient, task_id: &str) -> Result<TaskStatus, String> {
    let attempt = async {
        let cancelled = client.task_cancel(task_id, None).await;
        let status = client
            .task_get_detailed(task_id)
            .await
            .map_err(|_| "status unreadable".to_string())?
            .task
            .status();
        match cancelled {
            Err(_) if !status.is_terminal() => Err("cancel refused".to_string()),
            _ => Ok(status),
        }
    };
    tokio::time::timeout(CANCEL_BUDGET, attempt)
        .await
        .unwrap_or_else(|_| Err("cancellation timed out".to_string()))
}

// =============================================================================
// Clients
// =============================================================================

/// What the demo clients advertise: form and URL elicitation, and sampling.
pub fn host_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        elicitation: Some(ElicitationCapability {
            form: Some(ElicitationFormCapability::default()),
            url: Some(ElicitationUrlCapability::default()),
        }),
        sampling: Some(SamplingCapability::default()),
        ..ClientCapabilities::default()
    }
}

/// A 2025-11-25 client whose server-initiated requests go to the host.
pub async fn connect_stable(router: McpRouter, bridge: HostBridge) -> Result<McpClient, BoxError> {
    let client = McpClient::builder()
        .with_capabilities(host_capabilities())
        .connect(ChannelTransport::new(router), HostHandler::new(bridge))
        .await?;
    client.initialize("host-mediated-input", "1.0.0").await?;
    Ok(client)
}

/// A 2026-07-28 client. It has no [`ClientHandler`]: input requests reach the
/// host through [`call_tool_via_host`] and [`run_task_via_host`].
pub async fn connect_final(router: McpRouter) -> Result<McpClient, BoxError> {
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"])?)
        .with_capabilities(host_capabilities())
        .with_tasks()
        .connect_simple(ChannelTransport::new(router))
        .await?;
    client.discover("host-mediated-input", "1.0.0").await?;
    Ok(client)
}

// =============================================================================
// Demo servers
// =============================================================================
//
// Tool results report outcomes only, never form values or model output, so a
// host can print them.

const SIGN_IN_URL: &str = "https://auth.example.com/device";
const ONBOARD_STATE: &str = "onboard-v1";

#[derive(Debug, Deserialize, JsonSchema)]
struct DeployInput {
    environment: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SummarizeInput {
    text: String,
}

fn approval_form(message: &str) -> ElicitFormParams {
    ElicitFormParams {
        mode: None,
        message: message.to_string(),
        requested_schema: ElicitFormSchema::new().string_field(
            "approver",
            Some("Who approves this"),
            true,
        ),
        meta: None,
    }
}

fn form_request(message: &str) -> InputRequest {
    InputRequest::Elicit(ElicitRequestParams::Form(approval_form(message)))
}

fn summary_request(text: &str) -> CreateMessageParams {
    CreateMessageParams::new(vec![SamplingMessage::user(text)], 200)
        .system_prompt("Summarize in one sentence.")
}

fn done(text: impl Into<String>) -> RequestOutcome<CallToolResult> {
    RequestOutcome::Complete(CallToolResult::text(text))
}

fn input_required(requests: Vec<(&str, InputRequest)>) -> InputRequiredResult {
    InputRequiredResult::with_requests(
        requests
            .into_iter()
            .map(|(key, request)| (key.to_string(), request))
            .collect(),
    )
}

fn ask_client(requests: Vec<(&str, InputRequest)>) -> RequestOutcome<CallToolResult> {
    RequestOutcome::input_required(input_required(requests))
}

/// The elicitation answer sent under `key` on a retry.
fn elicit_answer(ctx: &RequestContext, key: &str) -> Option<ElicitResult> {
    match ctx.input_responses()?.get(key)? {
        InputResponse::Elicit(result) => Some(result.clone()),
        _ => None,
    }
}

/// The sampling answer sent under `key` on a retry.
fn sample_answer(ctx: &RequestContext, key: &str) -> Option<CreateMessageResult> {
    match ctx.input_responses()?.get(key)? {
        InputResponse::CreateMessage(result) => Some(result.clone()),
        _ => None,
    }
}

fn action_word(action: ElicitAction) -> &'static str {
    match action {
        ElicitAction::Accept => "accept",
        ElicitAction::Decline => "decline",
        ElicitAction::Cancel => "cancel",
        _ => "unknown",
    }
}

fn approved(result: &ElicitResult, what: &str) -> String {
    let has_approver = result
        .content
        .as_ref()
        .is_some_and(|content| content.contains_key("approver"));
    match result.action {
        ElicitAction::Accept if has_approver => what.to_string(),
        other => format!("not {what}: {}", action_word(other)),
    }
}

/// `accept` establishes only that the user confirmed the URL flow.
fn url_flow(result: &ElicitResult) -> String {
    match result.action {
        ElicitAction::Accept => "URL flow confirmed".to_string(),
        other => format!("URL flow not confirmed: {}", action_word(other)),
    }
}

fn summarized(result: Option<&CreateMessageResult>) -> &'static str {
    match result.and_then(CreateMessageResult::first_text) {
        Some(_) => "summary received",
        None => "summary unavailable",
    }
}

/// 2025-11-25 tools: each sends one server-initiated request.
pub fn stable_router() -> McpRouter {
    let deploy = ToolBuilder::new("deploy")
        .description("Deploy after a person approves")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<DeployInput>| async move {
                let message = format!("Deploy to {}?", input.environment);
                let result = ctx.elicit_form(approval_form(&message)).await?;
                let what = format!("deployed to {}", input.environment);
                Ok(CallToolResult::text(approved(&result, &what)))
            },
        )
        .build();
    let sign_in = ToolBuilder::new("sign_in")
        .description("Ask the user to complete a sign-in flow")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            let result = ctx
                .elicit_url(ElicitUrlParams {
                    mode: Some(ElicitMode::Url),
                    elicitation_id: "sign-in-1".to_string(),
                    message: "Sign in to continue".to_string(),
                    url: SIGN_IN_URL.to_string(),
                    meta: None,
                })
                .await?;
            Ok(CallToolResult::text(url_flow(&result)))
        })
        .build();
    let summarize = ToolBuilder::new("summarize")
        .description("Summarize text with the client's model")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<SummarizeInput>| async move {
                let result = ctx.sample(summary_request(&input.text)).await.ok();
                Ok(CallToolResult::text(summarized(result.as_ref())))
            },
        )
        .build();
    McpRouter::new()
        .server_info("host-mediated-input", env!("CARGO_PKG_VERSION"))
        .tool(deploy)
        .tool(sign_in)
        .tool(summarize)
}

/// 2026-07-28 tools: one result asking for all three kinds, and a task.
pub fn final_router() -> McpRouter {
    let onboard = ToolBuilder::new("onboard")
        .description("Approve, sign in and summarize in one round trip")
        .mrtr_handler(|ctx: RequestContext, _input: Value| async move {
            if let (Some(approval), Some(sign_in)) = (
                elicit_answer(&ctx, "approval"),
                elicit_answer(&ctx, "sign_in"),
            ) {
                if ctx.request_state() != Some(ONBOARD_STATE) {
                    return Ok(done("onboard: request state was not echoed"));
                }
                let summary = summarized(sample_answer(&ctx, "summary").as_ref());
                return Ok(done(format!(
                    "onboard: approval={}, {}, {summary}",
                    action_word(approval.action),
                    url_flow(&sign_in),
                )));
            }
            let sign_in = ElicitFinalUrlParams::new("Sign in to continue", SIGN_IN_URL);
            let summary = summary_request("A new teammate joins.");
            let required = input_required(vec![
                ("approval", form_request("Approve onboarding?")),
                (
                    "sign_in",
                    InputRequest::Elicit(ElicitRequestParams::FinalUrl(sign_in)),
                ),
                ("summary", InputRequest::CreateMessage(summary)),
            ]);
            Ok(RequestOutcome::input_required(
                required.with_request_state(ONBOARD_STATE),
            ))
        })
        .build();
    let report = ToolBuilder::new("nightly_report")
        .description("Publish the nightly report once approved")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler(|ctx: RequestContext, _input: Value| async move {
            if let Some(result) = elicit_answer(&ctx, "approval") {
                return Ok(done(approved(&result, "report published")));
            }
            let message = "Publish the nightly report?";
            Ok(ask_client(vec![("approval", form_request(message))]))
        })
        .build();
    McpRouter::new()
        .server_info("host-mediated-input", env!("CARGO_PKG_VERSION"))
        .task_store(Arc::new(MemoryTaskStore::new()))
        .tool(onboard)
        .tool(report)
        .with_tasks()
}

// =============================================================================
// A scripted host and user
// =============================================================================

/// Placeholder for the host's own model. tower-mcp never talks to a provider.
fn stub_model(params: &CreateMessageParams) -> CreateMessageResult {
    CreateMessageResult {
        role: ContentRole::Assistant,
        content: SamplingContentOrArray::Single(SamplingContent::Text {
            text: format!("stub summary of {} message(s)", params.messages.len()),
            annotations: None,
            meta: None,
        }),
        model: "host-stub".to_string(),
        stop_reason: Some("endTurn".to_string()),
        meta: None,
    }
}

/// What the person using the host decides about one card in its UI.
enum Decision {
    Approve,
    /// The user says they finished the URL flow. Only this makes a URL
    /// question `accept`.
    ConfirmUrlFlow,
}

/// Stand-in for a person. The host shows one card per question, and each
/// decision names the card it is for. A real host waits for the person here.
fn scripted_user(cards: &[(InputId, &str)], confirms_url_flow: bool) -> HashMap<InputId, Decision> {
    let decide = |kind: &str| match kind {
        "form" => Some(Decision::Approve),
        "url" if confirms_url_flow => Some(Decision::ConfirmUrlFlow),
        _ => None,
    };
    cards
        .iter()
        .filter_map(|(id, kind)| Some((*id, decide(kind)?)))
        .collect()
}

fn host_answer(request: &HostRequest, decision: Option<Decision>) -> Answer {
    match (&request.question, decision) {
        // Sampling goes to the host's model, not to the person.
        (Question::Sample { params }, _) => Answer::Sampled(Box::new(stub_model(params))),
        (Question::Form { .. }, Some(Decision::Approve)) => Answer::FormAccept(HashMap::from([(
            "approver".to_string(),
            ElicitFieldValue::String("demo-user".to_string()),
        )])),
        (Question::Url { .. }, Some(Decision::ConfirmUrlFlow)) => Answer::UrlAcceptedByUser,
        // Nobody decided: dismissed.
        _ => Answer::Cancel,
    }
}

/// The host's loop. For each batch size it holds that many questions, shows
/// them to the user, then answers in reverse order of arrival. It prints ids,
/// kinds and fixed outcome names, never the questions' contents.
async fn scripted_host(mut inbox: HostInbox, batches: Vec<usize>, confirms_url_flow: bool) {
    for size in batches {
        let mut held = Vec::new();
        while held.len() < size {
            match inbox.recv().await {
                Some(HostEvent::Ask(request)) => {
                    println!("  host <- {} ({})", request.id, request.question.kind());
                    held.push(request);
                }
                Some(HostEvent::Withdrawn { id, reason }) => println!("  host: {id} {reason:?}"),
                None => return,
            }
        }
        let cards: Vec<_> = held.iter().map(|r| (r.id, r.question.kind())).collect();
        let mut decisions = scripted_user(&cards, confirms_url_flow);
        for request in held.into_iter().rev() {
            let answer = host_answer(&request, decisions.remove(&request.id));
            // `SubmitError` has no payload, so its name is safe to print.
            match inbox.submit(request.id, answer) {
                Ok(()) => println!("  host -> {} answered", request.id),
                Err(error) => println!("  host -> {} refused ({error:?})", request.id),
            }
        }
    }
    while let Some(event) = inbox.recv().await {
        if let HostEvent::Ask(request) = event {
            let _ = inbox.submit(request.id, Answer::Cancel);
        }
    }
}

async fn run() -> Result<(), BoxError> {
    println!("=== 2025-11-25: server-initiated requests through ClientHandler ===\n");
    let (bridge, inbox) = HostBridge::new(8, 32, Duration::from_secs(30));
    let host = tokio::spawn(scripted_host(inbox, vec![3], true));
    let client = connect_stable(stable_router(), bridge).await?;
    // Three calls at once: three questions pending on the host together.
    let (deploy, sign_in, summary) = tokio::join!(
        client.call_tool("deploy", json!({"environment": "staging"})),
        client.call_tool("sign_in", json!({})),
        client.call_tool("summarize", json!({"text": "Tower composes services."})),
    );
    println!("deploy:    {}", deploy?.all_text());
    println!("sign_in:   {}", sign_in?.all_text());
    println!("summarize: {}\n", summary?.all_text());
    client.shutdown().await?;
    host.await?;

    println!("=== 2026-07-28: input_required results and tasks ===\n");
    let (bridge, inbox) = HostBridge::new(8, 32, Duration::from_secs(30));
    // This time the user never confirms the URL flow, so it ends as cancel.
    let host = tokio::spawn(scripted_host(inbox, vec![3, 1], false));
    let client = connect_final(final_router()).await?;
    let lifetime = StopFlag::default();
    let onboard = call_tool_via_host(&client, &bridge, &lifetime, "onboard", json!({})).await?;
    println!("onboard:        {}", onboard.all_text());
    let report = run_task_via_host(
        &client,
        &bridge,
        &lifetime,
        "nightly_report",
        json!({}),
        // Task ids act as bearer tokens, so the demo does not print it.
        |_task_id| println!("  task created; the driver owns its cancellation"),
    )
    .await?;
    println!("nightly_report: {}", report.all_text());
    lifetime.set();
    client.shutdown().await?;
    drop(bridge);
    host.await?;
    Ok(())
}

/// Prints a fixed category on failure. Returning the error from `main` would
/// print it with `Debug`, which can include server text.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is::<TaskAbandoned>() => {
            eprintln!("error: a task was abandoned and cancelled");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("error: an MCP call or host round failed");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "host_mediated_input/tests.rs"]
mod tests;
