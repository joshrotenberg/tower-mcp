//! Server-to-client requests waiting for the client's response.
//!
//! Every transport that carries legacy (2025-11-25) server-to-client
//! requests (`sampling/createMessage`, `elicitation/create`, ...) keeps a
//! map from the request id it wrote to the channel the waiting handler
//! listens on. This module is that map, shared by the stdio, WebSocket,
//! HTTP and in-process channel transports.
//!
//! It also notices when the waiting side gives up. A handler that times
//! out, is cancelled, or simply drops the future returned by
//! [`RequestContext::sample`](crate::RequestContext::sample) or
//! [`RequestContext::elicit_form`](crate::RequestContext::elicit_form)
//! drops the receiving half of its response channel. The registry removes
//! the entry and reports the id on the transport's abandon channel, and the
//! transport tells the client with `notifications/cancelled` (#1705), so a
//! host UI does not keep the question open until its own deadline.
//!
//! Every outcome is decided under the registry's one lock, by whichever
//! operation removes the entry: a response ([`complete`]), a transport
//! failure ([`fail`]), the originating request finishing ([`settle`]),
//! shutdown ([`clear`]), or abandonment by the waiter. Only abandonment
//! produces a cancellation, so a request that already received its
//! response, or whose waiter was told it failed, is never cancelled.
//!
//! The registry is owned by its transport. Watcher tasks hold only a weak
//! reference and stop as soon as their entry is removed, so dropping or
//! clearing the transport's registry releases every waiter and every
//! watcher.
//!
//! [`complete`]: PendingClientRequests::complete
//! [`fail`]: PendingClientRequests::fail
//! [`settle`]: PendingClientRequests::settle
//! [`clear`]: PendingClientRequests::clear

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::Poll;

use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::protocol::{CancelledParams, JsonRpcNotification, RequestId, notifications};

/// The `reason` sent with `notifications/cancelled` for an abandoned request.
///
/// The transport only sees the waiting future go away, so it cannot tell a
/// handler timeout from a cancelled handler; one fixed reason covers both.
pub(crate) const ABANDONED_REASON: &str = "server abandoned the request";

/// How many abandoned ids are remembered, so a late reply to one can be
/// told apart from a reply to an id this server never issued.
const REMEMBERED_ABANDONED: usize = 64;

/// Where the registry reports the ids of abandoned requests.
pub(crate) type AbandonSender = mpsc::UnboundedSender<RequestId>;

/// The transport's end of [`AbandonSender`].
pub(crate) type AbandonReceiver = mpsc::UnboundedReceiver<RequestId>;

/// Create the channel a transport drains to emit `notifications/cancelled`.
pub(crate) fn abandon_channel() -> (AbandonSender, AbandonReceiver) {
    mpsc::unbounded_channel()
}

/// Responder handed to the waiting handler by `ChannelClientRequester`.
type Responder = oneshot::Sender<Result<serde_json::Value>>;

struct Entry {
    responder: Responder,
    /// Dropped with the entry, which stops its watcher.
    _removed: oneshot::Sender<()>,
}

#[derive(Default)]
struct State {
    entries: HashMap<RequestId, Entry>,
    /// Recently abandoned ids, oldest first.
    abandoned: VecDeque<RequestId>,
}

impl State {
    /// Remove an entry whose waiter is gone and remember its id.
    fn abandon(&mut self, id: &RequestId) -> bool {
        if self.entries.remove(id).is_none() {
            return false;
        }
        if self.abandoned.len() == REMEMBERED_ABANDONED {
            self.abandoned.pop_front();
        }
        self.abandoned.push_back(id.clone());
        true
    }
}

type Shared = Mutex<State>;

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    // Nothing panics while holding this lock, but a poisoned map is still a
    // valid map, so recover it rather than propagate.
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What became of a response the client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// Delivered to the waiting handler.
    Delivered,
    /// A late reply to a request this server already abandoned and
    /// cancelled. Expected; not worth more than a debug log.
    Abandoned,
    /// No request with this id is pending or was recently abandoned.
    Unknown,
}

