//! The store contract suites, run the way a downstream crate would run them.
//!
//! Two kinds of test live here. The first runs each suite against every store
//! this crate ships, which is what keeps the shipped stores and the trait
//! documentation agreeing. The second runs each suite against a deliberately
//! broken store and expects the suite to fail with the right rule, which is
//! what shows the suites detect the mistakes they exist for. A suite that
//! passes everything proves nothing until it has been seen to fail.
//!
//! Everything here uses the public API only.

#![cfg(feature = "testing")]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tower_mcp::JsonRpcError;
use tower_mcp::async_task::{
    AppliedInputResponses, CancellationToken, MemoryTaskStore, MemoryTaskStoreConfig, Result,
    TaskOwner, TaskPresence, TaskResumeContext, TaskSnapshot, TaskStore, TaskStoreError,
};
use tower_mcp::protocol::{CallToolResult, InputRequests, InputResponses, TaskObject, TaskStatus};
use tower_mcp::testing::store_contracts::{task_store_contract, task_store_optional_contract};

// -- TaskStore ----------------------------------------------------------------

#[tokio::test]
async fn memory_task_store_satisfies_the_contract() {
    task_store_contract(MemoryTaskStore::new).await;
    task_store_optional_contract(MemoryTaskStore::new).await;
}

#[tokio::test]
async fn memory_task_store_satisfies_the_contract_with_aggressive_cleanup() {
    // A cleanup pass every millisecond physically removes expired records while
    // the suite is still reading, which the contract must not be able to see.
    let make = || {
        MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default().cleanup_interval(std::time::Duration::from_millis(1)),
        )
    };
    task_store_contract(make).await;
    task_store_optional_contract(make).await;
}

/// What the broken task store gets wrong.
#[derive(Clone, Copy)]
enum TaskFault {
    /// A completion is reported as applied for a task that already finished,
    /// and the later result replaces the recorded one.
    RewritesTerminalTasks,
    /// The owner is dropped when the task is created.
    ForgetsOwner,
    /// `create_task` returns a token the store never raises.
    DisconnectedToken,
    /// A spent input request key is accepted again.
    SwallowsSpentKeys,
    /// `set_ttl` reports that it did nothing.
    IgnoresSetTtl,
    /// The resume context carries no cancellation token.
    ResumesWithoutToken,
}

/// A real store with one fault injected.
struct BrokenTaskStore {
    inner: MemoryTaskStore,
    fault: TaskFault,
    /// Results written over already-terminal tasks (`RewritesTerminalTasks`).
    late_results: Mutex<HashMap<String, CallToolResult>>,
}

impl BrokenTaskStore {
    fn new(fault: TaskFault) -> Self {
        Self {
            inner: MemoryTaskStore::new(),
            fault,
            late_results: Mutex::new(HashMap::new()),
        }
    }

    fn late_result(&self, id: &str) -> Option<CallToolResult> {
        self.late_results.lock().unwrap().get(id).cloned()
    }
}

