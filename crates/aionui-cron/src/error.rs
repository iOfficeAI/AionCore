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

    #[error("Workspace path contains whitespace: {0}")]
    WorkspacePathContainsWhitespace(String),

    #[error("Workspace path contains whitespace and is unsupported at runtime: {0}")]
    WorkspacePathContainsWhitespaceRuntimeUnsupported(String),

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
            ConversationError::WorkspacePathContainsWhitespace { path } => Self::WorkspacePathContainsWhitespace(path),
            ConversationError::WorkspacePathContainsWhitespaceRuntimeUnsupported { path } => {
                Self::WorkspacePathContainsWhitespaceRuntimeUnsupported(path)
            }
            other => Self::Scheduler(format!("create conversation: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_create_preserves_workspace_error_code() {
        let err = CronError::from_conversation_create(ConversationError::WorkspacePathContainsWhitespace {
            path: "/tmp/a b".into(),
        });
        assert!(matches!(err, CronError::WorkspacePathContainsWhitespace(msg) if msg == "/tmp/a b"));
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
