//! Shared transport helpers for final-protocol subscriptions.

use crate::context::ServerNotification;
use crate::protocol::{
    Implementation, JsonRpcNotification, JsonRpcResponse, NotificationMeta, RequestId, ResultType,
    SubscriptionFilter, SubscriptionsAcknowledgedParams, SubscriptionsListenResult,
    SubscriptionsListenResultMeta, notifications,
};

/// Narrow a requested filter to what this server will actually honor.
///
/// The acknowledgement reports this filter back to the client, so anything
/// dropped here is a promise the server declines to make. `tasks_enabled`
/// reflects whether the server opted into the Tasks extension: without it
/// there is nothing to notify about, so the task IDs are dropped rather than
/// acknowledged and then silently ignored.
pub(crate) fn accepted_subscription_filter(
    requested: SubscriptionFilter,
    tasks_enabled: bool,
) -> SubscriptionFilter {
    SubscriptionFilter {
        tools_list_changed: requested.tools_list_changed.filter(|enabled| *enabled),
        prompts_list_changed: requested.prompts_list_changed.filter(|enabled| *enabled),
        resources_list_changed: requested.resources_list_changed.filter(|enabled| *enabled),
        resource_subscriptions: requested.resource_subscriptions,
        task_ids: requested.task_ids.filter(|_| tasks_enabled),
    }
}

pub(crate) fn subscription_matches(
    notification: &ServerNotification,
    filter: &SubscriptionFilter,
) -> bool {
    match notification {
        ServerNotification::ToolsListChanged => filter.tools_list_changed == Some(true),
        ServerNotification::PromptsListChanged => filter.prompts_list_changed == Some(true),
        ServerNotification::ResourcesListChanged => filter.resources_list_changed == Some(true),
        ServerNotification::ResourceUpdated { uri } => filter
            .resource_subscriptions
            .as_ref()
            .is_some_and(|subscriptions| subscriptions.iter().any(|item| item == uri)),
        // A task is named individually rather than opted into as a class, so
        // an unlisted task ID never matches even a subscriber that asked for
        // every other notification type.
        ServerNotification::FinalTaskStatusChanged(params) => filter
            .task_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == params.task.task_id())),
        _ => false,
    }
}

pub(crate) fn tagged_subscription_notification(
    notification: &ServerNotification,
    subscription_id: &RequestId,
) -> Option<String> {
    let json = crate::transport::stdio::serialize_notification(notification)?;
    let mut value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let object = value.as_object_mut()?;
    let params = object
        .entry("params")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    let meta = params
        .entry("_meta")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    meta.insert(
        "io.modelcontextprotocol/subscriptionId".to_string(),
        serde_json::to_value(subscription_id).ok()?,
    );
    serde_json::to_string(&value).ok()
}

pub(crate) fn subscription_acknowledgment(
    subscription_id: RequestId,
    notifications: SubscriptionFilter,
) -> JsonRpcNotification {
    JsonRpcNotification::new(notifications::SUBSCRIPTIONS_ACKNOWLEDGED).with_params(
        serde_json::to_value(SubscriptionsAcknowledgedParams {
            meta: Some(NotificationMeta {
                subscription_id: Some(subscription_id),
            }),
            notifications,
        })
        .expect("subscription acknowledgment is serializable"),
    )
}

pub(crate) fn subscription_complete_response(
    subscription_id: RequestId,
    server_info: Option<Implementation>,
) -> JsonRpcResponse {
    let result = SubscriptionsListenResult {
        result_type: ResultType::Complete,
        meta: SubscriptionsListenResultMeta {
            subscription_id: subscription_id.clone(),
            server_info,
        },
    };
    JsonRpcResponse::result(
        subscription_id,
        serde_json::to_value(result).expect("subscription result is serializable"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{DetailedTask, TaskMetadata, TaskStatusNotificationParams};

    fn task_notification(task_id: &str) -> ServerNotification {
        ServerNotification::FinalTaskStatusChanged(TaskStatusNotificationParams {
            task: DetailedTask::working(TaskMetadata::new(
                task_id.to_string(),
                "2026-07-28T00:00:00Z".to_string(),
                "2026-07-28T00:00:01Z".to_string(),
                Some(60_000),
            )),
            meta: None,
        })
    }

    fn subscribed_to(task_ids: &[&str]) -> SubscriptionFilter {
        SubscriptionFilter {
            task_ids: Some(task_ids.iter().map(|id| id.to_string()).collect()),
            ..SubscriptionFilter::default()
        }
    }

    #[test]
    fn task_notifications_match_only_the_named_task_ids() {
        let filter = subscribed_to(&["task-a", "task-b"]);
        assert!(subscription_matches(&task_notification("task-a"), &filter));
        assert!(subscription_matches(&task_notification("task-b"), &filter));
        assert!(!subscription_matches(&task_notification("task-c"), &filter));
    }

    #[test]
    fn a_broad_subscription_still_excludes_unnamed_tasks() {
        // Tasks are named individually rather than opted into as a class, so
        // asking for every other notification type grants nothing here.
        let filter = SubscriptionFilter {
            tools_list_changed: Some(true),
            prompts_list_changed: Some(true),
            resources_list_changed: Some(true),
            resource_subscriptions: Some(vec!["file:///everything".to_string()]),
            task_ids: None,
        };
        assert!(!subscription_matches(&task_notification("task-a"), &filter));
    }

    #[test]
    fn accepted_filter_declines_task_ids_when_the_server_has_no_tasks() {
        let requested = subscribed_to(&["task-a"]);

        let accepted = accepted_subscription_filter(requested.clone(), true);
        assert_eq!(
            accepted.task_ids.as_deref(),
            Some(&["task-a".to_string()][..])
        );

        // The acknowledgement reports what the server agreed to honor, so a
        // server without the extension must not echo the IDs back.
        let declined = accepted_subscription_filter(requested, false);
        assert!(declined.task_ids.is_none());
    }

    #[test]
    fn task_notifications_serialize_as_notifications_tasks() {
        let json = crate::transport::stdio::serialize_notification(&task_notification("task-a"))
            .expect("task notification is serializable");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["method"], "notifications/tasks");
        // The DetailedTask is flattened into params rather than nested.
        assert_eq!(value["params"]["taskId"], "task-a");
        assert_eq!(value["params"]["status"], "working");
        assert_eq!(value["params"]["ttlMs"], 60_000);
    }
}
