//! M9 E15 local-kb API DTOs. Brand: CSBU WorkMate. Default OFF. Not KnowHub / experience library.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalKbSettingsResponse {
    /// Always false until the user explicitly enables local-kb.
    pub enabled: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateLocalKbSettingsRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalKbSourceResponse {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_path: Option<String>,
    pub document_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateLocalKbSourceRequest {
    /// `text` | `file` | `directory`
    pub kind: String,
    pub label: String,
    /// Required for `file` / `directory`. Ignored for `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_path: Option<String>,
    /// Required for `text` kind: document body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Optional title for `text` kind (defaults to label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalKbSearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    /// Optional task id for provenance / future context_snapshots linkage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn default_search_limit() -> usize {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalKbSearchHit {
    pub document_id: String,
    pub source_id: String,
    pub title: String,
    pub snippet: String,
    pub content_hash: String,
    pub provider: String,
}
