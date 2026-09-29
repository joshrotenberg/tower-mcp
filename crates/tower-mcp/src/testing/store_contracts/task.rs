//! Contract suites for [`TaskStore`].

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use super::violated;
use crate::async_task::{
    AppliedInputResponses, CancellationToken, TaskOwner, TaskPresence, TaskSnapshot, TaskStore,
    TaskStoreError, owner_matches,
};
use crate::error::JsonRpcError;
use crate::protocol::{
    CallToolResult, ElicitAction, ElicitFormParams, ElicitFormSchema, ElicitRequestParams,
    ElicitResult, InputRequest, InputRequests, InputResponse, InputResponses, ListRootsParams,
    TaskObject, TaskStatus,
};

const KIND: &str = "TaskStore";

/// Retention window for tasks that must stay alive for the whole check.
const LONG_TTL: u64 = 60_000;

/// Upper bound on anything that must happen promptly. Reached only when the
/// store never does it, so passing runs do not wait this long.
const BOUND: Duration = Duration::from_secs(3);

/// Run the [`TaskStore`] contract against the store `make` builds.
///
/// `make` is called once per check and must return an empty store each time.
/// The suite covers every required method of the trait:
///
/// - creation and reads: a new task is `working`, its token starts un-raised,
///   IDs are distinct, and unknown IDs read as absent everywhere;
/// - ownership: the owner given to `create_task` is reported unchanged by
///   `task_owner` and `task_presence`, `None` and `Some("")` stay distinct, and
///   no transition changes it;
/// - completion and failure: results and structured errors are stored whole,
///   and an `isError` result completes the task rather than failing it;
/// - terminal states are immutable: every transition method answers
///   `Ok(false)` (`cancel_task`: the unchanged task) for a terminal task;
/// - cancellation raises the token returned by `create_task`, including for a
///   task that already finished;
/// - expiry: an expired task reads as absent through every method, its token
///   is raised, an expired waiter is woken with `None`, and a terminal write
///   cannot resurrect it;
/// - `set_ttl` and the `ttl` a task reports;
/// - `wait_for_completion` on a terminal, a finishing, and an expiring task;
/// - `list_tasks` and its status filter;
/// - input requests: `require_input`, `outstanding_input_requests`, and
///   `apply_input_responses`, including partial answers, ignored keys, and the
///   rule that a spent request key can never be reissued.
///
/// The methods with trait defaults (`set_status`, `resume_context`,
/// `input_responses`, `set_task_meta`, `discard_task`) are covered separately
/// by [`task_store_optional_contract`], because a store may legitimately keep
/// the default. Retention limits are not covered: they are a policy of
/// [`MemoryTaskStore`](crate::async_task::MemoryTaskStore), and the trait
/// leaves the recovery from an oversized payload to each implementation.
///
/// # Panics
///
/// Panics, naming the violated rule, at the first check the store fails.
///
/// # Example
///
/// ```rust
/// use tower_mcp::async_task::MemoryTaskStore;
/// use tower_mcp::testing::store_contracts::task_store_contract;
///
/// # #[tokio::main]
/// # async fn main() {
/// task_store_contract(MemoryTaskStore::new).await;
/// # }
/// ```
pub async fn task_store_contract<S, F>(make: F)
where
    S: TaskStore,
    F: Fn() -> S,
{
    creation(&make()).await;
    unknown_tasks(&make()).await;
    ownership(&make()).await;
    results_and_errors(&make()).await;
    terminal_states_are_immutable(&make()).await;
    cancellation(&make()).await;
    expiry(&make()).await;
    ttl_updates(&make()).await;
    completion_waiters(&make()).await;
    listing(&make()).await;
    input_requests(&make()).await;
}

/// Run the contract for the [`TaskStore`] methods that have trait defaults.
///
/// `set_status`, `resume_context`, `input_responses`, `set_task_meta`, and
/// `discard_task` default to "not supported", and the router turns that into a
/// visible failure rather than silent success. A store that supports them
/// overrides them, and this suite holds the overrides to the documented
/// behavior. A store that keeps a default fails this suite by design, so call
/// it only for a store that implements these methods. The router requires
/// `resume_context` for any task that asks the client for input, and
/// `set_task_meta` and `discard_task` for task preparation callbacks.
///
/// # Panics
///
/// Panics, naming the violated rule, at the first check the store fails.
///
/// # Example
///
/// ```rust
/// use tower_mcp::async_task::MemoryTaskStore;
/// use tower_mcp::testing::store_contracts::task_store_optional_contract;
///
/// # #[tokio::main]
/// # async fn main() {
/// task_store_optional_contract(MemoryTaskStore::new).await;
/// # }
/// ```
pub async fn task_store_optional_contract<S, F>(make: F)
where
    S: TaskStore,
    F: Fn() -> S,
{
    status_updates(&make()).await;
    resumption(&make()).await;
    metadata(&make()).await;
    discarding(&make()).await;
}

// -- Helpers ----------------------------------------------------------------

/// Poll `probe` until it holds, up to [`BOUND`].
async fn eventually<F, Fut>(rule: &str, what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + BOUND;
    while !probe().await {
        if Instant::now() >= deadline {
            violated(
                KIND,
                rule,
                format_args!("{what} did not happen within {BOUND:?}"),
            );
        }
        sleep(Duration::from_millis(5)).await;
    }
}

/// Await `future`, up to [`BOUND`].
async fn within<T>(rule: &str, what: &str, future: impl Future<Output = T>) -> T {
    match timeout(BOUND, future).await {
        Ok(value) => value,
        Err(_) => violated(
            KIND,
            rule,
            format_args!("{what} did not finish within {BOUND:?}"),
        ),
    }
}

