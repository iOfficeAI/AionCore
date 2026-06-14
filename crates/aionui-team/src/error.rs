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

    #[error("Team slot is busy: {0}")]
    SlotBusy(String),

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

    #[error("Workspace path is unavailable: {0}")]
    WorkspacePathUnavailable(String),

    #[error("Workspace path is unavailable during execution: {0}")]
    WorkspacePathRuntimeUnavailable(String),

    #[error("{0}")]
    Database(#[from] aionui_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(TeamError::TeamNotFound("t1".into()).to_string(), "Team not found: t1");
        assert_eq!(TeamError::AgentNotFound("s1".into()).to_string(), "Agent not found: s1");
        assert_eq!(TeamError::TaskNotFound("tk1".into()).to_string(), "Task not found: tk1");
        assert_eq!(
            TeamError::SlotBusy("lead-1".into()).to_string(),
            "Team slot is busy: lead-1"
        );
    }
}