#[async_trait]
impl TaskStore for BrokenTaskStore {
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: TaskOwner,
    ) -> Result<(String, CancellationToken)> {
        let owner = match self.fault {
            TaskFault::ForgetsOwner => None,
            _ => owner,
        };
        let (id, token) = self
            .inner
            .create_task(tool_name, arguments, ttl, owner)
            .await?;
        let token = match self.fault {
            TaskFault::DisconnectedToken => CancellationToken::new(),
            _ => token,
        };
        Ok((id, token))
    }

    async fn task_owner(&self, id: &str) -> Result<Option<TaskOwner>> {
        self.inner.task_owner(id).await
    }

    async fn task_presence(&self, id: &str) -> Result<TaskPresence> {
        self.inner.task_presence(id).await
    }

    async fn get_task(&self, id: &str) -> Result<Option<TaskObject>> {
        self.inner.get_task(id).await
    }

    async fn get_task_result(&self, id: &str) -> Result<Option<TaskSnapshot>> {
        let snapshot = self.inner.get_task_result(id).await?;
        Ok(match (snapshot, self.late_result(id)) {
            (Some((task, _, error)), Some(late)) => Some((task, Some(late), error)),
            (snapshot, _) => snapshot,
        })
    }

    async fn wait_for_completion(&self, id: &str) -> Result<Option<TaskSnapshot>> {
        self.inner.wait_for_completion(id).await
    }

    async fn list_tasks(&self, status: Option<TaskStatus>) -> Result<Vec<TaskObject>> {
        self.inner.list_tasks(status).await
    }

    async fn require_input(
        &self,
        id: &str,
        requests: InputRequests,
        message: Option<&str>,
    ) -> Result<bool> {
        let result = self.inner.require_input(id, requests, message).await;
        match (self.fault, result) {
            (TaskFault::SwallowsSpentKeys, Err(TaskStoreError::InvalidTransition(_))) => Ok(true),
            (_, result) => result,
        }
    }

    async fn outstanding_input_requests(&self, id: &str) -> Result<Option<InputRequests>> {
        self.inner.outstanding_input_requests(id).await
    }

    async fn apply_input_responses(
        &self,
        id: &str,
        responses: InputResponses,
    ) -> Result<Option<AppliedInputResponses>> {
        self.inner.apply_input_responses(id, responses).await
    }

    async fn input_responses(&self, id: &str) -> Result<Option<InputResponses>> {
        self.inner.input_responses(id).await
    }

    async fn set_status(
        &self,
        id: &str,
        status: TaskStatus,
        message: Option<&str>,
    ) -> Result<bool> {
        self.inner.set_status(id, status, message).await
    }

    async fn resume_context(&self, id: &str) -> Result<Option<TaskResumeContext>> {
        let mut resume = self.inner.resume_context(id).await?;
        if let (TaskFault::ResumesWithoutToken, Some(resume)) = (self.fault, resume.as_mut()) {
            resume.cancellation_token = None;
        }
        Ok(resume)
    }

    async fn set_ttl(&self, id: &str, ttl_ms: u64) -> Result<bool> {
        match self.fault {
            TaskFault::IgnoresSetTtl => Ok(false),
            _ => self.inner.set_ttl(id, ttl_ms).await,
        }
    }

    async fn set_task_meta(&self, id: &str, meta: serde_json::Value) -> Result<bool> {
        self.inner.set_task_meta(id, meta).await
    }

    async fn discard_task(&self, id: &str) -> Result<bool> {
        self.inner.discard_task(id).await
    }

    async fn complete_task(&self, id: &str, result: CallToolResult) -> Result<bool> {
        let applied = self.inner.complete_task(id, result.clone()).await?;
        if !applied
            && matches!(self.fault, TaskFault::RewritesTerminalTasks)
            && self.inner.get_task(id).await?.is_some()
        {
            self.late_results
                .lock()
                .unwrap()
                .insert(id.to_string(), result);
            return Ok(true);
        }
        Ok(applied)
    }

    async fn fail_task(&self, id: &str, error: JsonRpcError) -> Result<bool> {
        self.inner.fail_task(id, error).await
    }

    async fn cancel_task(&self, id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
        self.inner.cancel_task(id, reason).await
    }
}

#[tokio::test]
async fn a_broken_task_store_with_no_fault_passes() {
    // The wrapper's plumbing is not what fails the tests below: with the fault
    // switched to one this suite does not exercise, everything passes.
    task_store_contract(|| BrokenTaskStore::new(TaskFault::ResumesWithoutToken)).await;
}

#[tokio::test]
#[should_panic(expected = "TaskStore contract violated: terminal states are immutable")]
async fn task_suite_catches_a_terminal_task_that_changes_state() {
    task_store_contract(|| BrokenTaskStore::new(TaskFault::RewritesTerminalTasks)).await;
}

