//! Distinguishing an expired task from one that never existed (#1249).
//!
//! The distinction is useful to the owner and must never leak to anyone else.
//! `tasks/get` on another principal's task id has to answer exactly as it does
//! for an id that was never issued, whether that task is present, expired, or
//! absent. Otherwise it becomes an existence oracle: a caller can enumerate
//! task ids and learn which ones belong to somebody.
//!
//! These are store-level tests: they check that presence resolves correctly
//! and carries the owner. They deliberately do NOT prove the security
//! property, and it is worth being precise about why. The authorization
//! decision lives in the router, so a store-level test cannot see an
//! implementation that discloses expiry before checking ownership. I wrote one
//! that tried, installed exactly that bug, and watched it pass.
//!
//! The test that does prove it is
//! `router::tests::expiry_is_disclosed_to_the_owner_and_to_nobody_else`, which
//! drives the router with two principals and was confirmed to fail against the
//! naive implementation.

#![cfg(feature = "stateless")]

use std::sync::Arc;

use serde_json::json;
use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
use tower_mcp::async_task::{TaskPresence, owner_matches};
use tower_mcp::protocol::CallToolResult;

/// The whole point: an id belonging to someone else answers identically
/// whether that task is present, expired, or was never issued.
///
/// This currently passes because everything collapses to "not found". It must
/// keep passing once expired becomes distinguishable to the owner.
#[tokio::test]
async fn another_principals_tasks_are_indistinguishable_from_missing() {
    let store = Arc::new(MemoryTaskStore::new());

    // Owned by "alice", one long-lived and one that expires immediately.
    let (present, _) = store
        .create_task("work", json!({}), Some(60_000), Some("alice".into()))
        .await
        .expect("create");
    let (expired, _) = store
        .create_task("work", json!({}), Some(1), Some("alice".into()))
        .await
        .expect("create");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // Nothing was ever issued under this id.
    let missing = "never-issued";

    // As far as any other principal is concerned, all three are the same.
    for id in [present.as_str(), expired.as_str(), missing] {
        let owner = store.task_owner(id).await.expect("owner lookup");
        let visible_to_bob = owner
            .as_ref()
            .is_some_and(|o| owner_matches(o, Some("bob")));
        assert!(
            !visible_to_bob,
            "id {id} must not be attributable to another principal"
        );
    }
}

/// The owner gets the distinction the feature exists for.
#[tokio::test]
async fn the_owner_can_tell_expired_from_missing() {
    let store = MemoryTaskStore::new();
    let (expired, _) = store
        .create_task("work", json!({}), Some(1), Some("alice".into()))
        .await
        .expect("create");
    let (present, _) = store
        .create_task("work", json!({}), Some(60_000), Some("alice".into()))
        .await
        .expect("create");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    assert!(
        matches!(
            store.task_presence(&expired).await.unwrap(),
            TaskPresence::Expired { .. }
        ),
        "a retained record whose TTL elapsed is expired, not missing"
    );
    assert!(matches!(
        store.task_presence(&present).await.unwrap(),
        TaskPresence::Present { .. }
    ));
    assert_eq!(
        store.task_presence("never-issued").await.unwrap(),
        TaskPresence::Missing
    );

    // The owner travels with both, which is what lets the router authorize
    // before disclosing the difference.
    for id in [&expired, &present] {
        assert_eq!(
            store.task_presence(id).await.unwrap().owner(),
            Some(&Some("alice".to_string())),
            "presence must carry the owner for {id}"
        );
    }
}

/// A store that drops expired records answers `Missing`, which is correct for
/// it. The default keeps every existing store compiling and behaving as before.
#[tokio::test]
async fn the_default_reports_missing_for_a_store_without_tombstones() {
    struct Forgetful;

    #[async_trait::async_trait]
    impl TaskStore for Forgetful {
        async fn create_task(
            &self,
            _tool: &str,
            _args: serde_json::Value,
            _ttl: Option<u64>,
            _owner: Option<String>,
        ) -> tower_mcp::async_task::Result<(String, tower_mcp::async_task::CancellationToken)>
        {
            unimplemented!("not exercised")
        }
        async fn task_owner(
            &self,
            _task_id: &str,
        ) -> tower_mcp::async_task::Result<Option<Option<String>>> {
            // Expired and unknown are the same to this store.
            Ok(None)
        }
        async fn get_task(
            &self,
            _task_id: &str,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::protocol::TaskObject>> {
            Ok(None)
        }
        async fn set_task_meta(
            &self,
            _task_id: &str,
            _meta: serde_json::Value,
        ) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn discard_task(&self, _task_id: &str) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn get_task_result(
            &self,
            _task_id: &str,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::async_task::TaskSnapshot>> {
            Ok(None)
        }
        async fn wait_for_completion(
            &self,
            _task_id: &str,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::async_task::TaskSnapshot>> {
            Ok(None)
        }
        async fn list_tasks(
            &self,
            _status: Option<tower_mcp::protocol::TaskStatus>,
        ) -> tower_mcp::async_task::Result<Vec<tower_mcp::protocol::TaskObject>> {
            Ok(Vec::new())
        }
        async fn require_input(
            &self,
            _task_id: &str,
            _requests: tower_mcp::protocol::InputRequests,
            _message: Option<&str>,
        ) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn outstanding_input_requests(
            &self,
            _task_id: &str,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::protocol::InputRequests>> {
            Ok(None)
        }
        async fn apply_input_responses(
            &self,
            _task_id: &str,
            _responses: tower_mcp::protocol::InputResponses,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::async_task::AppliedInputResponses>>
        {
            Ok(None)
        }
        async fn set_ttl(&self, _task_id: &str, _ttl: u64) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn complete_task(
            &self,
            _task_id: &str,
            _result: CallToolResult,
        ) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn fail_task(
            &self,
            _task_id: &str,
            _error: tower_mcp::JsonRpcError,
        ) -> tower_mcp::async_task::Result<bool> {
            Ok(false)
        }
        async fn cancel_task(
            &self,
            _task_id: &str,
            _reason: Option<&str>,
        ) -> tower_mcp::async_task::Result<Option<tower_mcp::protocol::TaskObject>> {
            Ok(None)
        }
    }

    assert_eq!(
        Forgetful.task_presence("anything").await.unwrap(),
        TaskPresence::Missing,
        "the default must preserve today's behaviour for a store that keeps no tombstones"
    );
}
