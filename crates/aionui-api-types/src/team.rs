use aionui_common::TimestampMs;
use serde::{Deserialize, Deserializer, Serialize};

use crate::TeamMcpStdioConfig;

// ---------------------------------------------------------------------------
// A. Team management — Request DTOs
// ---------------------------------------------------------------------------

/// Input for a single agent when creating a team or adding an agent.
///
/// Each agent gets its own conversation. Create requests must include exactly
/// one agent with role `lead` or `leader`; that explicit role becomes the team
/// lead.
///
#[derive(Debug, Clone)]
pub struct TeamAgentInput {
    pub name: String,
    pub role: String,
    pub backend: Option<String>,
    pub model: String,
    pub assistant_id: Option<String>,
    /// Deprecated request-side field retained so old clients receive a clear
    /// validation error instead of silently reusing a solo conversation.
    ///
    /// New Team creation requests must omit this field or set it to null/empty.
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamAgentInputCompat {
    #[serde(default)]
    pub assistant_id: Option<String>,
    pub name: String,
    pub role: String,
    pub model: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

fn normalize_assistant_id(assistant_id: Option<String>) -> Option<String> {
    assistant_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

impl<'de> Deserialize<'de> for TeamAgentInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = TeamAgentInputCompat::deserialize(deserializer)?;
        let assistant_id =
            normalize_assistant_id(raw.assistant_id).ok_or_else(|| serde::de::Error::missing_field("assistant_id"))?;

        Ok(Self {
            name: raw.name,
            role: raw.role,
            backend: None,
            model: raw.model,
            assistant_id: Some(assistant_id),
            conversation_id: raw.conversation_id,
        })
    }
}

/// Request body for `POST /api/teams`.
///
/// Creates a team with the given name and agent list.
/// Exactly one agent with role `lead` or `leader` is designated as the lead.
#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
    #[serde(alias = "assistants")]
    pub agents: Vec<TeamAgentInput>,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `PATCH /api/teams/:id/name`.
#[derive(Debug, Deserialize)]
pub struct RenameTeamRequest {
    pub name: String,
}

// ---------------------------------------------------------------------------
// B. Agent management — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/teams/:id/agents`.
///
/// Adds a new agent to an existing team. A conversation is
/// created automatically for the new agent.
#[derive(Debug)]
pub struct AddAgentRequest {
    pub name: String,
    pub role: String,
    pub backend: Option<String>,
    pub model: String,
    pub assistant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAgentRequestCompat {
    #[serde(default)]
    assistant: Option<TeamAgentInput>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    assistant_id: Option<String>,
}

impl<'de> Deserialize<'de> for AddAgentRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = AddAgentRequestCompat::deserialize(deserializer)?;
        if let Some(assistant) = raw.assistant {
            return Ok(Self {
                name: assistant.name,
                role: assistant.role,
                backend: None,
                model: assistant.model,
                assistant_id: assistant.assistant_id,
            });
        }

        let name = raw.name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
        let role = raw.role.ok_or_else(|| serde::de::Error::missing_field("role"))?;
        let model = raw.model.ok_or_else(|| serde::de::Error::missing_field("model"))?;
        let assistant_id =
            normalize_assistant_id(raw.assistant_id).ok_or_else(|| serde::de::Error::missing_field("assistant_id"))?;

        Ok(Self {
            name,
            role,
            backend: None,
            model,
            assistant_id: Some(assistant_id),
        })
    }
}

/// Request body for `PATCH /api/teams/:id/agents/:slotId/name`.
#[derive(Debug, Deserialize)]
pub struct RenameAgentRequest {
    pub name: String,
}

// ---------------------------------------------------------------------------
// C. Team runtime context — persisted conversation.extra contract
// ---------------------------------------------------------------------------

/// Typed Team binding decoded from a team-owned conversation's `extra`.
///
/// This is the runtime-build contract consumed after `SessionContextBuilder`
/// has parsed persisted JSON. `team_id` is the ownership marker; `slot_id`
/// and `role` identify the agent slot when the conversation is attached to an
/// active Team session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamSessionBinding {
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub runtime_seed: TeamRuntimeSeed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<TeamMcpRuntimeConfig>,
}

impl TeamSessionBinding {
    pub fn from_extra_str(extra: &str) -> Result<Option<Self>, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(extra)?;
        Self::from_extra_value(&value)
    }

    pub fn from_extra_value(extra: &serde_json::Value) -> Result<Option<Self>, serde_json::Error> {
        let Some(team_id) = extra_string_field(extra, "teamId") else {
            return Ok(None);
        };

        let mcp = match extra.get("team_mcp_stdio_config").cloned() {
            Some(value) if !value.is_null() => Some(TeamMcpRuntimeConfig {
                stdio: serde_json::from_value(value)?,
            }),
            _ => None,
        };

        Ok(Some(Self {
            team_id,
            slot_id: extra_string_field(extra, "slot_id"),
            role: extra_string_field(extra, "role"),
            runtime_seed: TeamRuntimeSeed {
                backend: extra_string_field(extra, "backend"),
                session_mode: extra_string_field(extra, "session_mode"),
                current_model_id: extra_string_field(extra, "current_model_id"),
            },
            mcp,
        }))
    }

    pub fn team_id_marker_from_extra_str(extra: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(extra).ok()?;
        extra_string_field(&value, "teamId")
    }
}