#[tokio::test]
#[should_panic(
    expected = "TaskStore contract violated: task_owner reports the owner given to create_task, unchanged"
)]
async fn task_suite_catches_a_dropped_owner() {
    task_store_contract(|| BrokenTaskStore::new(TaskFault::ForgetsOwner)).await;
}

#[tokio::test]
#[should_panic(
    expected = "TaskStore contract violated: cancel_task cancels a working task and raises its token"
)]
async fn task_suite_catches_a_token_the_store_never_raises() {
    task_store_contract(|| BrokenTaskStore::new(TaskFault::DisconnectedToken)).await;
}

#[tokio::test]
#[should_panic(
    expected = "TaskStore contract violated: a superseded request key can never be reissued"
)]
async fn task_suite_catches_a_reusable_request_key() {
    task_store_contract(|| BrokenTaskStore::new(TaskFault::SwallowsSpentKeys)).await;
}

#[tokio::test]
#[should_panic(expected = "TaskStore contract violated: set_ttl applies to a live task")]
async fn task_suite_catches_a_set_ttl_that_does_nothing() {
    task_store_contract(|| BrokenTaskStore::new(TaskFault::IgnoresSetTtl)).await;
}

#[tokio::test]
#[should_panic(
    expected = "TaskStore contract violated: the resume context carries the task's cancellation token"
)]
async fn optional_task_suite_catches_a_resume_context_without_a_token() {
    task_store_optional_contract(|| BrokenTaskStore::new(TaskFault::ResumesWithoutToken)).await;
}

/// A store that keeps every default. It compiles, which is the trap: the
/// optional suite is what shows it cannot resume, tag, or discard a task.
struct DefaultsOnlyStore(MemoryTaskStore);

#[async_trait]
impl TaskStore for DefaultsOnlyStore {
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: TaskOwner,
    ) -> Result<(String, CancellationToken)> {
        self.0.create_task(tool_name, arguments, ttl, owner).await
    }
    async fn task_owner(&self, id: &str) -> Result<Option<TaskOwner>> {
        self.0.task_owner(id).await
    }
    async fn get_task(&self, id: &str) -> Result<Option<TaskObject>> {
        self.0.get_task(id).await
    }
    async fn get_task_result(&self, id: &str) -> Result<Option<TaskSnapshot>> {
        self.0.get_task_result(id).await
    }
    async fn wait_for_completion(&self, id: &str) -> Result<Option<TaskSnapshot>> {
        self.0.wait_for_completion(id).await
    }
    async fn list_tasks(&self, status: Option<TaskStatus>) -> Result<Vec<TaskObject>> {
        self.0.list_tasks(status).await
    }
    async fn require_input(
        &self,
        id: &str,
        requests: InputRequests,
        message: Option<&str>,
    ) -> Result<bool> {
        self.0.require_input(id, requests, message).await
    }
    async fn outstanding_input_requests(&self, id: &str) -> Result<Option<InputRequests>> {
        self.0.outstanding_input_requests(id).await
    }
    async fn apply_input_responses(
        &self,
        id: &str,
        responses: InputResponses,
    ) -> Result<Option<AppliedInputResponses>> {
        self.0.apply_input_responses(id, responses).await
    }
    async fn set_ttl(&self, id: &str, ttl_ms: u64) -> Result<bool> {
        self.0.set_ttl(id, ttl_ms).await
    }
    async fn complete_task(&self, id: &str, result: CallToolResult) -> Result<bool> {
        self.0.complete_task(id, result).await
    }
    async fn fail_task(&self, id: &str, error: JsonRpcError) -> Result<bool> {
        self.0.fail_task(id, error).await
    }
    async fn cancel_task(&self, id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
        self.0.cancel_task(id, reason).await
    }
}

#[tokio::test]
async fn a_store_that_keeps_the_defaults_passes_the_required_suite() {
    task_store_contract(|| DefaultsOnlyStore(MemoryTaskStore::new())).await;
}