fn json_of<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("contract fixtures serialize")
}

/// A working task that lives for the whole check.
async fn create<S: TaskStore>(store: &S, owner: TaskOwner) -> (String, CancellationToken) {
    ok!(
        "create_task accepts an ordinary request",
        store
            .create_task("contract-tool", json!({"n": 1}), Some(LONG_TTL), owner)
            .await
    )
}

async fn task<S: TaskStore>(store: &S, id: &str, rule: &str) -> TaskObject {
    match ok!(rule, store.get_task(id).await) {
        Some(task) => task,
        None => violated(
            KIND,
            rule,
            format_args!("get_task({id}) returned None for a task that must be readable"),
        ),
    }
}

async fn snapshot<S: TaskStore>(store: &S, id: &str, rule: &str) -> TaskSnapshot {
    match ok!(rule, store.get_task_result(id).await) {
        Some(snapshot) => snapshot,
        None => violated(
            KIND,
            rule,
            format_args!("get_task_result({id}) returned None for a task that must be readable"),
        ),
    }
}

async fn status_of<S: TaskStore>(store: &S, id: &str, rule: &str) -> TaskStatus {
    task(store, id, rule).await.status
}

/// The stored outcome of a task: status, result, and error, as JSON.
async fn outcome<S: TaskStore>(store: &S, id: &str, rule: &str) -> (TaskStatus, Value, Value) {
    let (task, result, error) = snapshot(store, id, rule).await;
    (task.status, json_of(&result), json_of(&error))
}

fn result_text(text: &str) -> CallToolResult {
    CallToolResult::text(text)
}

fn sample_error() -> JsonRpcError {
    let mut error = JsonRpcError::invalid_params("unknown region");
    error.data = Some(json!({"field": "region"}));
    error
}

fn ask(keys: &[&str]) -> InputRequests {
    keys.iter()
        .map(|key| {
            (
                key.to_string(),
                InputRequest::ListRoots(ListRootsParams { meta: None }),
            )
        })
        .collect()
}

/// A request that is not the one `ask` produces for the same key.
fn ask_something_else(key: &str) -> InputRequests {
    InputRequests::from([(
        key.to_string(),
        InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
            mode: None,
            message: "a different question".to_string(),
            requested_schema: ElicitFormSchema::new(),
            meta: None,
        })),
    )])
}

fn accept(key: &str) -> (String, InputResponse) {
    (
        key.to_string(),
        InputResponse::Elicit(ElicitResult {
            action: ElicitAction::Accept,
            content: None,
            meta: None,
        }),
    )
}

fn answers(keys: &[&str]) -> InputResponses {
    keys.iter().map(|key| accept(key)).collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| key.to_string()).collect()
}

async fn outstanding<S: TaskStore>(store: &S, id: &str, rule: &str) -> BTreeSet<String> {
    match ok!(rule, store.outstanding_input_requests(id).await) {
        Some(requests) => requests.into_keys().collect(),
        None => violated(
            KIND,
            rule,
            format_args!("outstanding_input_requests({id}) returned None for a readable task"),
        ),
    }
}

async fn apply<S: TaskStore>(
    store: &S,
    id: &str,
    responses: InputResponses,
    rule: &str,
) -> AppliedInputResponses {
    match ok!(rule, store.apply_input_responses(id, responses).await) {
        Some(applied) => applied,
        None => violated(
            KIND,
            rule,
            format_args!("apply_input_responses({id}) returned None for a live task"),
        ),
    }
}

fn is_invalid_transition<T>(result: &Result<T, TaskStoreError>) -> bool {
    matches!(result, Err(TaskStoreError::InvalidTransition(_)))
}

#[derive(Clone, Copy, Debug)]
enum Finish {
    Complete,
    Fail,
    Cancel,
}

impl Finish {
    fn status(self) -> TaskStatus {
        match self {
            Self::Complete => TaskStatus::Completed,
            Self::Fail => TaskStatus::Failed,
            Self::Cancel => TaskStatus::Cancelled,
        }
    }
}

/// Drive a live task to the terminal state `how`.
async fn finish<S: TaskStore>(store: &S, id: &str, how: Finish) {
    match how {
        Finish::Complete => {
            let applied = ok!(
                "complete_task applies to a live task",
                store.complete_task(id, result_text("done")).await
            );
            check!(
                applied,
                "complete_task applies to a live task",
                "complete_task({id}) answered Ok(false)"
            );
        }
        Finish::Fail => {
            let applied = ok!(
                "fail_task applies to a live task",
                store.fail_task(id, sample_error()).await
            );
            check!(
                applied,
                "fail_task applies to a live task",
                "fail_task({id}) answered Ok(false)"
            );
        }
        Finish::Cancel => {
            let cancelled = ok!(
                "cancel_task applies to a live task",
                store.cancel_task(id, Some("contract")).await
            );
            check!(
                cancelled.is_some(),
                "cancel_task applies to a live task",
                "cancel_task({id}) returned None"
            );
        }
    }
}

// -- Required methods --------------------------------------------------------