fn extra_string_field(extra: &serde_json::Value, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

/// Startup seed values Team provisioning persists for runtime build.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRuntimeSeed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_model_id: Option<String>,
}

/// Typed Team MCP runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamMcpRuntimeConfig {
    pub stdio: TeamMcpStdioConfig,
}

// ---------------------------------------------------------------------------
// D. Message & session — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/teams/:id/messages`.
///
/// Sends a user message to the team lead's mailbox, triggering a
/// wake cycle. `files` is optional and — when present — forwarded
/// to the underlying agent together with the wake payload.
#[derive(Debug, Deserialize)]
pub struct SendTeamMessageRequest {
    pub content: String,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

/// Request body for `POST /api/teams/:id/agents/:slotId/messages`.
///
/// Sends a user message directly to a specific agent's mailbox.
/// `files` semantics match [`SendTeamMessageRequest`].
#[derive(Debug, Deserialize)]
pub struct SendAgentMessageRequest {
    pub content: String,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunTargetRole {
    Lead,
    Teammate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunStatus {
    Accepted,
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunSource {
    UserMessage,
}

#[derive(Debug, Deserialize)]
pub struct CancelTeamRunRequest {
    #[serde(default)]
    pub target_slot_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CancelTeamChildTurnRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PauseTeamSlotRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRunAckResponse {
    pub enqueue_status: TeamMessageEnqueueStatus,
    pub message_id: String,
    pub run: TeamRunPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamSlotWorkState {
    Idle,
    Queued,
    Starting,
    Running,
    Paused,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamSlotBlockedReason {
    RuntimeStarting,
    RuntimeFailed,
    Removing,
    SessionStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamMessageEnqueueStatus {
    Accepted,
    Queued,
    BlockedRuntimeStarting,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamSlotWorkPayload {
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub state: TeamSlotWorkState,
    pub queued_foreground_count: usize,
    pub queued_background_count: usize,
    pub active_turn_id: Option<String>,
    pub active_turn_started_at_ms: Option<TimestampMs>,
    pub active_turn_elapsed_ms: Option<u64>,
    pub active_turn_slow: Option<bool>,
    pub active_turn_slow_threshold_ms: Option<u64>,
    pub blocked_reason: Option<TeamSlotBlockedReason>,
    pub team_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRunPayload {
    pub team_id: String,
    pub team_run_id: String,
    pub source: TeamRunSource,
    pub has_user_intervention: bool,
    pub target_slot_id: String,
    pub target_role: TeamRunTargetRole,
    pub status: TeamRunStatus,
    pub queued_intent_count: usize,
    pub starting_batch_count: usize,
    pub running_batch_count: usize,
    pub active_enqueue_lease_count: usize,
    pub slot_work: Vec<TeamSlotWorkPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRunStateResponse {
    pub session_generation: Option<String>,
    pub active_run: Option<TeamRunPayload>,
    pub slot_work: Vec<TeamSlotWorkPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamChildTurnPayload {
    pub team_id: String,
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
    pub status: TeamRunStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamSendMessageQueuedResponse {
    pub team_run_id: Option<String>,
    pub target: TeamSlotWorkPayload,
}

// ---------------------------------------------------------------------------
// E. Team management — Response DTOs
// ---------------------------------------------------------------------------

/// Single agent within a team response.
///
/// Corresponds to the `TeamAgent` shared type in the API Spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentResponse {
    pub slot_id: String,
    #[serde(default)]
    pub assistant_name: String,
    pub name: String,
    pub role: String,
    pub conversation_id: String,
    #[serde(default)]
    pub assistant_backend: String,
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub model: String,
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "custom_agent_id",
        alias = "customAgentId"
    )]
    pub assistant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub pending_confirmations: usize,
}

/// Full team response returned by create, get, and list endpoints.
///
/// Corresponds to the `TTeam` shared type in the API Spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamResponse {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(alias = "agents")]
    pub assistants: Vec<TeamAgentResponse>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "lead_agent_id")]
    pub leader_assistant_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Type alias for team list responses.
pub type TeamListResponse = Vec<TeamResponse>;

// ---------------------------------------------------------------------------
// F. WebSocket event payloads
// ---------------------------------------------------------------------------

/// Payload for `team.agentStatusChanged` WebSocket event.
///
/// Pushed when an agent's runtime status changes (e.g., idle → working).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentStatusPayload {
    pub team_id: String,
    pub slot_id: String,
    pub status: String,
}

/// Payload for `team.agentSpawned` WebSocket event.
///
/// Pushed when the lead dynamically creates a new agent via
/// `team_spawn_agent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentSpawnedPayload {
    pub team_id: String,
    #[serde(alias = "agent")]
    pub assistant: TeamAgentResponse,
}

/// Payload for `team.agentRemoved` WebSocket event.
///
/// Pushed when an agent is removed from the team.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentRemovedPayload {
    pub team_id: String,
    pub slot_id: String,
}

/// Payload for `team.agentRenamed` WebSocket event.
///
/// Pushed when an agent's display name is changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentRenamedPayload {
    pub team_id: String,
    pub slot_id: String,
    pub name: String,
}

/// Runtime attach/warmup status for a team agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TeamAgentRuntimeStatus {
    Pending,
    Ready,
    Failed,
}

/// Payload for `team.agentRuntimeStatusChanged` WebSocket event.
///
/// Pushed when a team member's runtime attach/warmup lifecycle changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamAgentRuntimeStatusPayload {
    pub team_id: String,
    pub slot_id: String,
    pub conversation_id: String,
    pub status: TeamAgentRuntimeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Team-level session availability status for `team.sessionStatusChanged`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TeamSessionStatus {
    Starting,
    Ready,
    Failed,
}

/// Diagnostic phase for team session startup.
///
/// Frontend gating must use [`TeamSessionStatus`]. This phase identifies
/// which ensure-session step is currently running or failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TeamSessionPhase {
    LoadingTeam,
    StartingBridge,
    AttachingAgents,
    Recovering,
}

/// Payload for `team.sessionStatusChanged` WebSocket event.
///
/// Pushed when the whole team session moves through startup, ready, or
/// failed states. Member runtime details are reported separately through
/// `team.agentRuntimeStatusChanged`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamSessionStatusPayload {
    pub team_id: String,
    pub status: TeamSessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<TeamSessionPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Payload for `team.teammateMessage` WebSocket event.
///
/// Pushed when a teammate sends a message to another agent within the
/// team; identifies both the sender (`from_slot_id` / `from_name`) and
/// the conversation the message belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeammateMessagePayload {
    pub conversation_id: String,
    pub content: String,
    pub from_slot_id: String,
    pub from_name: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. Team management requests ------------------------------------------