impl Completion {
    /// Log a response that did not reach a waiter, at a level matching how
    /// surprising it is.
    pub(crate) fn log_undelivered(self, id: &RequestId) {
        match self {
            Completion::Delivered => {}
            Completion::Abandoned => {
                tracing::debug!(id = ?id, "Ignoring late response to an abandoned request");
            }
            Completion::Unknown => {
                tracing::warn!(id = ?id, "Received response for unknown request");
            }
        }
    }
}

/// Server-to-client requests that have been written and not yet resolved.
///
/// Cloning shares the same map.
#[derive(Clone, Default)]
pub(crate) struct PendingClientRequests {
    shared: Arc<Shared>,
}

impl PendingClientRequests {
    /// Create an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Track a request the transport is about to write.
    ///
    /// Returns `false`, registering nothing, if the waiter has already gone
    /// away; the transport should then not write the request at all.
    ///
    /// Register before writing, and drain `on_abandon` on the same task that
    /// writes requests, so a cancellation can never reach the client ahead of
    /// the request it cancels.
    pub(crate) fn register(
        &self,
        id: RequestId,
        responder: Responder,
        on_abandon: AbandonSender,
    ) -> bool {
        if responder.is_closed() {
            return false;
        }
        let (removed_tx, removed_rx) = oneshot::channel();
        lock(&self.shared).entries.insert(
            id.clone(),
            Entry {
                responder,
                _removed: removed_tx,
            },
        );
        tokio::spawn(watch(
            Arc::downgrade(&self.shared),
            id,
            removed_rx,
            on_abandon,
        ));
        true
    }

    /// Deliver the client's response.
    pub(crate) fn complete(&self, id: &RequestId, result: Result<serde_json::Value>) -> Completion {
        let mut state = lock(&self.shared);
        if let Some(entry) = state.entries.remove(id) {
            drop(state);
            let _ = entry.responder.send(result);
            return Completion::Delivered;
        }
        match state.abandoned.iter().position(|abandoned| abandoned == id) {
            Some(index) => {
                state.abandoned.remove(index);
                Completion::Abandoned
            }
            None => Completion::Unknown,
        }
    }

    /// Fail the listed requests with `message`. Nothing is reported as
    /// abandoned, because the transport can no longer reach the client.
    pub(crate) fn fail(&self, ids: &[RequestId], message: &str) {
        let removed: Vec<Entry> = {
            let mut state = lock(&self.shared);
            ids.iter()
                .filter_map(|id| state.entries.remove(id))
                .collect()
        };
        for entry in removed {
            let _ = entry
                .responder
                .send(Err(Error::Transport(message.to_string())));
        }
    }

    /// Resolve the listed requests because the request that issued them
    /// finished while the client can still be reached.
    ///
    /// Under one lock, each request still pending is either abandoned, when
    /// its waiter is already gone, or failed with `message`, when something
    /// (such as a task holding a cloned context) is still waiting. Returns
    /// the abandoned ids, which the caller cancels on the wire; a failed
    /// request is never also cancelled. Ids a watcher already reported are
    /// on the abandon channel, not in the result.
    #[cfg(any(feature = "http", test))]
    pub(crate) fn settle(&self, ids: &[RequestId], message: &str) -> Vec<RequestId> {
        let mut abandoned = Vec::new();
        let mut failed = Vec::new();
        {
            let mut state = lock(&self.shared);
            for id in ids {
                let Some(entry) = state.entries.get(id) else {
                    continue;
                };
                if entry.responder.is_closed() {
                    state.abandon(id);
                    abandoned.push(id.clone());
                } else if let Some(entry) = state.entries.remove(id) {
                    failed.push(entry);
                }
            }
        }
        for entry in failed {
            let _ = entry
                .responder
                .send(Err(Error::Transport(message.to_string())));
        }
        abandoned
    }

    /// Drop every entry, as when the connection ends. Waiters see their
    /// response channel close; nothing is reported as abandoned.
    pub(crate) fn clear(&self) {
        let entries = std::mem::take(&mut lock(&self.shared).entries);
        drop(entries);
    }

    /// A guard that clears this registry when dropped, so every way out of
    /// a transport loop (including an error return or a dropped future)
    /// releases its waiters.
    pub(crate) fn clear_on_drop(&self) -> ClearOnDrop {
        ClearOnDrop(self.clone())
    }

    #[cfg(test)]
    fn contains(&self, id: &RequestId) -> bool {
        lock(&self.shared).entries.contains_key(id)
    }
}