async fn creation<S: TaskStore>(store: &S) {
    let (id, token) = create(store, None).await;
    check!(!id.is_empty(), "create_task returns a non-empty task id");
    check!(
        !token.is_cancelled(),
        "the token returned by create_task starts un-raised"
    );

    let rule = "a created task is readable with get_task";
    let task = task(store, &id, rule).await;
    check!(
        task.task_id == id,
        "get_task reports the id create_task returned",
        "got {:?}, expected {id:?}",
        task.task_id
    );
    check!(
        task.status == TaskStatus::Working,
        "a new task is working",
        "get_task reported {:?}",
        task.status
    );
    check!(
        task.ttl == Some(LONG_TTL),
        "get_task reports the ttl given to create_task",
        "got {:?}, expected Some({LONG_TTL})",
        task.ttl
    );

    let (snapshot_task, result, error) = snapshot(
        store,
        &id,
        "a created task is readable with get_task_result",
    )
    .await;
    check!(
        snapshot_task.task_id == id && snapshot_task.status == TaskStatus::Working,
        "get_task_result reports the same working task as get_task",
        "got {:?} in status {:?}",
        snapshot_task.task_id,
        snapshot_task.status
    );
    check!(
        result.is_none() && error.is_none(),
        "a working task has neither a result nor an error"
    );

    let mut ids = BTreeSet::from([id]);
    for _ in 0..8 {
        let (id, _) = create(store, None).await;
        check!(
            ids.insert(id.clone()),
            "every call to create_task returns a distinct id",
            "{id:?} was returned twice"
        );
    }
}

async fn unknown_tasks<S: TaskStore>(store: &S) {
    let id = crate::async_task::generate_task_id();
    let id = id.as_str();
    let rule = "an unknown task id reads as absent";

    check!(
        ok!(rule, store.get_task(id).await).is_none(),
        rule,
        "get_task returned a task"
    );
    check!(
        ok!(rule, store.get_task_result(id).await).is_none(),
        rule,
        "get_task_result returned a snapshot"
    );
    check!(
        ok!(rule, store.task_owner(id).await).is_none(),
        rule,
        "task_owner returned an owner"
    );
    check!(
        ok!(rule, store.task_presence(id).await) == TaskPresence::Missing,
        rule,
        "task_presence did not answer Missing"
    );
    check!(
        ok!(rule, store.outstanding_input_requests(id).await).is_none(),
        rule,
        "outstanding_input_requests returned a map"
    );
    check!(
        ok!(rule, store.apply_input_responses(id, answers(&["k"])).await).is_none(),
        rule,
        "apply_input_responses returned an outcome"
    );
    check!(
        ok!(
            rule,
            within(rule, "wait_for_completion", store.wait_for_completion(id)).await
        )
        .is_none(),
        rule,
        "wait_for_completion returned a snapshot"
    );
    let listed = ok!(rule, store.list_tasks(None).await);
    check!(
        listed.iter().all(|task| task.task_id != id),
        rule,
        "list_tasks listed the id"
    );

    let rule = "a transition on an unknown task id answers not-applied";
    check!(
        !ok!(rule, store.complete_task(id, result_text("x")).await),
        rule,
        "complete_task answered Ok(true)"
    );
    check!(
        !ok!(rule, store.fail_task(id, sample_error()).await),
        rule,
        "fail_task answered Ok(true)"
    );
    check!(
        ok!(rule, store.cancel_task(id, None).await).is_none(),
        rule,
        "cancel_task returned a task"
    );
    check!(
        !ok!(rule, store.set_ttl(id, LONG_TTL).await),
        rule,
        "set_ttl answered Ok(true)"
    );
    check!(
        !ok!(rule, store.require_input(id, ask(&["k"]), None).await),
        rule,
        "require_input answered Ok(true)"
    );
}

async fn ownership<S: TaskStore>(store: &S) {
    let cases: [(TaskOwner, &str); 4] = [
        (None, "an anonymous task"),
        (Some("alice".to_string()), "a task owned by alice"),
        (Some("bob".to_string()), "a task owned by bob"),
        (
            Some(String::new()),
            "a task owned by the empty principal, which is not anonymous",
        ),
    ];

    let mut created = Vec::new();
    for (owner, description) in &cases {
        let (id, _) = create(store, owner.clone()).await;
        created.push((id, owner.clone(), *description));
    }

    for (id, owner, description) in &created {
        let rule = "task_owner reports the owner given to create_task, unchanged";
        let reported = ok!(rule, store.task_owner(id).await);
        check!(
            reported.as_ref() == Some(owner),
            rule,
            "{description}: task_owner returned {reported:?}, expected Some({owner:?})"
        );

        let rule = "task_presence reports a live task as present with its owner";
        let presence = ok!(rule, store.task_presence(id).await);
        check!(
            presence
                == TaskPresence::Present {
                    owner: owner.clone()
                },
            rule,
            "{description}: task_presence returned {presence:?}"
        );

        // The matching table in the module docs, applied to what was stored.
        let stored = reported.expect("checked above");
        for principal in [None, Some("alice"), Some("bob"), Some("")] {
            let expected = owner.as_deref() == principal;
            check!(
                owner_matches(&stored, principal) == expected,
                "ownership is decided by equality with the recorded owner",
                "{description}: owner_matches({stored:?}, {principal:?}) disagreed with equality"
            );
        }
    }

    // A transition rewrites the record, and the owner has to survive it.
    let rule = "no transition changes a task's owner";
    let alice = Some("alice".to_string());
    let (id, _) = create(store, alice.clone()).await;
    ok!(rule, store.require_input(&id, ask(&["k"]), None).await);
    assert_owner(store, &id, &alice, "require_input").await;
    ok!(
        rule,
        store.apply_input_responses(&id, answers(&["k"])).await
    );
    assert_owner(store, &id, &alice, "apply_input_responses").await;
    ok!(rule, store.set_ttl(&id, LONG_TTL + 1).await);
    assert_owner(store, &id, &alice, "set_ttl").await;
    ok!(rule, store.complete_task(&id, result_text("done")).await);
    assert_owner(store, &id, &alice, "complete_task").await;

    for how in [Finish::Fail, Finish::Cancel] {
        let (id, _) = create(store, alice.clone()).await;
        finish(store, &id, how).await;
        assert_owner(store, &id, &alice, &format!("{how:?}")).await;
    }
}