    #[test]
    fn deserialize_create_team_request_full() {
        let raw = json!({
            "name": "Team Alpha",
            "agents": [
                {
                    "name": "Lead",
                    "role": "lead",
                    "model": "claude",
                    "assistant_id": "assistant-x"
                },
                {
                    "name": "Worker",
                    "role": "teammate",
                    "model": "claude",
                    "assistant_id": "assistant-y"
                }
            ]
        });
        let req: CreateTeamRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Team Alpha");
        assert_eq!(req.agents.len(), 2);
        assert_eq!(req.agents[0].name, "Lead");
        assert_eq!(req.agents[0].role, "lead");
        assert!(req.agents[0].backend.is_none());
        assert_eq!(req.agents[0].model, "claude");
        assert_eq!(req.agents[0].assistant_id.as_deref(), Some("assistant-x"));
        assert_eq!(req.agents[1].name, "Worker");
        assert_eq!(req.agents[1].assistant_id.as_deref(), Some("assistant-y"));
    }

    #[test]
    fn deserialize_create_team_request_from_assistants_field() {
        let raw = json!({
            "name": "Team Alpha",
            "assistants": [
                {
                    "name": "Lead",
                    "role": "lead",
                    "model": "claude",
                    "assistant_id": "assistant-x"
                }
            ]
        });
        let req: CreateTeamRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Team Alpha");
        assert_eq!(req.agents.len(), 1);
        assert_eq!(req.agents[0].name, "Lead");
        assert_eq!(req.agents[0].assistant_id.as_deref(), Some("assistant-x"));
        assert!(req.agents[0].backend.is_none());
    }

