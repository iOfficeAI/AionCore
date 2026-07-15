use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolSpec {
    Function {
        name: String,
        description: String,
        #[serde(rename = "inputSchema")]
        input_schema: Value,
        #[serde(rename = "deferLoading", default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
    },
    Namespace {
        name: String,
        description: String,
        tools: Vec<DynamicToolSpec>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolsRegisterRequest {
    pub request_id: String,
    pub conversation_id: String,
    pub tools: Vec<DynamicToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolsRegisteredPayload {
    pub request_id: String,
    pub conversation_id: String,
    pub accepted: bool,
    pub registration_id: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallPayload {
    pub conversation_id: String,
    pub registration_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolCallOutputContentItem {
    InputText {
        text: String,
    },
    InputImage {
        #[serde(rename = "imageUrl")]
        image_url: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallResponse {
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolResultPayload {
    pub conversation_id: String,
    pub registration_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    pub success: bool,
}

impl DynamicToolResultPayload {
    pub fn into_response(self) -> DynamicToolCallResponse {
        DynamicToolCallResponse {
            content_items: self.content_items,
            success: self.success,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dynamic_tool_wire_shapes_use_codex_camel_case() {
        let spec = DynamicToolSpec::Function {
            name: "list_threads".into(),
            description: "List tasks".into(),
            input_schema: json!({"type": "object"}),
            defer_loading: Some(true),
        };
        let value = serde_json::to_value(spec).unwrap();

        assert_eq!(value["type"], "function");
        assert_eq!(value["inputSchema"]["type"], "object");
        assert_eq!(value["deferLoading"], true);
    }

    #[test]
    fn dynamic_tool_result_round_trips_without_identity_loss() {
        let value = json!({
            "conversationId": "conversation-1",
            "registrationId": "dynamic-tools-7",
            "threadId": "thread-1",
            "turnId": "turn-1",
            "callId": "call-1",
            "namespace": null,
            "tool": "list_threads",
            "contentItems": [{"type": "inputText", "text": "ready"}],
            "success": true
        });

        let result: DynamicToolResultPayload = serde_json::from_value(value).unwrap();
        assert_eq!(result.thread_id, "thread-1");
        assert_eq!(result.into_response().content_items.len(), 1);
    }
}
