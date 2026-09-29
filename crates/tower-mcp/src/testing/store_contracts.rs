//! Reusable contract suites for the pluggable store traits.
//!
//! [`TaskStore`](crate::async_task::TaskStore),
//! [`SessionStore`](crate::session_store::SessionStore), and
//! [`EventStore`](crate::event_store::EventStore) are the extension points an
//! external backend implements to share state across server instances. Each
//! trait documents behavior that the transports and the router depend on, and
//! most of it is invisible to the compiler: terminal task states are
//! immutable, a session record's principal must come back unchanged, replay
//! returns events in order. A store that compiles can still break any of it.
//!
//! Each suite here exercises one trait through its public methods and panics,
//! naming the rule that was broken, when the implementation disagrees with the
//! trait documentation:
//!
//! | Suite | Trait | Feature |
//! |-------|-------|---------|
//! | [`task_store_contract`] and [`task_store_optional_contract`] | [`TaskStore`](crate::async_task::TaskStore) | `testing` |
//! | `session_store_contract` | [`SessionStore`](crate::session_store::SessionStore) | `testing` and `http` or `websocket` |
//! | `event_store_contract` | [`EventStore`](crate::event_store::EventStore) | `testing` and `http` or `websocket` |
//!
//! The suites assert documented behavior only. Where a trait leaves something
//! open, such as whether a session store hides an expired record, the suite
//! does not check it, so a correct implementation is never rejected for a
//! choice the trait allows.
//!
//! # Running a suite
//!
//! A suite takes a constructor and calls it once per check, so every check
//! starts from an empty store and one check cannot leak state into the next.
//! Call it from an ordinary async test:
//!
//! ```rust
//! use tower_mcp::async_task::MemoryTaskStore;
//! use tower_mcp::testing::store_contracts::task_store_contract;
//!
//! # #[tokio::main]
//! # async fn main() {
//! task_store_contract(MemoryTaskStore::new).await;
//! # }
//! ```
//!
//! For a store backed by a shared service, the constructor must hand back a
//! store whose records cannot collide with those of an earlier call, for
//! example by using a fresh key prefix or database. The suites tolerate other
//! records already being present in a shared backend where the trait allows it
//! (a task listing is checked for the tasks the suite created, not for
//! exact equality), but they do not clean up after themselves.
//!
//! A failing suite panics with a message of the form
//! `TaskStore contract violated: <rule>: <detail>`, so the failing test names
//! what to fix.
//!
//! # Time
//!
//! Expiry checks never sleep for a fixed duration. They set a time-to-live of
//! zero or one millisecond and then wait, polling, for the store to act, up to
//! a bound of a few seconds that is reached only when the store never does.
//! Do not run a suite under `tokio::time::pause`, because the bound would
//! then elapse without real time passing.

use std::fmt;

/// Panic with the standard contract-violation message.
#[track_caller]
fn violated(kind: &str, rule: &str, detail: fmt::Arguments<'_>) -> ! {
    panic!("{kind} contract violated: {rule}: {detail}")
}

/// Assert a documented rule, naming it in the panic message.
///
/// Resolves `KIND` at the use site, so each suite module defines its own.
macro_rules! check {
    ($cond:expr, $rule:expr) => {
        if !$cond {
            violated(
                KIND,
                $rule,
                format_args!("`{}` did not hold", stringify!($cond)),
            )
        }
    };
    ($cond:expr, $rule:expr, $($detail:tt)+) => {
        if !$cond {
            violated(KIND, $rule, format_args!($($detail)+))
        }
    };
}

/// Unwrap a store result, treating an unexpected error as a violation of
/// `$rule`.
macro_rules! ok {
    ($rule:expr, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => violated(
                KIND,
                $rule,
                format_args!("the store returned an unexpected error: {error:?}"),
            ),
        }
    };
}

#[cfg(any(feature = "http", feature = "websocket"))]
mod event;
#[cfg(any(feature = "http", feature = "websocket"))]
mod session;
mod task;

#[cfg(any(feature = "http", feature = "websocket"))]
pub use event::event_store_contract;
#[cfg(any(feature = "http", feature = "websocket"))]
pub use session::session_store_contract;
pub use task::{task_store_contract, task_store_optional_contract};
