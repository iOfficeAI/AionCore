use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
use crate::repository::channel::{IChannelRepository, UpdatePluginStatusParams};

/// SQLite-backed implementation of [`IChannelRepository`].
#[derive(Clone, Debug)]
pub struct SqliteChannelRepository {
    pool: SqlitePool,
}

impl SqliteChannelRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IChannelRepository for SqliteChannelRepository {
    // ── Plugin CRUD ──────────────────────────────────────────────────

    async fn get_all_plugins(&self, owner_user_id: &str) -> Result<Vec<ChannelPluginRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelPluginRow>(
            "SELECT * FROM assistant_plugins WHERE owner_user_id = ? ORDER BY created_at ASC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_plugin(&self, owner_user_id: &str, id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
        let row =
            sqlx::query_as::<_, ChannelPluginRow>("SELECT * FROM assistant_plugins WHERE owner_user_id = ? AND id = ?")
                .bind(owner_user_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn upsert_plugin(&self, owner_user_id: &str, row: &ChannelPluginRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_plugins \
                (id, owner_user_id, type, name, enabled, config, status, last_connected, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(owner_user_id, id) DO UPDATE SET \
                type = excluded.type, \
                name = excluded.name, \
                enabled = excluded.enabled, \
                config = excluded.config, \
                status = excluded.status, \
                last_connected = excluded.last_connected, \
                updated_at = excluded.updated_at",
        )
        .bind(&row.id)
        .bind(owner_user_id)
        .bind(&row.r#type)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.config)
        .bind(&row.status)
        .bind(row.last_connected)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_plugin_status(
        &self,
        owner_user_id: &str,
        id: &str,
        params: &UpdatePluginStatusParams,
    ) -> Result<(), DbError> {
        let mut set_clauses = Vec::new();
        if params.status.is_some() {
            set_clauses.push("status = ?");
        }
        if params.last_connected.is_some() {
            set_clauses.push("last_connected = ?");
        }
        if params.enabled.is_some() {
            set_clauses.push("enabled = ?");
        }

        if set_clauses.is_empty() {
            return Ok(());
        }

        set_clauses.push("updated_at = ?");
        let sql = format!(
            "UPDATE assistant_plugins SET {} WHERE owner_user_id = ? AND id = ?",
            set_clauses.join(", ")
        );

        let now = aionui_common::now_ms();
        let mut query = sqlx::query(&sql);

        if let Some(ref status) = params.status {
            query = query.bind(status);
        }
        if let Some(last_connected) = params.last_connected {
            query = query.bind(last_connected);
        }
        if let Some(enabled) = params.enabled {
            query = query.bind(enabled);
        }
        query = query.bind(now);
        query = query.bind(owner_user_id);
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_plugin(&self, owner_user_id: &str, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM assistant_plugins WHERE owner_user_id = ? AND id = ?")
            .bind(owner_user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Plugin '{id}' not found")));
        }
        Ok(())
    }

    // ── User CRUD ────────────────────────────────────────────────────

    async fn get_all_users(&self, owner_user_id: &str) -> Result<Vec<AssistantUserRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantUserRow>(
            "SELECT * FROM assistant_users WHERE owner_user_id = ? ORDER BY authorized_at DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_user_by_platform(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
    ) -> Result<Option<AssistantUserRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantUserRow>(
            "SELECT * FROM assistant_users \
             WHERE owner_user_id = ? AND platform_user_id = ? AND platform_type = ?",
        )
        .bind(owner_user_id)
        .bind(platform_user_id)
        .bind(platform_type)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_user(&self, owner_user_id: &str, row: &AssistantUserRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_users \
                (id, owner_user_id, platform_user_id, platform_type, display_name, \
                 authorized_at, last_active, session_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(owner_user_id)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.display_name)
        .bind(row.authorized_at)
        .bind(row.last_active)
        .bind(&row.session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!(
                    "User '{}' on platform '{}' already exists",
                    row.platform_user_id, row.platform_type
                ))
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn update_user_last_active(
        &self,
        owner_user_id: &str,
        id: &str,
        last_active: aionui_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_users SET last_active = ? WHERE owner_user_id = ? AND id = ?")
            .bind(last_active)
            .bind(owner_user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_user(&self, owner_user_id: &str, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM assistant_users WHERE owner_user_id = ? AND id = ?")
            .bind(owner_user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{id}' not found")));
        }
        Ok(())
    }

    // ── Session CRUD ─────────────────────────────────────────────────

    async fn get_all_sessions(&self, owner_user_id: &str) -> Result<Vec<AssistantSessionRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantSessionRow>(
            "SELECT s.* FROM assistant_sessions s \
             JOIN assistant_users u ON u.id = s.user_id \
             WHERE u.owner_user_id = ? \
             ORDER BY s.last_activity DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_session(&self, owner_user_id: &str, id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantSessionRow>(
            "SELECT s.* FROM assistant_sessions s \
             JOIN assistant_users u ON u.id = s.user_id \
             WHERE u.owner_user_id = ? AND s.id = ?",
        )
        .bind(owner_user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_or_create_session(
        &self,
        owner_user_id: &str,
        channel_user_id: &str,
        chat_id: &str,
        new_row: &AssistantSessionRow,
    ) -> Result<AssistantSessionRow, DbError> {
        // Try to find an existing session first.
        let existing = sqlx::query_as::<_, AssistantSessionRow>(
            "SELECT s.* FROM assistant_sessions s \
             JOIN assistant_users u ON u.id = s.user_id \
             WHERE u.owner_user_id = ? AND s.user_id = ? AND s.chat_id = ?",
        )
        .bind(owner_user_id)
        .bind(channel_user_id)
        .bind(chat_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            // Touch last_activity.
            let now = aionui_common::now_ms();
            sqlx::query("UPDATE assistant_sessions SET last_activity = ? WHERE id = ?")
                .bind(now)
                .bind(&row.id)
                .execute(&self.pool)
                .await?;

            return Ok(AssistantSessionRow {
                last_activity: now,
                ..row
            });
        }

        // Insert new session.
        sqlx::query(
            "INSERT INTO assistant_sessions \
                (id, user_id, agent_type, conversation_id, workspace, \
                 chat_id, created_at, last_activity) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ? \
             WHERE EXISTS (
                 SELECT 1 FROM assistant_users
                 WHERE owner_user_id = ? AND id = ?
             )
             AND (
                 ? IS NULL OR EXISTS (
                     SELECT 1 FROM conversations WHERE id = ? AND user_id = ?
                 )
             )",
        )
        .bind(&new_row.id)
        .bind(channel_user_id)
        .bind(&new_row.agent_type)
        .bind(&new_row.conversation_id)
        .bind(&new_row.workspace)
        .bind(&new_row.chat_id)
        .bind(new_row.created_at)
        .bind(new_row.last_activity)
        .bind(owner_user_id)
        .bind(channel_user_id)
        // Cross-account guard: a bound conversation must belong to the same
        // Core owner. NULL conversation_id (the current caller contract) is
        // allowed; a conversation owned by another user makes the INSERT match
        // zero rows, so get_session below returns NotFound.
        .bind(&new_row.conversation_id)
        .bind(&new_row.conversation_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;

        self.get_session(owner_user_id, &new_row.id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Channel user '{channel_user_id}' not found")))
    }

    async fn update_session_activity(
        &self,
        owner_user_id: &str,
        id: &str,
        last_activity: aionui_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE assistant_sessions SET last_activity = ? \
             WHERE id = ? AND EXISTS (
                 SELECT 1 FROM assistant_users u
                 WHERE u.id = assistant_sessions.user_id AND u.owner_user_id = ?
             )",
        )
        .bind(last_activity)
        .bind(id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn update_session_conversation(
        &self,
        owner_user_id: &str,
        id: &str,
        conversation_id: &str,
    ) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE assistant_sessions \
             SET conversation_id = ?, last_activity = ? \
             WHERE id = ? \
               AND EXISTS (
                   SELECT 1 FROM assistant_users u
                   WHERE u.id = assistant_sessions.user_id AND u.owner_user_id = ?
               )
               AND EXISTS (
                   SELECT 1 FROM conversations c
                   WHERE c.id = ? AND c.user_id = ?
               )",
        )
        .bind(conversation_id)
        .bind(now)
        .bind(id)
        .bind(owner_user_id)
        .bind(conversation_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let session_exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM assistant_sessions s \
                 JOIN assistant_users u ON u.id = s.user_id \
                 WHERE s.id = ? AND u.owner_user_id = ?",
            )
            .bind(id)
            .bind(owner_user_id)
            .fetch_one(&self.pool)
            .await?;

            if session_exists > 0 {
                let conversation_owner =
                    sqlx::query_scalar::<_, String>("SELECT user_id FROM conversations WHERE id = ?")
                        .bind(conversation_id)
                        .fetch_optional(&self.pool)
                        .await?;

                if conversation_owner
                    .as_deref()
                    .is_some_and(|user_id| user_id != owner_user_id)
                {
                    return Err(DbError::Conflict(
                        "CROSS_ACCOUNT_REFERENCE: channel session conversation belongs to another user".into(),
                    ));
                }
            }
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn update_session_agent_type(&self, owner_user_id: &str, id: &str, agent_type: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE assistant_sessions \
             SET agent_type = ?, last_activity = ? \
             WHERE id = ? AND EXISTS (
                 SELECT 1 FROM assistant_users u
                 WHERE u.id = assistant_sessions.user_id AND u.owner_user_id = ?
             )",
        )
        .bind(agent_type)
        .bind(now)
        .bind(id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Session '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_sessions_by_user(&self, owner_user_id: &str, channel_user_id: &str) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM assistant_sessions \
             WHERE user_id = ? AND EXISTS (
                 SELECT 1 FROM assistant_users u
                 WHERE u.id = assistant_sessions.user_id AND u.owner_user_id = ?
             )",
        )
        .bind(channel_user_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_session_by_user_chat(
        &self,
        owner_user_id: &str,
        channel_user_id: &str,
        chat_id: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM assistant_sessions \
             WHERE user_id = ? AND chat_id = ? AND EXISTS (
                 SELECT 1 FROM assistant_users u
                 WHERE u.id = assistant_sessions.user_id AND u.owner_user_id = ?
             )",
        )
        .bind(channel_user_id)
        .bind(chat_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Pairing Codes ────────────────────────────────────────────────

    async fn create_pairing(&self, owner_user_id: &str, row: &PairingCodeRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO assistant_pairing_codes \
                (code, owner_user_id, platform_user_id, platform_type, display_name, \
                 requested_at, expires_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.code)
        .bind(owner_user_id)
        .bind(&row.platform_user_id)
        .bind(&row.platform_type)
        .bind(&row.display_name)
        .bind(row.requested_at)
        .bind(row.expires_at)
        .bind(&row.status)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict(format!("Pairing code '{}' already exists", row.code))
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn get_pending_pairings(&self, owner_user_id: &str) -> Result<Vec<PairingCodeRow>, DbError> {
        let rows = sqlx::query_as::<_, PairingCodeRow>(
            "SELECT * FROM assistant_pairing_codes \
             WHERE owner_user_id = ? AND status = 'pending' \
             ORDER BY requested_at DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_pairing_by_code(&self, owner_user_id: &str, code: &str) -> Result<Option<PairingCodeRow>, DbError> {
        let row = sqlx::query_as::<_, PairingCodeRow>(
            "SELECT * FROM assistant_pairing_codes WHERE owner_user_id = ? AND code = ?",
        )
        .bind(owner_user_id)
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn update_pairing_status(&self, owner_user_id: &str, code: &str, status: &str) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE assistant_pairing_codes SET status = ? WHERE owner_user_id = ? AND code = ?")
            .bind(status)
            .bind(owner_user_id)
            .bind(code)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Pairing code '{code}' not found")));
        }
        Ok(())
    }

    async fn cleanup_expired_pairings(
        &self,
        owner_user_id: &str,
        now: aionui_common::TimestampMs,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE assistant_pairing_codes \
             SET status = 'expired' \
             WHERE owner_user_id = ? AND status = 'pending' AND expires_at <= ?",
        )
        .bind(owner_user_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Checks whether a sqlx error indicates a UNIQUE constraint violation.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => db_err.message().contains("UNIQUE constraint failed"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const OWNER_A: &str = "system_default_user";
    const OWNER_B: &str = "other_user";

    async fn setup() -> (SqliteChannelRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteChannelRepository::new(db.pool().clone());
        (repo, db)
    }

    async fn create_owner(pool: &SqlitePool, user_id: &str) {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT OR IGNORE INTO users \
                (id, username, password_hash, created_at, updated_at) \
             VALUES (?, ?, 'hash', ?, ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    fn sample_plugin() -> ChannelPluginRow {
        let now = aionui_common::now_ms();
        ChannelPluginRow {
            id: "tg-1".into(),
            owner_user_id: OWNER_A.into(),
            r#type: "telegram".into(),
            name: "My Telegram Bot".into(),
            enabled: false,
            config: r#"{"credentials":{"token":"enc_xxx"}}"#.into(),
            status: None,
            last_connected: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_user() -> AssistantUserRow {
        let now = aionui_common::now_ms();
        AssistantUserRow {
            id: "usr-1".into(),
            owner_user_id: OWNER_A.into(),
            platform_user_id: "tg_12345".into(),
            platform_type: "telegram".into(),
            display_name: Some("Alice".into()),
            authorized_at: now,
            last_active: None,
            session_id: None,
        }
    }

    fn sample_session(user_id: &str) -> AssistantSessionRow {
        let now = aionui_common::now_ms();
        AssistantSessionRow {
            id: "sess-1".into(),
            user_id: user_id.into(),
            agent_type: "gemini".into(),
            conversation_id: None,
            workspace: None,
            chat_id: Some("chat-abc".into()),
            created_at: now,
            last_activity: now,
        }
    }

    fn sample_pairing() -> PairingCodeRow {
        let now = aionui_common::now_ms();
        PairingCodeRow {
            code: "123456".into(),
            owner_user_id: OWNER_A.into(),
            platform_user_id: "tg_99".into(),
            platform_type: "telegram".into(),
            display_name: Some("Bob".into()),
            requested_at: now,
            expires_at: now + 600_000,
            status: "pending".into(),
        }
    }

    // ── Plugin tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_plugins_empty() {
        let (repo, _db) = setup().await;
        let plugins = repo.get_all_plugins(OWNER_A).await.unwrap();
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_get_plugin() {
        let (repo, _db) = setup().await;
        let plugin = sample_plugin();
        repo.upsert_plugin(OWNER_A, &plugin).await.unwrap();

        let found = repo.get_plugin(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.id, "tg-1");
        assert_eq!(found.r#type, "telegram");
        assert_eq!(found.name, "My Telegram Bot");
        assert!(!found.enabled);
    }

    #[tokio::test]
    async fn upsert_plugin_updates_existing() {
        let (repo, _db) = setup().await;
        let plugin = sample_plugin();
        repo.upsert_plugin(OWNER_A, &plugin).await.unwrap();

        let updated = ChannelPluginRow {
            name: "Updated Bot".into(),
            enabled: true,
            updated_at: aionui_common::now_ms(),
            ..plugin
        };
        repo.upsert_plugin(OWNER_A, &updated).await.unwrap();

        let found = repo.get_plugin(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Bot");
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn get_all_plugins_returns_multiple() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(OWNER_A, &sample_plugin()).await.unwrap();

        let now = aionui_common::now_ms();
        let lark = ChannelPluginRow {
            id: "lark-1".into(),
            owner_user_id: OWNER_A.into(),
            r#type: "lark".into(),
            name: "Lark Bot".into(),
            enabled: true,
            config: "{}".into(),
            status: Some("running".into()),
            last_connected: Some(now),
            created_at: now,
            updated_at: now,
        };
        repo.upsert_plugin(OWNER_A, &lark).await.unwrap();

        let all = repo.get_all_plugins(OWNER_A).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn plugins_are_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.upsert_plugin(OWNER_A, &sample_plugin()).await.unwrap();

        assert!(repo.get_plugin(OWNER_B, "tg-1").await.unwrap().is_none());
        assert!(repo.get_all_plugins(OWNER_B).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_plugin_id_can_exist_for_different_owners() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        let owner_a_plugin = sample_plugin();
        let owner_b_plugin = ChannelPluginRow {
            owner_user_id: OWNER_B.into(),
            name: "Owner B Telegram Bot".into(),
            enabled: true,
            ..sample_plugin()
        };

        repo.upsert_plugin(OWNER_A, &owner_a_plugin).await.unwrap();
        repo.upsert_plugin(OWNER_B, &owner_b_plugin).await.unwrap();

        let owner_a_found = repo.get_plugin(OWNER_A, "tg-1").await.unwrap().unwrap();
        let owner_b_found = repo.get_plugin(OWNER_B, "tg-1").await.unwrap().unwrap();
        assert_eq!(owner_a_found.id, "tg-1");
        assert_eq!(owner_b_found.id, "tg-1");
        assert_eq!(owner_a_found.name, "My Telegram Bot");
        assert_eq!(owner_b_found.name, "Owner B Telegram Bot");

        repo.update_plugin_status(
            OWNER_B,
            "tg-1",
            &UpdatePluginStatusParams {
                status: Some("running".into()),
                last_connected: None,
                enabled: None,
            },
        )
        .await
        .unwrap();

        let owner_a_after = repo.get_plugin(OWNER_A, "tg-1").await.unwrap().unwrap();
        let owner_b_after = repo.get_plugin(OWNER_B, "tg-1").await.unwrap().unwrap();
        assert_eq!(owner_a_after.status, None);
        assert_eq!(owner_b_after.status, Some("running".into()));
    }

    #[tokio::test]
    async fn update_plugin_status_sets_fields() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(OWNER_A, &sample_plugin()).await.unwrap();

        let now = aionui_common::now_ms();
        repo.update_plugin_status(
            OWNER_A,
            "tg-1",
            &UpdatePluginStatusParams {
                status: Some("running".into()),
                last_connected: Some(now),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();

        let found = repo.get_plugin(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.status.as_deref(), Some("running"));
        assert_eq!(found.last_connected, Some(now));
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn update_plugin_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_plugin_status(
                OWNER_A,
                "nope",
                &UpdatePluginStatusParams {
                    status: Some("error".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_plugin_status_empty_params_is_noop() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(OWNER_A, &sample_plugin()).await.unwrap();
        // No fields to update → no-op, no error.
        repo.update_plugin_status(OWNER_A, "tg-1", &UpdatePluginStatusParams::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_plugin_removes_row() {
        let (repo, _db) = setup().await;
        repo.upsert_plugin(OWNER_A, &sample_plugin()).await.unwrap();
        repo.delete_plugin(OWNER_A, "tg-1").await.unwrap();
        assert!(repo.get_plugin(OWNER_A, "tg-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_plugin_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete_plugin(OWNER_A, "nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    // ── User tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_users_empty() {
        let (repo, _db) = setup().await;
        let users = repo.get_all_users(OWNER_A).await.unwrap();
        assert!(users.is_empty());
    }

    #[tokio::test]
    async fn create_and_get_user_by_platform() {
        let (repo, _db) = setup().await;
        let user = sample_user();
        repo.create_user(OWNER_A, &user).await.unwrap();

        let found = repo
            .get_user_by_platform(OWNER_A, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "usr-1");
        assert_eq!(found.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn create_duplicate_user_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let dup = AssistantUserRow {
            id: "usr-2".into(),
            ..sample_user()
        };
        let err = repo.create_user(OWNER_A, &dup).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn platform_users_are_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.create_user(OWNER_A, &sample_user()).await.unwrap();
        let other = AssistantUserRow {
            id: "usr-2".into(),
            owner_user_id: OWNER_B.into(),
            platform_user_id: "tg_other".into(),
            ..sample_user()
        };
        repo.create_user(OWNER_B, &other).await.unwrap();

        let owner_a = repo
            .get_user_by_platform(OWNER_A, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        let owner_b = repo
            .get_user_by_platform(OWNER_B, "tg_other", "telegram")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(owner_a.id, "usr-1");
        assert_eq!(owner_b.id, "usr-2");
        assert!(
            repo.get_user_by_platform(OWNER_A, "tg_other", "telegram")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn same_platform_user_can_exist_for_different_owners() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.create_user(OWNER_A, &sample_user()).await.unwrap();
        let other_owner_user = AssistantUserRow {
            id: "usr-2".into(),
            owner_user_id: OWNER_B.into(),
            ..sample_user()
        };
        repo.create_user(OWNER_B, &other_owner_user).await.unwrap();

        let owner_a = repo
            .get_user_by_platform(OWNER_A, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        let owner_b = repo
            .get_user_by_platform(OWNER_B, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner_a.id, "usr-1");
        assert_eq!(owner_b.id, "usr-2");
    }

    #[tokio::test]
    async fn get_user_by_platform_not_found() {
        let (repo, _db) = setup().await;
        assert!(
            repo.get_user_by_platform(OWNER_A, "nope", "telegram")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_user_last_active_updates_timestamp() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new_ts = aionui_common::now_ms() + 5000;
        repo.update_user_last_active(OWNER_A, "usr-1", new_ts).await.unwrap();

        let found = repo
            .get_user_by_platform(OWNER_A, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.last_active, Some(new_ts));
    }

    #[tokio::test]
    async fn update_user_last_active_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_user_last_active(OWNER_A, "nope", 123).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_removes_row() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();
        repo.delete_user(OWNER_A, "usr-1").await.unwrap();
        assert!(
            repo.get_user_by_platform(OWNER_A, "tg_12345", "telegram")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_user_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete_user(OWNER_A, "nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_user_cascades_sessions() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let session = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &session)
            .await
            .unwrap();

        // Sessions exist before delete.
        assert_eq!(repo.get_all_sessions(OWNER_A).await.unwrap().len(), 1);

        repo.delete_user(OWNER_A, "usr-1").await.unwrap();

        // Sessions cascade-deleted.
        assert!(repo.get_all_sessions(OWNER_A).await.unwrap().is_empty());
    }

    // ── Session tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_sessions_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get_all_sessions(OWNER_A).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_or_create_session_creates_new() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        let result = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();
        assert_eq!(result.id, "sess-1");
        assert_eq!(result.user_id, "usr-1");
        assert_eq!(result.chat_id.as_deref(), Some("chat-abc"));
    }

    #[tokio::test]
    async fn get_or_create_session_reuses_existing() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        let first = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        // Second call with different new_row id should still return the first.
        let another = AssistantSessionRow {
            id: "sess-2".into(),
            ..new
        };
        let second = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &another)
            .await
            .unwrap();
        assert_eq!(second.id, first.id);
        // last_activity should be updated.
        assert!(second.last_activity >= first.last_activity);
    }

    #[tokio::test]
    async fn per_chat_isolation_different_chats() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-xyz", &s2)
            .await
            .unwrap();

        assert_eq!(repo.get_all_sessions(OWNER_A).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_session_by_id() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.agent_type, "gemini");
    }

    #[tokio::test]
    async fn get_session_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_session(OWNER_A, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_activity_updates_timestamp() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        let new_ts = aionui_common::now_ms() + 5000;
        repo.update_session_activity(OWNER_A, "sess-1", new_ts).await.unwrap();

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.last_activity, new_ts);
    }

    #[tokio::test]
    async fn update_session_activity_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_session_activity(OWNER_A, "nope", 123).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_sessions_by_user_removes_all() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-xyz", &s2)
            .await
            .unwrap();

        repo.delete_sessions_by_user(OWNER_A, "usr-1").await.unwrap();
        assert!(repo.get_all_sessions(OWNER_A).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_sessions_by_user_no_sessions_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist for this user — should not error.
        repo.delete_sessions_by_user(OWNER_A, "usr-1").await.unwrap();
    }

    /// Helper to create a stub conversation for FK-constrained tests.
    async fn create_stub_conversation(pool: &SqlitePool, user_id: &str, conv_id: &str) {
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Test Conv', 'chat', ?3, ?3)",
        )
        .bind(conv_id)
        .bind(user_id)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_session_conversation_persists() {
        let (repo, db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        create_stub_conversation(db.pool(), OWNER_A, "conv-42").await;

        repo.update_session_conversation(OWNER_A, "sess-1", "conv-42")
            .await
            .unwrap();

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.conversation_id.as_deref(), Some("conv-42"));
    }

    #[tokio::test]
    async fn update_session_conversation_rejects_cross_owner_conversation() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        create_stub_conversation(db.pool(), OWNER_B, "conv-other").await;

        let err = repo
            .update_session_conversation(OWNER_A, "sess-1", "conv-other")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DbError::Conflict(msg) if msg.starts_with("CROSS_ACCOUNT_REFERENCE:")
        ));
        assert!(
            repo.get_session(OWNER_A, "sess-1")
                .await
                .unwrap()
                .unwrap()
                .conversation_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_session_conversation_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_session_conversation(OWNER_A, "nope", "conv-1")
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_session_agent_type_persists() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        assert_eq!(
            repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap().agent_type,
            "gemini"
        );

        repo.update_session_agent_type(OWNER_A, "sess-1", "acp").await.unwrap();

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.agent_type, "acp");
    }

    #[tokio::test]
    async fn update_session_agent_type_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_session_agent_type(OWNER_A, "nope", "acp")
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_removes_only_target() {
        let (repo, _db) = setup().await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = AssistantSessionRow {
            id: "sess-2".into(),
            chat_id: Some("chat-xyz".into()),
            ..sample_session("usr-1")
        };
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-xyz", &s2)
            .await
            .unwrap();

        repo.delete_session_by_user_chat(OWNER_A, "usr-1", "chat-abc")
            .await
            .unwrap();

        let remaining = repo.get_all_sessions(OWNER_A).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chat_id.as_deref(), Some("chat-xyz"));
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_no_match_is_ok() {
        let (repo, _db) = setup().await;
        // No sessions exist — should not error.
        repo.delete_session_by_user_chat(OWNER_A, "usr-1", "chat-abc")
            .await
            .unwrap();
    }

    // ── Pairing tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_pairing() {
        let (repo, _db) = setup().await;
        let pairing = sample_pairing();
        repo.create_pairing(OWNER_A, &pairing).await.unwrap();

        let found = repo.get_pairing_by_code(OWNER_A, "123456").await.unwrap().unwrap();
        assert_eq!(found.platform_user_id, "tg_99");
        assert_eq!(found.status, "pending");
    }

    #[tokio::test]
    async fn create_duplicate_pairing_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();
        let err = repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn pairing_lookup_is_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        assert!(repo.get_pairing_by_code(OWNER_B, "123456").await.unwrap().is_none());
        assert!(repo.get_pending_pairings(OWNER_B).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_pairing_code_can_exist_for_different_owners() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();
        let owner_b_pairing = PairingCodeRow {
            owner_user_id: OWNER_B.into(),
            platform_user_id: "tg_owner_b".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_B, &owner_b_pairing).await.unwrap();

        let owner_a = repo.get_pairing_by_code(OWNER_A, "123456").await.unwrap().unwrap();
        let owner_b = repo.get_pairing_by_code(OWNER_B, "123456").await.unwrap().unwrap();
        assert_eq!(owner_a.platform_user_id, "tg_99");
        assert_eq!(owner_b.platform_user_id, "tg_owner_b");
    }

    #[tokio::test]
    async fn get_pending_pairings_filters_by_status() {
        let (repo, _db) = setup().await;
        let p1 = sample_pairing();
        repo.create_pairing(OWNER_A, &p1).await.unwrap();

        let p2 = PairingCodeRow {
            code: "654321".into(),
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &p2).await.unwrap();

        let pending = repo.get_pending_pairings(OWNER_A).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, "123456");
    }

    #[tokio::test]
    async fn get_pairing_by_code_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_pairing_by_code(OWNER_A, "000000").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_pairing_status_changes_status() {
        let (repo, _db) = setup().await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        repo.update_pairing_status(OWNER_A, "123456", "approved").await.unwrap();

        let found = repo.get_pairing_by_code(OWNER_A, "123456").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
    }

    #[tokio::test]
    async fn update_pairing_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_pairing_status(OWNER_A, "000000", "approved")
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_marks_expired() {
        let (repo, _db) = setup().await;
        let now = aionui_common::now_ms();

        // Create an already-expired pairing.
        let expired = PairingCodeRow {
            code: "111111".into(),
            expires_at: now - 1000,
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &expired).await.unwrap();

        // Create a still-valid pairing.
        let valid = PairingCodeRow {
            code: "222222".into(),
            expires_at: now + 600_000,
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &valid).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(OWNER_A, now).await.unwrap();
        assert_eq!(cleaned, 1);

        let found_expired = repo.get_pairing_by_code(OWNER_A, "111111").await.unwrap().unwrap();
        assert_eq!(found_expired.status, "expired");

        let found_valid = repo.get_pairing_by_code(OWNER_A, "222222").await.unwrap().unwrap();
        assert_eq!(found_valid.status, "pending");
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_skips_non_pending() {
        let (repo, _db) = setup().await;
        let now = aionui_common::now_ms();

        // Create an expired pairing that is already approved.
        let approved = PairingCodeRow {
            code: "333333".into(),
            expires_at: now - 1000,
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &approved).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(OWNER_A, now).await.unwrap();
        assert_eq!(cleaned, 0);

        let found = repo.get_pairing_by_code(OWNER_A, "333333").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
    }
}
