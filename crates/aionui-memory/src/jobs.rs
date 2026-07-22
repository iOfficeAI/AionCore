use aionui_api_types::{MemoryJobResponse, MemoryJobState};
use aionui_db::models::{ConversationRow, EffectiveMemoryPolicyRow, MemoryJobRow, MessageRow};
use serde_json::Value;

/// Conversation-orchestrator outcome observed after canonical persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTurnOutcome {
    Completed,
    Failed,
    Canceled,
}

/// A claimed job paired with the opaque server-issued lease capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedMemoryJob {
    pub job: MemoryJobResponse,
    pub lease_token: String,
}

impl std::ops::Deref for ClaimedMemoryJob {
    type Target = MemoryJobResponse;

    fn deref(&self) -> &Self::Target {
        &self.job
    }
}

pub(crate) const MEMORY_DISCLOSURE_VERSION: i64 = 1;

pub(crate) fn eligible_completed_turn(
    conversation: &ConversationRow,
    policy: &EffectiveMemoryPolicyRow,
    messages: &[MessageRow],
    outcome: MemoryTurnOutcome,
) -> bool {
    if outcome != MemoryTurnOutcome::Completed
        || conversation.status.as_deref() != Some("finished")
        || !policy.enabled
        || !policy.capture_enabled
        || policy.consent_version != Some(MEMORY_DISCLOSURE_VERSION)
        || is_excluded_conversation(conversation)
    {
        return false;
    }

    let latest_message_at = messages.iter().map(|message| message.created_at).max();
    if latest_message_at.is_none()
        || policy
            .reset_at
            .is_some_and(|reset_at| latest_message_at.is_none_or(|created_at| created_at <= reset_at))
    {
        return false;
    }

    let has_visible_user_work = messages.iter().any(|message| visible_text(message, "right"));
    let has_visible_assistant_outcome = messages.iter().any(visible_assistant_outcome);
    has_visible_user_work && has_visible_assistant_outcome
}

fn is_excluded_conversation(conversation: &ConversationRow) -> bool {
    let kind = conversation.r#type.trim().to_ascii_lowercase();
    let source = conversation
        .source
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(
        kind.as_str(),
        "health_check" | "health-check" | "internal" | "ephemeral"
    ) || matches!(source.as_str(), "health_check" | "health-check" | "internal")
    {
        return true;
    }
    let Ok(Value::Object(extra)) = serde_json::from_str(&conversation.extra) else {
        return true;
    };
    ["health_check", "internal", "ephemeral"]
        .into_iter()
        .any(|key| extra.get(key).and_then(Value::as_bool) == Some(true))
}

fn visible_text(message: &MessageRow, position: &str) -> bool {
    !message.hidden
        && message.position.as_deref() == Some(position)
        && message.r#type == "text"
        && message.status.as_deref() == Some("finish")
        && serde_json::from_str::<Value>(&message.content)
            .ok()
            .and_then(|value| value.get("content").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|content| !content.trim().is_empty())
}

fn visible_assistant_outcome(message: &MessageRow) -> bool {
    if message.hidden || message.position.as_deref() != Some("left") || message.status.as_deref() != Some("finish") {
        return false;
    }
    let field = match message.r#type.as_str() {
        "text" | "artifact" => "content",
        "tool_result_summary" => "summary",
        _ => return false,
    };
    serde_json::from_str::<Value>(&message.content)
        .ok()
        .and_then(|value| value.get(field).and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|content| !content.trim().is_empty())
}

