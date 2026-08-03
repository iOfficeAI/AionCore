use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusScopeKind {
    Conversation,
    Mcp,
    CustomAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatusScope {
    pub kind: RuntimeStatusScopeKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeResourceKind {
    Node,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusPhase {
    WaitingForLock,
    Downloading,
    Extracting,
    Validating,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureKind {
    Timeout,
    DownloadFailed,
    HttpStatus,
    ChecksumMismatch,
    ValidationFailed,
    UnsupportedPlatform,
    BundledResourceMissing,
    BundledResourceInvalid,
    ActivationIoFailed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatusPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub resource: RuntimeResourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    pub scope: RuntimeStatusScope,
    pub phase: RuntimeStatusPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<RuntimeFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureNodeRuntimeRequest {
    pub scope: RuntimeStatusScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureNodeRuntimeResponse {
    pub ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_payload_serializes() {
        let payload = RuntimeStatusPayload {
            user_id: Some("user-1".into()),
            resource: RuntimeResourceKind::Node,
            resource_id: None,
            scope: RuntimeStatusScope {
                kind: RuntimeStatusScopeKind::Conversation,
                id: "conv-1".into(),
            },
            phase: RuntimeStatusPhase::Downloading,
            failure_kind: None,
            message: Some("downloading".into()),
            status_code: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["resource"], "node");
        assert_eq!(json["user_id"], "user-1");
        assert_eq!(json["scope"]["kind"], "conversation");
        assert_eq!(json["phase"], "downloading");
        assert_eq!(json["message"], "downloading");
    }

    #[test]
    fn bundled_runtime_failure_kind_serializes_as_snake_case() {
        let missing = serde_json::to_value(RuntimeFailureKind::BundledResourceMissing).unwrap();
        let invalid = serde_json::to_value(RuntimeFailureKind::BundledResourceInvalid).unwrap();

        assert_eq!(missing, "bundled_resource_missing");
        assert_eq!(invalid, "bundled_resource_invalid");
    }

    #[test]
    fn activation_io_failed_serializes_to_snake_case() {
        let json = serde_json::to_string(&RuntimeFailureKind::ActivationIoFailed).unwrap();
        assert_eq!(json, "\"activation_io_failed\"");
        let parsed: RuntimeFailureKind = serde_json::from_str("\"activation_io_failed\"").unwrap();
        assert_eq!(parsed, RuntimeFailureKind::ActivationIoFailed);
    }

    #[test]
    fn ensure_node_runtime_request_roundtrips() {
        let request = EnsureNodeRuntimeRequest {
            scope: RuntimeStatusScope {
                kind: RuntimeStatusScopeKind::Mcp,
                id: "chrome-devtools".into(),
            },
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["scope"]["kind"], "mcp");
        let parsed: EnsureNodeRuntimeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, request);
    }
}