#[tokio::test]
#[should_panic(expected = "TaskStore contract violated: set_status records a non-terminal status")]
async fn optional_task_suite_rejects_a_store_that_keeps_the_defaults() {
    task_store_optional_contract(|| DefaultsOnlyStore(MemoryTaskStore::new())).await;
}

// -- SessionStore and EventStore ----------------------------------------------

#[cfg(any(feature = "http", feature = "websocket"))]
mod sessions_and_events {
    use async_trait::async_trait;
    use tower_mcp::event_store::{
        CachingEventStore, EventRecord, EventStore, MemoryEventStore, Result as EventResult,
    };
    use tower_mcp::session_store::{
        CachingSessionStore, MemorySessionStore, Result as SessionResult, SessionRecord,
        SessionStore,
    };
    use tower_mcp::testing::store_contracts::{event_store_contract, session_store_contract};

    #[tokio::test]
    async fn memory_session_store_satisfies_the_contract() {
        session_store_contract(MemorySessionStore::new).await;
    }

    #[tokio::test]
    async fn caching_session_store_satisfies_the_contract() {
        session_store_contract(|| {
            CachingSessionStore::new(MemorySessionStore::new(), MemorySessionStore::new())
        })
        .await;
    }

    #[tokio::test]
    async fn memory_event_store_satisfies_the_contract() {
        event_store_contract(MemoryEventStore::new).await;
    }

    #[tokio::test]
    async fn caching_event_store_satisfies_the_contract() {
        event_store_contract(|| {
            CachingEventStore::new(MemoryEventStore::new(), MemoryEventStore::new())
        })
        .await;
    }

    #[derive(Clone, Copy)]
    enum SessionFault {
        /// The principal is dropped on the way in.
        DropsPrincipal,
        /// `create` overwrites a record that has the same ID.
        OverwritesOnCollision,
        /// `save` keeps the old optional fields when the new ones are `None`.
        MergesOnSave,
        /// Records stay loadable for an hour past their `expires_at`.
        OutlivesExpiry,
    }

    /// The record with its lifetime stretched by an hour.
    fn outliving(record: &SessionRecord) -> SessionRecord {
        let mut stretched = record.clone();
        stretched.expires_at += std::time::Duration::from_secs(3600);
        stretched
    }

    struct BrokenSessionStore {
        inner: MemorySessionStore,
        fault: SessionFault,
    }

    impl BrokenSessionStore {
        fn new(fault: SessionFault) -> Self {
            Self {
                inner: MemorySessionStore::new(),
                fault,
            }
        }
    }

    #[async_trait]
    impl SessionStore for BrokenSessionStore {
        async fn create(&self, record: &mut SessionRecord) -> SessionResult<()> {
            match self.fault {
                SessionFault::DropsPrincipal => {
                    let mut stripped = record.clone();
                    stripped.principal = None;
                    self.inner.create(&mut stripped).await?;
                    record.id = stripped.id;
                    Ok(())
                }
                SessionFault::OverwritesOnCollision => self.inner.save(record).await,
                SessionFault::MergesOnSave => self.inner.create(record).await,
                SessionFault::OutlivesExpiry => {
                    let mut stretched = outliving(record);
                    self.inner.create(&mut stretched).await?;
                    record.id = stretched.id;
                    Ok(())
                }
            }
        }

        async fn save(&self, record: &SessionRecord) -> SessionResult<()> {
            match self.fault {
                SessionFault::MergesOnSave => {
                    let mut merged = record.clone();
                    if let Some(existing) = self.inner.load(&record.id).await? {
                        merged.client_info = merged.client_info.or(existing.client_info);
                        merged.client_capabilities =
                            merged.client_capabilities.or(existing.client_capabilities);
                        merged.principal = merged.principal.or(existing.principal);
                    }
                    self.inner.save(&merged).await
                }
                SessionFault::OutlivesExpiry => self.inner.save(&outliving(record)).await,
                _ => self.inner.save(record).await,
            }
        }

        async fn load(&self, id: &str) -> SessionResult<Option<SessionRecord>> {
            self.inner.load(id).await
        }