async fn assert_owner<S: TaskStore>(store: &S, id: &str, expected: &TaskOwner, when: &str) {
    let rule = "no transition changes a task's owner";
    let owner = ok!(rule, store.task_owner(id).await);
    check!(
        owner.as_ref() == Some(expected),
        rule,
        "after {when}, task_owner returned {owner:?}"
    );
}

async fn results_and_errors<S: TaskStore>(store: &S) {
    // A result is stored whole and completes the task.
    let (id, _) = create(store, None).await;
    let rule = "complete_task stores the result and completes the task";
    let given = result_text("report ready");
    check!(
        ok!(rule, store.complete_task(&id, given.clone()).await),
        rule,
        "complete_task answered Ok(false) for a working task"
    );
    let (status, result, error) = outcome(store, &id, rule).await;
    check!(
        status == TaskStatus::Completed,
        rule,
        "status is {status:?}"
    );
    check!(
        result == json_of(&given),
        rule,
        "get_task_result returned {result} instead of the stored result"
    );
    check!(
        error.is_null(),
        rule,
        "a completed task reported an error: {error}"
    );

    // A domain error is still a completed task, not a failed one.
    let (id, _) = create(store, None).await;
    let rule = "a result with isError set completes the task rather than failing it";
    let mut given = result_text("region eu-west-3 is not enabled");
    given.is_error = true;
    check!(
        ok!(rule, store.complete_task(&id, given.clone()).await),
        rule,
        "complete_task answered Ok(false) for a working task"
    );
    let (status, result, error) = outcome(store, &id, rule).await;
    check!(
        status == TaskStatus::Completed,
        rule,
        "status is {status:?}"
    );
    check!(
        result == json_of(&given),
        rule,
        "the isError result was not returned whole: {result}"
    );
    check!(
        error.is_null(),
        rule,
        "isError was reported as a JSON-RPC error: {error}"
    );

    // A failure keeps the structured error and has no result.
    let (id, _) = create(store, None).await;
    let rule = "fail_task stores the structured error and fails the task";
    let given = sample_error();
    check!(
        ok!(rule, store.fail_task(&id, given.clone()).await),
        rule,
        "fail_task answered Ok(false) for a working task"
    );
    let (status, result, error) = outcome(store, &id, rule).await;
    check!(status == TaskStatus::Failed, rule, "status is {status:?}");
    check!(
        error == json_of(&given),
        rule,
        "get_task_result returned {error} instead of the whole error object"
    );
    check!(
        result.is_null(),
        rule,
        "a failed task reported a result: {result}"
    );
}

async fn terminal_states_are_immutable<S: TaskStore>(store: &S) {
    let rule = "terminal states are immutable";

    // Completed stays completed, with the first result.
    let (id, _) = create(store, None).await;
    finish(store, &id, Finish::Complete).await;
    let before = outcome(store, &id, rule).await;
    check!(
        !ok!(rule, store.complete_task(&id, result_text("stale")).await),
        rule,
        "complete_task applied to a completed task"
    );
    check!(
        !ok!(rule, store.fail_task(&id, sample_error()).await),
        rule,
        "fail_task applied to a completed task"
    );
    let cancelled = ok!(rule, store.cancel_task(&id, Some("late")).await);
    check!(
        cancelled.is_some_and(|task| task.status == TaskStatus::Completed),
        rule,
        "cancel_task on a completed task must return it still completed"
    );
    check!(
        !ok!(rule, store.require_input(&id, ask(&["k"]), None).await),
        rule,
        "require_input applied to a completed task"
    );
    check!(
        ok!(
            rule,
            store.apply_input_responses(&id, answers(&["k"])).await
        )
        .is_none(),
        rule,
        "apply_input_responses applied to a completed task"
    );
    check!(
        outcome(store, &id, rule).await == before,
        rule,
        "a completed task's recorded outcome changed"
    );

    // Failed stays failed, with the first error.
    let (id, _) = create(store, None).await;
    finish(store, &id, Finish::Fail).await;
    let before = outcome(store, &id, rule).await;
    check!(
        !ok!(rule, store.complete_task(&id, result_text("stale")).await),
        rule,
        "complete_task applied to a failed task"
    );
    check!(
        !ok!(
            rule,
            store
                .fail_task(&id, JsonRpcError::internal_error("second"))
                .await
        ),
        rule,
        "fail_task applied to a failed task"
    );
    let cancelled = ok!(rule, store.cancel_task(&id, None).await);
    check!(
        cancelled.is_some_and(|task| task.status == TaskStatus::Failed),
        rule,
        "cancel_task on a failed task must return it still failed"
    );
    check!(
        outcome(store, &id, rule).await == before,
        rule,
        "a failed task's recorded outcome changed"
    );

    // Cancelled stays cancelled.
    let (id, _) = create(store, None).await;
    finish(store, &id, Finish::Cancel).await;
    check!(
        status_of(store, &id, rule).await == TaskStatus::Cancelled,
        rule,
        "cancel_task did not leave the task cancelled"
    );
    check!(
        !ok!(rule, store.complete_task(&id, result_text("stale")).await),
        rule,
        "complete_task applied to a cancelled task"
    );
    check!(
        !ok!(rule, store.fail_task(&id, sample_error()).await),
        rule,
        "fail_task applied to a cancelled task"
    );
    let again = ok!(rule, store.cancel_task(&id, None).await);
    check!(
        again.is_some_and(|task| task.status == TaskStatus::Cancelled),
        rule,
        "a second cancel_task must return the task still cancelled"
    );
    let (status, result, error) = outcome(store, &id, rule).await;
    check!(
        status == TaskStatus::Cancelled && result.is_null() && error.is_null(),
        rule,
        "a cancelled task's outcome changed to {status:?}, {result}, {error}"
    );
}

