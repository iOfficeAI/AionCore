//! SQLite-backed `acp_session` repository.

use aionui_common::now_ms;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::AcpSessionRow;
use crate::repository::acp_session::{
    CreateAcpSessionParams, IAcpSessionRepository, PersistedSessionState, SaveRuntimeStateParams,
};

#[derive(Clone, Debug)]
pub struct SqliteAcpSessionRepository {
    pool: SqlitePool,
}

impl SqliteAcpSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    #[cfg(test)]
    async fn get_unscoped(&self, conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
        let row = sqlx::query_as::<_, AcpSessionRow>("SELECT * FROM acp_session WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    #[cfg(test)]
    async fn update_session_id_unscoped(&self, conversation_id: &str, session_id: &str) -> Result<bool, DbError> {
        let now = now_ms();
        let result = sqlx::query("UPDATE acp_session SET session_id = ?, last_active_at = ? WHERE conversation_id = ?")
            .bind(session_id)
            .bind(now)
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    #[cfg(test)]
    async fn delete_unscoped(&self, conversation_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM acp_session WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    #[cfg(test)]
    async fn load_runtime_state_unscoped(
        &self,
        conversation_id: &str,
    ) -> Result<Option<PersistedSessionState>, DbError> {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT session_config FROM acp_session WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await?;

        let Some(raw) = raw else {
            return Ok(None);
        };

        Ok(Some(decode_runtime_state(&raw)?))
    }

    #[cfg(test)]
    async fn save_runtime_state_unscoped(
        &self,
        conversation_id: &str,
        params: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, DbError> {
        if params.is_empty() {
            return Ok(true);
        }

        // Read-modify-write. The service layer serialises writes per
        // conversation_id through a single consumer task, so a naive
        // RMW is race-free for our callers.
        let raw: Option<String> =
            sqlx::query_scalar("SELECT session_config FROM acp_session WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await?;

        let Some(raw) = raw else {
            return Ok(false);
        };

        let new_config = merge_runtime_state(&raw, params)?;
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE acp_session SET session_config = ?, last_active_at = ? \
             WHERE conversation_id = ?",
        )
        .bind(new_config)
        .bind(now)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

fn decode_runtime_state(raw: &str) -> Result<PersistedSessionState, DbError> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| DbError::Init(format!("invalid session_config JSON: {e}")))?;
    let runtime = parsed.get("runtime");

    let mut state = PersistedSessionState::default();
    if let Some(rt) = runtime {
        state.current_mode_id = rt.get("current_mode_id").and_then(Value::as_str).map(ToOwned::to_owned);
        state.current_model_id = rt
            .get("current_model_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        state.config_selections_json = rt.get("config_selections").map(serde_json::Value::to_string);
        state.context_usage_json = rt.get("context_usage").map(serde_json::Value::to_string);
    }
    Ok(state)
}

fn merge_runtime_state(raw: &str, params: &SaveRuntimeStateParams<'_>) -> Result<String, DbError> {
    let mut parsed: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::Object(Default::default()));
    let runtime = parsed
        .as_object_mut()
        .ok_or_else(|| DbError::Init("session_config is not a JSON object".into()))?
        .entry("runtime")
        .or_insert_with(|| Value::Object(Default::default()));
    let runtime = runtime
        .as_object_mut()
        .ok_or_else(|| DbError::Init("session_config.runtime is not a JSON object".into()))?;

    if let Some(outer) = params.current_mode_id {
        match outer {
            Some(v) => {
                runtime.insert("current_mode_id".into(), Value::String(v.to_owned()));
            }
            None => {
                runtime.remove("current_mode_id");
            }
        }
    }
    if let Some(outer) = params.current_model_id {
        match outer {
            Some(v) => {
                runtime.insert("current_model_id".into(), Value::String(v.to_owned()));
            }
            None => {
                runtime.remove("current_model_id");
            }
        }
    }
    if let Some(outer) = params.config_selections_json {
        match outer {
            Some(json) => {
                let v: Value = serde_json::from_str(json)
                    .map_err(|e| DbError::Init(format!("invalid config_selections JSON: {e}")))?;
                runtime.insert("config_selections".into(), v);
            }
            None => {
                runtime.remove("config_selections");
            }
        }
    }
    if let Some(outer) = params.context_usage_json {
        match outer {
            Some(json) => {
                let v: Value = serde_json::from_str(json)
                    .map_err(|e| DbError::Init(format!("invalid context_usage JSON: {e}")))?;
                runtime.insert("context_usage".into(), v);
            }
            None => {
                runtime.remove("context_usage");
            }
        }
    }

    serde_json::to_string(&parsed).map_err(|e| DbError::Init(format!("encode session_config: {e}")))
}

#[async_trait::async_trait]
impl IAcpSessionRepository for SqliteAcpSessionRepository {
    async fn get_for_user(&self, user_id: &str, conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
        let row = sqlx::query_as::<_, AcpSessionRow>(
            "SELECT a.* FROM acp_session a \
             WHERE a.conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = a.conversation_id AND c.user_id = ?)",
        )
        .bind(conversation_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create(&self, params: &CreateAcpSessionParams<'_>) -> Result<AcpSessionRow, DbError> {
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO acp_session \
                (conversation_id, agent_source, agent_id, \
                 session_id, session_status, session_config, last_active_at) \
             SELECT c.id, ?, ?, NULL, 'idle', '{}', ? \
             FROM conversations c \
             WHERE c.id = ? AND c.user_id = ?",
        )
        .bind(params.agent_source)
        .bind(params.agent_id)
        .bind(now)
        .bind(params.conversation_id)
        .bind(params.user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => DbError::Conflict(format!(
                "acp_session row for conversation '{}' already exists",
                params.conversation_id
            )),
            _ => DbError::Query(e),
        })?;

        if result.rows_affected() == 0 {
            let conversation_owner = sqlx::query_scalar::<_, String>("SELECT user_id FROM conversations WHERE id = ?")
                .bind(params.conversation_id)
                .fetch_optional(&self.pool)
                .await?;

            if conversation_owner
                .as_deref()
                .is_some_and(|user_id| user_id != params.user_id)
            {
                return Err(DbError::Conflict(format!(
                    "CROSS_ACCOUNT_REFERENCE: acp_session conversation '{}' belongs to another user",
                    params.conversation_id
                )));
            }

            return Err(DbError::NotFound(format!(
                "conversation '{}' for user '{}'",
                params.conversation_id, params.user_id
            )));
        }

        self.get_for_user(params.user_id, params.conversation_id)
            .await?
            .ok_or_else(|| {
                DbError::Init(format!(
                    "create did not produce acp_session row for '{}'",
                    params.conversation_id
                ))
            })
    }

    async fn update_session_id_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
        session_id: &str,
    ) -> Result<bool, DbError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE acp_session SET session_id = ?, last_active_at = ? \
             WHERE conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = acp_session.conversation_id AND c.user_id = ?)",
        )
        .bind(session_id)
        .bind(now)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn clear_session_id_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE acp_session SET session_id = NULL, last_active_at = ? \
             WHERE conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = acp_session.conversation_id AND c.user_id = ?)",
        )
        .bind(now)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query(
            "DELETE FROM acp_session \
             WHERE conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = acp_session.conversation_id AND c.user_id = ?)",
        )
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn load_runtime_state_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<PersistedSessionState>, DbError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT a.session_config FROM acp_session a \
             WHERE a.conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = a.conversation_id AND c.user_id = ?)",
        )
        .bind(conversation_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(raw) = raw else {
            return Ok(None);
        };

        Ok(Some(decode_runtime_state(&raw)?))
    }

    async fn save_runtime_state_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, DbError> {
        if params.is_empty() {
            return Ok(true);
        }

        let raw: Option<String> = sqlx::query_scalar(
            "SELECT a.session_config FROM acp_session a \
             WHERE a.conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = a.conversation_id AND c.user_id = ?)",
        )
        .bind(conversation_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(raw) = raw else {
            return Ok(false);
        };

        let new_config = merge_runtime_state(&raw, params)?;
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE acp_session SET session_config = ?, last_active_at = ? \
             WHERE conversation_id = ? \
               AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = acp_session.conversation_id AND c.user_id = ?)",
        )
        .bind(new_config)
        .bind(now)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteAcpSessionRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteAcpSessionRepository::new(db.pool().clone());
        insert_conversation(&repo, "user-1", "conv-1").await;
        (repo, db)
    }

    fn create_params<'a>(conversation_id: &'a str) -> CreateAcpSessionParams<'a> {
        CreateAcpSessionParams {
            user_id: "user-1",
            conversation_id,
            agent_source: "builtin",
            agent_id: "2d23ff1c",
        }
    }

    async fn insert_conversation(repo: &SqliteAcpSessionRepository, user_id: &str, conversation_id: &str) {
        let now = now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO users \
                (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, ?, ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(&repo.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT OR IGNORE INTO conversations \
                (id, user_id, name, type, extra, model, status, source, \
                 channel_chat_id, pinned, pinned_at, created_at, updated_at) \
             VALUES (?, ?, 'Test', 'acp', '{}', NULL, 'pending', NULL, NULL, 0, NULL, ?, ?)",
        )
        .bind(conversation_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(&repo.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn create_then_get_roundtrips() {
        let (repo, _db) = setup().await;
        let row = repo.create(&create_params("conv-1")).await.unwrap();
        assert_eq!(row.conversation_id, "conv-1");
        assert_eq!(row.agent_id, "2d23ff1c");
        assert_eq!(row.session_id, None);
        assert_eq!(row.session_status, "idle");
        assert_eq!(row.session_config, "{}");

        let fetched = repo.get_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(fetched.conversation_id, "conv-1");
    }

    #[tokio::test]
    async fn create_duplicate_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();
        let err = repo.create(&create_params("conv-1")).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn create_rejects_cross_user_parent() {
        let (repo, _db) = setup().await;
        let err = repo
            .create(&CreateAcpSessionParams {
                user_id: "user-2",
                conversation_id: "conv-1",
                agent_source: "builtin",
                agent_id: "2d23ff1c",
            })
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            DbError::Conflict(msg) if msg.starts_with("CROSS_ACCOUNT_REFERENCE:")
        ));
        assert!(repo.get_for_user("user-1", "conv-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_id_flips_field() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();
        assert!(repo.update_session_id_unscoped("conv-1", "sess-abc").await.unwrap());

        let fetched = repo.get_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(fetched.session_id.as_deref(), Some("sess-abc"));
        assert!(fetched.last_active_at.is_some());
    }

    #[tokio::test]
    async fn update_session_id_missing_row_returns_false() {
        let (repo, _db) = setup().await;
        assert!(!repo.update_session_id_unscoped("nope", "sid").await.unwrap());
    }

    #[tokio::test]
    async fn update_session_id_for_user_rejects_other_owner() {
        let (repo, _db) = setup().await;
        insert_conversation(&repo, "user-1", "conv-1").await;
        repo.create(&create_params("conv-1")).await.unwrap();

        assert!(
            !repo
                .update_session_id_for_user("user-2", "conv-1", "sess-other")
                .await
                .unwrap()
        );
        assert!(
            repo.update_session_id_for_user("user-1", "conv-1", "sess-owner")
                .await
                .unwrap()
        );

        let fetched = repo.get_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(fetched.session_id.as_deref(), Some("sess-owner"));
    }

    #[tokio::test]
    async fn clear_session_id_nulls_field_but_keeps_row() {
        let (repo, _db) = setup().await;
        insert_conversation(&repo, "user-1", "conv-1").await;
        repo.create(&create_params("conv-1")).await.unwrap();
        repo.update_session_id_unscoped("conv-1", "sess-abc").await.unwrap();

        assert!(repo.clear_session_id_for_user("user-1", "conv-1").await.unwrap());
        let fetched = repo.get_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(fetched.session_id, None, "the resume anchor must be nulled");
        assert_eq!(
            fetched.session_status, "idle",
            "the row (and its config) survives the clear"
        );
    }

    #[tokio::test]
    async fn clear_session_id_rejects_other_owner() {
        let (repo, _db) = setup().await;
        insert_conversation(&repo, "user-1", "conv-1").await;
        repo.create(&create_params("conv-1")).await.unwrap();
        repo.update_session_id_unscoped("conv-1", "sess-abc").await.unwrap();

        assert!(!repo.clear_session_id_for_user("intruder", "conv-1").await.unwrap());
        let fetched = repo.get_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(
            fetched.session_id.as_deref(),
            Some("sess-abc"),
            "a foreign user must not clear another owner's resume anchor"
        );
    }

    #[tokio::test]
    async fn clear_session_id_missing_row_returns_false() {
        let (repo, _db) = setup().await;
        assert!(!repo.clear_session_id_for_user("user-1", "nope").await.unwrap());
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();
        assert!(repo.delete_unscoped("conv-1").await.unwrap());
        assert!(repo.get_unscoped("conv-1").await.unwrap().is_none());
        assert!(!repo.delete_unscoped("conv-1").await.unwrap());
    }

    #[tokio::test]
    async fn get_and_delete_for_user_are_owner_scoped() {
        let (repo, _db) = setup().await;
        insert_conversation(&repo, "user-1", "conv-1").await;
        repo.create(&create_params("conv-1")).await.unwrap();

        assert!(repo.get_for_user("user-2", "conv-1").await.unwrap().is_none());
        assert!(repo.get_for_user("user-1", "conv-1").await.unwrap().is_some());
        assert!(!repo.delete_for_user("user-2", "conv-1").await.unwrap());
        assert!(repo.get_unscoped("conv-1").await.unwrap().is_some());
        assert!(repo.delete_for_user("user-1", "conv-1").await.unwrap());
        assert!(repo.get_unscoped("conv-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_runtime_state_missing_row() {
        let (repo, _db) = setup().await;
        assert!(repo.load_runtime_state_unscoped("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_runtime_state_empty_config_returns_defaults() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();
        let state = repo.load_runtime_state_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(state, PersistedSessionState::default());
    }

    #[tokio::test]
    async fn save_runtime_state_writes_each_field() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();

        assert!(
            repo.save_runtime_state_unscoped(
                "conv-1",
                &SaveRuntimeStateParams {
                    current_mode_id: Some(Some("code")),
                    current_model_id: Some(Some("claude-sonnet-4")),
                    config_selections_json: Some(Some(r#"{"reasoning":"high"}"#)),
                    context_usage_json: Some(Some(r#"{"used":10,"total":100}"#)),
                },
            )
            .await
            .unwrap()
        );

        let state = repo.load_runtime_state_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(state.current_mode_id.as_deref(), Some("code"));
        assert_eq!(state.current_model_id.as_deref(), Some("claude-sonnet-4"));
        // The stored JSON should parse back to the same payload
        // regardless of key order (serde_json::Map preserves insertion
        // order but the caller shouldn't depend on it here).
        let selections: Value = serde_json::from_str(state.config_selections_json.as_deref().unwrap()).unwrap();
        assert_eq!(selections["reasoning"], "high");
        let usage: Value = serde_json::from_str(state.context_usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(usage["used"], 10);
        assert_eq!(usage["total"], 100);
    }

    #[tokio::test]
    async fn runtime_state_for_user_is_owner_scoped() {
        let (repo, _db) = setup().await;
        insert_conversation(&repo, "user-1", "conv-1").await;
        repo.create(&create_params("conv-1")).await.unwrap();

        assert!(
            !repo
                .save_runtime_state_for_user(
                    "user-2",
                    "conv-1",
                    &SaveRuntimeStateParams {
                        current_mode_id: Some(Some("other-mode")),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
        );
        assert!(
            repo.save_runtime_state_for_user(
                "user-1",
                "conv-1",
                &SaveRuntimeStateParams {
                    current_mode_id: Some(Some("owner-mode")),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        );

        assert!(
            repo.load_runtime_state_for_user("user-2", "conv-1")
                .await
                .unwrap()
                .is_none()
        );
        let state = repo
            .load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.current_mode_id.as_deref(), Some("owner-mode"));
    }

    #[tokio::test]
    async fn save_runtime_state_partial_preserves_siblings() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();

        repo.save_runtime_state_unscoped(
            "conv-1",
            &SaveRuntimeStateParams {
                current_mode_id: Some(Some("code")),
                current_model_id: Some(Some("sonnet-4")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Later write only touches current_model_id.
        repo.save_runtime_state_unscoped(
            "conv-1",
            &SaveRuntimeStateParams {
                current_model_id: Some(Some("opus-4")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let state = repo.load_runtime_state_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(
            state.current_mode_id.as_deref(),
            Some("code"),
            "mode must survive the model-only write"
        );
        assert_eq!(state.current_model_id.as_deref(), Some("opus-4"));
    }

    #[tokio::test]
    async fn save_runtime_state_some_none_clears_field() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();

        repo.save_runtime_state_unscoped(
            "conv-1",
            &SaveRuntimeStateParams {
                current_mode_id: Some(Some("code")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        repo.save_runtime_state_unscoped(
            "conv-1",
            &SaveRuntimeStateParams {
                current_mode_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let state = repo.load_runtime_state_unscoped("conv-1").await.unwrap().unwrap();
        assert!(state.current_mode_id.is_none());
    }

    #[tokio::test]
    async fn save_runtime_state_empty_params_is_noop() {
        let (repo, _db) = setup().await;
        repo.create(&create_params("conv-1")).await.unwrap();
        assert!(
            repo.save_runtime_state_unscoped("conv-1", &SaveRuntimeStateParams::default())
                .await
                .unwrap()
        );
        let state = repo.load_runtime_state_unscoped("conv-1").await.unwrap().unwrap();
        assert_eq!(state, PersistedSessionState::default());
    }

    #[tokio::test]
    async fn save_runtime_state_missing_row_returns_false() {
        let (repo, _db) = setup().await;
        let ok = repo
            .save_runtime_state_unscoped(
                "nope",
                &SaveRuntimeStateParams {
                    current_mode_id: Some(Some("x")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!ok);
    }
}
