use agent_client_protocol::schema::v1::Meta as SdkMeta;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Data for the `ToolCall` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEventData {
    pub call_id: String,
    /// Skipped when empty: downstream storage merges frames with RFC 7396
    /// merge-patch, so an empty name would ERASE the stored one (the terminal
    /// ToolResult and turn-end closes legitimately carry no name).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Skipped when null for the same reason: a null would DELETE the stored
    /// arguments under merge-patch semantics.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub args: serde_json::Value,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallEventData {
    pub session_id: String,
    pub update: AcpToolCallUpdateData,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SdkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallUpdateData {
    #[serde(rename = "sessionUpdate")]
    pub session_update: AcpToolCallSessionUpdateKind,
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AcpToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<AcpToolCallKind>,
    #[serde(rename = "rawInput", skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<AcpToolCallContentItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<AcpToolCallLocationItem>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallSessionUpdateKind {
    ToolCall,
    ToolCallUpdate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallKind {
    Read,
    Edit,
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpToolCallContentItem {
    Content {
        content: AcpToolCallTextBlock,
    },
    Diff {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallTextBlock {
    #[serde(rename = "type")]
    pub block_type: AcpToolCallTextBlockType,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallTextBlockType {
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallLocationItem {
    pub path: String,
}

/// Status of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Running,
    Completed,
    Error,
    /// The turn ended (cancel/crash) while this call was still open, so no
    /// terminal ToolResult will ever arrive; emitted by the fold layer to
    /// close the call instead of leaving it `Running` forever.
    Canceled,
}

/// Status vocabulary of a `tool_group` ENTRY.
///
/// Deliberately NOT [`ToolCallStatus`]: the two channels have different wire
/// vocabularies on the frontend, and mixing them silently breaks the group.
///
/// - `tool_call` entries go through `normalizeToolCallStatus`, whose arms are
///   lowercase (`running` / `completed` / `error` / `canceled`) — matching
///   `ToolCallStatus`'s `rename_all = "snake_case"`.
/// - `tool_group` entries go through `normalizeToolGroupStatus`, whose arms are
///   the Gemini PascalCase set below, and whose `default:` arm returns
///   **`running`**.
///
/// So a snake_case value here does not merely render oddly — it misses every arm,
/// falls to `default:`, and pins the entry to a permanent spinner. It also keeps
/// `hasRunningToolMessages` true forever, which lights the conversation's running
/// indicator with nothing to ever clear it. (The frontend type is
/// `IMessageToolGroup['content'][number]['status']`.)
///
/// `ToolGroupEntry` previously reused `ToolCallStatus`; that was latent because no
/// production code emitted a `ToolGroup` — only a relay test did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolGroupStatus {
    Pending,
    Confirming,
    Executing,
    Success,
    Error,
    Canceled,
}

impl ToolGroupStatus {
    /// Terminal = the entry will not change again (drives the group row's
    /// persisted `finish` status).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Error | Self::Canceled)
    }
}

/// A single entry in a `ToolGroup` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGroupEntry {
    pub call_id: String,
    pub name: String,
    pub status: ToolGroupStatus,
    #[serde(default)]
    pub description: Option<String>,
}