async fn cancellation<S: TaskStore>(store: &S) {
    let rule = "cancel_task cancels a working task and raises its token";
    let (id, token) = create(store, None).await;
    let (other_id, other_token) = create(store, None).await;
    let clone = token.clone();

    let cancelled = ok!(
        rule,
        store.cancel_task(&id, Some("user closed the tab")).await
    );
    check!(
        cancelled.is_some_and(|task| task.status == TaskStatus::Cancelled),
        rule,
        "cancel_task did not return the task as cancelled"
    );
    check!(
        status_of(store, &id, rule).await == TaskStatus::Cancelled,
        rule,
        "get_task did not report the task as cancelled"
    );
    check!(
        token.is_cancelled() && clone.is_cancelled(),
        rule,
        "the token returned by create_task was not raised"
    );

    check!(
        !other_token.is_cancelled()
            && status_of(store, &other_id, rule).await == TaskStatus::Working,
        "cancelling one task does not affect another",
        "the other task's token or status changed"
    );

    // The token is a persistent signal, so a finished task still raises it.
    let rule = "cancel_task raises the token even for a task that already finished";
    for how in [Finish::Complete, Finish::Fail] {
        let (id, token) = create(store, None).await;
        finish(store, &id, how).await;
        ok!(rule, store.cancel_task(&id, None).await);
        check!(
            token.is_cancelled(),
            rule,
            "the token of a {:?} task was not raised",
            how.status()
        );
    }
}

/// Every read and transition answers "no such task" for an expired one.
async fn assert_expired<S: TaskStore>(store: &S, id: &str, token: &CancellationToken) {
    let rule = "an expired task reads as absent through every method";
    eventually(rule, "the task becoming absent", || async move {
        ok!(rule, store.get_task(id).await).is_none()
    })
    .await;

    check!(
        ok!(rule, store.get_task_result(id).await).is_none(),
        rule,
        "get_task_result still returned a snapshot"
    );
    check!(
        ok!(rule, store.task_owner(id).await).is_none(),
        rule,
        "task_owner still returned an owner"
    );
    let presence = ok!(rule, store.task_presence(id).await);
    check!(
        !matches!(presence, TaskPresence::Present { .. }),
        rule,
        "task_presence still reported the task as present"
    );
    check!(
        ok!(rule, store.outstanding_input_requests(id).await).is_none(),
        rule,
        "outstanding_input_requests still returned a map"
    );
    check!(
        ok!(rule, store.list_tasks(None).await)
            .iter()
            .all(|task| task.task_id != id),
        rule,
        "list_tasks still listed the task"
    );

    let rule = "an expired task cannot be transitioned or resurrected";
    check!(
        !ok!(rule, store.complete_task(id, result_text("late")).await),
        rule,
        "complete_task applied"
    );
    check!(
        !ok!(rule, store.fail_task(id, sample_error()).await),
        rule,
        "fail_task applied"
    );
    check!(
        ok!(rule, store.cancel_task(id, None).await).is_none(),
        rule,
        "cancel_task returned a task"
    );
    check!(
        !ok!(rule, store.set_ttl(id, LONG_TTL).await),
        rule,
        "set_ttl applied"
    );
    check!(
        !ok!(rule, store.require_input(id, ask(&["k"]), None).await),
        rule,
        "require_input applied"
    );
    check!(
        ok!(rule, store.apply_input_responses(id, answers(&["k"])).await).is_none(),
        rule,
        "apply_input_responses applied"
    );
    check!(
        ok!(rule, store.get_task(id).await).is_none(),
        rule,
        "a write after expiry made the task readable again"
    );

    within(
        "expiry raises the task's cancellation token",
        "the token being raised",
        token.cancelled(),
    )
    .await;
}

async fn expiry<S: TaskStore>(store: &S) {
    // A task that expires while still working, after its ttl is shortened.
    // The ttl runs from creation, so a ttl of zero has already elapsed.
    let (control, control_token) = create(store, None).await;
    let (id, token) = create(store, Some("alice".to_string())).await;
    let rule = "set_ttl applies to a live task";
    check!(
        ok!(rule, store.set_ttl(&id, 0).await),
        rule,
        "set_ttl answered Ok(false)"
    );
    assert_expired(store, &id, &token).await;

    // A ttl given at creation is honored without any later call.
    let (id, token) = ok!(
        "create_task accepts a short ttl",
        store
            .create_task("contract-tool", json!({}), Some(1), None)
            .await
    );
    assert_expired(store, &id, &token).await;

    // One task expiring does not disturb another.
    let rule = "expiry of one task does not affect another";
    check!(
        status_of(store, &control, rule).await == TaskStatus::Working
            && !control_token.is_cancelled(),
        rule,
        "the other task changed when a task expired"
    );
}

async fn ttl_updates<S: TaskStore>(store: &S) {
    let rule = "set_ttl changes the ttl a task reports";
    let (id, _) = create(store, None).await;
    check!(
        ok!(rule, store.set_ttl(&id, 120_000).await),
        rule,
        "set_ttl answered Ok(false) for a live task"
    );
    let reported = task(store, &id, rule).await.ttl;
    check!(
        reported == Some(120_000),
        rule,
        "get_task reported {reported:?} after set_ttl(120000)"
    );

    // Terminal states do not freeze the ttl or the task's readability.
    let rule = "a terminal task stays readable until its ttl elapses";
    finish(store, &id, Finish::Complete).await;
    check!(
        status_of(store, &id, rule).await == TaskStatus::Completed,
        rule,
        "a completed task with a long ttl was not readable"
    );
}

