use agent_client_protocol::schema::Meta as SdkMeta;
use aionui_common::{Confirmation, ConfirmationOption};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::tool_call::{AcpToolCallContentItem, AcpToolCallKind, AcpToolCallLocationItem, AcpToolCallStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpPermissionEventData {
    Request(AcpPermissionRequestData),
    Confirmation(Confirmation),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpPermissionRequestData {
    #[serde(default)]
    pub session_id: String,
    pub tool_call: AcpPermissionToolCall,
    pub options: Vec<AcpPermissionOptionData>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SdkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpPermissionToolCall {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AcpToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<AcpToolCallKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<AcpToolCallContentItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<AcpToolCallLocationItem>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SdkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpPermissionOptionData {
    pub option_id: String,
    pub name: String,
    pub kind: AcpPermissionOptionKind,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SdkMeta>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl AcpPermissionEventData {
    pub fn as_confirmation(&self) -> Option<Confirmation> {
        match self {
            Self::Confirmation(conf) => Some(conf.clone()),
            Self::Request(req) => Some(req.to_confirmation()),
        }
    }
}

impl AcpPermissionRequestData {
    pub fn to_confirmation(&self) -> Confirmation {
        Confirmation {
            id: self.tool_call.tool_call_id.clone(),
            call_id: self.tool_call.tool_call_id.clone(),
            title: self.tool_call.title.clone(),
            action: None,
            description: self
                .tool_call
                .raw_input
                .as_ref()
                .and_then(|raw| raw.get("description").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    self.tool_call
                        .raw_input
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or_default()
                }),
            command_type: self.tool_call.kind.map(|kind| match kind {
                AcpToolCallKind::Read => "read".to_owned(),
                AcpToolCallKind::Edit => "edit".to_owned(),
                AcpToolCallKind::Execute => "execute".to_owned(),
            }),
            options: self
                .options
                .iter()
                .map(|opt| ConfirmationOption {
                    label: opt.name.clone(),
                    value: Value::String(opt.option_id.clone()),
                    params: match opt.kind {
                        AcpPermissionOptionKind::AllowAlways => {
                            Some(HashMap::from([("always_allow".into(), "true".into())]))
                        }
                        AcpPermissionOptionKind::RejectOnce | AcpPermissionOptionKind::RejectAlways => {
                            Some(HashMap::from([("decision".into(), "reject".into())]))
                        }
                        AcpPermissionOptionKind::AllowOnce => None,
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_preserves_approval_semantics() {
        let request = AcpPermissionRequestData {
            session_id: "session".into(),
            tool_call: AcpPermissionToolCall {
                tool_call_id: "call".into(),
                status: None,
                title: None,
                kind: Some(AcpToolCallKind::Execute),
                raw_input: None,
                raw_output: None,
                content: None,
                locations: None,
                meta: None,
            },
            options: vec![
                AcpPermissionOptionData {
                    option_id: "always".into(),
                    name: "Always allow".into(),
                    kind: AcpPermissionOptionKind::AllowAlways,
                    meta: None,
                },
                AcpPermissionOptionData {
                    option_id: "reject".into(),
                    name: "Reject".into(),
                    kind: AcpPermissionOptionKind::RejectOnce,
                    meta: None,
                },
            ],
            meta: None,
        };
        let options = request.to_confirmation().options;
        assert_eq!(options[0].params.as_ref().unwrap()["always_allow"], "true");
        assert_eq!(options[1].params.as_ref().unwrap()["decision"], "reject");
    }
}