/// Clears a [`PendingClientRequests`] when dropped.
pub(crate) struct ClearOnDrop(PendingClientRequests);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// Wait for the waiter of `id` to go away, then abandon the request.
///
/// Ends without reporting anything once the entry is removed by any other
/// operation (`removed` resolves when the entry, and with it the sender, is
/// dropped) or the registry itself is gone.
async fn watch(
    shared: Weak<Shared>,
    id: RequestId,
    removed: oneshot::Receiver<()>,
    on_abandon: AbandonSender,
) {
    let waiter_gone = std::future::poll_fn(|cx| {
        let Some(shared) = shared.upgrade() else {
            return Poll::Ready(());
        };
        let mut state = lock(&shared);
        let Some(entry) = state.entries.get_mut(&id) else {
            return Poll::Ready(());
        };
        if entry.responder.poll_closed(cx).is_pending() {
            return Poll::Pending;
        }
        // Remove and report under the lock, so an operation that takes the
        // lock afterwards either still finds the entry or finds its id
        // already queued on `on_abandon`, never neither.
        if state.abandon(&id) {
            let _ = on_abandon.send(id.clone());
        }
        Poll::Ready(())
    });
    tokio::select! {
        _ = removed => {}
        () = waiter_gone => {}
    }
}

/// Serialize the `notifications/cancelled` frame for an abandoned request.
pub(crate) fn cancelled_notification_json(id: &RequestId) -> Option<String> {
    let params = CancelledParams {
        request_id: Some(id.clone()),
        reason: Some(ABANDONED_REASON.to_string()),
        meta: None,
    };
    let params = serde_json::to_value(params).ok()?;
    let notification = JsonRpcNotification::new(notifications::CANCELLED).with_params(params);
    match serde_json::to_string(&notification) {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::error!(%error, "Failed to serialize notifications/cancelled");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const ID: RequestId = RequestId::Number(7);

    /// The next abandoned id, or `None` once every sender is gone. Fails the
    /// test if neither happens promptly, so a leaked watcher cannot pass for
    /// a closed channel.
    async fn next_abandoned(rx: &mut AbandonReceiver) -> Option<RequestId> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("abandon channel neither reported nor closed: a watcher leaked")
    }

    async fn assert_closed_without_report(rx: &mut AbandonReceiver) {
        assert_eq!(next_abandoned(rx).await, None, "nothing may be abandoned");
    }

    /// Let spawned watchers run until they block.
    async fn settle_tasks() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn dropped_waiter_reports_abandonment_once() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));

        drop(rx);

        assert_eq!(next_abandoned(&mut abandon_rx).await, Some(ID));
        assert_closed_without_report(&mut abandon_rx).await;
        assert!(!pending.contains(&ID));
        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Abandoned);
        // Only the first late reply is recognized; the id is forgotten after.
        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Unknown);
    }

    #[tokio::test]
    async fn completed_request_is_never_abandoned() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));

        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Delivered);
        assert_eq!(rx.await.unwrap().unwrap(), json());
        assert_closed_without_report(&mut abandon_rx).await;
    }

    /// The response is removed first, then the waiter goes away: delivered,
    /// never abandoned.
    #[tokio::test]
    async fn response_before_waiter_drop_is_delivered() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));
        settle_tasks().await; // the watcher is parked on the open waiter

        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Delivered);
        drop(rx);
        assert_closed_without_report(&mut abandon_rx).await;
    }

    /// The waiter goes away and the watcher removes the entry first, then
    /// the response arrives: abandoned, and the reply is recognized as late.
    #[tokio::test]
    async fn waiter_drop_before_response_is_abandoned() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));
        settle_tasks().await;

        drop(rx);
        assert_eq!(next_abandoned(&mut abandon_rx).await, Some(ID));
        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Abandoned);
        assert_closed_without_report(&mut abandon_rx).await;
    }

    /// The waiter goes away but the response takes the entry before the
    /// watcher runs: the response wins and nothing is cancelled.
    #[tokio::test]
    async fn response_racing_an_unobserved_waiter_drop_wins() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));
        settle_tasks().await;

        // No await between these two: the current-thread runtime cannot run
        // the watcher in between.
        drop(rx);
        assert_eq!(pending.complete(&ID, Ok(json())), Completion::Delivered);
        assert_closed_without_report(&mut abandon_rx).await;
    }

    #[tokio::test]
    async fn failed_and_cleared_requests_are_not_abandoned() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        assert!(pending.register(RequestId::Number(1), tx1, abandon_tx.clone()));
        assert!(pending.register(RequestId::Number(2), tx2, abandon_tx));

        pending.fail(&[RequestId::Number(1), RequestId::Number(9)], "gone");
        assert!(matches!(rx1.await.unwrap(), Err(Error::Transport(m)) if m == "gone"));

        pending.clear();
        assert!(rx2.await.is_err(), "cleared waiter sees its channel close");
        assert_closed_without_report(&mut abandon_rx).await;
    }

    /// `settle` decides fail versus abandon per request, under one lock.
    #[tokio::test]
    async fn settle_fails_live_waiters_and_abandons_gone_ones() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (live_tx, live_rx) = oneshot::channel();
        let (gone_tx, gone_rx) = oneshot::channel();
        assert!(pending.register(RequestId::Number(1), live_tx, abandon_tx.clone()));
        assert!(pending.register(RequestId::Number(2), gone_tx, abandon_tx));
        settle_tasks().await;

        // The waiter for 2 leaves, but its watcher has not run yet.
        drop(gone_rx);
        let abandoned = pending.settle(
            &[
                RequestId::Number(1),
                RequestId::Number(2),
                RequestId::Number(3),
            ],
            "finished",
        );
        assert_eq!(abandoned, vec![RequestId::Number(2)]);
        assert!(matches!(live_rx.await.unwrap(), Err(Error::Transport(m)) if m == "finished"));
        // The watcher for 2 finds its entry gone and reports nothing more.
        assert_closed_without_report(&mut abandon_rx).await;
        assert_eq!(
            pending.complete(&RequestId::Number(2), Ok(json())),
            Completion::Abandoned
        );
        assert_eq!(
            pending.complete(&RequestId::Number(1), Ok(json())),
            Completion::Unknown
        );
    }

    #[tokio::test]
    async fn already_abandoned_request_is_not_registered() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        drop(rx);
        assert!(!pending.register(ID, tx, abandon_tx));
        assert!(!pending.contains(&ID));
        assert_closed_without_report(&mut abandon_rx).await;
    }

    /// Dropping the last registry handle releases the waiter and stops the
    /// watcher, even while the waiter is still open.
    #[tokio::test]
    async fn dropping_the_registry_releases_waiters_and_watchers() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));
        settle_tasks().await;

        drop(pending);
        assert!(rx.await.is_err(), "the waiter sees its channel close");
        assert_closed_without_report(&mut abandon_rx).await;
    }

    #[tokio::test]
    async fn clear_on_drop_guard_clears() {
        let pending = PendingClientRequests::new();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let (tx, rx) = oneshot::channel();
        assert!(pending.register(ID, tx, abandon_tx));
        drop(pending.clear_on_drop());
        assert!(rx.await.is_err());
        assert_closed_without_report(&mut abandon_rx).await;
    }

    #[test]
    fn remembers_a_bounded_number_of_abandoned_ids() {
        let mut state = State::default();
        for n in 0..(REMEMBERED_ABANDONED as i64 + 10) {
            let (responder, _rx) = oneshot::channel();
            let (removed, _) = oneshot::channel();
            state.entries.insert(
                RequestId::Number(n),
                Entry {
                    responder,
                    _removed: removed,
                },
            );
            assert!(state.abandon(&RequestId::Number(n)));
        }
        assert_eq!(state.abandoned.len(), REMEMBERED_ABANDONED);
        assert_eq!(state.abandoned.front(), Some(&RequestId::Number(10)));
    }

    #[test]
    fn cancelled_notification_carries_id_and_reason() {
        let json = cancelled_notification_json(&RequestId::Number(42)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "notifications/cancelled");
        assert_eq!(value["params"]["requestId"], 42);
        assert_eq!(value["params"]["reason"], ABANDONED_REASON);
        assert!(value.get("id").is_none());
    }

    fn json() -> serde_json::Value {
        serde_json::json!({"ok": true})
    }
}
