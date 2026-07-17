use aionui_common::Confirmation;
use async_trait::async_trait;

use crate::error::ChannelError;
use crate::types::PluginType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelApprovalContext {
    pub source_user_id: String,
    pub conversation_id: String,
    pub agent_id: Option<String>,
    pub platform: PluginType,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
}

#[async_trait]
pub trait ChannelApprovalPort: Send + Sync {
    async fn create(&self, context: ChannelApprovalContext, confirmation: Confirmation)
    -> Result<String, ChannelError>;

    async fn resolve(
        &self,
        source_user_id: &str,
        platform: PluginType,
        chat_id: &str,
        message_thread_id: Option<i64>,
        approval_id: &str,
        option_index: usize,
    ) -> Result<String, ChannelError>;
}
