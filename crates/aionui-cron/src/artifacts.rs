use std::sync::Arc;

use aionui_api_types::{ConversationArtifactResponse, WebSocketMessage};
use aionui_common::generate_id;
use aionui_db::ConversationArtifactRow;
use aionui_realtime::EventBroadcaster;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::error::CronError;
use crate::types::CronJob;

pub(crate) fn build_cron_trigger_artifact(
    conversation_id: &str,
    job: &CronJob,
    created_at: i64,
) -> ConversationArtifactRow {
    let id = format!("{conversation_id}:cron_trigger:{}", generate_id());
    let payload = json!({
        "cron_job_id": job.id,
        "cron_job_name": job.name,
        "triggered_at": created_at,
    });

    ConversationArtifactRow {
        id,
        conversation_id: conversation_id.to_owned(),
        cron_job_id: Some(job.id.clone()),
        kind: "cron_trigger".into(),
        status: "active".into(),
        payload: payload.to_string(),
        created_at,
        updated_at: created_at,
    }
}

pub(crate) fn build_skill_suggest_artifact(
    conversation_id: &str,
    job_id: &str,
    name: &str,
    description: &str,
    skill_content: &str,
    now: i64,
) -> ConversationArtifactRow {
    let id = format!("{conversation_id}:skill_suggest:{job_id}");
    let payload = json!({
        "cron_job_id": job_id,
        "name": name,
        "description": description,
        "skillContent": skill_content,
    });

    ConversationArtifactRow {
        id,
        conversation_id: conversation_id.to_owned(),
        cron_job_id: Some(job_id.to_owned()),
        kind: "skill_suggest".into(),
        status: "pending".into(),
        payload: payload.to_string(),
        created_at: now,
        updated_at: now,
    }
}

pub(crate) fn artifact_response_from_row(
    row: &ConversationArtifactRow,
) -> Result<ConversationArtifactResponse, CronError> {
    Ok(ConversationArtifactResponse {
        id: row.id.clone(),
        conversation_id: row.conversation_id.clone(),
        cron_job_id: row.cron_job_id.clone(),
        kind: parse_enum(&row.kind)?,
        status: parse_enum(&row.status)?,
        payload: serde_json::from_str(&row.payload)
            .map_err(|e| CronError::Scheduler(format!("invalid artifact payload JSON: {e}")))?,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub(crate) fn broadcast_artifact(
    broadcaster: &Arc<dyn EventBroadcaster>,
    row: &ConversationArtifactRow,
) -> Result<(), CronError> {
    let payload = serde_json::to_value(artifact_response_from_row(row)?)
        .map_err(|e| CronError::Scheduler(format!("failed to serialize artifact event: {e}")))?;
    broadcaster.broadcast(WebSocketMessage::new("conversation.artifact", payload));
    Ok(())
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, CronError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|e| CronError::Scheduler(format!("invalid artifact enum value '{value}': {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreatedBy, CronJob, CronSchedule, ExecutionMode};

    fn sample_job() -> CronJob {
        CronJob {
            id: "cron_1".into(),
            name: "Daily Report".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60_000,
                description: None,
            },
            message: "Run".into(),
            execution_mode: ExecutionMode::NewConversation,
            agent_config: None,
            conversation_id: "conv_1".into(),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 1000,
            next_run_at: Some(2000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        }
    }

    #[test]
    fn builds_skill_suggest_response() {
        let row = build_skill_suggest_artifact(
            "conv_1",
            "cron_1",
            "daily-report",
            "Daily report",
            "---\nname: daily-report\n---\nUse it.",
            1234,
        );

        let response = artifact_response_from_row(&row).unwrap();
        assert_eq!(response.kind, aionui_api_types::ConversationArtifactKind::SkillSuggest);
        assert_eq!(response.status, aionui_api_types::ConversationArtifactStatus::Pending);
        assert_eq!(response.payload["name"], "daily-report");
    }

    #[test]
    fn builds_cron_trigger_payload() {
        let row = build_cron_trigger_artifact("conv_1", &sample_job(), 1234);
        let response = artifact_response_from_row(&row).unwrap();
        assert_eq!(response.kind, aionui_api_types::ConversationArtifactKind::CronTrigger);
        assert_eq!(response.payload["cron_job_id"], "cron_1");
        assert_eq!(response.payload["cron_job_name"], "Daily Report");
    }
}
