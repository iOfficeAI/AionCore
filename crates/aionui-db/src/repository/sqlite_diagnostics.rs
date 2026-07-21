use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::repository::diagnostics::{
    FeedbackDiagnosticsProfile, FeedbackDiagnosticsProfileResult, FeedbackDiagnosticsRequest,
    FeedbackDiagnosticsResult, IFeedbackDiagnosticsRepository,
};
use crate::repository::diagnostics_sanitizer::sanitize_mcp_original_json;

const RECENT_CONVERSATION_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const RECENT_CONVERSATION_LIMIT: i64 = 20;
const GLOBAL_RECENT_CONVERSATION_LIMIT: i64 = 20;
const GLOBAL_RECENT_CONVERSATION_SCAN_LIMIT: i64 = 100;
const GLOBAL_CONVERSATION_ERROR_LIMIT: i64 = 10;
const GLOBAL_CONVERSATION_MESSAGE_LIMIT: i64 = 10;
const GLOBAL_RECENT_ERROR_LIMIT: i64 = 20;
const GLOBAL_HEALTH_LIMIT: i64 = 20;

#[derive(Clone, Debug)]
pub struct SqliteFeedbackDiagnosticsRepository {
    pool: SqlitePool,
}

impl SqliteFeedbackDiagnosticsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn collect_conversation_session(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let Some(conversation_id) = request.context.conversation_id.as_deref() else {
            return self.collect_conversation_summary(&request.user_id).await;
        };

        let conversation = sqlx::query(
            "SELECT \
                id, type, status, source, pinned, created_at, updated_at, \
                name AS title, length(name) AS name_length, length(extra) AS extra_bytes, \
                CASE WHEN json_valid(model) THEN COALESCE(json_extract(model, '$.providerId'), json_extract(model, '$.provider_id')) END AS model_provider_id, \
                CASE WHEN json_valid(model) THEN COALESCE(json_extract(model, '$.modelId'), json_extract(model, '$.model_id')) END AS model_id, \
                CASE WHEN json_valid(extra) THEN COALESCE(json_extract(extra, '$.agentId'), json_extract(extra, '$.agent_id')) END AS extra_agent_id, \
                CASE WHEN json_valid(extra) THEN COALESCE(json_extract(extra, '$.teamId'), json_extract(extra, '$.team_id')) END AS extra_team_id \
             FROM conversations \
             WHERE id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(&request.user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(conversation) = conversation else {
            return Ok(profile_result(
                FeedbackDiagnosticsProfile::ConversationSession,
                "not_found",
                json!({ "conversation": Value::Null }),
            ));
        };

        let agent_id = request.context.agent_id.as_deref().map(ToOwned::to_owned).or_else(|| {
            conversation
                .try_get::<Option<String>, _>("extra_agent_id")
                .ok()
                .flatten()
        });

        let data = json!({
            "conversation": {
                "id": conversation.try_get::<String, _>("id")?,
                "type": conversation.try_get::<String, _>("type")?,
                "status": conversation.try_get::<Option<String>, _>("status")?,
                "source": conversation.try_get::<Option<String>, _>("source")?,
                "pinned": conversation.try_get::<bool, _>("pinned")?,
                "title": conversation.try_get::<String, _>("title")?,
                "name_length": conversation.try_get::<Option<i64>, _>("name_length")?,
                "extra_bytes": conversation.try_get::<Option<i64>, _>("extra_bytes")?,
                "model_provider_id": conversation.try_get::<Option<String>, _>("model_provider_id")?,
                "model_id": conversation.try_get::<Option<String>, _>("model_id")?,
                "extra_agent_id": conversation.try_get::<Option<String>, _>("extra_agent_id")?,
                "extra_team_id": conversation.try_get::<Option<String>, _>("extra_team_id")?,
                "created_at": conversation.try_get::<i64, _>("created_at")?,
                "updated_at": conversation.try_get::<i64, _>("updated_at")?,
            },
            "messages": self.collect_message_diagnostics(conversation_id).await?,
            "recent_conversations": self.collect_recent_conversations(
                &request.user_id,
                conversation_id,
                conversation.try_get::<i64, _>("updated_at")?,
            ).await?,
            "acp_session": self.collect_acp_session(conversation_id).await?,
            "agent_metadata": self.collect_agent_metadata(agent_id.as_deref()).await?,
            "assistant_snapshot": self.collect_assistant_snapshot(&request.user_id, conversation_id).await?,
        });

        Ok(profile_result(
            FeedbackDiagnosticsProfile::ConversationSession,
            "detail",
            data,
        ))
    }

    async fn collect_conversation_summary(&self, user_id: &str) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let rows = sqlx::query(
            "SELECT c.type, c.status, COUNT(*) AS count, MAX(c.updated_at) AS last_updated_at \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL)) \
             GROUP BY c.type, c.status \
             ORDER BY last_updated_at DESC \
             LIMIT 25",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| {
                Ok(json!({
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "count": row.try_get::<i64, _>("count")?,
                    "last_updated_at": row.try_get::<Option<i64>, _>("last_updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(profile_result(
            FeedbackDiagnosticsProfile::ConversationSession,
            "summary",
            json!({ "conversations": items }),
        ))
    }

    async fn collect_recent_conversations(
        &self,
        user_id: &str,
        anchor_conversation_id: &str,
        anchor_updated_at: i64,
    ) -> Result<Value, DbError> {
        let window_start = anchor_updated_at.saturating_sub(RECENT_CONVERSATION_WINDOW_MS);
        let rows = sqlx::query(
            "SELECT \
                c.id, c.name AS title, c.type, c.status, c.source, c.pinned, c.created_at, c.updated_at, \
                length(c.extra) AS extra_bytes, \
                CASE WHEN json_valid(c.model) THEN COALESCE(json_extract(c.model, '$.providerId'), json_extract(c.model, '$.provider_id')) END AS model_provider_id, \
                CASE WHEN json_valid(c.model) THEN COALESCE(json_extract(c.model, '$.modelId'), json_extract(c.model, '$.model_id')) END AS model_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.agentId'), json_extract(c.extra, '$.agent_id')) END AS extra_agent_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END AS extra_team_id, \
                (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS message_count, \
                (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id AND (m.status = 'error' OR m.type = 'tips' OR (json_valid(m.content) AND json_extract(m.content, '$.error.code') IS NOT NULL))) AS error_message_count, \
                (SELECT CASE WHEN json_valid(m.content) THEN COALESCE(json_extract(m.content, '$.error.code'), json_extract(m.content, '$.code')) END \
                  FROM messages m \
                  WHERE m.conversation_id = c.id \
                    AND (m.status = 'error' OR m.type = 'tips' OR (json_valid(m.content) AND json_extract(m.content, '$.error.code') IS NOT NULL)) \
                    AND json_valid(m.content) \
                    AND COALESCE(json_extract(m.content, '$.error.code'), json_extract(m.content, '$.code')) IS NOT NULL \
                  ORDER BY m.created_at DESC, m.id DESC \
                  LIMIT 1) AS latest_error_code \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? AND c.updated_at >= ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL)) \
             ORDER BY CASE WHEN c.id = ? THEN 0 ELSE 1 END, c.updated_at DESC, c.id DESC \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(window_start)
        .bind(anchor_conversation_id)
        .bind(RECENT_CONVERSATION_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| {
                let id = row.try_get::<String, _>("id")?;
                Ok(json!({
                    "id": id,
                    "is_anchor": id == anchor_conversation_id,
                    "title": row.try_get::<String, _>("title")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "source": row.try_get::<Option<String>, _>("source")?,
                    "pinned": row.try_get::<bool, _>("pinned")?,
                    "extra_bytes": row.try_get::<Option<i64>, _>("extra_bytes")?,
                    "model_provider_id": row.try_get::<Option<String>, _>("model_provider_id")?,
                    "model_id": row.try_get::<Option<String>, _>("model_id")?,
                    "extra_agent_id": row.try_get::<Option<String>, _>("extra_agent_id")?,
                    "extra_team_id": row.try_get::<Option<String>, _>("extra_team_id")?,
                    "message_count": row.try_get::<i64, _>("message_count")?,
                    "error_message_count": row.try_get::<i64, _>("error_message_count")?,
                    "latest_error_code": row.try_get::<Option<String>, _>("latest_error_code")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(json!({
            "window_ms": RECENT_CONVERSATION_WINDOW_MS,
            "limit": RECENT_CONVERSATION_LIMIT,
            "anchor_updated_at": anchor_updated_at,
            "items": items,
        }))
    }

    async fn collect_message_diagnostics(&self, conversation_id: &str) -> Result<Value, DbError> {
        let aggregate_rows = sqlx::query(
            "SELECT type, status, hidden, COUNT(*) AS count, SUM(length(content)) AS content_bytes \
             FROM messages \
             WHERE conversation_id = ? \
             GROUP BY type, status, hidden",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        let mut total_count = 0_i64;
        let mut total_content_bytes = 0_i64;
        let mut by_type = Map::new();
        let mut by_status = Map::new();
        let mut hidden = Map::new();
        for row in aggregate_rows {
            let count = row.try_get::<i64, _>("count")?;
            let content_bytes = row.try_get::<Option<i64>, _>("content_bytes")?.unwrap_or_default();
            total_count += count;
            total_content_bytes += content_bytes;
            bump_count(&mut by_type, &row.try_get::<String, _>("type")?, count);
            if let Some(status) = row.try_get::<Option<String>, _>("status")? {
                bump_count(&mut by_status, &status, count);
            }
            let hidden_key = if row.try_get::<bool, _>("hidden")? {
                "true"
            } else {
                "false"
            };
            bump_count(&mut hidden, hidden_key, count);
        }

        let recent_messages = sqlx::query(
            "SELECT \
                id, msg_id, type, position, status, hidden, created_at, length(content) AS content_bytes, \
                CASE WHEN json_valid(content) THEN length(json_extract(content, '$.text')) END AS text_length, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.attachments') END AS attachment_count, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.images') END AS image_count, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.toolCalls') END AS tool_call_count \
             FROM messages \
             WHERE conversation_id = ? \
             ORDER BY created_at DESC, id DESC \
             LIMIT 20",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        let recent_messages = recent_messages
            .into_iter()
            .map(|row| {
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "msg_id": row.try_get::<Option<String>, _>("msg_id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "hidden": row.try_get::<bool, _>("hidden")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "text_length": row.try_get::<Option<i64>, _>("text_length")?,
                    "attachment_count": row.try_get::<Option<i64>, _>("attachment_count")?.unwrap_or_default(),
                    "image_count": row.try_get::<Option<i64>, _>("image_count")?.unwrap_or_default(),
                    "tool_call_count": row.try_get::<Option<i64>, _>("tool_call_count")?.unwrap_or_default(),
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let recent_errors = sqlx::query(
            "SELECT \
                id, type, status, position, created_at, length(content) AS content_bytes, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.code'), json_extract(content, '$.code')) END AS code, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.ownership'), json_extract(content, '$.ownership')) END AS ownership, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.retryable'), json_extract(content, '$.retryable')) END AS retryable, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.resolution.kind') END AS resolution_kind, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.resolution.targetId') END AS resolution_target_id, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.feedbackRecommended') END AS feedback_recommended \
             FROM messages \
             WHERE conversation_id = ? \
               AND (status = 'error' OR type = 'tips' OR (json_valid(content) AND json_extract(content, '$.error.code') IS NOT NULL)) \
             ORDER BY created_at DESC, id DESC \
             LIMIT 10",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        let recent_errors = recent_errors
            .into_iter()
            .map(|row| {
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "code": row.try_get::<Option<String>, _>("code")?,
                    "ownership": row.try_get::<Option<String>, _>("ownership")?,
                    "retryable": sqlite_bool_value(row.try_get::<Option<i64>, _>("retryable")?),
                    "resolution_kind": row.try_get::<Option<String>, _>("resolution_kind")?,
                    "resolution_target_id": row.try_get::<Option<String>, _>("resolution_target_id")?,
                    "feedback_recommended": sqlite_bool_value(row.try_get::<Option<i64>, _>("feedback_recommended")?),
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let recent_error_detail_rows = sqlx::query(
            "SELECT \
                id, msg_id, type, status, position, hidden, created_at, length(content) AS content_bytes, content, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.code'), json_extract(content, '$.code')) END AS code, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.ownership'), json_extract(content, '$.ownership')) END AS ownership, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.retryable'), json_extract(content, '$.retryable')) END AS retryable \
             FROM messages \
             WHERE conversation_id = ? \
               AND (status = 'error' OR type = 'tips' OR (json_valid(content) AND json_extract(content, '$.error.code') IS NOT NULL)) \
             ORDER BY created_at DESC, id DESC \
             LIMIT 10",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        let recent_error_details = recent_error_detail_rows
            .into_iter()
            .map(|row| {
                let content = row.try_get::<String, _>("content")?;
                let content = json_value_or_string(&content);
                let failure_summary = message_failure_summary(&content);
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "msg_id": row.try_get::<Option<String>, _>("msg_id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "hidden": row.try_get::<bool, _>("hidden")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "content": content,
                    "code": row.try_get::<Option<String>, _>("code")?,
                    "ownership": row.try_get::<Option<String>, _>("ownership")?,
                    "retryable": sqlite_bool_value(row.try_get::<Option<i64>, _>("retryable")?),
                    "failure_summary": failure_summary,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let failure_summaries = recent_error_details
            .iter()
            .filter_map(|item| {
                item.get("failure_summary")
                    .filter(|summary| !summary.is_null())
                    .cloned()
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "total_count": total_count,
            "total_content_bytes": total_content_bytes,
            "by_type": by_type,
            "by_status": by_status,
            "hidden": hidden,
            "recent_messages": recent_messages,
            "recent_errors": recent_errors,
            "recent_error_details": recent_error_details,
            "failure_summaries": failure_summaries,
        }))
    }

    async fn collect_acp_session(&self, conversation_id: &str) -> Result<Value, DbError> {
        let row = sqlx::query(
            "SELECT \
                conversation_id, agent_source, agent_id, session_id, session_status, \
                session_config, length(session_config) AS session_config_bytes, \
                last_active_at, suspended_at, \
                CASE WHEN json_valid(session_config) THEN json_extract(session_config, '$.runtime.current_mode_id') END AS current_mode_id, \
                CASE WHEN json_valid(session_config) THEN json_extract(session_config, '$.runtime.current_model_id') END AS current_model_id \
             FROM acp_session \
             WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(Value::Null);
        };

        let session_config = row.try_get::<String, _>("session_config")?;
        let config_summary = runtime_config_selection_summary(&session_config);

        Ok(json!({
            "conversation_id": row.try_get::<String, _>("conversation_id")?,
            "agent_source": row.try_get::<String, _>("agent_source")?,
            "agent_id": row.try_get::<String, _>("agent_id")?,
            "has_session_id": row.try_get::<Option<String>, _>("session_id")?.is_some(),
            "session_status": row.try_get::<String, _>("session_status")?,
            "session_config_bytes": row.try_get::<Option<i64>, _>("session_config_bytes")?,
            "last_active_at": row.try_get::<Option<i64>, _>("last_active_at")?,
            "suspended_at": row.try_get::<Option<i64>, _>("suspended_at")?,
            "current_mode_id": row.try_get::<Option<String>, _>("current_mode_id")?,
            "current_model_id": row.try_get::<Option<String>, _>("current_model_id")?,
            "config_selection_keys": config_summary.keys,
            "config_selections": config_summary.values,
            "mode_selection": config_summary.mode_selection,
            "model_selection": config_summary.model_selection,
        }))
    }

    async fn collect_agent_metadata(&self, agent_id: Option<&str>) -> Result<Value, DbError> {
        let Some(agent_id) = agent_id else {
            return Ok(Value::Null);
        };

        let row = sqlx::query(
            "SELECT \
                id, name, backend, agent_type, agent_source, enabled, sort_order, \
                length(command) AS command_bytes, length(args) AS args_bytes, length(env) AS env_bytes, \
                available_modes, available_models, available_commands, config_options, \
                last_check_status, last_check_kind, last_check_error_code, last_check_latency_ms, \
                last_check_at, last_success_at, last_failure_at \
             FROM agent_metadata \
             WHERE id = ?",
        )
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(Value::Null);
        };

        Ok(json!({
            "id": row.try_get::<String, _>("id")?,
            "name": row.try_get::<String, _>("name")?,
            "backend": row.try_get::<Option<String>, _>("backend")?,
            "agent_type": row.try_get::<String, _>("agent_type")?,
            "agent_source": row.try_get::<String, _>("agent_source")?,
            "enabled": row.try_get::<bool, _>("enabled")?,
            "sort_order": row.try_get::<i64, _>("sort_order")?,
            "command_bytes": row.try_get::<Option<i64>, _>("command_bytes")?,
            "args_bytes": row.try_get::<Option<i64>, _>("args_bytes")?,
            "env_bytes": row.try_get::<Option<i64>, _>("env_bytes")?,
            "available_mode_count": json_array_count(row.try_get::<Option<String>, _>("available_modes")?.as_deref()),
            "available_model_count": json_array_count(row.try_get::<Option<String>, _>("available_models")?.as_deref()),
            "available_command_count": json_array_count(row.try_get::<Option<String>, _>("available_commands")?.as_deref()),
            "config_option_count": json_array_count(row.try_get::<Option<String>, _>("config_options")?.as_deref()),
            "last_check_status": row.try_get::<Option<String>, _>("last_check_status")?,
            "last_check_kind": row.try_get::<Option<String>, _>("last_check_kind")?,
            "last_check_error_code": row.try_get::<Option<String>, _>("last_check_error_code")?,
            "last_check_latency_ms": row.try_get::<Option<i64>, _>("last_check_latency_ms")?,
            "last_check_at": row.try_get::<Option<i64>, _>("last_check_at")?,
            "last_success_at": row.try_get::<Option<i64>, _>("last_success_at")?,
            "last_failure_at": row.try_get::<Option<i64>, _>("last_failure_at")?,
        }))
    }

    async fn collect_assistant_snapshot(&self, user_id: &str, conversation_id: &str) -> Result<Value, DbError> {
        let row = sqlx::query(
            "SELECT \
                cas.assistant_definition_id, cas.assistant_id, cas.assistant_source, cas.agent_id, \
                cas.default_model_mode, cas.resolved_model_id, \
                cas.default_permission_mode, cas.resolved_permission_value, \
                cas.default_thought_level_mode, cas.resolved_thought_level_value, \
                cas.default_skills_mode, cas.resolved_skill_ids, cas.resolved_disabled_builtin_skill_ids, \
                cas.default_mcps_mode, cas.resolved_mcp_ids, \
                length(cas.rules_content) AS rules_content_bytes, cas.created_at, cas.updated_at \
             FROM conversation_assistant_snapshots cas \
             JOIN conversations c ON c.id = cas.conversation_id \
             WHERE cas.conversation_id = ? AND c.user_id = ?",
        )
        .bind(conversation_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(Value::Null);
        };

        Ok(json!({
            "assistant_definition_id": row.try_get::<String, _>("assistant_definition_id")?,
            "assistant_id": row.try_get::<String, _>("assistant_id")?,
            "assistant_source": row.try_get::<String, _>("assistant_source")?,
            "agent_id": row.try_get::<String, _>("agent_id")?,
            "default_model_mode": row.try_get::<String, _>("default_model_mode")?,
            "resolved_model_id": row.try_get::<Option<String>, _>("resolved_model_id")?,
            "default_permission_mode": row.try_get::<String, _>("default_permission_mode")?,
            "resolved_permission_value": row.try_get::<Option<String>, _>("resolved_permission_value")?,
            "default_thought_level_mode": row.try_get::<String, _>("default_thought_level_mode")?,
            "resolved_thought_level_value": row.try_get::<Option<String>, _>("resolved_thought_level_value")?,
            "default_skills_mode": row.try_get::<String, _>("default_skills_mode")?,
            "resolved_skill_count": json_array_count(Some(&row.try_get::<String, _>("resolved_skill_ids")?)),
            "resolved_disabled_builtin_skill_count": json_array_count(Some(&row.try_get::<String, _>("resolved_disabled_builtin_skill_ids")?)),
            "default_mcps_mode": row.try_get::<String, _>("default_mcps_mode")?,
            "resolved_mcp_count": json_array_count(Some(&row.try_get::<String, _>("resolved_mcp_ids")?)),
            "rules_content_bytes": row.try_get::<Option<i64>, _>("rules_content_bytes")?,
            "created_at": row.try_get::<i64, _>("created_at")?,
            "updated_at": row.try_get::<i64, _>("updated_at")?,
        }))
    }

    async fn collect_model_auth(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let provider_id = self.resolve_provider_id(request).await?;
        let mut query = "SELECT id, platform, name, base_url, api_key_encrypted, models, enabled, capabilities, \
                            context_limit, model_enabled, model_health, is_full_url, created_at, updated_at \
                         FROM providers"
            .to_owned();
        if provider_id.is_some() {
            query.push_str(" WHERE id = ?");
        }
        query.push_str(" ORDER BY updated_at DESC LIMIT 20");

        let rows = if let Some(provider_id) = provider_id.as_deref() {
            sqlx::query(&query).bind(provider_id).fetch_all(&self.pool).await?
        } else {
            sqlx::query(&query).fetch_all(&self.pool).await?
        };

        let providers = rows
            .into_iter()
            .map(|row| {
                let models = row.try_get::<String, _>("models")?;
                let model_enabled = row.try_get::<Option<String>, _>("model_enabled")?;
                let model_health = row.try_get::<Option<String>, _>("model_health")?;
                let api_key_encrypted = row.try_get::<String, _>("api_key_encrypted")?;
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "platform": row.try_get::<String, _>("platform")?,
                    "name": row.try_get::<String, _>("name")?,
                    "base_url_host": base_url_host(&row.try_get::<String, _>("base_url")?),
                    "api_key_configured": !api_key_encrypted.trim().is_empty(),
                    "enabled": row.try_get::<bool, _>("enabled")?,
                    "model_count": json_array_count(Some(&models)),
                    "disabled_model_count": disabled_model_count(model_enabled.as_deref()),
                    "unhealthy_model_count": unhealthy_model_count(model_health.as_deref()),
                    "capability_count": json_array_count(Some(&row.try_get::<String, _>("capabilities")?)),
                    "context_limit": row.try_get::<Option<i64>, _>("context_limit")?,
                    "is_full_url": row.try_get::<bool, _>("is_full_url")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let mode = if provider_id.is_some() { "detail" } else { "summary" };
        Ok(profile_result(
            FeedbackDiagnosticsProfile::ModelAuth,
            mode,
            json!({ "providers": providers }),
        ))
    }

    async fn resolve_provider_id(&self, request: &FeedbackDiagnosticsRequest) -> Result<Option<String>, DbError> {
        if request.context.provider_id.is_some() {
            return Ok(request.context.provider_id.clone());
        }

        let Some(conversation_id) = request.context.conversation_id.as_deref() else {
            return Ok(None);
        };

        let provider_id: Option<String> = sqlx::query_scalar(
            "SELECT CASE WHEN json_valid(model) THEN COALESCE(json_extract(model, '$.providerId'), json_extract(model, '$.provider_id')) END \
             FROM conversations \
             WHERE id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(&request.user_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();

        Ok(provider_id)
    }

    async fn collect_agent_team(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let team_id = self.resolve_team_id(request).await?;
        let Some(team_id) = team_id else {
            return Ok(profile_result(
                FeedbackDiagnosticsProfile::AgentTeam,
                "summary",
                json!({ "teams": self.collect_team_summary(&request.user_id).await? }),
            ));
        };

        let team = sqlx::query(
            "SELECT id, name, workspace_mode, session_mode, agents, lead_agent_id, agents_version, created_at, updated_at \
             FROM teams \
             WHERE id = ? AND user_id = ?",
        )
        .bind(&team_id)
        .bind(&request.user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(team) = team else {
            return Ok(profile_result(
                FeedbackDiagnosticsProfile::AgentTeam,
                "not_found",
                json!({ "team": Value::Null }),
            ));
        };

        let task_rows = sqlx::query(
            "SELECT status, COUNT(*) AS count, MAX(updated_at) AS last_updated_at \
             FROM team_tasks \
             WHERE team_id = ? \
             GROUP BY status",
        )
        .bind(&team_id)
        .fetch_all(&self.pool)
        .await?;

        let mailbox_rows = sqlx::query(
            "SELECT type, read, COUNT(*) AS count, MAX(created_at) AS last_created_at \
             FROM mailbox \
             WHERE team_id = ? \
             GROUP BY type, read",
        )
        .bind(&team_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(profile_result(
            FeedbackDiagnosticsProfile::AgentTeam,
            "detail",
            json!({
                "team": {
                    "id": team.try_get::<String, _>("id")?,
                    "name_length": team.try_get::<String, _>("name")?.chars().count(),
                    "workspace_mode": team.try_get::<String, _>("workspace_mode")?,
                    "session_mode": team.try_get::<Option<String>, _>("session_mode")?,
                    "agent_count": json_array_count(Some(&team.try_get::<String, _>("agents")?)),
                    "lead_agent_id": team.try_get::<Option<String>, _>("lead_agent_id")?,
                    "agents_version": team.try_get::<String, _>("agents_version")?,
                    "created_at": team.try_get::<i64, _>("created_at")?,
                    "updated_at": team.try_get::<i64, _>("updated_at")?,
                },
                "tasks": rows_to_group_counts(task_rows, "status")?,
                "mailbox": mailbox_rows_to_counts(mailbox_rows)?,
            }),
        ))
    }

    async fn resolve_team_id(&self, request: &FeedbackDiagnosticsRequest) -> Result<Option<String>, DbError> {
        if request.context.team_id.is_some() {
            return Ok(request.context.team_id.clone());
        }

        let Some(conversation_id) = request.context.conversation_id.as_deref() else {
            return Ok(None);
        };

        let team_id: Option<String> = sqlx::query_scalar(
            "SELECT CASE WHEN json_valid(extra) THEN COALESCE(json_extract(extra, '$.teamId'), json_extract(extra, '$.team_id')) END \
             FROM conversations \
             WHERE id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(&request.user_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();

        Ok(team_id)
    }

    async fn collect_team_summary(&self, user_id: &str) -> Result<Value, DbError> {
        let rows = sqlx::query(
            "SELECT workspace_mode, session_mode, COUNT(*) AS count, MAX(updated_at) AS last_updated_at \
             FROM teams \
             WHERE user_id = ? \
             GROUP BY workspace_mode, session_mode",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(json!({
                    "workspace_mode": row.try_get::<String, _>("workspace_mode")?,
                    "session_mode": row.try_get::<Option<String>, _>("session_mode")?,
                    "count": row.try_get::<i64, _>("count")?,
                    "last_updated_at": row.try_get::<Option<i64>, _>("last_updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map(Value::Array)
            .map_err(DbError::from)
    }

    async fn collect_mcp_tools(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let mut query = "SELECT id, name, enabled, transport_type, tools, last_test_status, last_connected, \
                            original_json, builtin, deleted_at, created_at, updated_at, length(transport_config) AS transport_config_bytes, \
                            length(original_json) AS original_json_bytes \
                         FROM mcp_servers \
                         WHERE deleted_at IS NULL"
            .to_owned();
        if request.context.mcp_server_id.is_some() {
            query.push_str(" AND id = ?");
        }
        query.push_str(" ORDER BY updated_at DESC LIMIT 30");

        let rows = if let Some(mcp_server_id) = request.context.mcp_server_id.as_deref() {
            sqlx::query(&query).bind(mcp_server_id).fetch_all(&self.pool).await?
        } else {
            sqlx::query(&query).fetch_all(&self.pool).await?
        };

        let servers = rows
            .into_iter()
            .map(|row| {
                let original_json = row.try_get::<Option<String>, _>("original_json")?;
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "name": row.try_get::<String, _>("name")?,
                    "enabled": row.try_get::<bool, _>("enabled")?,
                    "transport_type": row.try_get::<String, _>("transport_type")?,
                    "tool_count": json_array_count(row.try_get::<Option<String>, _>("tools")?.as_deref()),
                    "last_test_status": row.try_get::<String, _>("last_test_status")?,
                    "last_connected": row.try_get::<Option<i64>, _>("last_connected")?,
                    "original_json": sanitize_mcp_original_json(original_json.as_deref()),
                    "builtin": row.try_get::<bool, _>("builtin")?,
                    "deleted": row.try_get::<Option<i64>, _>("deleted_at")?.is_some(),
                    "transport_config_bytes": row.try_get::<Option<i64>, _>("transport_config_bytes")?,
                    "original_json_bytes": row.try_get::<Option<i64>, _>("original_json_bytes")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let mode = if request.context.mcp_server_id.is_some() {
            "detail"
        } else {
            "summary"
        };
        Ok(profile_result(
            FeedbackDiagnosticsProfile::McpTools,
            mode,
            json!({ "servers": servers }),
        ))
    }

    async fn collect_client_ui_settings(&self) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let preference_rows = sqlx::query(
            "SELECT key, value, updated_at \
             FROM client_preferences \
             WHERE key LIKE 'appearance.%' \
                OR key LIKE 'window.%' \
                OR key LIKE 'display.%' \
                OR key LIKE 'workspace.%' \
                OR key LIKE 'settings.%' \
             ORDER BY updated_at DESC, key ASC \
             LIMIT 50",
        )
        .fetch_all(&self.pool)
        .await?;

        let preferences = preference_rows
            .into_iter()
            .map(|row| {
                let value = row.try_get::<String, _>("value")?;
                Ok(json!({
                    "key": row.try_get::<String, _>("key")?,
                    "value": json_value_or_string(&value),
                    "value_bytes": value.len(),
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let settings = sqlx::query(
            "SELECT language, notification_enabled, cron_notification_enabled, command_queue_enabled, save_upload_to_workspace, updated_at \
             FROM system_settings \
             WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        let system_settings = if let Some(row) = settings {
            Some(json!({
                    "language": row.try_get::<String, _>("language")?,
                    "notification_enabled": row.try_get::<bool, _>("notification_enabled")?,
                    "cron_notification_enabled": row.try_get::<bool, _>("cron_notification_enabled")?,
                    "command_queue_enabled": row.try_get::<bool, _>("command_queue_enabled")?,
                    "save_upload_to_workspace": row.try_get::<bool, _>("save_upload_to_workspace")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
            }))
        } else {
            None
        };

        Ok(profile_result(
            FeedbackDiagnosticsProfile::ClientUiSettings,
            "summary",
            json!({
                "preferences": {
                    "limit": 50,
                    "items": preferences,
                },
                "system_settings": system_settings,
            }),
        ))
    }

    async fn collect_workspace_summary(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let Some(conversation_id) = request.context.conversation_id.as_deref() else {
            return Ok(profile_result(
                FeedbackDiagnosticsProfile::WorkspaceSummary,
                "summary",
                json!({
                    "conversation": Value::Null,
                    "team": Value::Null,
                    "runtime_checks": workspace_runtime_checks_boundary(),
                }),
            ));
        };

        let conversation = sqlx::query(
            "SELECT \
                id, name AS title, extra, model, status, source, updated_at, \
                CASE WHEN json_valid(extra) THEN COALESCE(json_extract(extra, '$.teamId'), json_extract(extra, '$.team_id')) END AS extra_team_id, \
                CASE WHEN json_valid(extra) THEN COALESCE(json_extract(extra, '$.workspace'), json_extract(extra, '$.workspacePath'), json_extract(extra, '$.workspace_path')) END AS extra_workspace \
             FROM conversations \
             WHERE id = ? AND user_id = ?",
        )
        .bind(conversation_id)
        .bind(&request.user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(conversation) = conversation else {
            return Ok(profile_result(
                FeedbackDiagnosticsProfile::WorkspaceSummary,
                "not_found",
                json!({
                    "conversation": Value::Null,
                    "team": Value::Null,
                    "runtime_checks": workspace_runtime_checks_boundary(),
                }),
            ));
        };

        let team_id = request.context.team_id.clone().or_else(|| {
            conversation
                .try_get::<Option<String>, _>("extra_team_id")
                .ok()
                .flatten()
        });

        let team = if let Some(team_id) = team_id.as_deref() {
            sqlx::query(
                "SELECT id, name, workspace, workspace_mode, session_mode, lead_agent_id, updated_at \
                 FROM teams \
                 WHERE id = ? AND user_id = ?",
            )
            .bind(team_id)
            .bind(&request.user_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| {
                Ok::<Value, sqlx::Error>(json!({
                    "team_id": row.try_get::<String, _>("id")?,
                    "name": row.try_get::<String, _>("name")?,
                    "workspace": row.try_get::<String, _>("workspace")?,
                    "workspace_mode": row.try_get::<String, _>("workspace_mode")?,
                    "session_mode": row.try_get::<Option<String>, _>("session_mode")?,
                    "lead_agent_id": row.try_get::<Option<String>, _>("lead_agent_id")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .transpose()?
        } else {
            None
        };

        Ok(profile_result(
            FeedbackDiagnosticsProfile::WorkspaceSummary,
            "detail",
            json!({
                "conversation": {
                    "id": conversation.try_get::<String, _>("id")?,
                    "title": conversation.try_get::<String, _>("title")?,
                    "status": conversation.try_get::<Option<String>, _>("status")?,
                    "source": conversation.try_get::<Option<String>, _>("source")?,
                    "updated_at": conversation.try_get::<i64, _>("updated_at")?,
                    "extra_team_id": conversation.try_get::<Option<String>, _>("extra_team_id")?,
                    "extra_workspace": conversation.try_get::<Option<String>, _>("extra_workspace")?,
                },
                "team": team,
                "runtime_checks": workspace_runtime_checks_boundary(),
            }),
        ))
    }

    async fn collect_global_summary(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsProfileResult, DbError> {
        let conversation_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL))",
        )
        .bind(&request.user_id)
        .fetch_one(&self.pool)
        .await?;
        let message_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM messages m \
             JOIN conversations c ON c.id = m.conversation_id \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL))",
        )
        .bind(&request.user_id)
        .fetch_one(&self.pool)
        .await?;
        let provider_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers")
            .fetch_one(&self.pool)
            .await?;
        let agent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_metadata")
            .fetch_one(&self.pool)
            .await?;
        let active_mcp_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE deleted_at IS NULL")
            .fetch_one(&self.pool)
            .await?;

        Ok(profile_result(
            FeedbackDiagnosticsProfile::GlobalSummary,
            "summary",
            json!({
                "conversation_count": conversation_count,
                "message_count": message_count,
                "provider_count": provider_count,
                "agent_count": agent_count,
                "active_mcp_count": active_mcp_count,
                "conversation_status_counts": self.collect_global_conversation_status_counts(&request.user_id).await?,
                "recent_conversations": self.collect_global_recent_conversations(&request.user_id).await?,
                "recent_errors": self.collect_global_recent_errors(&request.user_id).await?,
                "agent_health": self.collect_global_agent_health().await?,
                "provider_health": self.collect_global_provider_health().await?,
            }),
        ))
    }

    async fn collect_global_conversation_status_counts(&self, user_id: &str) -> Result<Value, DbError> {
        let rows = sqlx::query(
            "SELECT c.type, c.status, COUNT(*) AS count, MAX(c.updated_at) AS last_updated_at \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL)) \
             GROUP BY c.type, c.status \
             ORDER BY last_updated_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(json!({
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "count": row.try_get::<i64, _>("count")?,
                    "last_updated_at": row.try_get::<Option<i64>, _>("last_updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map(Value::Array)
            .map_err(DbError::from)
    }

    async fn collect_global_recent_conversations(&self, user_id: &str) -> Result<Value, DbError> {
        struct RecentTeamGroup {
            team_id: String,
            name: Option<String>,
            name_length: Option<usize>,
            workspace_mode: Option<String>,
            session_mode: Option<String>,
            lead_agent_id: Option<String>,
            agent_count: usize,
            agents_version: Option<String>,
            created_at: Option<i64>,
            updated_at: Option<i64>,
            latest_updated_at: i64,
            message_count: i64,
            error_message_count: i64,
            conversations: Vec<Value>,
        }

        let rows = sqlx::query(
            "SELECT \
                c.id, c.name AS title, c.type, c.status, c.source, c.pinned, c.created_at, c.updated_at, \
                length(c.extra) AS extra_bytes, \
                CASE WHEN json_valid(c.model) OR json_valid(c.extra) THEN COALESCE( \
                    CASE WHEN json_valid(c.model) THEN json_extract(c.model, '$.providerId') END, \
                    CASE WHEN json_valid(c.model) THEN json_extract(c.model, '$.provider_id') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.providerId') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.provider_id') END \
                ) END AS model_provider_id, \
                CASE WHEN json_valid(c.model) OR json_valid(c.extra) THEN COALESCE( \
                    CASE WHEN json_valid(c.model) THEN json_extract(c.model, '$.modelId') END, \
                    CASE WHEN json_valid(c.model) THEN json_extract(c.model, '$.model_id') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.currentModelId') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.current_model_id') END \
                ) END AS model_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.agentId'), json_extract(c.extra, '$.agent_id')) END AS extra_agent_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END AS extra_team_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.assistantId'), json_extract(c.extra, '$.assistant_id')) END AS extra_assistant_id, \
                CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.role') END AS role, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.slotId'), json_extract(c.extra, '$.slot_id')) END AS slot_id, \
                CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.sessionMode'), json_extract(c.extra, '$.session_mode')) END AS session_mode, \
                cas.assistant_definition_id, cas.assistant_id AS snapshot_assistant_id, cas.assistant_source, \
                cas.agent_id AS snapshot_agent_id, cas.default_model_mode, cas.resolved_model_id, \
                cas.default_permission_mode, cas.resolved_permission_value, \
                cas.default_thought_level_mode, cas.resolved_thought_level_value, \
                s.agent_source AS session_agent_source, s.agent_id AS session_agent_id, s.session_status, \
                CASE WHEN json_valid(s.session_config) OR json_valid(c.extra) THEN COALESCE( \
                    CASE WHEN json_valid(s.session_config) THEN json_extract(s.session_config, '$.runtime.current_mode_id') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.currentModeId') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.current_mode_id') END \
                ) END AS current_mode_id, \
                CASE WHEN json_valid(s.session_config) OR json_valid(c.extra) THEN COALESCE( \
                    CASE WHEN json_valid(s.session_config) THEN json_extract(s.session_config, '$.runtime.current_model_id') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.currentModelId') END, \
                    CASE WHEN json_valid(c.extra) THEN json_extract(c.extra, '$.current_model_id') END \
                ) END AS current_model_id, \
                (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS message_count, \
                (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id AND (m.status = 'error' OR m.type = 'tips' OR (json_valid(m.content) AND json_extract(m.content, '$.error.code') IS NOT NULL))) AS error_message_count, \
                (SELECT CASE WHEN json_valid(m.content) THEN COALESCE(json_extract(m.content, '$.error.code'), json_extract(m.content, '$.code')) END \
                  FROM messages m \
                  WHERE m.conversation_id = c.id \
                    AND (m.status = 'error' OR m.type = 'tips' OR (json_valid(m.content) AND json_extract(m.content, '$.error.code') IS NOT NULL)) \
                    AND json_valid(m.content) \
                    AND COALESCE(json_extract(m.content, '$.error.code'), json_extract(m.content, '$.code')) IS NOT NULL \
                  ORDER BY m.created_at DESC, m.id DESC \
                  LIMIT 1) AS latest_error_code, \
                t.id AS team_row_id, t.name AS team_name, t.workspace_mode AS team_workspace_mode, \
                t.session_mode AS team_session_mode, t.agents AS team_agents, \
                t.lead_agent_id AS team_lead_agent_id, t.agents_version AS team_agents_version, \
                t.created_at AS team_created_at, t.updated_at AS team_updated_at \
             FROM conversations c \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN acp_session s ON s.conversation_id = c.id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL)) \
             ORDER BY c.updated_at DESC, c.id DESC \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(GLOBAL_RECENT_CONVERSATION_SCAN_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let mut direct_items = Vec::new();
        let mut team_groups = Vec::<RecentTeamGroup>::new();
        for row in rows {
            let id = row.try_get::<String, _>("id")?;
            let team_id = row.try_get::<Option<String>, _>("extra_team_id")?;
            let conversation_message_count = row.try_get::<i64, _>("message_count")?;
            let conversation_error_message_count = row.try_get::<i64, _>("error_message_count")?;
            let conversation_updated_at = row.try_get::<i64, _>("updated_at")?;
            let messages = self.collect_global_conversation_message_samples(&id).await?;
            let item = json!({
                "id": id.clone(),
                "conversation_id": id,
                "title": row.try_get::<String, _>("title")?,
                "type": row.try_get::<String, _>("type")?,
                "status": row.try_get::<Option<String>, _>("status")?,
                "source": row.try_get::<Option<String>, _>("source")?,
                "pinned": row.try_get::<bool, _>("pinned")?,
                "extra_bytes": row.try_get::<Option<i64>, _>("extra_bytes")?,
                "provider_id": row.try_get::<Option<String>, _>("model_provider_id")?,
                "model_provider_id": row.try_get::<Option<String>, _>("model_provider_id")?,
                "model_id": row.try_get::<Option<String>, _>("model_id")?,
                "assistant_definition_id": row.try_get::<Option<String>, _>("assistant_definition_id")?,
                "assistant_id": row.try_get::<Option<String>, _>("snapshot_assistant_id")?
                    .or(row.try_get::<Option<String>, _>("extra_assistant_id")?),
                "assistant_source": row.try_get::<Option<String>, _>("assistant_source")?,
                "agent_id": row.try_get::<Option<String>, _>("snapshot_agent_id")?
                    .or(row.try_get::<Option<String>, _>("session_agent_id")?)
                    .or(row.try_get::<Option<String>, _>("extra_agent_id")?),
                "extra_agent_id": row.try_get::<Option<String>, _>("extra_agent_id")?,
                "team_id": team_id.clone(),
                "extra_team_id": team_id.clone(),
                "role": row.try_get::<Option<String>, _>("role")?,
                "slot_id": row.try_get::<Option<String>, _>("slot_id")?,
                "session_mode": row.try_get::<Option<String>, _>("session_mode")?,
                "session_agent_source": row.try_get::<Option<String>, _>("session_agent_source")?,
                "session_status": row.try_get::<Option<String>, _>("session_status")?,
                "current_mode_id": row.try_get::<Option<String>, _>("current_mode_id")?,
                "current_model_id": row.try_get::<Option<String>, _>("current_model_id")?,
                "default_model_mode": row.try_get::<Option<String>, _>("default_model_mode")?,
                "resolved_model_id": row.try_get::<Option<String>, _>("resolved_model_id")?,
                "default_permission_mode": row.try_get::<Option<String>, _>("default_permission_mode")?,
                "resolved_permission_value": row.try_get::<Option<String>, _>("resolved_permission_value")?,
                "default_thought_level_mode": row.try_get::<Option<String>, _>("default_thought_level_mode")?,
                "resolved_thought_level_value": row.try_get::<Option<String>, _>("resolved_thought_level_value")?,
                "message_count": conversation_message_count,
                "error_message_count": conversation_error_message_count,
                "latest_error_code": row.try_get::<Option<String>, _>("latest_error_code")?,
                "created_at": row.try_get::<i64, _>("created_at")?,
                "updated_at": conversation_updated_at,
                "recent_errors": messages.get("recent_errors").cloned().unwrap_or_else(|| json!([])),
                "recent_messages": messages.get("recent_messages").cloned().unwrap_or_else(|| json!([])),
            });

            if let Some(team_id) = team_id {
                let group_index = team_groups.iter().position(|group| group.team_id == team_id);
                let Some(group_index) = group_index.or_else(|| {
                    if team_groups.len() >= GLOBAL_RECENT_CONVERSATION_LIMIT as usize {
                        return None;
                    }

                    let team_name = row.try_get::<Option<String>, _>("team_name").ok().flatten();
                    let team_agents = row.try_get::<Option<String>, _>("team_agents").ok().flatten();
                    team_groups.push(RecentTeamGroup {
                        team_id: team_id.clone(),
                        name_length: team_name.as_ref().map(|name| name.chars().count()),
                        name: team_name,
                        workspace_mode: row.try_get::<Option<String>, _>("team_workspace_mode").ok().flatten(),
                        session_mode: row.try_get::<Option<String>, _>("team_session_mode").ok().flatten(),
                        lead_agent_id: row.try_get::<Option<String>, _>("team_lead_agent_id").ok().flatten(),
                        agent_count: json_array_count(team_agents.as_deref()),
                        agents_version: row.try_get::<Option<String>, _>("team_agents_version").ok().flatten(),
                        created_at: row.try_get::<Option<i64>, _>("team_created_at").ok().flatten(),
                        updated_at: row.try_get::<Option<i64>, _>("team_updated_at").ok().flatten(),
                        latest_updated_at: conversation_updated_at,
                        message_count: 0,
                        error_message_count: 0,
                        conversations: Vec::new(),
                    });
                    Some(team_groups.len() - 1)
                }) else {
                    continue;
                };

                let group = &mut team_groups[group_index];
                group.latest_updated_at = group.latest_updated_at.max(conversation_updated_at);
                if group.conversations.len() < GLOBAL_RECENT_CONVERSATION_LIMIT as usize {
                    group.message_count += conversation_message_count;
                    group.error_message_count += conversation_error_message_count;
                    group.conversations.push(item);
                }
            } else if direct_items.len() < GLOBAL_RECENT_CONVERSATION_LIMIT as usize {
                direct_items.push(item);
            }
        }

        let team_items = team_groups
            .into_iter()
            .map(|group| {
                let team_id = group.team_id;
                json!({
                    "id": team_id.clone(),
                    "team_id": team_id,
                    "name": group.name,
                    "name_length": group.name_length,
                    "workspace_mode": group.workspace_mode,
                    "session_mode": group.session_mode,
                    "lead_agent_id": group.lead_agent_id,
                    "agent_count": group.agent_count,
                    "agents_version": group.agents_version,
                    "created_at": group.created_at,
                    "updated_at": group.updated_at,
                    "latest_updated_at": group.latest_updated_at,
                    "conversation_count": group.conversations.len(),
                    "message_count": group.message_count,
                    "error_message_count": group.error_message_count,
                    "conversations": {
                        "limit": GLOBAL_RECENT_CONVERSATION_LIMIT,
                        "items": group.conversations,
                    },
                })
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "direct": {
                "limit": GLOBAL_RECENT_CONVERSATION_LIMIT,
                "items": direct_items,
            },
            "team": {
                "limit": GLOBAL_RECENT_CONVERSATION_LIMIT,
                "items": team_items,
            },
        }))
    }

    async fn collect_global_conversation_message_samples(&self, conversation_id: &str) -> Result<Value, DbError> {
        let error_rows = sqlx::query(
            "SELECT \
                id, msg_id, type, status, position, hidden, created_at, length(content) AS content_bytes, content, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.code'), json_extract(content, '$.code')) END AS code, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.ownership'), json_extract(content, '$.ownership')) END AS ownership, \
                CASE WHEN json_valid(content) THEN COALESCE(json_extract(content, '$.error.retryable'), json_extract(content, '$.retryable')) END AS retryable, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.resolution.kind') END AS resolution_kind, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.resolution.targetId') END AS resolution_target_id, \
                CASE WHEN json_valid(content) THEN json_extract(content, '$.feedbackRecommended') END AS feedback_recommended \
             FROM messages \
             WHERE conversation_id = ? \
               AND (status = 'error' OR type = 'tips' OR (json_valid(content) AND json_extract(content, '$.error.code') IS NOT NULL)) \
             ORDER BY created_at DESC, id DESC \
             LIMIT ?",
        )
        .bind(conversation_id)
        .bind(GLOBAL_CONVERSATION_ERROR_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let recent_errors = error_rows
            .into_iter()
            .map(|row| {
                let content = row.try_get::<String, _>("content")?;
                let content = json_value_or_string(&content);
                let failure_summary = message_failure_summary(&content);
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "msg_id": row.try_get::<Option<String>, _>("msg_id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "hidden": row.try_get::<bool, _>("hidden")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "content": content,
                    "code": row.try_get::<Option<String>, _>("code")?,
                    "ownership": row.try_get::<Option<String>, _>("ownership")?,
                    "retryable": sqlite_bool_value(row.try_get::<Option<i64>, _>("retryable")?),
                    "resolution_kind": row.try_get::<Option<String>, _>("resolution_kind")?,
                    "resolution_target_id": row.try_get::<Option<String>, _>("resolution_target_id")?,
                    "feedback_recommended": sqlite_bool_value(row.try_get::<Option<i64>, _>("feedback_recommended")?),
                    "failure_summary": failure_summary,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        let message_rows = sqlx::query(
            "SELECT \
                id, msg_id, type, status, position, hidden, created_at, length(content) AS content_bytes, content, \
                CASE WHEN json_valid(content) THEN length(json_extract(content, '$.text')) END AS text_length, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.attachments') END AS attachment_count, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.images') END AS image_count, \
                CASE WHEN json_valid(content) THEN json_array_length(content, '$.toolCalls') END AS tool_call_count \
             FROM messages \
             WHERE conversation_id = ? \
               AND NOT (status = 'error' OR type = 'tips' OR (json_valid(content) AND json_extract(content, '$.error.code') IS NOT NULL)) \
             ORDER BY created_at DESC, id DESC \
             LIMIT ?",
        )
        .bind(conversation_id)
        .bind(GLOBAL_CONVERSATION_MESSAGE_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let recent_messages = message_rows
            .into_iter()
            .map(|row| {
                let content = row.try_get::<String, _>("content")?;
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "msg_id": row.try_get::<Option<String>, _>("msg_id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "hidden": row.try_get::<bool, _>("hidden")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "text_length": row.try_get::<Option<i64>, _>("text_length")?,
                    "attachment_count": row.try_get::<Option<i64>, _>("attachment_count")?.unwrap_or_default(),
                    "image_count": row.try_get::<Option<i64>, _>("image_count")?.unwrap_or_default(),
                    "tool_call_count": row.try_get::<Option<i64>, _>("tool_call_count")?.unwrap_or_default(),
                    "content": sanitized_message_content(&content),
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(json!({
            "recent_errors": recent_errors,
            "recent_messages": recent_messages,
        }))
    }

    async fn collect_global_recent_errors(&self, user_id: &str) -> Result<Value, DbError> {
        let rows = sqlx::query(
            "SELECT \
                c.id AS conversation_id, c.name AS conversation_title, c.type AS conversation_type, \
                c.status AS conversation_status, c.updated_at AS conversation_updated_at, \
                m.id, m.msg_id, m.type, m.status, m.position, m.created_at, length(m.content) AS content_bytes, m.content, \
                CASE WHEN json_valid(m.content) THEN COALESCE(json_extract(m.content, '$.error.code'), json_extract(m.content, '$.code')) END AS code, \
                CASE WHEN json_valid(m.content) THEN COALESCE(json_extract(m.content, '$.error.ownership'), json_extract(m.content, '$.ownership')) END AS ownership, \
                CASE WHEN json_valid(m.content) THEN COALESCE(json_extract(m.content, '$.error.retryable'), json_extract(m.content, '$.retryable')) END AS retryable, \
                CASE WHEN json_valid(m.content) THEN json_extract(m.content, '$.resolution.kind') END AS resolution_kind, \
                CASE WHEN json_valid(m.content) THEN json_extract(m.content, '$.resolution.targetId') END AS resolution_target_id, \
                CASE WHEN json_valid(m.content) THEN json_extract(m.content, '$.feedbackRecommended') END AS feedback_recommended \
             FROM messages m \
             JOIN conversations c ON c.id = m.conversation_id \
             LEFT JOIN conversation_assistant_snapshots cas ON cas.conversation_id = c.id \
             LEFT JOIN assistant_definitions ad ON ad.id = cas.assistant_definition_id \
             LEFT JOIN teams t ON t.id = CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END \
                AND t.user_id = c.user_id \
             WHERE c.user_id = ? \
               AND (CASE WHEN json_valid(c.extra) THEN COALESCE(json_extract(c.extra, '$.teamId'), json_extract(c.extra, '$.team_id')) END IS NULL OR t.id IS NOT NULL) \
               AND (cas.conversation_id IS NULL OR (ad.id IS NOT NULL AND ad.deleted_at IS NULL)) \
               AND (m.status = 'error' OR m.type = 'tips' OR (json_valid(m.content) AND json_extract(m.content, '$.error.code') IS NOT NULL)) \
             ORDER BY m.created_at DESC, m.id DESC \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(GLOBAL_RECENT_ERROR_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| {
                let content = row.try_get::<String, _>("content")?;
                let content = json_value_or_string(&content);
                let failure_summary = message_failure_summary(&content);
                Ok(json!({
                    "conversation_id": row.try_get::<String, _>("conversation_id")?,
                    "conversation_title": row.try_get::<String, _>("conversation_title")?,
                    "conversation_type": row.try_get::<String, _>("conversation_type")?,
                    "conversation_status": row.try_get::<Option<String>, _>("conversation_status")?,
                    "conversation_updated_at": row.try_get::<i64, _>("conversation_updated_at")?,
                    "id": row.try_get::<String, _>("id")?,
                    "msg_id": row.try_get::<Option<String>, _>("msg_id")?,
                    "type": row.try_get::<String, _>("type")?,
                    "status": row.try_get::<Option<String>, _>("status")?,
                    "position": row.try_get::<Option<String>, _>("position")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "content_bytes": row.try_get::<Option<i64>, _>("content_bytes")?,
                    "content": content,
                    "code": row.try_get::<Option<String>, _>("code")?,
                    "ownership": row.try_get::<Option<String>, _>("ownership")?,
                    "retryable": sqlite_bool_value(row.try_get::<Option<i64>, _>("retryable")?),
                    "resolution_kind": row.try_get::<Option<String>, _>("resolution_kind")?,
                    "resolution_target_id": row.try_get::<Option<String>, _>("resolution_target_id")?,
                    "feedback_recommended": sqlite_bool_value(row.try_get::<Option<i64>, _>("feedback_recommended")?),
                    "failure_summary": failure_summary,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(json!({
            "limit": GLOBAL_RECENT_ERROR_LIMIT,
            "items": items,
        }))
    }

    async fn collect_global_agent_health(&self) -> Result<Value, DbError> {
        let rows = sqlx::query(
            "SELECT \
                id, name, backend, agent_type, agent_source, enabled, sort_order, \
                length(command) AS command_bytes, length(args) AS args_bytes, length(env) AS env_bytes, \
                last_check_status, last_check_kind, last_check_error_code, last_check_latency_ms, \
                last_check_at, last_success_at, last_failure_at, updated_at \
             FROM agent_metadata \
             ORDER BY updated_at DESC, id DESC \
             LIMIT ?",
        )
        .bind(GLOBAL_HEALTH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| {
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "name": row.try_get::<String, _>("name")?,
                    "backend": row.try_get::<Option<String>, _>("backend")?,
                    "agent_type": row.try_get::<String, _>("agent_type")?,
                    "agent_source": row.try_get::<String, _>("agent_source")?,
                    "enabled": row.try_get::<bool, _>("enabled")?,
                    "sort_order": row.try_get::<i64, _>("sort_order")?,
                    "command_bytes": row.try_get::<Option<i64>, _>("command_bytes")?,
                    "args_bytes": row.try_get::<Option<i64>, _>("args_bytes")?,
                    "env_bytes": row.try_get::<Option<i64>, _>("env_bytes")?,
                    "last_check_status": row.try_get::<Option<String>, _>("last_check_status")?,
                    "last_check_kind": row.try_get::<Option<String>, _>("last_check_kind")?,
                    "last_check_error_code": row.try_get::<Option<String>, _>("last_check_error_code")?,
                    "last_check_latency_ms": row.try_get::<Option<i64>, _>("last_check_latency_ms")?,
                    "last_check_at": row.try_get::<Option<i64>, _>("last_check_at")?,
                    "last_success_at": row.try_get::<Option<i64>, _>("last_success_at")?,
                    "last_failure_at": row.try_get::<Option<i64>, _>("last_failure_at")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(json!({
            "limit": GLOBAL_HEALTH_LIMIT,
            "items": items,
        }))
    }

    async fn collect_global_provider_health(&self) -> Result<Value, DbError> {
        let rows = sqlx::query(
            "SELECT id, platform, name, base_url, api_key_encrypted, models, enabled, capabilities, \
                    context_limit, model_enabled, model_health, is_full_url, created_at, updated_at \
             FROM providers \
             ORDER BY updated_at DESC, id DESC \
             LIMIT ?",
        )
        .bind(GLOBAL_HEALTH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|row| {
                let models = row.try_get::<String, _>("models")?;
                let model_enabled = row.try_get::<Option<String>, _>("model_enabled")?;
                let model_health = row.try_get::<Option<String>, _>("model_health")?;
                let api_key_encrypted = row.try_get::<String, _>("api_key_encrypted")?;
                Ok(json!({
                    "id": row.try_get::<String, _>("id")?,
                    "platform": row.try_get::<String, _>("platform")?,
                    "name": row.try_get::<String, _>("name")?,
                    "base_url_host": base_url_host(&row.try_get::<String, _>("base_url")?),
                    "api_key_configured": !api_key_encrypted.trim().is_empty(),
                    "enabled": row.try_get::<bool, _>("enabled")?,
                    "model_count": json_array_count(Some(&models)),
                    "disabled_model_count": disabled_model_count(model_enabled.as_deref()),
                    "unhealthy_model_count": unhealthy_model_count(model_health.as_deref()),
                    "capability_count": json_array_count(Some(&row.try_get::<String, _>("capabilities")?)),
                    "context_limit": row.try_get::<Option<i64>, _>("context_limit")?,
                    "is_full_url": row.try_get::<bool, _>("is_full_url")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                    "updated_at": row.try_get::<i64, _>("updated_at")?,
                }))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(json!({
            "limit": GLOBAL_HEALTH_LIMIT,
            "items": items,
        }))
    }
}

#[async_trait::async_trait]
impl IFeedbackDiagnosticsRepository for SqliteFeedbackDiagnosticsRepository {
    async fn collect_feedback_diagnostics(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsResult, DbError> {
        let mut profiles = Vec::new();
        let mut seen = BTreeSet::new();

        for profile in &request.profiles {
            if !seen.insert(profile.clone()) {
                continue;
            }
            let result = match profile {
                FeedbackDiagnosticsProfile::ConversationSession => self.collect_conversation_session(request).await?,
                FeedbackDiagnosticsProfile::ModelAuth => self.collect_model_auth(request).await?,
                FeedbackDiagnosticsProfile::AgentTeam => self.collect_agent_team(request).await?,
                FeedbackDiagnosticsProfile::McpTools => self.collect_mcp_tools(request).await?,
                FeedbackDiagnosticsProfile::ClientUiSettings => self.collect_client_ui_settings().await?,
                FeedbackDiagnosticsProfile::WorkspaceSummary => self.collect_workspace_summary(request).await?,
                FeedbackDiagnosticsProfile::GlobalSummary => self.collect_global_summary(request).await?,
            };
            profiles.push(result);
        }

        Ok(FeedbackDiagnosticsResult {
            schema_version: "feedback-diagnostics/v1".to_owned(),
            profiles,
        })
    }
}

fn profile_result(profile: FeedbackDiagnosticsProfile, mode: &str, data: Value) -> FeedbackDiagnosticsProfileResult {
    FeedbackDiagnosticsProfileResult {
        name: profile.as_name().to_owned(),
        mode: mode.to_owned(),
        data,
        warnings: Vec::new(),
    }
}

fn bump_count(map: &mut Map<String, Value>, key: &str, count: i64) {
    let current = map.get(key).and_then(Value::as_i64).unwrap_or_default();
    map.insert(key.to_owned(), Value::from(current + count));
}

fn sqlite_bool_value(value: Option<i64>) -> Value {
    value.map_or(Value::Null, |v| Value::Bool(v != 0))
}

fn json_value_or_string(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn message_failure_summary(content: &Value) -> Option<Value> {
    let tool_name = content
        .pointer("/_meta/claudeCode/toolName")
        .and_then(Value::as_str)
        .or_else(|| content.pointer("/raw_input/tool").and_then(Value::as_str))
        .or_else(|| content.pointer("/update/title").and_then(Value::as_str));
    let exit_code = content
        .pointer("/_meta/terminal_exit/exit_code")
        .and_then(Value::as_i64)
        .or_else(|| extract_exit_code_from_text(content));
    let error_code = content
        .pointer("/error/code")
        .and_then(Value::as_str)
        .or_else(|| content.pointer("/code").and_then(Value::as_str));

    if tool_name.is_none() && exit_code.is_none() && error_code.is_none() {
        return None;
    }

    Some(json!({
        "kind": if tool_name.is_some() || exit_code.is_some() { "tool" } else { "error" },
        "tool_name": tool_name,
        "exit_code": exit_code,
        "error_code": error_code,
        "stderr_class": classify_error_text(content),
        "terminal_id": content.pointer("/_meta/terminal_exit/terminal_id").and_then(Value::as_str),
        "cwd_present": content.pointer("/_meta/terminal_info/cwd").is_some(),
    }))
}

fn extract_exit_code_from_text(value: &Value) -> Option<i64> {
    let text = value.to_string().to_ascii_lowercase();
    let marker = "exit code ";
    let start = text.find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

fn classify_error_text(value: &Value) -> Option<&'static str> {
    let text = value.to_string().to_ascii_lowercase();
    if text.contains("command not found") || text.contains("not recognized") {
        Some("command_not_found")
    } else if text.contains("permission denied") || text.contains("access is denied") {
        Some("permission_denied")
    } else if text.contains("rate limit") || text.contains("too many requests") {
        Some("rate_limited")
    } else if text.contains("connection error") || text.contains("network") {
        Some("network")
    } else {
        None
    }
}

fn workspace_runtime_checks_boundary() -> Value {
    json!({
        "path_exists": Value::Null,
        "os_errno": Value::Null,
        "watcher_state": Value::Null,
        "file_lock_owner": Value::Null,
        "source": "not-db-backed",
    })
}

fn sanitized_message_content(raw: &str) -> Value {
    sanitize_message_value(&json_value_or_string(raw), None)
}

fn sanitize_message_value(value: &Value, key: Option<&str>) -> Value {
    if key.is_some_and(|key| is_sensitive_key(key) || is_message_content_key(key)) {
        return redacted_value_summary(value);
    }

    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), sanitize_message_value(value, Some(key))))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|item| sanitize_message_value(item, key)).collect()),
        Value::String(_) => redacted_value_summary(value),
        _ => value.clone(),
    }
}

fn redacted_value_summary(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({
            "redacted": true,
            "chars": text.chars().count(),
        }),
        Value::Array(items) => json!({
            "redacted": true,
            "items": items.len(),
        }),
        Value::Object(object) => json!({
            "redacted": true,
            "keys": object.len(),
        }),
        _ => json!({ "redacted": true }),
    }
}

fn is_message_content_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "text" | "content" | "message" | "prompt" | "input" | "output" | "raw"
    )
}

fn json_array_count(raw: Option<&str>) -> usize {
    raw.and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| value.as_array().map(Vec::len))
        .unwrap_or_default()
}

struct RuntimeConfigSelectionSummary {
    keys: Vec<String>,
    values: Map<String, Value>,
    mode_selection: Option<String>,
    model_selection: Option<String>,
}

fn runtime_config_selection_summary(raw: &str) -> RuntimeConfigSelectionSummary {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return RuntimeConfigSelectionSummary {
            keys: Vec::new(),
            values: Map::new(),
            mode_selection: None,
            model_selection: None,
        };
    };
    let Some(selections) = value
        .get("runtime")
        .and_then(|runtime| runtime.get("config_selections"))
        .and_then(Value::as_object)
    else {
        return RuntimeConfigSelectionSummary {
            keys: Vec::new(),
            values: Map::new(),
            mode_selection: None,
            model_selection: None,
        };
    };

    let mut keys = Vec::new();
    let mut values = Map::new();
    let mut mode_selection = None;
    let mut model_selection = None;

    for (key, value) in selections {
        if is_sensitive_key(key) {
            continue;
        }
        keys.push(key.clone());
        values.insert(key.clone(), safe_config_value(value));
        if key == "mode" {
            mode_selection = value.as_str().map(ToOwned::to_owned);
        } else if key == "model" {
            model_selection = value.as_str().map(ToOwned::to_owned);
        }
    }

    keys.sort();
    RuntimeConfigSelectionSummary {
        keys,
        values,
        mode_selection,
        model_selection,
    }
}

fn safe_config_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sanitized = object
                .iter()
                .filter(|(key, _)| !is_sensitive_key(key))
                .map(|(key, value)| (key.clone(), safe_config_value(value)))
                .collect();
            Value::Object(sanitized)
        }
        Value::Array(items) => Value::Array(items.iter().map(safe_config_value).collect()),
        _ => value.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let normalized = key.replace(['_', '-'], "");
    normalized.contains("apikey")
        || normalized.contains("accesskey")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("prompt")
}

fn base_url_host(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let without_userinfo = without_scheme.rsplit_once('@').map_or(without_scheme, |(_, rest)| rest);
    let authority = without_userinfo
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();

    if authority.starts_with('[') {
        return authority
            .split(']')
            .next()
            .map(|host| format!("{host}]"))
            .unwrap_or_default();
    }

    authority.split(':').next().unwrap_or_default().to_owned()
}

fn disabled_model_count(raw: Option<&str>) -> usize {
    raw.and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|object| object.values().filter(|value| value.as_bool() == Some(false)).count())
        .unwrap_or_default()
}

fn unhealthy_model_count(raw: Option<&str>) -> usize {
    raw.and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .values()
                .filter(|value| {
                    if let Some(ok) = value.get("ok").and_then(Value::as_bool) {
                        return !ok;
                    }
                    value
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| !matches!(status, "ok" | "healthy" | "available"))
                })
                .count()
        })
        .unwrap_or_default()
}

fn rows_to_group_counts(rows: Vec<sqlx::sqlite::SqliteRow>, key_column: &str) -> Result<Value, sqlx::Error> {
    let mut result = BTreeMap::new();
    for row in rows {
        let key = row.try_get::<String, _>(key_column)?;
        result.insert(
            key,
            json!({
                "count": row.try_get::<i64, _>("count")?,
                "last_updated_at": row.try_get::<Option<i64>, _>("last_updated_at")?,
            }),
        );
    }
    Ok(json!(result))
}

fn mailbox_rows_to_counts(rows: Vec<sqlx::sqlite::SqliteRow>) -> Result<Value, sqlx::Error> {
    let mut by_type = BTreeMap::new();
    let mut unread_count = 0_i64;
    for row in rows {
        let count = row.try_get::<i64, _>("count")?;
        let read = row.try_get::<bool, _>("read")?;
        if !read {
            unread_count += count;
        }
        by_type.insert(
            format!(
                "{}:{}",
                row.try_get::<String, _>("type")?,
                if read { "read" } else { "unread" }
            ),
            json!({
                "count": count,
                "last_created_at": row.try_get::<Option<i64>, _>("last_created_at")?,
            }),
        );
    }
    Ok(json!({
        "unread_count": unread_count,
        "by_type": by_type,
    }))
}
