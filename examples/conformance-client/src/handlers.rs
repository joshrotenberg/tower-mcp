//! Client handlers for server-initiated requests (sampling, elicitation, roots).

use std::collections::HashMap;
use tower_mcp::client::ClientHandler;
use tower_mcp::client::ServerNotification;
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::*;

/// Basic handler that does nothing special. Used for `initialize` and `sse-retry`.
pub struct BasicHandler;

#[async_trait::async_trait]
impl ClientHandler for BasicHandler {
    async fn handle_create_message(
        &self,
        _params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        Err(JsonRpcError::method_not_found("sampling not supported"))
    }

    async fn handle_elicit(
        &self,
        _params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        Err(JsonRpcError::method_not_found("elicitation not supported"))
    }

    async fn handle_list_roots(&self) -> Result<ListRootsResult, JsonRpcError> {
        Ok(ListRootsResult {
            roots: vec![],
            meta: None,
        })
    }

    async fn on_notification(&self, _notification: ServerNotification) {}
}

/// Full handler supporting sampling and elicitation. Used for `tools_call`.
pub struct FullHandler;

#[async_trait::async_trait]
impl ClientHandler for FullHandler {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        let first_text = params
            .messages
            .first()
            .and_then(|m| {
                m.content.items().into_iter().find_map(|c| {
                    if let SamplingContent::Text { text, .. } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "(empty)".to_string());

        Ok(CreateMessageResult {
            role: ContentRole::Assistant,
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: format!("Mock response to: {}", first_text),
                annotations: None,
                meta: None,
            }),
            model: "mock-model".to_string(),
            stop_reason: Some("endTurn".to_string()),
            meta: None,
        })
    }

    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        match params {
            ElicitRequestParams::Form(form_params) => {
                let mut content = HashMap::new();
                for name in form_params.requested_schema.properties.keys() {
                    content.insert(name.clone(), ElicitFieldValue::String("test".to_string()));
                }
                Ok(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(content),
                    meta: None,
                })
            }
            ElicitRequestParams::Url(_) | _ => Ok(ElicitResult {
                action: ElicitAction::Accept,
                content: None,
                meta: None,
            }),
        }
    }

    async fn handle_list_roots(&self) -> Result<ListRootsResult, JsonRpcError> {
        Ok(ListRootsResult {
            roots: vec![],
            meta: None,
        })
    }

    async fn on_notification(&self, _notification: ServerNotification) {}
}

/// Elicitation handler that applies default values from the schema.
/// Used for `elicitation-defaults` scenario (SEP-1034).
pub struct ElicitationDefaultsHandler;

#[async_trait::async_trait]
impl ClientHandler for ElicitationDefaultsHandler {
    async fn handle_create_message(
        &self,
        _params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        Err(JsonRpcError::method_not_found("sampling not supported"))
    }

    async fn handle_elicit(
        &self,
        params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        match params {
            ElicitRequestParams::Form(form_params) => {
                let mut content = HashMap::new();
                for (name, def) in &form_params.requested_schema.properties {
                    if let Some(value) = extract_default(def) {
                        content.insert(name.clone(), value);
                    }
                }
                Ok(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(content),
                    meta: None,
                })
            }
            ElicitRequestParams::Url(_) | _ => Ok(ElicitResult {
                action: ElicitAction::Accept,
                content: None,
                meta: None,
            }),
        }
    }

    async fn handle_list_roots(&self) -> Result<ListRootsResult, JsonRpcError> {
        Ok(ListRootsResult {
            roots: vec![],
            meta: None,
        })
    }

    async fn on_notification(&self, _notification: ServerNotification) {}
}

/// Extract the default value from a primitive schema definition.
fn extract_default(def: &PrimitiveSchemaDefinition) -> Option<ElicitFieldValue> {
    match def {
        PrimitiveSchemaDefinition::String(s) => s
            .default
            .as_ref()
            .map(|d| ElicitFieldValue::String(d.clone())),
        PrimitiveSchemaDefinition::Integer(i) => i.default.map(ElicitFieldValue::Integer),
        PrimitiveSchemaDefinition::Number(n) => n.default.map(ElicitFieldValue::Number),
        PrimitiveSchemaDefinition::Boolean(b) => b.default.map(ElicitFieldValue::Boolean),
        PrimitiveSchemaDefinition::SingleSelectEnum(e) => e
            .default
            .as_ref()
            .map(|d| ElicitFieldValue::String(d.clone())),
        PrimitiveSchemaDefinition::MultiSelectEnum(e) => e
            .default
            .as_ref()
            .map(|d| ElicitFieldValue::StringArray(d.clone())),
        _ => None,
    }
}