async fn completion_waiters<S: TaskStore>(store: &S) {
    // Already terminal: returns the snapshot without waiting for anything.
    let rule = "wait_for_completion returns a terminal task's snapshot";
    let (id, _) = create(store, None).await;
    finish(store, &id, Finish::Complete).await;
    let waited = within(rule, "wait_for_completion", store.wait_for_completion(&id)).await;
    let snapshot = match ok!(rule, waited) {
        Some(snapshot) => snapshot,
        None => violated(KIND, rule, format_args!("it returned None")),
    };
    check!(
        snapshot.0.status == TaskStatus::Completed && snapshot.1.is_some(),
        rule,
        "the snapshot was {:?} with result {:?}",
        snapshot.0.status,
        snapshot.1.is_some()
    );

    // A waiter already parked on a working task is woken by each terminal state.
    for how in [Finish::Complete, Finish::Fail, Finish::Cancel] {
        let rule = "wait_for_completion wakes when the task reaches a terminal state";
        let (id, _) = create(store, None).await;
        let (waited, ()) = within(rule, "the waiter and the transition", async {
            tokio::join!(store.wait_for_completion(&id), async {
                sleep(Duration::from_millis(20)).await;
                finish(store, &id, how).await;
            })
        })
        .await;
        let snapshot = match ok!(rule, waited) {
            Some(snapshot) => snapshot,
            None => violated(
                KIND,
                rule,
                format_args!("a waiter woken by {how:?} returned None"),
            ),
        };
        check!(
            snapshot.0.status == how.status(),
            rule,
            "a waiter woken by {how:?} saw status {:?}",
            snapshot.0.status
        );
        match how {
            Finish::Complete => check!(
                snapshot.1.is_some() && snapshot.2.is_none(),
                rule,
                "the completion snapshot must carry the result and no error"
            ),
            Finish::Fail => check!(
                snapshot.1.is_none() && snapshot.2.is_some(),
                rule,
                "the failure snapshot must carry the error and no result"
            ),
            Finish::Cancel => check!(
                snapshot.1.is_none() && snapshot.2.is_none(),
                rule,
                "the cancellation snapshot must carry neither result nor error"
            ),
        }
    }

    // Expiry wakes the waiter, which then finds nothing.
    let rule = "expiry wakes a completion waiter, which returns None";
    let (id, _) = create(store, None).await;
    let (waited, ()) = within(rule, "the waiter and the expiry", async {
        tokio::join!(store.wait_for_completion(&id), async {
            sleep(Duration::from_millis(20)).await;
            let shortened = ok!(rule, store.set_ttl(&id, 0).await);
            check!(shortened, rule, "set_ttl answered Ok(false)");
        })
    })
    .await;
    check!(
        ok!(rule, waited).is_none(),
        rule,
        "the waiter returned a snapshot for an expired task"
    );
}

async fn listing<S: TaskStore>(store: &S) {
    let rule = "list_tasks lists live tasks and honors its status filter";
    let (working, _) = create(store, None).await;
    let (completed, _) = create(store, None).await;
    let (cancelled, _) = create(store, None).await;
    finish(store, &completed, Finish::Complete).await;
    finish(store, &cancelled, Finish::Cancel).await;

    let ids = |tasks: &[TaskObject]| -> BTreeSet<String> {
        tasks.iter().map(|task| task.task_id.clone()).collect()
    };

    let all = ids(&ok!(rule, store.list_tasks(None).await));
    for (id, what) in [
        (&working, "the working task"),
        (&completed, "the completed task"),
        (&cancelled, "the cancelled task"),
    ] {
        check!(
            all.contains(id),
            rule,
            "list_tasks(None) omitted {what} {id}"
        );
    }

    for (filter, wanted, others) in [
        (TaskStatus::Working, &working, [&completed, &cancelled]),
        (TaskStatus::Completed, &completed, [&working, &cancelled]),
        (TaskStatus::Cancelled, &cancelled, [&working, &completed]),
    ] {
        let listed = ok!(rule, store.list_tasks(Some(filter)).await);
        check!(
            listed.iter().all(|task| task.status == filter),
            rule,
            "list_tasks(Some({filter:?})) returned a task in another status"
        );
        let listed = ids(&listed);
        check!(
            listed.contains(wanted),
            rule,
            "list_tasks(Some({filter:?})) omitted the {filter:?} task"
        );
        check!(
            others.iter().all(|other| !listed.contains(*other)),
            rule,
            "list_tasks(Some({filter:?})) included a task in another status"
        );
    }
}