    #[test]
    fn deserialize_team_agent_input_with_conversation_id() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "model": "claude",
            "assistant_id": "assistant-x",
            "conversation_id": "existing-conv-123"
        });
        let input: TeamAgentInput = serde_json::from_value(raw).unwrap();
        assert_eq!(input.conversation_id.as_deref(), Some("existing-conv-123"));
    }

    #[test]
    fn deserialize_team_agent_input_rejects_legacy_custom_agent_id() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "backend": "acp",
            "model": "claude",
            "custom_agent_id": "assistant-legacy"
        });
        let result = serde_json::from_value::<TeamAgentInput>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_team_agent_input_conversation_id_defaults_to_none() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "model": "claude",
            "assistant_id": "assistant-x"
        });
        let input: TeamAgentInput = serde_json::from_value(raw).unwrap();
        assert!(input.conversation_id.is_none());
    }

    #[test]
    fn deserialize_team_agent_input_allows_missing_backend_when_assistant_id_present() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "model": "claude",
            "assistant_id": "assistant-x"
        });
        let input: TeamAgentInput = serde_json::from_value(raw).unwrap();
        assert!(input.backend.is_none());
        assert_eq!(input.assistant_id.as_deref(), Some("assistant-x"));
    }

    #[test]
    fn deserialize_team_agent_input_rejects_backend_field() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "backend": "acp",
            "model": "claude",
            "assistant_id": "assistant-x"
        });
        let result = serde_json::from_value::<TeamAgentInput>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_team_agent_input_requires_assistant_id() {
        let raw = json!({
            "name": "Lead",
            "role": "lead",
            "backend": "acp",
            "model": "claude"
        });
        let result = serde_json::from_value::<TeamAgentInput>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_create_team_request_empty_agents() {
        let raw = json!({ "name": "Empty", "agents": [] });
        let req: CreateTeamRequest = serde_json::from_value(raw).unwrap();
        assert!(req.agents.is_empty());
    }

    #[test]
    fn deserialize_create_team_request_missing_name() {
        let raw = json!({ "agents": [] });
        let result = serde_json::from_value::<CreateTeamRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_create_team_request_missing_agents() {
        let raw = json!({ "name": "Team" });
        let result = serde_json::from_value::<CreateTeamRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_rename_team_request() {
        let raw = json!({ "name": "New Name" });
        let req: RenameTeamRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "New Name");
    }

    #[test]
    fn deserialize_rename_team_request_missing_name() {
        let raw = json!({});
        let result = serde_json::from_value::<RenameTeamRequest>(raw);
        assert!(result.is_err());
    }

    // -- B. Agent management requests -----------------------------------------

    #[test]
    fn deserialize_add_agent_request() {
        let raw = json!({
            "name": "Helper",
            "role": "teammate",
            "model": "claude",
            "assistant_id": "assistant-1"
        });
        let req: AddAgentRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Helper");
        assert_eq!(req.role, "teammate");
        assert!(req.backend.is_none());
        assert_eq!(req.model, "claude");
        assert_eq!(req.assistant_id.as_deref(), Some("assistant-1"));
    }

    #[test]
    fn deserialize_add_agent_request_rejects_custom_agent_id() {
        let raw = json!({
            "name": "Custom",
            "role": "teammate",
            "model": "claude",
            "custom_agent_id": "custom-1"
        });
        let result = serde_json::from_value::<AddAgentRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_add_agent_request_with_assistant_id() {
        let raw = json!({
            "name": "Custom",
            "role": "teammate",
            "model": "claude",
            "assistant_id": "assistant-1"
        });
        let req: AddAgentRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.assistant_id.as_deref(), Some("assistant-1"));
    }

    #[test]
    fn deserialize_add_agent_request_from_assistant_field() {
        let raw = json!({
            "assistant": {
                "name": "Helper",
                "role": "teammate",
                "model": "claude",
                "assistant_id": "assistant-1"
            }
        });
        let req: AddAgentRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Helper");
        assert_eq!(req.role, "teammate");
        assert_eq!(req.model, "claude");
        assert_eq!(req.assistant_id.as_deref(), Some("assistant-1"));
        assert!(req.backend.is_none());
    }

    #[test]
    fn deserialize_add_agent_request_missing_name() {
        let raw = json!({
            "role": "teammate",
            "model": "claude",
            "assistant_id": "assistant-1"
        });
        let result = serde_json::from_value::<AddAgentRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_add_agent_request_requires_assistant_id() {
        let raw = json!({ "name": "X", "role": "teammate", "model": "claude" });
        let result = serde_json::from_value::<AddAgentRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_add_agent_request_allows_missing_backend_when_assistant_id_present() {
        let raw = json!({
            "name": "X",
            "role": "teammate",
            "model": "claude",
            "assistant_id": "assistant-1"
        });
        let req = serde_json::from_value::<AddAgentRequest>(raw).unwrap();
        assert!(req.backend.is_none());
        assert_eq!(req.assistant_id.as_deref(), Some("assistant-1"));
    }

    #[test]
    fn deserialize_add_agent_request_rejects_backend_field() {
        let raw = json!({
            "name": "X",
            "role": "teammate",
            "backend": "acp",
            "model": "claude",
            "assistant_id": "assistant-1"
        });
        let result = serde_json::from_value::<AddAgentRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_rename_agent_request() {
        let raw = json!({ "name": "New Agent Name" });
        let req: RenameAgentRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "New Agent Name");
    }

    #[test]
    fn deserialize_rename_agent_request_missing_name() {
        let raw = json!({});
        let result = serde_json::from_value::<RenameAgentRequest>(raw);
        assert!(result.is_err());
    }

    // -- C. Message & session requests ----------------------------------------

    #[test]
    fn deserialize_send_team_message_request() {
        let raw = json!({ "content": "Hello team!" });
        let req: SendTeamMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hello team!");
    }

    #[test]
    fn deserialize_send_team_message_request_missing_content() {
        let raw = json!({});
        let result = serde_json::from_value::<SendTeamMessageRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_send_agent_message_request() {
        let raw = json!({ "content": "Do this task" });
        let req: SendAgentMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Do this task");
    }

    #[test]
    fn deserialize_send_agent_message_request_missing_content() {
        let raw = json!({});
        let result = serde_json::from_value::<SendAgentMessageRequest>(raw);
        assert!(result.is_err());
    }

    // -- D. Response DTOs -----------------------------------------------------

    #[test]
    fn serialize_team_agent_response_snake_case() {
        let agent = TeamAgentResponse {
            slot_id: "slot-1".into(),
            assistant_name: "Lead Agent".into(),
            name: "Lead Agent".into(),
            role: "lead".into(),
            conversation_id: "conv-1".into(),
            assistant_backend: "acp".into(),
            backend: "acp".into(),
            icon: Some("/api/assets/logos/ai-major/claude.svg".into()),
            model: "claude".into(),
            assistant_id: Some("assistant-x".into()),
            status: Some("idle".into()),
            pending_confirmations: 2,
        };
        let json = serde_json::to_value(&agent).unwrap();
        assert_eq!(json["slot_id"], "slot-1");
        assert_eq!(json["assistant_name"], "Lead Agent");
        assert_eq!(json["name"], "Lead Agent");
        assert_eq!(json["role"], "lead");
        assert_eq!(json["conversation_id"], "conv-1");
        assert_eq!(json["assistant_backend"], "acp");
        assert_eq!(json["backend"], "acp");
        assert_eq!(json["icon"], "/api/assets/logos/ai-major/claude.svg");
        assert_eq!(json["model"], "claude");
        assert_eq!(json["assistant_id"], "assistant-x");
        assert!(json.get("custom_agent_id").is_none());
        assert_eq!(json["status"], "idle");
        assert_eq!(json["pending_confirmations"], 2);
    }

    #[test]
    fn serialize_team_agent_response_optional_fields_omitted() {
        let agent = TeamAgentResponse {
            slot_id: "slot-2".into(),
            assistant_name: "Worker".into(),
            name: "Worker".into(),
            role: "teammate".into(),
            conversation_id: "conv-2".into(),
            assistant_backend: "acp".into(),
            backend: "acp".into(),
            icon: None,
            model: "claude".into(),
            assistant_id: None,
            status: None,
            pending_confirmations: 0,
        };
        let json = serde_json::to_value(&agent).unwrap();
        assert!(json.get("icon").is_none());
        assert!(json.get("custom_agent_id").is_none());
        assert!(json.get("status").is_none());
    }

    #[test]
    fn serialize_team_response_snake_case() {
        let team = TeamResponse {
            id: "team-1".into(),
            name: "Alpha".into(),
            workspace: "/workspace/team-1".into(),
            assistants: vec![TeamAgentResponse {
                slot_id: "slot-1".into(),
                assistant_name: "Lead".into(),
                name: "Lead".into(),
                role: "lead".into(),
                conversation_id: "conv-1".into(),
                assistant_backend: "acp".into(),
                backend: "acp".into(),
                icon: Some("/api/assets/logos/ai-major/claude.svg".into()),
                model: "claude".into(),
                assistant_id: Some("assistant-x".into()),
                status: None,
                pending_confirmations: 0,
            }],
            leader_assistant_id: Some("slot-1".into()),
            created_at: 1700000000000,
            updated_at: 1700001000000,
        };
        let json = serde_json::to_value(&team).unwrap();
        assert_eq!(json["id"], "team-1");
        assert_eq!(json["name"], "Alpha");
        assert_eq!(json["workspace"], "/workspace/team-1");
        assert_eq!(json["leader_assistant_id"], "slot-1");
        assert_eq!(json["created_at"], 1700000000000_i64);
        assert_eq!(json["updated_at"], 1700001000000_i64);
        assert_eq!(json["assistants"].as_array().unwrap().len(), 1);
        assert_eq!(json["assistants"][0]["slot_id"], "slot-1");
    }

    #[test]
    fn serialize_team_response_no_lead() {
        let team = TeamResponse {
            id: "team-2".into(),
            name: "Beta".into(),
            workspace: String::new(),
            assistants: vec![],
            leader_assistant_id: None,
            created_at: 1700000000000,
            updated_at: 1700000000000,
        };
        let json = serde_json::to_value(&team).unwrap();
        assert!(json.get("leader_assistant_id").is_none());
        assert!(json["assistants"].as_array().unwrap().is_empty());
    }

    // -- E. WebSocket event payloads ------------------------------------------

    #[test]
    fn serialize_team_agent_status_payload() {
        let payload = TeamAgentStatusPayload {
            team_id: "team-1".into(),
            slot_id: "slot-1".into(),
            status: "working".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["slot_id"], "slot-1");
        assert_eq!(json["status"], "working");
    }

    #[test]
    fn serialize_team_agent_spawned_payload() {
        let payload = TeamAgentSpawnedPayload {
            team_id: "team-1".into(),
            assistant: TeamAgentResponse {
                slot_id: "slot-3".into(),
                assistant_name: "Dynamic Worker".into(),
                name: "Dynamic Worker".into(),
                role: "teammate".into(),
                conversation_id: "conv-3".into(),
                assistant_backend: "claude".into(),
                backend: "claude".into(),
                icon: Some("/api/assets/logos/ai-major/claude.svg".into()),
                model: "opus".into(),
                assistant_id: None,
                status: Some("idle".into()),
                pending_confirmations: 0,
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["assistant"]["slot_id"], "slot-3");
        assert_eq!(json["assistant"]["name"], "Dynamic Worker");
        assert_eq!(json["assistant"]["role"], "teammate");
        assert_eq!(json["assistant"]["status"], "idle");
    }

    #[test]
    fn serialize_team_agent_removed_payload() {
        let payload = TeamAgentRemovedPayload {
            team_id: "team-1".into(),
            slot_id: "slot-2".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["slot_id"], "slot-2");
    }

    #[test]
    fn serialize_team_agent_renamed_payload() {
        let payload = TeamAgentRenamedPayload {
            team_id: "team-1".into(),
            slot_id: "slot-1".into(),
            name: "Renamed Agent".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["slot_id"], "slot-1");
        assert_eq!(json["name"], "Renamed Agent");
    }

    // -- Roundtrip tests ------------------------------------------------------

    #[test]
    fn team_agent_response_roundtrip() {
        let agent = TeamAgentResponse {
            slot_id: "slot-1".into(),
            assistant_name: "Agent".into(),
            name: "Agent".into(),
            role: "lead".into(),
            conversation_id: "conv-1".into(),
            assistant_backend: "acp".into(),
            backend: "acp".into(),
            icon: Some("/api/assets/logos/ai-major/claude.svg".into()),
            model: "claude".into(),
            assistant_id: Some("custom-1".into()),
            status: Some("working".into()),
            pending_confirmations: 1,
        };
        let json = serde_json::to_string(&agent).unwrap();
        let parsed: TeamAgentResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, agent);
    }

    #[test]
    fn team_response_roundtrip() {
        let team = TeamResponse {
            id: "team-1".into(),
            name: "Alpha".into(),
            workspace: "/workspace/team-1".into(),
            assistants: vec![
                TeamAgentResponse {
                    slot_id: "s1".into(),
                    assistant_name: "Lead".into(),
                    name: "Lead".into(),
                    role: "lead".into(),
                    conversation_id: "c1".into(),
                    assistant_backend: "acp".into(),
                    backend: "acp".into(),
                    icon: None,
                    model: "claude".into(),
                    assistant_id: None,
                    status: None,
                    pending_confirmations: 0,
                },
                TeamAgentResponse {
                    slot_id: "s2".into(),
                    assistant_name: "Worker".into(),
                    name: "Worker".into(),
                    role: "teammate".into(),
                    conversation_id: "c2".into(),
                    assistant_backend: "acp".into(),
                    backend: "acp".into(),
                    icon: Some("/api/assets/logos/tools/coding/codex.svg".into()),
                    model: "claude".into(),
                    assistant_id: Some("x".into()),
                    status: Some("idle".into()),
                    pending_confirmations: 3,
                },
            ],
            leader_assistant_id: Some("s1".into()),
            created_at: 1000,
            updated_at: 2000,
        };
        let json = serde_json::to_string(&team).unwrap();
        let parsed: TeamResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, team);
    }

    #[test]
    fn team_agent_status_payload_roundtrip() {
        let payload = TeamAgentStatusPayload {
            team_id: "t1".into(),
            slot_id: "s1".into(),
            status: "thinking".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: TeamAgentStatusPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn team_agent_spawned_payload_roundtrip() {
        let payload = TeamAgentSpawnedPayload {
            team_id: "t1".into(),
            assistant: TeamAgentResponse {
                slot_id: "s3".into(),
                assistant_name: "New".into(),
                name: "New".into(),
                role: "teammate".into(),
                conversation_id: "c3".into(),
                assistant_backend: "claude".into(),
                backend: "claude".into(),
                icon: None,
                model: "sonnet".into(),
                assistant_id: None,
                status: None,
                pending_confirmations: 0,
            },
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: TeamAgentSpawnedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn team_agent_removed_payload_roundtrip() {
        let payload = TeamAgentRemovedPayload {
            team_id: "t1".into(),
            slot_id: "s2".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: TeamAgentRemovedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn team_agent_renamed_payload_roundtrip() {
        let payload = TeamAgentRenamedPayload {
            team_id: "t1".into(),
            slot_id: "s1".into(),
            name: "Renamed".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: TeamAgentRenamedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    // -- Deserialize from snake_case JSON (matching Rust field names) -----------

    #[test]
    fn deserialize_team_agent_response_from_snake_case() {
        let raw = json!({
            "slot_id": "s1",
            "name": "Agent",
            "role": "lead",
            "conversation_id": "c1",
            "backend": "acp",
            "model": "claude",
            "custom_agent_id": "cust-1",
            "status": "idle"
        });
        let agent: TeamAgentResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(agent.slot_id, "s1");
        assert_eq!(agent.conversation_id, "c1");
        assert_eq!(agent.assistant_id.as_deref(), Some("cust-1"));
        assert_eq!(agent.status.as_deref(), Some("idle"));
        assert_eq!(agent.pending_confirmations, 0);
    }

    #[test]
    fn deserialize_team_response_from_snake_case() {
        let raw = json!({
            "id": "team-1",
            "name": "Alpha",
            "agents": [],
            "lead_agent_id": "s1",
            "created_at": 1000,
            "updated_at": 2000
        });
        let team: TeamResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(team.id, "team-1");
        assert!(team.assistants.is_empty());
        assert_eq!(team.leader_assistant_id.as_deref(), Some("s1"));
        assert_eq!(team.created_at, 1000);
    }

    // -- F. TeamSessionStatus / TeamSessionPhase serde roundtrip -----------------

    fn assert_session_status_roundtrip(status: TeamSessionStatus, wire: &str) {
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json, serde_json::Value::String(wire.into()));
        let parsed: TeamSessionStatus = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, status);
    }

    fn assert_session_phase_roundtrip(phase: TeamSessionPhase, wire: &str) {
        let json = serde_json::to_value(&phase).unwrap();
        assert_eq!(json, serde_json::Value::String(wire.into()));
        let parsed: TeamSessionPhase = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, phase);
    }

    #[test]
    fn team_session_status_roundtrips() {
        assert_session_status_roundtrip(TeamSessionStatus::Starting, "starting");
        assert_session_status_roundtrip(TeamSessionStatus::Ready, "ready");
        assert_session_status_roundtrip(TeamSessionStatus::Failed, "failed");
    }

    #[test]
    fn team_session_phase_roundtrips() {
        assert_session_phase_roundtrip(TeamSessionPhase::LoadingTeam, "loading_team");
        assert_session_phase_roundtrip(TeamSessionPhase::StartingBridge, "starting_bridge");
        assert_session_phase_roundtrip(TeamSessionPhase::AttachingAgents, "attaching_agents");
        assert_session_phase_roundtrip(TeamSessionPhase::Recovering, "recovering");
    }

    // -- G. TeamSessionStatusPayload & TeammateMessagePayload --------------------

    #[test]
    fn serialize_team_session_status_payload_all_fields_present() {
        let payload = TeamSessionStatusPayload {
            team_id: "team-1".into(),
            status: TeamSessionStatus::Ready,
            phase: Some(TeamSessionPhase::Recovering),
            server_count: Some(7),
            error: Some("boom".into()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["status"], "ready");
        assert_eq!(json["phase"], "recovering");
        assert_eq!(json["server_count"], 7);
        assert_eq!(json["error"], "boom");
    }

    #[test]
    fn serialize_team_session_status_payload_optional_fields_omitted() {
        let payload = TeamSessionStatusPayload {
            team_id: "team-1".into(),
            status: TeamSessionStatus::Starting,
            phase: None,
            server_count: None,
            error: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["team_id"], "team-1");
        assert_eq!(json["status"], "starting");
        assert!(json.get("phase").is_none());
        assert!(json.get("server_count").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn serialize_teammate_message_payload_all_fields_present() {
        let payload = TeammateMessagePayload {
            conversation_id: "conv-9".into(),
            content: "ping".into(),
            from_slot_id: "slot-1".into(),
            from_name: "Lead".into(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["conversation_id"], "conv-9");
        assert_eq!(json["content"], "ping");
        assert_eq!(json["from_slot_id"], "slot-1");
        assert_eq!(json["from_name"], "Lead");
    }

    fn active_slot_work() -> TeamSlotWorkPayload {
        TeamSlotWorkPayload {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            state: TeamSlotWorkState::Running,
            queued_foreground_count: 2,
            queued_background_count: 1,
            active_turn_id: Some("turn-1".into()),
            active_turn_started_at_ms: Some(10),
            active_turn_elapsed_ms: Some(25),
            active_turn_slow: Some(false),
            active_turn_slow_threshold_ms: Some(600_000),
            blocked_reason: None,
            team_run_id: Some("run-1".into()),
        }
    }

    fn active_run_payload() -> TeamRunPayload {
        TeamRunPayload {
            team_id: "team-1".into(),
            team_run_id: "run-1".into(),
            source: TeamRunSource::UserMessage,
            has_user_intervention: true,
            target_slot_id: "lead-1".into(),
            target_role: TeamRunTargetRole::Lead,
            status: TeamRunStatus::Running,
            queued_intent_count: 2,
            starting_batch_count: 0,
            running_batch_count: 1,
            active_enqueue_lease_count: 0,
            slot_work: vec![active_slot_work()],
        }
    }

    #[test]
    fn team_run_source_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TeamRunSource::UserMessage).unwrap(),
            serde_json::json!("user_message")
        );
    }

    #[test]
    fn team_slot_work_payload_serializes_authoritative_coordinator_state() {
        assert_eq!(
            serde_json::to_value(active_slot_work()).unwrap(),
            serde_json::json!({
                "slot_id": "lead-1",
                "role": "lead",
                "state": "running",
                "queued_foreground_count": 2,
                "queued_background_count": 1,
                "active_turn_id": "turn-1",
                "active_turn_started_at_ms": 10,
                "active_turn_elapsed_ms": 25,
                "active_turn_slow": false,
                "active_turn_slow_threshold_ms": 600000,
                "blocked_reason": null,
                "team_run_id": "run-1"
            })
        );
    }

    #[test]
    fn team_run_payload_serializes_authoritative_work_counts() {
        let value = serde_json::to_value(active_run_payload()).unwrap();
        assert_eq!(value["queued_intent_count"], 2);
        assert_eq!(value["running_batch_count"], 1);
        assert_eq!(value["slot_work"][0]["state"], "running");
        assert!(value.get("content").is_none());
    }

    #[test]
    fn team_run_ack_embeds_the_complete_run_snapshot() {
        let ack = TeamRunAckResponse {
            enqueue_status: TeamMessageEnqueueStatus::Queued,
            message_id: "message-1".into(),
            run: active_run_payload(),
        };

        let value = serde_json::to_value(ack).unwrap();
        assert_eq!(value["enqueue_status"], "queued");
        assert_eq!(value["message_id"], "message-1");
        assert_eq!(value["run"]["team_run_id"], "run-1");
        assert_eq!(value["run"]["slot_work"][0]["active_turn_id"], "turn-1");
    }

    #[test]
    fn team_run_state_response_keeps_global_slot_work_without_active_run() {
        let value = serde_json::to_value(TeamRunStateResponse {
            session_generation: Some("generation-1".into()),
            active_run: None,
            slot_work: vec![active_slot_work()],
        })
        .unwrap();

        assert_eq!(value["session_generation"], "generation-1");
        assert!(value["active_run"].is_null());
        assert_eq!(value["slot_work"][0]["slot_id"], "lead-1");
    }

    #[test]
    fn team_slot_work_block_reasons_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(TeamSlotBlockedReason::RuntimeStarting).unwrap(),
            serde_json::json!("runtime_starting")
        );
        assert_eq!(
            serde_json::to_value(TeamSlotBlockedReason::SessionStopped).unwrap(),
            serde_json::json!("session_stopped")
        );
    }

    #[test]
    fn team_send_message_queued_response_serializes_authoritative_target() {
        let response = TeamSendMessageQueuedResponse {
            team_run_id: Some("run-1".into()),
            target: active_slot_work(),
        };
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["team_run_id"], "run-1");
        assert_eq!(value["target"]["state"], "running");
        assert_eq!(value["target"]["queued_foreground_count"], 2);
    }

    #[test]
    fn team_session_binding_decodes_persisted_extra_contract() {
        let extra = serde_json::json!({
            "teamId": "team-1",
            "slot_id": "lead-1",
            "role": "lead",
            "backend": "claude",
            "session_mode": "full_auto",
            "current_model_id": "opus",
            "team_mcp_stdio_config": {
                "team_id": "team-1",
                "port": 4242,
                "token": "token",
                "slot_id": "lead-1",
                "binary_path": "/tmp/aioncore"
            }
        });

        let binding = TeamSessionBinding::from_extra_value(&extra).unwrap().unwrap();

        assert_eq!(binding.team_id, "team-1");
        assert_eq!(binding.slot_id.as_deref(), Some("lead-1"));
        assert_eq!(binding.role.as_deref(), Some("lead"));
        assert_eq!(binding.runtime_seed.backend.as_deref(), Some("claude"));
        assert_eq!(binding.runtime_seed.session_mode.as_deref(), Some("full_auto"));
        assert_eq!(binding.runtime_seed.current_model_id.as_deref(), Some("opus"));
        let mcp = binding.mcp.unwrap();
        assert_eq!(mcp.stdio.team_id, "team-1");
        assert_eq!(mcp.stdio.slot_id, "lead-1");
    }

    #[test]
    fn team_session_binding_ignores_missing_or_blank_team_marker() {
        assert!(
            TeamSessionBinding::from_extra_value(&serde_json::json!({}))
                .unwrap()
                .is_none()
        );
        assert!(
            TeamSessionBinding::from_extra_value(&serde_json::json!({"teamId": "  "}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn team_session_binding_marker_reader_extracts_team_id_only() {
        let extra = r#"{"teamId":"team-9","team_mcp_stdio_config":{"invalid":true}}"#;
        assert_eq!(
            TeamSessionBinding::team_id_marker_from_extra_str(extra).as_deref(),
            Some("team-9")
        );
    }
}
