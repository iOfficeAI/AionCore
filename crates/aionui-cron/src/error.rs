use aionui_conversation::ConversationError;

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("Cron job not found: {0}")]
    JobNotFound(String),

    #[error("Invalid schedule: {0}")]
    InvalidSchedule(String),

    #[error("Invalid cron expression: {0}")]
    InvalidCronExpression(String),

    #[error("Invalid execution mode: {0}")]
    InvalidExecutionMode(String),

    #[error("Invalid created-by value: {0}")]
    InvalidCreatedBy(String),

    #[error("Invalid job status: {0}")]
    InvalidJobStatus(String),

    #[error("Invalid timezone: {0}")]
    InvalidTimezone(String),

    #[error("Invalid skill content: {0}")]
    InvalidSkillContent(String),

    #[error("Invalid agent config: {0}")]
    InvalidAgentConfig(String),

    #[error("Scheduler error: {0}")]
    Scheduler(String),

    #[error("Workspace path is unavailable: {0}")]
    WorkspacePathUnavailable(String),

    #[error("Workspace path is unavailable during execution: {0}")]
    WorkspacePathRuntimeUnavailable(String),

    #[error(transparent)]
    Conversation(#[from] ConversationError),

    #[error("{0}")]
    Database(#[from] aionui_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl CronError {
    pub(crate) fn from_conversation_create(error: ConversationError) -> Self {
        match error {
            ConversationError::WorkspacePathUnavailable { path } => Self::WorkspacePathUnavailable(path),
            ConversationError::WorkspacePathRuntimeUnavailable { path } => Self::WorkspacePathRuntimeUnavailable(path),
            other => Self::Scheduler(format!("create conversation: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_create_preserves_workspace_error_code() {
        let err = CronError::from_conversation_create(ConversationError::WorkspacePathUnavailable {
            path: "/tmp/a b".into(),
        });
        assert!(matches!(err, CronError::WorkspacePathUnavailable(msg) if msg == "/tmp/a b"));
    }

    #[test]
    fn display_messages() {
        assert_eq!(
            CronError::JobNotFound("cron_1".into()).to_string(),
            "Cron job not found: cron_1"
        );
        assert_eq!(
            CronError::InvalidSchedule("bad".into()).to_string(),
            "Invalid schedule: bad"
        );
        assert_eq!(
            CronError::InvalidCronExpression("* *".into()).to_string(),
            "Invalid cron expression: * *"
        );
    }
}
