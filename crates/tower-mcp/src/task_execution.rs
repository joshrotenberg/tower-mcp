//! Lifecycle control for built-in live Task executions.
//!
//! A live task handler keeps running after its `tools/call` response has
//! returned. This registry gives the embedding application a process-lifetime
//! boundary around those detached futures without coupling transport shutdown
//! to an application cancellation policy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::context::CancellationToken;

/// A cloneable host-side handle for built-in live Task executions.
///
/// Obtain this from [`crate::McpRouter::live_task_execution_handle`] before
/// moving the router into a transport. The handle covers only handlers built
/// with [`crate::ToolBuilder::live_task_handler`] (and its context-aware
/// variant). Replay handlers and arbitrary records in a [`crate::TaskStore`]
/// are intentionally outside its scope.
///
/// Admission closure is permanent for this router. A typical graceful
/// shutdown closes admission when listener shutdown begins, lets the transport
/// finish its own drain, then either waits for live executions or requests
/// cancellation and waits with a caller-owned timeout.
#[derive(Clone)]
pub struct LiveTaskExecutionHandle {
    pub(crate) registry: Arc<LiveTaskExecutionRegistry>,
}

impl std::fmt::Debug for LiveTaskExecutionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveTaskExecutionHandle")
            .field("active_count", &self.active_count())
            .finish_non_exhaustive()
    }
}

impl LiveTaskExecutionHandle {
    pub(crate) fn new() -> Self {
        Self {
            registry: Arc::new(LiveTaskExecutionRegistry::new()),
        }
    }

    /// Permanently stop admitting new built-in live Task executions.
    ///
    /// Calls already admitted continue to preparation or execution and remain
    /// visible to [`active_count`](Self::active_count) and
    /// [`drained`](Self::drained). Calls rejected after this point fail before
    /// the [`crate::TaskStore`] allocates a Task record.
    pub fn close_admission(&self) {
        self.registry.close_admission();
    }

    /// Return the number of admitted executions that have not settled.
    ///
    /// This includes reservations whose task preparation has not reached the
    /// point where an ID can be published. Consequently this count can exceed
    /// the length of [`active_task_ids`](Self::active_task_ids).
    pub fn active_count(&self) -> usize {
        self.registry.active_count()
    }

    /// Return the IDs of admitted executions that have finished preparation.
    ///
    /// IDs are returned in lexical order for stable logs and diagnostics.
    /// Preparing reservations count as active but do not appear here until
    /// they atomically promote to an ID-bearing execution.
    pub fn active_task_ids(&self) -> Vec<String> {
        self.registry.active_task_ids()
    }

    /// Request cancellation of every execution admitted at this instant.
    ///
    /// Returns the number of reservations and ID-bearing executions signalled.
    /// The reason is propagated to the Task's terminal status when a handler
    /// observes cancellation without supplying a more specific message.
    ///
    /// This method does not close admission. During graceful shutdown call
    /// [`close_admission`](Self::close_admission) first if later executions
    /// must not escape this cancellation pass.
    pub fn cancel_all(&self, reason: impl Into<String>) -> usize {
        self.registry.cancel_all(reason.into())
    }

    /// Wait until every admitted execution has settled.
    ///
    /// Settlement means the handler future has left its lifecycle boundary and
    /// any terminal Task-store write has completed. This method deliberately
    /// has no built-in deadline; the embedding application owns shutdown
    /// timeout and escalation policy. It does not close admission, so call
    /// [`close_admission`](Self::close_admission) first when no execution may
    /// begin after this wait resolves.
    pub async fn drained(&self) {
        self.registry.drained().await;
    }
}

pub(crate) struct LiveTaskCancellation {
    token: CancellationToken,
    /// Store-owned cancellation/expiry signal, attached after the durable
    /// task record is created and before preparation begins.
    task_lifecycle: OnceLock<crate::async_task::CancellationToken>,
    state: Mutex<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    requested: bool,
    reason: Option<String>,
}

impl LiveTaskCancellation {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            task_lifecycle: OnceLock::new(),
            state: Mutex::new(CancellationState::default()),
        }
    }

    pub(crate) fn attach_task_lifecycle(&self, token: crate::async_task::CancellationToken) {
        let attached = self.task_lifecycle.set(token).is_ok();
        debug_assert!(attached, "a live execution has one task lifecycle token");
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
            || self
                .task_lifecycle
                .get()
                .is_some_and(crate::async_task::CancellationToken::is_cancelled)
    }

    pub(crate) async fn cancelled(&self) {
        match self.task_lifecycle.get() {
            Some(task_lifecycle) => {
                tokio::select! {
                    _ = self.token.cancelled() => {}
                    _ = task_lifecycle.cancelled() => {}
                }
            }
            None => self.token.cancelled().await,
        }
    }

    pub(crate) fn cancel(&self, reason: Option<String>) {
        {
            let mut state = lock_recover(&self.state);
            if !state.requested {
                state.requested = true;
                state.reason = reason;
            }
        }
        // Publish the reason before waking a waiter so it can immediately read
        // the reason associated with the signal it observed.
        self.token.cancel();
    }

    pub(crate) fn reason(&self) -> Option<String> {
        lock_recover(&self.state).reason.clone()
    }
}