        async fn delete(&self, id: &str) -> SessionResult<()> {
            self.inner.delete(id).await
        }
    }

    #[tokio::test]
    #[should_panic(
        expected = "SessionStore contract violated: a created record loads back with every field intact"
    )]
    async fn session_suite_catches_a_dropped_principal() {
        session_store_contract(|| BrokenSessionStore::new(SessionFault::DropsPrincipal)).await;
    }

    #[tokio::test]
    #[should_panic(
        expected = "SessionStore contract violated: create keeps a colliding record apart from the existing session"
    )]
    async fn session_suite_catches_a_create_that_overwrites() {
        session_store_contract(|| BrokenSessionStore::new(SessionFault::OverwritesOnCollision))
            .await;
    }

    #[tokio::test]
    #[should_panic(
        expected = "SessionStore contract violated: an overwrite replaces the whole record, including clearing optional fields"
    )]
    async fn session_suite_catches_a_save_that_merges() {
        session_store_contract(|| BrokenSessionStore::new(SessionFault::MergesOnSave)).await;
    }

    #[tokio::test]
    #[should_panic(
        expected = "SessionStore contract violated: load answers None for an expired record"
    )]
    async fn session_suite_catches_an_expired_record_that_still_loads() {
        session_store_contract(|| BrokenSessionStore::new(SessionFault::OutlivesExpiry)).await;
    }

    #[derive(Clone, Copy)]
    enum EventFault {
        /// Events come back newest first.
        ReplaysOutOfOrder,
        /// `replay_after(n)` includes event `n` itself.
        InclusiveReplay,
        /// `purge_session` does nothing.
        IgnoresPurge,
    }

    struct BrokenEventStore {
        inner: MemoryEventStore,
        fault: EventFault,
    }

    impl BrokenEventStore {
        fn new(fault: EventFault) -> Self {
            Self {
                inner: MemoryEventStore::new(),
                fault,
            }
        }
    }

    #[async_trait]
    impl EventStore for BrokenEventStore {
        async fn append(&self, session_id: &str, event: EventRecord) -> EventResult<()> {
            self.inner.append(session_id, event).await
        }

        async fn replay_after(
            &self,
            session_id: &str,
            after_id: u64,
        ) -> EventResult<Vec<EventRecord>> {
            match self.fault {
                EventFault::ReplaysOutOfOrder => {
                    let mut events = self.inner.replay_after(session_id, after_id).await?;
                    events.reverse();
                    Ok(events)
                }
                EventFault::InclusiveReplay => {
                    self.inner
                        .replay_after(session_id, after_id.saturating_sub(1))
                        .await
                }
                EventFault::IgnoresPurge => self.inner.replay_after(session_id, after_id).await,
            }
        }

        async fn purge_session(&self, session_id: &str) -> EventResult<()> {
            match self.fault {
                EventFault::IgnoresPurge => Ok(()),
                _ => self.inner.purge_session(session_id).await,
            }
        }
    }

    #[tokio::test]
    #[should_panic(
        expected = "EventStore contract violated: replay_after returns events with ids strictly greater than after_id, in order"
    )]
    async fn event_suite_catches_replay_out_of_order() {
        event_store_contract(|| BrokenEventStore::new(EventFault::ReplaysOutOfOrder)).await;
    }

    #[tokio::test]
    #[should_panic(
        expected = "EventStore contract violated: replay_after returns events with ids strictly greater than after_id, in order"
    )]
    async fn event_suite_catches_an_inclusive_replay() {
        event_store_contract(|| BrokenEventStore::new(EventFault::InclusiveReplay)).await;
    }

    #[tokio::test]
    #[should_panic(
        expected = "EventStore contract violated: purge_session removes every event for the session"
    )]
    async fn event_suite_catches_a_purge_that_does_nothing() {
        event_store_contract(|| BrokenEventStore::new(EventFault::IgnoresPurge)).await;
    }
}
