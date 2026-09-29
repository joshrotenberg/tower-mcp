//! Contract suite for [`EventStore`].

use super::violated;
use crate::event_store::{EventRecord, EventStore};

const KIND: &str = "EventStore";

/// Run the [`EventStore`] contract against the store `make` builds.
///
/// `make` is called once per check and must return an empty store each time.
/// The suite covers:
///
/// - `replay_after` returns exactly the events whose IDs are strictly greater
///   than `after_id`, in ID order, with each payload preserved byte for byte;
/// - replay does not consume events, so a client that reconnects twice sees the
///   same events both times;
/// - a session with no events, whether unknown or purged, replays as an empty
///   list rather than an error;
/// - sessions are isolated: appending to or purging one leaves another alone;
/// - `purge_session` removes every event for the session, is idempotent, and
///   leaves the session able to record new events.
///
/// Events are appended with increasing IDs, as the transport does. The buffer
/// capacity of [`MemoryEventStore`](crate::event_store::MemoryEventStore) is
/// not checked because the trait does not define one, and neither is what a
/// store does with two events that share an ID. The record's timestamp is not
/// compared, because the trait does not fix its storage precision.
///
/// # Panics
///
/// Panics, naming the violated rule, at the first check the store fails.
///
/// # Example
///
/// ```rust
/// use tower_mcp::event_store::MemoryEventStore;
/// use tower_mcp::testing::store_contracts::event_store_contract;
///
/// # #[tokio::main]
/// # async fn main() {
/// event_store_contract(MemoryEventStore::new).await;
/// # }
/// ```
pub async fn event_store_contract<S, F>(make: F)
where
    S: EventStore,
    F: Fn() -> S,
{
    replay_is_strictly_after_and_ordered(&make()).await;
    payloads_are_preserved(&make()).await;
    replay_does_not_consume(&make()).await;
    unknown_sessions_replay_empty(&make()).await;
    sessions_are_isolated(&make()).await;
    purge_removes_the_session_log(&make()).await;
}

async fn append<S: EventStore>(store: &S, session: &str, id: u64, rule: &str) {
    ok!(
        rule,
        store
            .append(session, EventRecord::new(id, format!("event-{id}")))
            .await
    );
}

async fn replay<S: EventStore>(
    store: &S,
    session: &str,
    after: u64,
    rule: &str,
) -> Vec<EventRecord> {
    ok!(rule, store.replay_after(session, after).await)
}

fn ids(events: &[EventRecord]) -> Vec<u64> {
    events.iter().map(|event| event.id).collect()
}

async fn replay_is_strictly_after_and_ordered<S: EventStore>(store: &S) {
    // Enough events that an ordering by the decimal text of the ID (9, 10, 11
    // sorting as 10, 11, 9) would show.
    let rule = "replay_after returns events with ids strictly greater than after_id, in order";
    for id in 1..=25 {
        append(store, "s", id, rule).await;
    }

    let all = replay(store, "s", 0, rule).await;
    check!(
        ids(&all) == (1..=25).collect::<Vec<_>>(),
        rule,
        "replay_after(0) returned ids {:?}",
        ids(&all)
    );

    for after in [1, 9, 10, 24] {
        let tail = replay(store, "s", after, rule).await;
        check!(
            ids(&tail) == (after + 1..=25).collect::<Vec<_>>(),
            rule,
            "replay_after({after}) returned ids {:?}",
            ids(&tail)
        );
    }

    let none = replay(store, "s", 25, rule).await;
    check!(
        none.is_empty(),
        rule,
        "replay_after(25), the last id, returned {:?}",
        ids(&none)
    );
    let none = replay(store, "s", u64::MAX, rule).await;
    check!(
        none.is_empty(),
        rule,
        "replay_after(u64::MAX) returned {:?}",
        ids(&none)
    );
}