pub(crate) struct LiveTaskExecutionRegistry {
    state: Mutex<RegistryState>,
    active_count_tx: tokio::sync::watch::Sender<usize>,
}

struct RegistryState {
    admission_open: bool,
    next_reservation: u64,
    reservations: HashMap<u64, Arc<LiveTaskCancellation>>,
    executions: HashMap<String, Arc<crate::tool::LiveTask>>,
}

impl LiveTaskExecutionRegistry {
    fn new() -> Self {
        let (active_count_tx, _active_count_rx) = tokio::sync::watch::channel(0);
        Self {
            state: Mutex::new(RegistryState {
                admission_open: true,
                next_reservation: 0,
                reservations: HashMap::new(),
                executions: HashMap::new(),
            }),
            active_count_tx,
        }
    }

    pub(crate) fn admit(self: &Arc<Self>) -> Option<LiveTaskAdmission> {
        let cancellation = Arc::new(LiveTaskCancellation::new());
        let reservation = {
            let mut state = lock_recover(&self.state);
            if !state.admission_open {
                return None;
            }
            let reservation = state.next_reservation;
            state.next_reservation = state.next_reservation.wrapping_add(1);
            state.reservations.insert(reservation, cancellation.clone());
            // Publish while holding the state lock. Otherwise two mutations
            // could compute ordered counts but send them in reverse order,
            // leaving a drain asleep on a stale nonzero value.
            self.active_count_tx.send_replace(state.active_count());
            reservation
        };
        Some(LiveTaskAdmission {
            registry: self.clone(),
            reservation: Some(reservation),
            cancellation,
        })
    }

    fn close_admission(&self) {
        lock_recover(&self.state).admission_open = false;
    }

    fn active_count(&self) -> usize {
        lock_recover(&self.state).active_count()
    }

    fn active_task_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = lock_recover(&self.state)
            .executions
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    fn cancel_all(&self, reason: String) -> usize {
        let cancellations: Vec<_> = {
            let state = lock_recover(&self.state);
            state
                .reservations
                .values()
                .cloned()
                .chain(
                    state
                        .executions
                        .values()
                        .map(|live| live.cancellation.clone()),
                )
                .collect()
        };
        let count = cancellations.len();
        for cancellation in cancellations {
            cancellation.cancel(Some(reason.clone()));
        }
        count
    }

    async fn drained(&self) {
        let mut active_count = self.active_count_tx.subscribe();
        while *active_count.borrow_and_update() != 0 {
            // The sender is owned by this registry and therefore cannot close
            // while this borrowed registry is alive.
            active_count
                .changed()
                .await
                .expect("live task execution count sender remains open");
        }
    }

    fn release_reservation(&self, reservation: u64) {
        let mut state = lock_recover(&self.state);
        state.reservations.remove(&reservation);
        self.active_count_tx.send_replace(state.active_count());
    }

    fn unregister(&self, task_id: &str) {
        let mut state = lock_recover(&self.state);
        state.executions.remove(task_id);
        self.active_count_tx.send_replace(state.active_count());
    }

    pub(crate) fn get(&self, task_id: &str) -> Option<Arc<crate::tool::LiveTask>> {
        lock_recover(&self.state).executions.get(task_id).cloned()
    }
}

impl RegistryState {
    fn active_count(&self) -> usize {
        self.reservations.len() + self.executions.len()
    }
}

/// An execution admitted before any durable Task record is allocated.
///
/// Dropping this value releases the reservation, including every error path
/// through task creation and preparation.
pub(crate) struct LiveTaskAdmission {
    registry: Arc<LiveTaskExecutionRegistry>,
    reservation: Option<u64>,
    cancellation: Arc<LiveTaskCancellation>,
}

impl LiveTaskAdmission {
    pub(crate) fn cancellation(&self) -> Arc<LiveTaskCancellation> {
        self.cancellation.clone()
    }

    pub(crate) fn promote(
        mut self,
        task_id: String,
        live: Arc<crate::tool::LiveTask>,
    ) -> LiveTaskRegistration {
        let reservation = self
            .reservation
            .take()
            .expect("a live task admission promotes only once");
        {
            let mut state = lock_recover(&self.registry.state);
            let removed = state.reservations.remove(&reservation);
            debug_assert!(
                removed.is_some(),
                "admission reservation remains registered"
            );
            let replaced = state.executions.insert(task_id.clone(), live);
            debug_assert!(replaced.is_none(), "task IDs are unique");
        }
        // Reservation removal and task insertion happen under one lock and do
        // not change the active count, so a drain can never observe a gap.
        LiveTaskRegistration {
            registry: self.registry.clone(),
            task_id,
        }
    }
}

impl Drop for LiveTaskAdmission {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.registry.release_reservation(reservation);
        }
    }
}

/// Releases an ID-bearing execution however its handler future leaves.
pub(crate) struct LiveTaskRegistration {
    registry: Arc<LiveTaskExecutionRegistry>,
    task_id: String,
}

impl Drop for LiveTaskRegistration {
    fn drop(&mut self) {
        self.registry.unregister(&self.task_id);
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
