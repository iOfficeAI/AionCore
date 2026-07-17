use async_trait::async_trait;

use crate::error::ChannelError;
use crate::types::PluginType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDevelopmentCommand {
    Project,
    RunInfo,
    DiffSummary,
    Test,
    Stop,
    Retry,
    Handoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDevelopmentContext {
    pub source_user_id: String,
    pub conversation_id: Option<String>,
    pub platform: PluginType,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
}

#[async_trait]
pub trait ChannelDevelopmentPort: Send + Sync {
    async fn execute(
        &self,
        context: ChannelDevelopmentContext,
        command: ChannelDevelopmentCommand,
    ) -> Result<String, ChannelError>;
}
