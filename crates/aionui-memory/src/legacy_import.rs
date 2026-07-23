use std::sync::Arc;

use aionui_api_types::MemorySummary;
use aionui_db::{
    IConversationRepository, IMemoryRepository, ImportLegacyMemoryPageRow, LegacyConversationCursor,
    LegacyConversationImportBoundary, LegacyMemorySummaryRow,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{MemoryError, retrieval::RetrievalTarget, validation::sanitize_summary};

const LEGACY_IMPORT_PAGE_SIZE: u32 = 32;
const LEGACY_IMPORT_CURSOR_VERSION: u8 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyImportCursor {
    version: u8,
    boundary: LegacyConversationImportBoundary,
    after: Option<LegacyConversationCursor>,
}

impl LegacyImportCursor {
    fn parse(value: &str) -> Option<Self> {
        serde_json::from_str::<Self>(value)
            .ok()
            .filter(|cursor| cursor.version == LEGACY_IMPORT_CURSOR_VERSION)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySnapshot {
    goal: String,
    current_state: Vec<String>,
    decisions: Vec<String>,
    artifacts: Vec<String>,
    user_preferences: Vec<String>,
    open_questions: Vec<String>,
    next_steps: Vec<String>,
    do_not_forget: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyContextHandoff {
    snapshot: LegacySnapshot,
    #[serde(default)]
    last_compacted_turn_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyExtra {
    context_handoff: LegacyContextHandoff,
}

struct LegacySummary {
    summary: MemorySummary,
    through_turn_id: Option<String>,
}

fn legacy_summary(extra: &str) -> Option<LegacySummary> {
    let handoff = serde_json::from_str::<LegacyExtra>(extra).ok()?.context_handoff;
    let _ = handoff.snapshot.user_preferences;
    let summary = sanitize_summary(MemorySummary {
        goal: handoff.snapshot.goal,
        current_state: handoff.snapshot.current_state,
        decisions: handoff.snapshot.decisions,
        artifacts: handoff.snapshot.artifacts,
        issues: handoff.snapshot.open_questions,
        next_steps: handoff.snapshot.next_steps,
        work_constraints: handoff.snapshot.do_not_forget,
    })
    .ok()?;
    Some(LegacySummary {
        summary,
        through_turn_id: handoff
            .last_compacted_turn_id
            .filter(|turn_id| !turn_id.trim().is_empty()),
    })
}

pub(crate) async fn ensure_legacy_import(
    memory: &Arc<dyn IMemoryRepository>,
    conversations: &Arc<dyn IConversationRepository>,
    user_id: &str,
) -> Result<(), MemoryError> {
    let state = memory
        .get_import_state(user_id)
        .await
        .map_err(crate::service::map_db_error)?;
    if state.as_ref().is_some_and(|state| state.completed) {
        return Ok(());
    }
    let expected_cursor = state.as_ref().and_then(|state| state.cursor.clone());
    let cursor = match expected_cursor.as_deref() {
        Some(value) => match LegacyImportCursor::parse(value) {
            Some(cursor) => cursor,
            None => {
                warn!(
                    user_id,
                    status = "invalid_cursor",
                    "Legacy Memory import cursor quarantined"
                );
                memory
                    .import_legacy_memory_page(ImportLegacyMemoryPageRow {
                        user_id: user_id.into(),
                        expected_cursor: expected_cursor.clone(),
                        next_cursor: expected_cursor,
                        max_conversation_sequence: None,
                        completed: true,
                        summaries: Vec::new(),
                        now: aionui_common::now_ms(),
                    })
                    .await
                    .map_err(crate::service::map_db_error)?;
                return Ok(());
            }
        },
        None => {
            let Some(upper) = conversations
                .memory_import_upper_bound(user_id)
                .await
                .map_err(crate::service::map_db_error)?
            else {
                memory
                    .import_legacy_memory_page(ImportLegacyMemoryPageRow {
                        user_id: user_id.into(),
                        expected_cursor,
                        next_cursor: None,
                        max_conversation_sequence: None,
                        completed: true,
                        summaries: Vec::new(),
                        now: aionui_common::now_ms(),
                    })
                    .await
                    .map_err(crate::service::map_db_error)?;
                return Ok(());
            };
            LegacyImportCursor {
                version: LEGACY_IMPORT_CURSOR_VERSION,
                boundary: upper,
                after: None,
            }
        }
    };
    let rows = conversations
        .list_for_memory_import(
            user_id,
            cursor.after.as_ref(),
            &cursor.boundary,
            LEGACY_IMPORT_PAGE_SIZE,
        )
        .await
        .map_err(crate::service::map_db_error)?;
    let completed = rows.len() < LEGACY_IMPORT_PAGE_SIZE as usize;
    let max_conversation_sequence = cursor.boundary.max_sequence;
    let next_cursor = LegacyImportCursor {
        version: LEGACY_IMPORT_CURSOR_VERSION,
        boundary: cursor.boundary,
        after: rows
            .last()
            .map(|row| LegacyConversationCursor {
                updated_at: row.updated_at,
                id: row.id.clone(),
            })
            .or(cursor.after),
    };
    let mut summaries = Vec::new();
    for row in &rows {
        let Some(imported) = legacy_summary(&row.extra) else {
            continue;
        };
        let policy = memory
            .effective_policy(user_id, &row.id)
            .await
            .map_err(crate::service::map_db_error)?;
        if policy.reset_at.is_some() {
            continue;
        }
        let target = RetrievalTarget::from_conversation(row);
        summaries.push(LegacyMemorySummaryRow {
            conversation_id: row.id.clone(),
            expected_updated_at: row.updated_at,
            expected_extra: row.extra.clone(),
            expected_conversation_epoch: policy.conversation_epoch,
            project_id: target.project_id,
            workspace_key: target.workspace_key,
            summary_json: serde_json::to_string(&imported.summary).map_err(|_| MemoryError::Internal)?,
            through_turn_id: imported
                .through_turn_id
                .unwrap_or_else(|| format!("legacy-context-handoff:{}", row.id)),
            created_at: row.created_at,
            updated_at: row.updated_at,
        });
    }
    memory
        .import_legacy_memory_page(ImportLegacyMemoryPageRow {
            user_id: user_id.into(),
            expected_cursor,
            next_cursor: Some(serde_json::to_string(&next_cursor).map_err(|_| MemoryError::Internal)?),
            max_conversation_sequence: Some(max_conversation_sequence),
            completed,
            summaries,
            now: aionui_common::now_ms(),
        })
        .await
        .map_err(crate::service::map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_db::models::ConversationRow;
    use aionui_db::{
        IConversationRepository, IMemoryRepository, ImportLegacyMemoryPageRow, LegacyMemorySummaryRow,
        SqliteConversationRepository, SqliteMemoryRepository, UpdateMemorySettingsRow, init_database_memory,
    };

    use super::legacy_summary;
    use crate::{AppOperationsReadinessPort, MemoryError, MemoryService};

    const USER_ID: &str = "system_default_user";

    struct Ready;

    #[async_trait::async_trait]
    impl AppOperationsReadinessPort for Ready {
        async fn is_usable(&self) -> Result<bool, MemoryError> {
            Ok(true)
        }
    }

    fn extra(goal: &str, turn_id: &str) -> String {
        serde_json::json!({
            "workspace": "/work/memory",
            "project_id": "memory-project",
            "context_handoff": {
                "snapshot": {
                    "goal": goal,
                    "current_state": ["Backend is ready"],
                    "decisions": ["Use local SQLite"],
                    "artifacts": ["docs/memory.md"],
                    "user_preferences": ["Always call me Ada"],
                    "open_questions": ["Which rollout cohort?"],
                    "next_steps": ["Run verification"],
                    "do_not_forget": ["Do not rewrite Context.md"]
                },
                "last_compacted_turn_id": turn_id,
                "context_file_path": "/tmp/Context.md",
                "revision": 3,
                "status": "ready"
            }
        })
        .to_string()
    }

    #[test]
    fn legacy_import_maps_only_structured_handoff_work_state() {
        let extra = serde_json::json!({
            "context_handoff": {
                "snapshot": {
                    "goal": "Ship Memory",
                    "current_state": ["Backend is ready"],
                    "decisions": ["Use local SQLite"],
                    "artifacts": ["docs/memory.md"],
                    "user_preferences": ["Always call me Ada"],
                    "open_questions": ["Which rollout cohort?"],
                    "next_steps": ["Run verification"],
                    "do_not_forget": ["Do not rewrite Context.md"]
                },
                "last_compacted_turn_id": "turn-42",
                "context_file_path": "/tmp/Context.md"
            }
        })
        .to_string();

        let imported = legacy_summary(&extra).expect("valid structured snapshot");
        assert_eq!(imported.through_turn_id.as_deref(), Some("turn-42"));
        assert_eq!(imported.summary.goal, "Ship Memory");
        assert_eq!(imported.summary.issues, ["Which rollout cohort?"]);
        assert_eq!(imported.summary.work_constraints, ["Do not rewrite Context.md"]);
        let serialized = serde_json::to_string(&imported.summary).unwrap();
        assert!(!serialized.contains("Ada"));
        assert!(!serialized.contains("user_preferences"));
    }

    #[test]
    fn legacy_import_skips_malformed_or_unstructured_extra() {
        for extra in [
            "{}",
            r#"{"context_handoff":{"snapshot":{"goal":"missing fields"}}}"#,
            r#"{"context_handoff":{"snapshot":{"goal":"x","current_state":[],"decisions":[],"artifacts":[],"user_preferences":[],"open_questions":[],"next_steps":[],"do_not_forget":"not an array"}}}"#,
            "not json",
        ] {
            assert!(legacy_summary(extra).is_none());
        }
    }

    #[tokio::test]
    async fn legacy_import_is_bounded_resumable_idempotent_and_content_free() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        let context_path = std::env::temp_dir().join(format!(
            "{}-Context.md",
            aionui_common::generate_prefixed_id("legacy-memory-test")
        ));
        std::fs::write(&context_path, "legacy context contents").unwrap();
        for index in 1..=33 {
            let value = if index == 20 {
                r#"{"context_handoff":{"snapshot":"malformed"}}"#.into()
            } else {
                let value = extra(&format!("Goal {index}"), &format!("turn-{index}"));
                if index == 33 {
                    value.replace("/tmp/Context.md", &context_path.to_string_lossy())
                } else {
                    value
                }
            };
            conversations
                .create(&ConversationRow {
                    id: format!("conversation-{index:02}"),
                    user_id: USER_ID.into(),
                    name: format!("Conversation {index}"),
                    r#type: "acp".into(),
                    extra: value,
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: index,
                    updated_at: index,
                })
                .await
                .unwrap();
        }
        let original_extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = 'conversation-33'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let service = MemoryService::with_job_dependencies(memory.clone(), conversations.clone(), Arc::new(Ready));

        service.get_settings(USER_ID).await.unwrap();
        let first_state = memory.get_import_state(USER_ID).await.unwrap().unwrap();
        assert!(!first_state.completed);
        assert!(first_state.cursor.is_some());
        let durable_cursor: serde_json::Value = serde_json::from_str(first_state.cursor.as_deref().unwrap()).unwrap();
        assert_eq!(durable_cursor["version"], 2);
        assert!(durable_cursor["boundary"]["upper"]["updated_at"].is_number());
        assert!(durable_cursor["boundary"]["max_sequence"].is_number());
        assert!(durable_cursor["after"]["updated_at"].is_number());
        let first_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_memories WHERE user_id = ?")
            .bind(USER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(first_count, 31);

        // Reconstructing the service simulates a process restart; the durable cursor resumes.
        let restarted = MemoryService::with_job_dependencies(memory.clone(), conversations.clone(), Arc::new(Ready));
        restarted.get_settings(USER_ID).await.unwrap();
        let completed_state = memory.get_import_state(USER_ID).await.unwrap().unwrap();
        assert!(completed_state.completed);
        let completed_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_memories WHERE user_id = ?")
            .bind(USER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(completed_count, 32);

        restarted.get_settings(USER_ID).await.unwrap();
        let idempotent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_memories WHERE user_id = ?")
            .bind(USER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(idempotent_count, completed_count);
        let (source, summary_json, through_turn_id, project_id, workspace_key): (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT source,summary_json,through_turn_id,project_id,workspace_key
             FROM conversation_memories WHERE conversation_id = 'conversation-33'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(source, "legacy_context_snapshot");
        assert_eq!(through_turn_id, "turn-33");
        assert_eq!(project_id.as_deref(), Some("memory-project"));
        assert_eq!(workspace_key.as_deref(), Some("/work/memory"));
        assert!(!summary_json.contains("Ada"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT
                    (SELECT COUNT(*) FROM memory_jobs) +
                    (SELECT COUNT(*) FROM memory_entries) +
                    (SELECT COUNT(*) FROM memory_change_sets)",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
            0,
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT extra FROM conversations WHERE id = 'conversation-33'")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            original_extra,
        );
        assert_eq!(
            std::fs::read_to_string(&context_path).unwrap(),
            "legacy context contents"
        );
        std::fs::remove_file(context_path).unwrap();
    }

    #[tokio::test]
    async fn concurrent_importers_do_not_advance_twice_and_clear_is_terminal() {
        let db = init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory: Arc<dyn IMemoryRepository> = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        for index in 1..=33 {
            conversations
                .create(&ConversationRow {
                    id: format!("race-{index:02}"),
                    user_id: USER_ID.into(),
                    name: "Race".into(),
                    r#type: "acp".into(),
                    extra: extra("Race-safe", &format!("turn-{index}")),
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: index,
                    updated_at: index,
                })
                .await
                .unwrap();
        }

        let first = super::ensure_legacy_import(&memory, &conversations, USER_ID);
        let second = super::ensure_legacy_import(&memory, &conversations, USER_ID);
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            32,
        );

        memory.clear_memory(USER_ID, aionui_common::now_ms()).await.unwrap();
        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        assert!(
            memory
                .get_import_state(USER_ID)
                .await
                .unwrap()
                .is_some_and(|state| state.completed),
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0,
        );
    }

    #[tokio::test]
    async fn import_uses_a_stable_ascending_snapshot_and_excludes_later_mutations() {
        let db = init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory: Arc<dyn IMemoryRepository> = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        for index in 1..=34 {
            conversations
                .create(&ConversationRow {
                    id: format!("stable-{index:02}"),
                    user_id: USER_ID.into(),
                    name: "Stable".into(),
                    r#type: "acp".into(),
                    extra: extra("Stable import", &format!("turn-{index}")),
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: index,
                    updated_at: 10,
                })
                .await
                .unwrap();
        }

        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM conversation_memories WHERE conversation_id = 'stable-01')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM conversation_memories WHERE conversation_id = 'stable-34')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
        );

        // Deleting the maximum member must not let SQLite rowid reuse admit a
        // replacement whose tied tuple falls between the cursor and upper bound.
        conversations.delete("stable-34").await.unwrap();
        conversations
            .create(&ConversationRow {
                id: "stable-33a".into(),
                user_id: USER_ID.into(),
                name: "New".into(),
                r#type: "acp".into(),
                extra: extra("New after boundary", "turn-new"),
                model: None,
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 10,
                updated_at: 10,
            })
            .await
            .unwrap();
        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        assert!(memory.get_import_state(USER_ID).await.unwrap().unwrap().completed);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            33,
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM conversation_memories WHERE conversation_id = 'stable-33a')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
        );
    }

    #[tokio::test]
    async fn completion_trigger_imports_only_after_durable_turn_eligibility() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "ineligible-completion".into(),
                user_id: USER_ID.into(),
                name: "Ineligible".into(),
                r#type: "acp".into(),
                extra: extra("Must not import", "turn-1"),
                model: None,
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        let service = MemoryService::with_job_dependencies(memory.clone(), conversations.clone(), Arc::new(Ready));

        assert!(
            !service
                .admit_turn_completed(
                    USER_ID,
                    "ineligible-completion",
                    "missing-turn",
                    crate::MemoryTurnOutcome::Completed,
                )
                .await
                .unwrap(),
        );
        assert!(memory.get_import_state(USER_ID).await.unwrap().is_none());
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: USER_ID.into(),
                enabled: Some(true),
                default_capture: Some(true),
                default_recall: None,
                consent_version: None,
                now: 2,
            })
            .await
            .unwrap();
        assert!(
            !service
                .admit_turn_completed(
                    USER_ID,
                    "ineligible-completion",
                    "missing-turn",
                    crate::MemoryTurnOutcome::Completed
                )
                .await
                .unwrap(),
        );
        assert!(memory.get_import_state(USER_ID).await.unwrap().is_none());
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: USER_ID.into(),
                enabled: None,
                default_capture: Some(false),
                default_recall: None,
                consent_version: Some(crate::jobs::MEMORY_DISCLOSURE_VERSION),
                now: 3,
            })
            .await
            .unwrap();
        assert!(
            !service
                .admit_turn_completed(
                    USER_ID,
                    "ineligible-completion",
                    "missing-turn",
                    crate::MemoryTurnOutcome::Completed,
                )
                .await
                .unwrap(),
        );
        assert!(memory.get_import_state(USER_ID).await.unwrap().is_none());
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: USER_ID.into(),
                enabled: None,
                default_capture: Some(true),
                default_recall: None,
                consent_version: None,
                now: 4,
            })
            .await
            .unwrap();
        assert!(
            !service
                .admit_turn_completed(
                    USER_ID,
                    "ineligible-completion",
                    "missing-turn",
                    crate::MemoryTurnOutcome::Completed,
                )
                .await
                .unwrap(),
        );
        assert!(memory.get_import_state(USER_ID).await.unwrap().is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0,
        );
    }

    #[tokio::test]
    async fn obsolete_v1_cursor_is_terminally_quarantined_without_reindexing() {
        let db = init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory: Arc<dyn IMemoryRepository> = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "cursor-row".into(),
                user_id: USER_ID.into(),
                name: "Cursor".into(),
                r#type: "acp".into(),
                extra: extra("Do not reindex", "turn-1"),
                model: None,
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        let obsolete_cursor =
            r#"{"version":1,"boundary":{"upper":{"updated_at":1,"id":"cursor-row"},"max_rowid":1},"after":null}"#;
        memory
            .upsert_import_state(aionui_db::models::MemoryImportStateRow {
                user_id: USER_ID.into(),
                cursor: Some(obsolete_cursor.into()),
                completed: false,
                started_at: Some(1),
                completed_at: None,
                updated_at: 1,
            })
            .await
            .unwrap();

        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        let state = memory.get_import_state(USER_ID).await.unwrap().unwrap();
        assert!(state.completed);
        assert_eq!(state.cursor.as_deref(), Some(obsolete_cursor));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0,
        );
    }

    #[tokio::test]
    async fn conversation_forget_fences_an_unscanned_summary() {
        let db = init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory: Arc<dyn IMemoryRepository> = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        for index in 1..=33 {
            conversations
                .create(&ConversationRow {
                    id: format!("forget-{index:02}"),
                    user_id: USER_ID.into(),
                    name: "Forget".into(),
                    r#type: "acp".into(),
                    extra: extra("Forget fence", &format!("turn-{index}")),
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: index,
                    updated_at: index,
                })
                .await
                .unwrap();
        }
        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        memory
            .delete_conversation_memory(USER_ID, "forget-33", 100)
            .await
            .unwrap();
        super::ensure_legacy_import(&memory, &conversations, USER_ID)
            .await
            .unwrap();
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM conversation_memories WHERE conversation_id = 'forget-33')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            32,
        );
    }

    #[tokio::test]
    async fn atomic_page_commit_rejects_scanned_mutation_and_forget_races() {
        let db = init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory: Arc<dyn IMemoryRepository> = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        let scanned_extra = extra("Scanned", "turn-1");
        let forgotten_extra = extra("Scanned before forget", "turn-forgotten");
        for (id, value) in [
            ("stale-mutation", scanned_extra.clone()),
            ("stale-forget", forgotten_extra.clone()),
        ] {
            conversations
                .create(&ConversationRow {
                    id: id.into(),
                    user_id: USER_ID.into(),
                    name: "Stale".into(),
                    r#type: "acp".into(),
                    extra: value,
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: 1,
                    updated_at: 1,
                })
                .await
                .unwrap();
        }
        memory
            .delete_conversation_memory(USER_ID, "stale-forget", 2)
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET updated_at = 2,extra = ? WHERE id = 'stale-mutation'")
            .bind(extra("Changed", "turn-2"))
            .execute(db.pool())
            .await
            .unwrap();
        let summary_row = |conversation_id: &str, value: String, through_turn_id: &str| LegacyMemorySummaryRow {
            conversation_id: conversation_id.into(),
            expected_updated_at: 1,
            expected_extra: value.clone(),
            expected_conversation_epoch: 0,
            project_id: None,
            workspace_key: None,
            summary_json: serde_json::to_string(&legacy_summary(&value).unwrap().summary).unwrap(),
            through_turn_id: through_turn_id.into(),
            created_at: 1,
            updated_at: 1,
        };
        memory
            .import_legacy_memory_page(ImportLegacyMemoryPageRow {
                user_id: USER_ID.into(),
                expected_cursor: None,
                next_cursor: Some(r#"{"version":1}"#.into()),
                max_conversation_sequence: Some(i64::MAX),
                completed: false,
                summaries: vec![
                    summary_row("stale-mutation", scanned_extra, "turn-1"),
                    summary_row("stale-forget", forgotten_extra, "turn-forgotten"),
                ],
                now: 3,
            })
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversation_memories")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0,
        );
    }
}