pub(crate) fn job_response(row: MemoryJobRow) -> Result<MemoryJobResponse, crate::MemoryError> {
    Ok(MemoryJobResponse {
        id: row.id,
        user_id: row.user_id,
        conversation_id: row.conversation_id,
        from_turn_id: row.from_turn_id,
        through_turn_id: row.through_turn_id,
        operation_version: row.operation_version,
        input_hash: row.input_hash,
        expected_revision: row
            .expected_revision
            .try_into()
            .map_err(|_| crate::MemoryError::Internal)?,
        state: match row.state.as_str() {
            "pending" => MemoryJobState::Pending,
            "running" => MemoryJobState::Running,
            "retry_wait" => MemoryJobState::RetryWait,
            "blocked" => MemoryJobState::Blocked,
            "succeeded" => MemoryJobState::Succeeded,
            "failed" => MemoryJobState::Failed,
            "canceled" => MemoryJobState::Canceled,
            _ => return Err(crate::MemoryError::Internal),
        },
        attempt_count: row.attempt_count.try_into().map_err(|_| crate::MemoryError::Internal)?,
        next_attempt_at: row.next_attempt_at,
        lease_owner: row.lease_owner,
        lease_expires_at: row.lease_expires_at,
        last_error_code: row.last_error_code,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use aionui_db::models::{ConversationRow, EffectiveMemoryPolicyRow, MessageRow};

    use super::{MemoryTurnOutcome, eligible_completed_turn};

    const USER_ID: &str = "system_default_user";
    const CONVERSATION_ID: &str = "conversation-1";
    const TURN_ID: &str = "turn-1";

    #[test]
    fn only_durable_finished_visible_work_is_eligible() {
        let conversation = make_conversation("gemini", "{}", Some("aionui"));
        let policy = make_policy(Some(1), None);
        let ordinary = vec![
            user_message(false, "text", "finish", 10),
            assistant_message(false, "text", "finish", 11),
        ];

        assert!(eligible_completed_turn(
            &conversation,
            &policy,
            &ordinary,
            MemoryTurnOutcome::Completed,
        ));

        let cases = [
            (
                "empty",
                Vec::new(),
                conversation.clone(),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "canceled",
                ordinary.clone(),
                conversation.clone(),
                policy.clone(),
                MemoryTurnOutcome::Canceled,
            ),
            (
                "failed",
                ordinary.clone(),
                conversation.clone(),
                policy.clone(),
                MemoryTurnOutcome::Failed,
            ),
            (
                "hidden-only",
                vec![
                    user_message(true, "text", "finish", 10),
                    assistant_message(true, "text", "finish", 11),
                ],
                conversation.clone(),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "permission-only",
                vec![user_message(false, "permission_prompt", "finish", 10)],
                conversation.clone(),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "health-check",
                ordinary.clone(),
                make_conversation("health_check", "{}", Some("aionui")),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "internal",
                ordinary.clone(),
                make_conversation("gemini", r#"{"internal":true}"#, Some("aionui")),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "ephemeral",
                ordinary.clone(),
                make_conversation("gemini", r#"{"ephemeral":true}"#, Some("aionui")),
                policy.clone(),
                MemoryTurnOutcome::Completed,
            ),
            (
                "pre-reset",
                ordinary.clone(),
                conversation.clone(),
                make_policy(Some(1), Some(11)),
                MemoryTurnOutcome::Completed,
            ),
            (
                "capture-disabled",
                ordinary.clone(),
                conversation.clone(),
                EffectiveMemoryPolicyRow {
                    capture_enabled: false,
                    ..policy.clone()
                },
                MemoryTurnOutcome::Completed,
            ),
            (
                "disclosure-not-accepted",
                ordinary,
                conversation,
                make_policy(None, None),
                MemoryTurnOutcome::Completed,
            ),
        ];

        for (label, messages, conversation, policy, outcome) in cases {
            assert!(
                !eligible_completed_turn(&conversation, &policy, &messages, outcome),
                "{label} turn should be ineligible",
            );
        }

        for assistant_type in ["artifact", "tool_result_summary"] {
            let field = if assistant_type == "artifact" {
                "content"
            } else {
                "summary"
            };
            let mut assistant = assistant_message(false, assistant_type, "finish", 11);
            assistant.content = serde_json::json!({ (field): "Durable assistant outcome" }).to_string();
            assert!(eligible_completed_turn(
                &make_conversation("gemini", "{}", Some("aionui")),
                &make_policy(Some(1), None),
                &[user_message(false, "text", "finish", 10), assistant],
                MemoryTurnOutcome::Completed,
            ));
        }
    }

    fn make_conversation(kind: &str, extra: &str, source: Option<&str>) -> ConversationRow {
        ConversationRow {
            id: CONVERSATION_ID.into(),
            user_id: USER_ID.into(),
            name: "Conversation".into(),
            r#type: kind.into(),
            extra: extra.into(),
            model: None,
            status: Some("finished".into()),
            source: source.map(str::to_owned),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn make_policy(consent_version: Option<i64>, reset_at: Option<i64>) -> EffectiveMemoryPolicyRow {
        EffectiveMemoryPolicyRow {
            user_id: USER_ID.into(),
            conversation_id: CONVERSATION_ID.into(),
            enabled: true,
            capture_enabled: true,
            recall_enabled: true,
            capture_override: None,
            recall_override: None,
            consent_version,
            consented_at: consent_version.map(|_| 1),
            reset_at,
        }
    }

    fn user_message(hidden: bool, kind: &str, status: &str, created_at: i64) -> MessageRow {
        message("user", "right", hidden, kind, status, "Do the work", created_at)
    }

    fn assistant_message(hidden: bool, kind: &str, status: &str, created_at: i64) -> MessageRow {
        message("assistant", "left", hidden, kind, status, "Work completed", created_at)
    }

    fn message(
        id: &str,
        position: &str,
        hidden: bool,
        kind: &str,
        status: &str,
        content: &str,
        created_at: i64,
    ) -> MessageRow {
        MessageRow {
            id: id.into(),
            conversation_id: CONVERSATION_ID.into(),
            turn_id: Some(TURN_ID.into()),
            msg_id: Some(id.into()),
            r#type: kind.into(),
            content: serde_json::json!({ "content": content }).to_string(),
            position: Some(position.into()),
            status: Some(status.into()),
            hidden,
            created_at,
        }
    }
}