async fn payloads_are_preserved<S: EventStore>(store: &S) {
    let rule = "an appended event is replayed with its id and data unchanged";
    let payloads = [
        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}"#,
        "two\nlines\r\nand a trailing newline\n",
        "unicode: caf\u{e9} \u{1F980} \u{4e2d}\u{6587}",
        "",
    ];
    for (offset, data) in payloads.iter().enumerate() {
        let id = offset as u64 + 1;
        ok!(rule, store.append("s", EventRecord::new(id, *data)).await);
    }

    let replayed = replay(store, "s", 0, rule).await;
    check!(
        replayed.len() == payloads.len(),
        rule,
        "appended {} events, replayed {}",
        payloads.len(),
        replayed.len()
    );
    for (offset, (event, data)) in replayed.iter().zip(payloads).enumerate() {
        let id = offset as u64 + 1;
        check!(
            event.id == id && event.data == data,
            rule,
            "event {id} replayed as id {} with data {:?}, expected {data:?}",
            event.id,
            event.data
        );
    }
}

async fn replay_does_not_consume<S: EventStore>(store: &S) {
    let rule = "replaying does not consume events";
    for id in 1..=3 {
        append(store, "s", id, rule).await;
    }
    let first = replay(store, "s", 0, rule).await;
    let second = replay(store, "s", 0, rule).await;
    check!(
        ids(&first) == vec![1, 2, 3] && ids(&second) == vec![1, 2, 3],
        rule,
        "two replays returned {:?} and {:?}",
        ids(&first),
        ids(&second)
    );

    // Appending after a replay extends the log rather than restarting it.
    append(store, "s", 4, rule).await;
    let extended = replay(store, "s", 2, rule).await;
    check!(
        ids(&extended) == vec![3, 4],
        rule,
        "replay_after(2) after a later append returned {:?}",
        ids(&extended)
    );
}

async fn unknown_sessions_replay_empty<S: EventStore>(store: &S) {
    let rule = "a session with no events replays as an empty list";
    let events = replay(store, "no-such-session", 0, rule).await;
    check!(
        events.is_empty(),
        rule,
        "an unknown session replayed {:?}",
        ids(&events)
    );
}

async fn sessions_are_isolated<S: EventStore>(store: &S) {
    let rule = "sessions are isolated from one another";
    // Interleaved, so a store that keeps one shared log would mix them.
    for id in 1..=4 {
        append(store, "a", id, rule).await;
        append(store, "b", id + 100, rule).await;
    }

    let a = replay(store, "a", 0, rule).await;
    let b = replay(store, "b", 0, rule).await;
    check!(
        ids(&a) == vec![1, 2, 3, 4],
        rule,
        "session a replayed {:?}",
        ids(&a)
    );
    check!(
        ids(&b) == vec![101, 102, 103, 104],
        rule,
        "session b replayed {:?}",
        ids(&b)
    );

    let rule = "purging a session leaves other sessions untouched";
    ok!(rule, store.purge_session("a").await);
    let b = replay(store, "b", 0, rule).await;
    check!(
        ids(&b) == vec![101, 102, 103, 104],
        rule,
        "session b replayed {:?} after session a was purged",
        ids(&b)
    );
}

async fn purge_removes_the_session_log<S: EventStore>(store: &S) {
    let rule = "purge_session removes every event for the session";
    for id in 1..=5 {
        append(store, "s", id, rule).await;
    }
    ok!(rule, store.purge_session("s").await);
    let after = replay(store, "s", 0, rule).await;
    check!(
        after.is_empty(),
        rule,
        "replay_after(0) returned {:?} after the purge",
        ids(&after)
    );

    let rule = "purge_session is idempotent";
    ok!(rule, store.purge_session("s").await);
    ok!(rule, store.purge_session("never-existed").await);

    let rule = "a purged session can record new events";
    append(store, "s", 6, rule).await;
    let fresh = replay(store, "s", 0, rule).await;
    check!(
        ids(&fresh) == vec![6],
        rule,
        "replay_after(0) returned {:?} after a purge and one append",
        ids(&fresh)
    );
}
