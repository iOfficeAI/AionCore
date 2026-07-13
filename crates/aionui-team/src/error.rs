use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error("Team not found: {0}")]
    TeamNotFound(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Leader-only action: {0}")]
    LeaderOnly(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Blocked task not found: {0}")]
    BlockedTaskNotFound(String),

    #[error("Backend not allowed: {0}")]
    BackendNotAllowed(String),

    #[error("Agent name already taken: {0}")]
    DuplicateAgentName(String),

    #[error("Team agent runtime is not ready for conversation: {conversation_id}")]
    RuntimeNotReady { conversation_id: String },

    #[error("Team member runtime failed: {slot_id}")]
    MemberRuntimeFailed {
        team_id: String,
        slot_id: String,
        conversation_id: String,
        public_reason: String,
    },

    #[error("Workspace path is unavailable: {0}")]
    WorkspacePathUnavailable(String),

    #[error("Workspace path is unavailable during execution: {0}")]
    WorkspacePathRuntimeUnavailable(String),

    #[error("{0}")]
    Database(#[from] aionui_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TeamPublicError {
    pub code: &'static str,
    pub details: Option<Value>,
}

impl TeamPublicError {
    fn new(code: &'static str, details: Option<Value>) -> Self {
        Self { code, details }
    }
}

pub fn classify_public_error(message: &str) -> Option<TeamPublicError> {
    if matches!(
        message,
        "Missing required field: assistant_id"
            | "spawn_agent.assistant_id is required"
            | "assistant_id is required when the caller conversation is not assistant-backed"
    ) {
        return Some(TeamPublicError::new(
            "TEAM_ASSISTANT_ID_REQUIRED",
            Some(json!({ "field": "assistant_id" })),
        ));
    }

    if let Some(assistant_id) = message.strip_prefix("Preset assistant not found: ") {
        return Some(TeamPublicError::new(
            "TEAM_ASSISTANT_NOT_FOUND",
            Some(json!({ "assistant_id": assistant_id })),
        ));
    }

    for field in ["backend", "agent_type", "custom_agent_id"] {
        if message == format!("{field} is no longer accepted; use assistant_id") {
            return Some(TeamPublicError::new(
                "TEAM_ASSISTANT_FIELD_UNSUPPORTED",
                Some(json!({
                    "field": field,
                    "required_field": "assistant_id",
                })),
            ));
        }
    }

    if message == "model is no longer accepted; use the assistant configuration or UI model selector" {
        return Some(TeamPublicError::new(
            "TEAM_ASSISTANT_FIELD_UNSUPPORTED",
            Some(json!({
                "field": "model",
                "required_field": "assistant_id",
            })),
        ));
    }

    if message == "team_list_assistants does not accept arguments" {
        return Some(TeamPublicError::new(
            "TEAM_TOOL_ARGUMENTS_NOT_ALLOWED",
            Some(json!({
                "tool": "team_list_assistants",
            })),
        ));
    }

    if message == "Team service not available" {
        return Some(TeamPublicError::new("TEAM_SERVICE_UNAVAILABLE", None));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(TeamError::TeamNotFound("t1".into()).to_string(), "Team not found: t1");
        assert_eq!(TeamError::AgentNotFound("s1".into()).to_string(), "Agent not found: s1");
        assert_eq!(TeamError::TaskNotFound("tk1".into()).to_string(), "Task not found: tk1");
    }

    #[test]
    fn classify_public_error_recognizes_branch_assistant_first_failures() {
        let required = classify_public_error("Missing required field: assistant_id").expect("classified");
        assert_eq!(required.code, "TEAM_ASSISTANT_ID_REQUIRED");
        assert_eq!(required.details, Some(json!({ "field": "assistant_id" })));

        let assistant = classify_public_error("Preset assistant not found: bare:abcd1234").expect("assistant lookup");
        assert_eq!(assistant.code, "TEAM_ASSISTANT_NOT_FOUND");
        assert_eq!(assistant.details, Some(json!({ "assistant_id": "bare:abcd1234" })));

        let legacy = classify_public_error("backend is no longer accepted; use assistant_id").expect("legacy field");
        assert_eq!(legacy.code, "TEAM_ASSISTANT_FIELD_UNSUPPORTED");
        assert_eq!(
            legacy.details,
            Some(json!({
                "field": "backend",
                "required_field": "assistant_id",
            }))
        );

        let model =
            classify_public_error("model is no longer accepted; use the assistant configuration or UI model selector")
                .expect("model field");
        assert_eq!(model.code, "TEAM_ASSISTANT_FIELD_UNSUPPORTED");
        assert_eq!(
            model.details,
            Some(json!({
                "field": "model",
                "required_field": "assistant_id",
            }))
        );
    }
}
