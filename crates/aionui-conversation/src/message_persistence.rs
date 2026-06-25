use aionui_ai_agent::AgentSendError;
use aionui_common::{ErrorChain, now_ms};
use aionui_db::models::MessageRow;
use tracing::warn;

use crate::runtime_persistence::RuntimeWriteKind;
use crate::service::ConversationService;

impl ConversationService {
    pub(crate) async fn persist_send_failure_tip(
        &self,
        conversation_id: &str,
        err: &AgentSendError,
        top_level_code: Option<&'static str>,
    ) -> Option<MessageRow> {
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::SendFailureTip)
        {
            return None;
        }

        let stream_error = err.stream_error();
        let code = top_level_code.map(str::to_owned).or_else(|| {
            stream_error
                .code
                .and_then(|code| serde_json::to_value(code).ok())
                .and_then(|value| value.as_str().map(str::to_owned))
        });
        let details = match stream_error.workspace_path.as_deref() {
            Some(workspace_path) => serde_json::json!({
                "detail": stream_error.detail,
                "workspace_path": workspace_path,
            }),
            None => serde_json::to_value(&stream_error.detail).unwrap_or(serde_json::Value::Null),
        };
        let row = MessageRow {
            id: Self::mint_msg_id(),
            conversation_id: conversation_id.to_owned(),
            msg_id: None,
            r#type: "tips".into(),
            content: serde_json::json!({
                "content": &stream_error.message,
                "type": "error",
                "source": "send_failed",
                "code": code,
                "details": details,
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