async fn input_requests<S: TaskStore>(store: &S) {
    let (id, _) = create(store, None).await;

    let rule = "outstanding_input_requests is empty for a task that is not input_required";
    check!(
        outstanding(store, &id, rule).await.is_empty(),
        rule,
        "a working task reported outstanding requests"
    );

    // Parking a task.
    let rule = "require_input parks the task and records the requests";
    check!(
        ok!(
            rule,
            store
                .require_input(&id, ask(&["approval"]), Some("needs a decision"))
                .await
        ),
        rule,
        "require_input answered Ok(false) for a working task"
    );
    let parked = task(store, &id, rule).await;
    check!(
        parked.status == TaskStatus::InputRequired,
        rule,
        "status is {:?}",
        parked.status
    );
    check!(
        parked.status_message.as_deref() == Some("needs a decision"),
        rule,
        "status message is {:?}",
        parked.status_message
    );
    check!(
        outstanding(store, &id, rule).await == set(&["approval"]),
        rule,
        "the outstanding requests are not the ones given"
    );

    // The snapshot is replaced wholesale, and a live key is carried forward by
    // reissuing it.
    let rule = "require_input replaces the outstanding set, and a live key can be carried forward";
    check!(
        ok!(
            rule,
            store
                .require_input(&id, ask(&["approval", "region"]), None)
                .await
        ),
        rule,
        "reissuing a live key alongside a new one was refused"
    );
    check!(
        outstanding(store, &id, rule).await == set(&["approval", "region"]),
        rule,
        "the outstanding requests are not the set given"
    );

    // Dropping a key from the snapshot supersedes it, and a superseded key is
    // spent.
    let rule = "a superseded request key can never be reissued";
    check!(
        ok!(rule, store.require_input(&id, ask(&["region"]), None).await),
        rule,
        "narrowing the outstanding set was refused"
    );
    let reissued = store.require_input(&id, ask(&["approval"]), None).await;
    check!(
        is_invalid_transition(&reissued),
        rule,
        "reissuing a superseded key returned {reissued:?} instead of InvalidTransition"
    );
    check!(
        outstanding(store, &id, rule).await == set(&["region"]),
        rule,
        "a rejected require_input changed the outstanding set"
    );

    // Partial answers.
    let rule = "apply_input_responses consumes matching keys and leaves the rest outstanding";
    check!(
        ok!(
            rule,
            store
                .require_input(&id, ask(&["region", "zone"]), None)
                .await
        ),
        rule,
        "carrying region forward next to a new key was refused"
    );
    let applied = apply(store, &id, answers(&["region"]), rule).await;
    check!(
        applied.accepted == set(&["region"])
            && applied.ignored.is_empty()
            && applied.still_outstanding == set(&["zone"])
            && !applied.is_complete(),
        rule,
        "a partial answer produced {applied:?}"
    );
    check!(
        status_of(store, &id, rule).await == TaskStatus::InputRequired,
        rule,
        "a partial answer resumed the task early"
    );

    // Keys that are not outstanding are ignored, not rejected.
    let rule = "response keys that are not outstanding are ignored";
    let applied = apply(store, &id, answers(&["region", "never-issued"]), rule).await;
    check!(
        applied.accepted.is_empty()
            && applied.ignored == set(&["region", "never-issued"])
            && applied.still_outstanding == set(&["zone"]),
        rule,
        "an answered key and an unknown key produced {applied:?}"
    );
    check!(
        status_of(store, &id, rule).await == TaskStatus::InputRequired,
        rule,
        "ignored responses changed the task's status"
    );

    // An answered key is spent, and a live key cannot be repointed.
    let rule = "an answered request key can never be reissued";
    let reissued = store.require_input(&id, ask(&["region"]), None).await;
    check!(
        is_invalid_transition(&reissued),
        rule,
        "reissuing an answered key returned {reissued:?} instead of InvalidTransition"
    );
    let rule = "a live request key cannot be pointed at a different request";
    let repointed = store
        .require_input(&id, ask_something_else("zone"), None)
        .await;
    check!(
        is_invalid_transition(&repointed),
        rule,
        "repointing a live key returned {repointed:?} instead of InvalidTransition"
    );
    check!(
        outstanding(store, &id, rule).await == set(&["zone"]),
        rule,
        "a rejected require_input changed the outstanding set"
    );

    // Answering the last request resumes the task.
    let rule = "answering the last outstanding request returns the task to working";
    let applied = apply(store, &id, answers(&["zone"]), rule).await;
    check!(
        applied.is_complete() && applied.accepted == set(&["zone"]),
        rule,
        "the last answer produced {applied:?}"
    );
    check!(
        status_of(store, &id, rule).await == TaskStatus::Working,
        rule,
        "the task did not return to working"
    );
    check!(
        outstanding(store, &id, rule).await.is_empty(),
        rule,
        "a resumed task still reported outstanding requests"
    );

    // A new key on a later round is the normal case.
    let rule = "a task can ask again under a new key";
    check!(
        ok!(
            rule,
            store.require_input(&id, ask(&["confirm"]), None).await
        ),
        rule,
        "require_input with a fresh key was refused"
    );

    // With nothing outstanding, a live task ignores everything.
    let (idle, _) = create(store, None).await;
    let rule = "apply_input_responses on a task with nothing outstanding ignores every key";
    let applied = apply(store, &idle, answers(&["stray"]), rule).await;
    check!(
        applied.accepted.is_empty() && applied.ignored == set(&["stray"]),
        rule,
        "a stray answer produced {applied:?}"
    );
    check!(
        status_of(store, &idle, rule).await == TaskStatus::Working,
        rule,
        "a stray answer changed the task's status"
    );

    // Terminal states end the wait.
    let rule = "a terminal task has no outstanding input requests";
    for how in [Finish::Complete, Finish::Fail, Finish::Cancel] {
        let (id, _) = create(store, None).await;
        ok!(rule, store.require_input(&id, ask(&["k"]), None).await);
        finish(store, &id, how).await;
        check!(
            outstanding(store, &id, rule).await.is_empty(),
            rule,
            "a {:?} task still reported outstanding requests",
            how.status()
        );
    }
}

// -- Methods with trait defaults ----------------------------------------------

