use aionui_ai_agent::AgentSendError;
use aionui_common::{AppError, ErrorChain, now_ms};
use aionui_db::models::MessageRow;
use tracing::warn;

use crate::service::ConversationService;

impl ConversationService {
    pub(crate) async fn persist_send_failure_tip(&self, conversation_id: &str, err: &AppError) -> Option<MessageRow> {
        let stream_error = AgentSendError::from_app_error_ref(err).into_stream_error();
        let row = MessageRow {
            id: Self::mint_msg_id(),
            conversation_id: conversation_id.to_owned(),
            msg_id: None,
            r#type: "tips".into(),
            content: serde_json::json!({
                "content": &stream_error.message,
                "type": "error",
                "source": "send_failed",
                "code": err.error_code(),
                "details": err.error_details(),
                "error": stream_error,
            })
            .to_string(),
            position: Some("center".into()),
            status: Some("error".into()),
            hidden: false,
            created_at: now_ms(),
        };

        if let Err(store_err) = self.conversation_repo().insert_message(&row).await {
            warn!(
                conversation_id,
                error = %ErrorChain(&store_err),
                "Failed to persist send failure error tip"
            );
            return None;
        }

        Some(row)
    }
}
