use aionui_common::ErrorChain;
use aionui_db::MessageRowUpdate;
use aionui_db::models::MessageRow;
use tracing::{info, warn};

use crate::runtime_persistence::RuntimeWriteKind;
use crate::service::ConversationService;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupRecoveryAction {
    FinishVisibleOutput,
    FinishEmptyPlaceholder,
}

impl ConversationService {
    pub async fn recover_stale_runtime_state_on_startup(&self) {
        let rows = match self.conversation_repo().list_stale_runtime_messages().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(
                    error = %ErrorChain(&error),
                    "startup recovery skipped because stale runtime message query failed"
                );
                return;
            }
        };

        let mut recovered = 0usize;
        for row in rows {
            if !self
                .runtime_persistence()
                .allows(&row.conversation_id, RuntimeWriteKind::StartupRecovery)
            {
                continue;
            }

            let action = classify_recovery_action(&row);
            let update = MessageRowUpdate {
                content: None,
                status: Some(Some("finish".to_owned())),
                hidden: Some(matches!(action, StartupRecoveryAction::FinishEmptyPlaceholder)),
            };

            match self.conversation_repo().update_message(&row.id, &update).await {
                Ok(()) => {
                    recovered += 1;
                    info!(
                        conversation_id = %row.conversation_id,
                        msg_id = ?row.msg_id,
                        message_type = %row.r#type,
                        recovery_action = ?action,
                        "startup recovery closed stale runtime message"
                    );
                }
                Err(error) => {
                    warn!(
                        conversation_id = %row.conversation_id,
                        msg_id = ?row.msg_id,
                        error = %ErrorChain(&error),
                        "startup recovery failed to close stale runtime message"
                    );
                }
            }
        }

        if recovered > 0 {
            info!(recovered, "startup recovery completed for stale runtime messages");
        }
    }
}

fn classify_recovery_action(row: &MessageRow) -> StartupRecoveryAction {
    if message_has_visible_content(row) {
        StartupRecoveryAction::FinishVisibleOutput
    } else {
        StartupRecoveryAction::FinishEmptyPlaceholder
    }
}

fn message_has_visible_content(row: &MessageRow) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&row.content) else {
        return !row.content.trim().is_empty();
    };

    value
        .get("content")
        .and_then(|content| content.as_str())
        .is_some_and(|content| !content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use aionui_db::models::MessageRow;

    use super::*;

    #[test]
    fn visible_text_finishes_as_visible_output() {
        let row = MessageRow {
            id: "msg-1".into(),
            conversation_id: "conv-1".into(),
            msg_id: Some("msg-1".into()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": "hello" }).to_string(),
            position: Some("left".into()),
            status: Some("work".into()),
            hidden: false,
            created_at: 1,
        };

        assert_eq!(
            classify_recovery_action(&row),
            StartupRecoveryAction::FinishVisibleOutput
        );
    }

    #[test]
    fn empty_text_finishes_as_hidden_placeholder() {
        let row = MessageRow {
            id: "msg-1".into(),
            conversation_id: "conv-1".into(),
            msg_id: Some("msg-1".into()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": "" }).to_string(),
            position: Some("left".into()),
            status: Some("work".into()),
            hidden: false,
            created_at: 1,
        };

        assert_eq!(
            classify_recovery_action(&row),
            StartupRecoveryAction::FinishEmptyPlaceholder
        );
    }
}