async fn status_updates<S: TaskStore>(store: &S) {
    let rule = "set_status records a non-terminal status and message";
    let (id, _) = create(store, None).await;
    check!(
        ok!(
            rule,
            store
                .set_status(&id, TaskStatus::Working, Some("step 2 of 5"))
                .await
        ),
        rule,
        "set_status answered Ok(false) for a live task"
    );
    let updated = task(store, &id, rule).await;
    check!(
        updated.status == TaskStatus::Working
            && updated.status_message.as_deref() == Some("step 2 of 5"),
        rule,
        "get_task reported {:?} with message {:?}",
        updated.status,
        updated.status_message
    );

    let rule = "set_status answers not-applied for an unknown or terminal task";
    let unknown = crate::async_task::generate_task_id();
    check!(
        !ok!(
            rule,
            store.set_status(&unknown, TaskStatus::Working, None).await
        ),
        rule,
        "set_status applied to an unknown task"
    );
    finish(store, &id, Finish::Complete).await;
    check!(
        !ok!(
            rule,
            store
                .set_status(&id, TaskStatus::Working, Some("too late"))
                .await
        ),
        rule,
        "set_status applied to a completed task"
    );
    let after = task(store, &id, rule).await;
    check!(
        after.status == TaskStatus::Completed
            && after.status_message.as_deref() != Some("too late"),
        rule,
        "a completed task changed to {:?} with message {:?}",
        after.status,
        after.status_message
    );
}

async fn resumption<S: TaskStore>(store: &S) {
    let rule = "resume_context returns what the handler needs to run again";
    let (id, token) = ok!(
        rule,
        store
            .create_task("deploy", json!({"service": "api"}), Some(LONG_TTL), None)
            .await
    );
    ok!(
        rule,
        store.require_input(&id, ask(&["approval"]), None).await
    );
    ok!(
        rule,
        store
            .apply_input_responses(&id, answers(&["approval"]))
            .await
    );
    ok!(rule, store.require_input(&id, ask(&["region"]), None).await);
    ok!(
        rule,
        store.apply_input_responses(&id, answers(&["region"])).await
    );

    let resume = match ok!(rule, store.resume_context(&id).await) {
        Some(resume) => resume,
        None => violated(
            KIND,
            rule,
            format_args!("resume_context returned None for a live task"),
        ),
    };
    check!(
        resume.tool_name == "deploy",
        rule,
        "tool_name is {:?}",
        resume.tool_name
    );
    check!(
        resume.arguments == json!({"service": "api"}),
        rule,
        "arguments are {} instead of the original ones",
        resume.arguments
    );
    check!(
        json_of(&resume.input_responses) == json_of(&answers(&["approval", "region"])),
        rule,
        "the accumulated responses are {}",
        json_of(&resume.input_responses)
    );

    let rule = "input_responses returns every answer accumulated so far";
    let recorded = match ok!(rule, store.input_responses(&id).await) {
        Some(recorded) => recorded,
        None => violated(
            KIND,
            rule,
            format_args!("input_responses returned None for a live task"),
        ),
    };
    check!(
        json_of(&recorded) == json_of(&answers(&["approval", "region"])),
        rule,
        "the recorded responses are {}",
        json_of(&recorded)
    );

    // The token has to be the task's own signal, or a replayed handler outlives
    // the task.
    let rule = "the resume context carries the task's cancellation token";
    let replay_token = match resume.cancellation_token {
        Some(replay_token) => replay_token,
        None => violated(KIND, rule, format_args!("cancellation_token is None")),
    };
    check!(
        !replay_token.is_cancelled(),
        rule,
        "the token was already raised for a live task"
    );
    ok!(rule, store.cancel_task(&id, None).await);
    check!(
        token.is_cancelled() && replay_token.is_cancelled(),
        rule,
        "cancel_task did not raise both the create_task token and the resume token"
    );

    // Expiry raises it as well.
    let rule = "expiry raises the resume context's cancellation token";
    let (id, _) = create(store, None).await;
    let replay_token = match ok!(rule, store.resume_context(&id).await)
        .and_then(|resume| resume.cancellation_token)
    {
        Some(replay_token) => replay_token,
        None => violated(
            KIND,
            rule,
            format_args!("resume_context returned no cancellation token"),
        ),
    };
    ok!(rule, store.set_ttl(&id, 0).await);
    within(rule, "the token being raised", replay_token.cancelled()).await;
}

async fn metadata<S: TaskStore>(store: &S) {
    let rule = "set_task_meta stores metadata that every later view of the task carries";
    let (id, _) = create(store, None).await;
    let meta = json!({"io.example/trace": "abc123"});
    check!(
        ok!(rule, store.set_task_meta(&id, meta.clone()).await),
        rule,
        "set_task_meta answered Ok(false) for a live task"
    );
    let stored = task(store, &id, rule).await.meta;
    check!(
        stored.as_ref() == Some(&meta),
        rule,
        "get_task reported metadata {stored:?}"
    );

    finish(store, &id, Finish::Complete).await;
    let (completed, _, _) = snapshot(store, &id, rule).await;
    check!(
        completed.meta.as_ref() == Some(&meta),
        rule,
        "metadata was lost when the task completed: {:?}",
        completed.meta
    );

    let rule = "set_task_meta answers not-applied for an unknown task";
    let unknown = crate::async_task::generate_task_id();
    check!(
        !ok!(rule, store.set_task_meta(&unknown, json!({})).await),
        rule,
        "set_task_meta applied to an unknown task"
    );
}

async fn discarding<S: TaskStore>(store: &S) {
    let rule = "discard_task removes a task that could not finish initialization";
    let (id, _) = create(store, None).await;
    check!(
        ok!(rule, store.discard_task(&id).await),
        rule,
        "discard_task answered Ok(false) for a live task"
    );
    check!(
        ok!(rule, store.get_task(&id).await).is_none(),
        rule,
        "the task was still readable after discard_task"
    );
    check!(
        ok!(rule, store.task_owner(&id).await).is_none(),
        rule,
        "the task still had an owner after discard_task"
    );

    let rule = "discard_task answers not-applied when there is nothing to remove";
    check!(
        !ok!(rule, store.discard_task(&id).await),
        rule,
        "a second discard_task answered Ok(true)"
    );
}
