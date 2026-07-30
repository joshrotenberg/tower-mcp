//! Shared transport helpers for final-protocol subscriptions.

use crate::context::ServerNotification;
use crate::protocol::{
    Implementation, JsonRpcNotification, JsonRpcResponse, NotificationMeta, RequestId, ResultType,
    SubscriptionFilter, SubscriptionsAcknowledgedParams, SubscriptionsListenResult,
    SubscriptionsListenResultMeta, notifications,
};

pub(crate) fn accepted_subscription_filter(requested: SubscriptionFilter) -> SubscriptionFilter {
    SubscriptionFilter {
        tools_list_changed: requested.tools_list_changed.filter(|enabled| *enabled),
        prompts_list_changed: requested.prompts_list_changed.filter(|enabled| *enabled),
        resources_list_changed: requested.resources_list_changed.filter(|enabled| *enabled),
        resource_subscriptions: requested.resource_subscriptions,
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
