//! HTTP contract types for `/api/agent-center/*` (CSBU WorkMate 智能体中心).
//!
//! Agent Center evolves the existing Assistant entity (same `id`); these types
//! layer visibility / ACL / skill pins / MCP allowlist / publish revisions
//! without a second runtime.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AssistantConversationOverridesRequest, AssistantConversationRequest, AssistantDetailResponse, AssistantResponse,
    CreateAssistantRequest, UpdateAssistantRequest,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentVisibility {
    Private,
    Team,
    Enterprise,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentPublishStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMcpPolicy {
    /// Only `mcp_ids` / assistant default_mcp_ids; **empty = mount no MCP**.
    Allowlist,
    /// Explicit opt-in: intersect with the caller's globally enabled MCP set.
    InheritUserEnabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillVersionPolicy {
    Pin,
    Latest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSkillRef {
    pub skill_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default = "default_pin_policy")]
    pub version_policy: SkillVersionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_version: Option<String>,
}

fn default_pin_policy() -> SkillVersionPolicy {
    SkillVersionPolicy::Pin
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeScopeRef {
    pub knowhub_space_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ids: Option<Vec<String>>,
    #[serde(default = "default_read_access")]
    pub access: String,
}

fn default_read_access() -> String {
    "read".to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAclRole {
    Owner,
    Editor,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRoleBinding {
    pub subject_type: String,
    pub subject_id: String,
    pub role: AgentAclRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterMeta {
    pub visibility: AgentVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_id: Option<String>,
    pub status: AgentPublishStatus,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knowledge_scopes: Vec<KnowledgeScopeRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_refs: Vec<AgentSkillRef>,
    pub mcp_policy: AgentMcpPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub role_bindings: Vec<AgentRoleBinding>,
}

impl Default for AgentCenterMeta {
    fn default() -> Self {
        Self {
            visibility: AgentVisibility::Private,
            team_id: None,
            enterprise_id: None,
            status: AgentPublishStatus::Draft,
            version: 0,
            published_revision_id: None,
            knowledge_scopes: Vec::new(),
            skill_refs: Vec::new(),
            mcp_policy: AgentMcpPolicy::Allowlist,
            role_bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterListItem {
    pub assistant: AssistantResponse,
    pub meta: AgentCenterMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterDetailResponse {
    pub assistant: AssistantDetailResponse,
    pub meta: AgentCenterMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AgentCenterMetaPatch {
    #[serde(default)]
    pub visibility: Option<AgentVisibility>,
    #[serde(default)]
    pub team_id: Option<Option<String>>,
    #[serde(default)]
    pub enterprise_id: Option<Option<String>>,
    #[serde(default)]
    pub knowledge_scopes: Option<Vec<KnowledgeScopeRef>>,
    #[serde(default)]
    pub skill_refs: Option<Vec<AgentSkillRef>>,
    #[serde(default)]
    pub mcp_policy: Option<AgentMcpPolicy>,
    #[serde(default)]
    pub role_bindings: Option<Vec<AgentRoleBinding>>,
    /// When set with `mcp_policy=allowlist`, written to assistant defaults as fixed mcp ids.
    #[serde(default)]
    pub mcp_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateAgentCenterRequest {
    #[serde(flatten)]
    pub assistant: CreateAssistantRequest,
    #[serde(default)]
    pub meta: AgentCenterMetaPatch,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UpdateAgentCenterRequest {
    #[serde(flatten)]
    pub assistant: UpdateAssistantRequest,
    #[serde(default)]
    pub meta: AgentCenterMetaPatch,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublishAgentCenterRequest {
    #[serde(default)]
    pub changelog: Option<String>,
    /// When true (default), skill_refs with `latest` are resolved to a pin in the snapshot.
    #[serde(default = "default_true")]
    pub pin_skills_on_publish: bool,
}

impl Default for PublishAgentCenterRequest {
    fn default() -> Self {
        Self {
            changelog: None,
            pin_skills_on_publish: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterRevisionResponse {
    pub id: String,
    pub revision: i64,
    pub changelog: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterRunPlanResponse {
    pub assistant_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    pub revision: i64,
    /// Ready-to-POST body for `POST /api/conversations` (existing runtime path).
    pub create_conversation: CreateConversationRequestWire,
}

/// Serde-friendly mirror of create-conversation fields (request type is Deserialize-only upstream).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateConversationRequestWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<AssistantConversationRequest>,
    #[serde(default)]
    pub extra: Value,
}

impl CreateConversationRequestWire {
    pub fn for_assistant(assistant_id: &str, overrides: Option<AssistantConversationOverridesRequest>) -> Self {
        Self {
            name: None,
            assistant: Some(AssistantConversationRequest {
                id: assistant_id.to_owned(),
                locale: None,
                conversation_overrides: overrides,
            }),
            extra: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCenterListQuery {
    /// `mine` | `team` | `enterprise`
    #[serde(default = "default_scope_mine")]
    pub scope: String,
    #[serde(default)]
    pub team_id: Option<String>,
}

fn default_scope_mine() -> String {
    "mine".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_ref_defaults_to_pin() {
        let raw = serde_json::json!({"skill_key": "workmate-presentation"});
        let parsed: AgentSkillRef = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.version_policy, SkillVersionPolicy::Pin);
    }

    #[test]
    fn mcp_policy_allowlist_roundtrip() {
        let meta = AgentCenterMeta::default();
        assert_eq!(meta.mcp_policy, AgentMcpPolicy::Allowlist);
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["mcp_policy"], "allowlist");
    }
}
