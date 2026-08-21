use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{ChannelConnectionRow, ChannelConversationBindingRow, ChannelPairingRequestRow, ChannelUserRow};
use crate::repository::channel::{IChannelRepository, UpdateConnectionStatusParams};

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
    // ── Connection CRUD ──────────────────────────────────────────────

    async fn get_all_connections(&self, owner_user_id: &str) -> Result<Vec<ChannelConnectionRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelConnectionRow>(
            "SELECT * FROM channel_connections WHERE owner_user_id = ? ORDER BY created_at ASC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_connection(&self, owner_user_id: &str, id: &str) -> Result<Option<ChannelConnectionRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelConnectionRow>(
            "SELECT * FROM channel_connections WHERE owner_user_id = ? AND id = ?",
        )
        .bind(owner_user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_connection_by_plugin_key(
        &self,
        owner_user_id: &str,
        plugin_key: &str,
    ) -> Result<Option<ChannelConnectionRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelConnectionRow>(
            "SELECT * FROM channel_connections WHERE owner_user_id = ? AND plugin_key = ?",
        )
        .bind(owner_user_id)
        .bind(plugin_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert_connection(&self, owner_user_id: &str, row: &ChannelConnectionRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO channel_connections \
                (id, owner_user_id, plugin_key, name, enabled, config, status, last_connected, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(owner_user_id, id) DO UPDATE SET \
                plugin_key = excluded.plugin_key, \
                name = excluded.name, \
                enabled = excluded.enabled, \
                config = excluded.config, \
                status = excluded.status, \
                last_connected = excluded.last_connected, \
                updated_at = excluded.updated_at",
        )
        .bind(&row.id)
        .bind(owner_user_id)
        .bind(&row.plugin_key)
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

    async fn update_connection_status(
        &self,
        owner_user_id: &str,
        id: &str,
        params: &UpdateConnectionStatusParams,
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
            "UPDATE channel_connections SET {} WHERE owner_user_id = ? AND id = ?",
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
            return Err(DbError::NotFound(format!("Connection '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_connection(&self, owner_user_id: &str, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM channel_connections WHERE owner_user_id = ? AND id = ?")
            .bind(owner_user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Connection '{id}' not found")));
        }
        Ok(())
    }

    // ── Channel user CRUD ────────────────────────────────────────────

    async fn get_all_users(&self, owner_user_id: &str) -> Result<Vec<ChannelUserRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelUserRow>(
            "SELECT u.*, c.plugin_key AS platform_type \
             FROM channel_users u \
             JOIN channel_connections c ON c.owner_user_id = u.owner_user_id AND c.id = u.connection_id \
             WHERE u.owner_user_id = ? AND u.status = 'active' \
             ORDER BY u.authorized_at DESC",
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
    ) -> Result<Option<ChannelUserRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelUserRow>(
            "SELECT u.*, c.plugin_key AS platform_type \
             FROM channel_users u \
             JOIN channel_connections c ON c.owner_user_id = u.owner_user_id AND c.id = u.connection_id \
             WHERE u.owner_user_id = ? AND u.external_user_id = ? AND c.plugin_key = ? \
               AND u.status = 'active'",
        )
        .bind(owner_user_id)
        .bind(platform_user_id)
        .bind(platform_type)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create_user(&self, owner_user_id: &str, row: &ChannelUserRow) -> Result<(), DbError> {
        // Reactivate a previously revoked row for the same identity;
        // an already-active row is a conflict.
        let mut tx = self.pool.begin().await?;
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT id, status FROM channel_users \
             WHERE owner_user_id = ? AND connection_id = ? AND external_user_id = ?",
        )
        .bind(owner_user_id)
        .bind(&row.connection_id)
        .bind(&row.platform_user_id)
        .fetch_optional(&mut *tx)
        .await?;

        match existing {
            Some((_, status)) if status == "active" => {
                return Err(DbError::Conflict(format!(
                    "User '{}' on connection '{}' already exists",
                    row.platform_user_id, row.connection_id
                )));
            }
            Some((existing_id, _)) => {
                sqlx::query(
                    "UPDATE channel_users \
                     SET status = 'active', revoked_at = NULL, display_name = ?, \
                         authorized_at = ?, last_active = ? \
                     WHERE owner_user_id = ? AND id = ?",
                )
                .bind(&row.display_name)
                .bind(row.authorized_at)
                .bind(row.last_active)
                .bind(owner_user_id)
                .bind(&existing_id)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO channel_users \
                        (id, owner_user_id, connection_id, external_user_id, display_name, \
                         status, revoked_at, authorized_at, last_active) \
                     VALUES (?, ?, ?, ?, ?, 'active', NULL, ?, ?)",
                )
                .bind(&row.id)
                .bind(owner_user_id)
                .bind(&row.connection_id)
                .bind(&row.platform_user_id)
                .bind(&row.display_name)
                .bind(row.authorized_at)
                .bind(row.last_active)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    if is_unique_violation(&e) {
                        DbError::Conflict(format!(
                            "User '{}' on connection '{}' already exists",
                            row.platform_user_id, row.connection_id
                        ))
                    } else {
                        DbError::Query(e)
                    }
                })?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn update_user_last_active(
        &self,
        owner_user_id: &str,
        id: &str,
        last_active: aionui_common::TimestampMs,
    ) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE channel_users SET last_active = ? WHERE owner_user_id = ? AND id = ?")
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

    async fn revoke_user(&self, owner_user_id: &str, id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE channel_users SET status = 'revoked', revoked_at = ? \
             WHERE owner_user_id = ? AND id = ? AND status = 'active'",
        )
        .bind(aionui_common::now_ms())
        .bind(owner_user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{id}' not found")));
        }
        // Soft delete keeps the audit row, so sessions no longer cascade —
        // remove them explicitly to stop message routing for this user. The
        // owner predicate is defense in depth: the UPDATE above already
        // proved ownership within this transaction.
        sqlx::query("DELETE FROM channel_conversation_bindings WHERE owner_user_id = ? AND channel_user_id = ?")
            .bind(owner_user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ── Conversation binding CRUD ────────────────────────────────────

    async fn get_all_sessions(&self, owner_user_id: &str) -> Result<Vec<ChannelConversationBindingRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelConversationBindingRow>(
            "SELECT * FROM channel_conversation_bindings \
             WHERE owner_user_id = ? \
             ORDER BY last_active_at DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_session(
        &self,
        owner_user_id: &str,
        id: &str,
    ) -> Result<Option<ChannelConversationBindingRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelConversationBindingRow>(
            "SELECT * FROM channel_conversation_bindings WHERE owner_user_id = ? AND id = ?",
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
        new_row: &ChannelConversationBindingRow,
    ) -> Result<ChannelConversationBindingRow, DbError> {
        // Try to find an existing binding first.
        let existing = sqlx::query_as::<_, ChannelConversationBindingRow>(
            "SELECT * FROM channel_conversation_bindings \
             WHERE owner_user_id = ? AND channel_user_id = ? AND external_chat_id = ?",
        )
        .bind(owner_user_id)
        .bind(channel_user_id)
        .bind(chat_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            // Touch last_active_at.
            let now = aionui_common::now_ms();
            sqlx::query("UPDATE channel_conversation_bindings SET last_active_at = ? WHERE id = ?")
                .bind(now)
                .bind(&row.id)
                .execute(&self.pool)
                .await?;

            return Ok(ChannelConversationBindingRow {
                last_activity: now,
                ..row
            });
        }

        // Insert a new binding. The owner/connection columns derive from the
        // ACTIVE channel user row, so a foreign or revoked channel user makes
        // the INSERT match zero rows. A conversation owned by another Core
        // user is rejected by the cross-account trigger; the INSERT-side
        // EXISTS keeps that failure a clean zero-row no-op instead of an
        // opaque trigger abort for the common caller path.
        sqlx::query(
            "INSERT INTO channel_conversation_bindings \
                (id, owner_user_id, connection_id, channel_user_id, external_chat_id, \
                 conversation_id, created_at, last_active_at) \
             SELECT ?, u.owner_user_id, u.connection_id, u.id, ?, ?, ?, ? \
             FROM channel_users u \
             WHERE u.owner_user_id = ? AND u.id = ? AND u.status = 'active' \
               AND (
                   ? IS NULL OR EXISTS (
                       SELECT 1 FROM conversations WHERE id = ? AND user_id = ?
                   )
               )",
        )
        .bind(&new_row.id)
        .bind(&new_row.chat_id)
        .bind(&new_row.conversation_id)
        .bind(new_row.created_at)
        .bind(new_row.last_activity)
        .bind(owner_user_id)
        .bind(channel_user_id)
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
            "UPDATE channel_conversation_bindings SET last_active_at = ? \
             WHERE owner_user_id = ? AND id = ?",
        )
        .bind(last_activity)
        .bind(owner_user_id)
        .bind(id)
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
            "UPDATE channel_conversation_bindings \
             SET conversation_id = ?, last_active_at = ? \
             WHERE owner_user_id = ? AND id = ? \
               AND EXISTS (
                   SELECT 1 FROM conversations c
                   WHERE c.id = ? AND c.user_id = ?
               )",
        )
        .bind(conversation_id)
        .bind(now)
        .bind(owner_user_id)
        .bind(id)
        .bind(conversation_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let session_exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM channel_conversation_bindings \
                 WHERE owner_user_id = ? AND id = ?",
            )
            .bind(owner_user_id)
            .bind(id)
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

    async fn delete_sessions_by_user(&self, owner_user_id: &str, channel_user_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM channel_conversation_bindings WHERE owner_user_id = ? AND channel_user_id = ?")
            .bind(owner_user_id)
            .bind(channel_user_id)
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
            "DELETE FROM channel_conversation_bindings \
             WHERE owner_user_id = ? AND channel_user_id = ? AND external_chat_id = ?",
        )
        .bind(owner_user_id)
        .bind(channel_user_id)
        .bind(chat_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Pairing requests ─────────────────────────────────────────────

    async fn create_pairing(&self, owner_user_id: &str, row: &ChannelPairingRequestRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO channel_pairing_requests \
                (id, owner_user_id, connection_id, external_user_id, display_name, \
                 code_hash, status, requested_at, expires_at, approved_channel_user_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(owner_user_id)
        .bind(&row.connection_id)
        .bind(&row.platform_user_id)
        .bind(&row.display_name)
        .bind(&row.code_hash)
        .bind(&row.status)
        .bind(row.requested_at)
        .bind(row.expires_at)
        .bind(&row.approved_channel_user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                DbError::Conflict("A pending pairing request already exists for this user or code".into())
            } else {
                DbError::Query(e)
            }
        })?;
        Ok(())
    }

    async fn get_pending_pairings(&self, owner_user_id: &str) -> Result<Vec<ChannelPairingRequestRow>, DbError> {
        let rows = sqlx::query_as::<_, ChannelPairingRequestRow>(
            "SELECT p.*, c.plugin_key AS platform_type \
             FROM channel_pairing_requests p \
             JOIN channel_connections c ON c.owner_user_id = p.owner_user_id AND c.id = p.connection_id \
             WHERE p.owner_user_id = ? AND p.status = 'pending' \
             ORDER BY p.requested_at DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_pairing(&self, owner_user_id: &str, id: &str) -> Result<Option<ChannelPairingRequestRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelPairingRequestRow>(
            "SELECT p.*, c.plugin_key AS platform_type \
             FROM channel_pairing_requests p \
             JOIN channel_connections c ON c.owner_user_id = p.owner_user_id AND c.id = p.connection_id \
             WHERE p.owner_user_id = ? AND p.id = ?",
        )
        .bind(owner_user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_pending_pairing_by_code_hash(
        &self,
        owner_user_id: &str,
        code_hash: &str,
    ) -> Result<Option<ChannelPairingRequestRow>, DbError> {
        let row = sqlx::query_as::<_, ChannelPairingRequestRow>(
            "SELECT p.*, c.plugin_key AS platform_type \
             FROM channel_pairing_requests p \
             JOIN channel_connections c ON c.owner_user_id = p.owner_user_id AND c.id = p.connection_id \
             WHERE p.owner_user_id = ? AND p.code_hash = ? AND p.status = 'pending'",
        )
        .bind(owner_user_id)
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn update_pairing_status(
        &self,
        owner_user_id: &str,
        id: &str,
        status: &str,
        approved_channel_user_id: Option<&str>,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE channel_pairing_requests \
             SET status = ?, approved_channel_user_id = COALESCE(?, approved_channel_user_id) \
             WHERE owner_user_id = ? AND id = ?",
        )
        .bind(status)
        .bind(approved_channel_user_id)
        .bind(owner_user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Pairing request '{id}' not found")));
        }
        Ok(())
    }

    async fn expire_pending_pairings_for_user(
        &self,
        owner_user_id: &str,
        connection_id: &str,
        external_user_id: &str,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE channel_pairing_requests \
             SET status = 'expired' \
             WHERE owner_user_id = ? AND connection_id = ? AND external_user_id = ? \
               AND status = 'pending'",
        )
        .bind(owner_user_id)
        .bind(connection_id)
        .bind(external_user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn cleanup_expired_pairings(
        &self,
        owner_user_id: &str,
        now: aionui_common::TimestampMs,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE channel_pairing_requests \
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

    fn sample_connection() -> ChannelConnectionRow {
        let now = aionui_common::now_ms();
        ChannelConnectionRow {
            id: "tg-1".into(),
            owner_user_id: OWNER_A.into(),
            plugin_key: "telegram".into(),
            name: "My Telegram Bot".into(),
            enabled: false,
            config: r#"{"credentials":{"token":"enc_xxx"}}"#.into(),
            status: None,
            last_connected: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Seeds the connection row users/pairings attach to (FK parent).
    /// Connection identity is per-owner, so the same id is seeded per owner.
    async fn seed_connection(repo: &SqliteChannelRepository, owner: &str) {
        repo.upsert_connection(
            owner,
            &ChannelConnectionRow {
                owner_user_id: owner.into(),
                ..sample_connection()
            },
        )
        .await
        .unwrap();
    }

    fn sample_user() -> ChannelUserRow {
        let now = aionui_common::now_ms();
        ChannelUserRow {
            id: "usr-1".into(),
            owner_user_id: OWNER_A.into(),
            connection_id: "tg-1".into(),
            platform_user_id: "tg_12345".into(),
            platform_type: "telegram".into(),
            display_name: Some("Alice".into()),
            status: "active".into(),
            revoked_at: None,
            authorized_at: now,
            last_active: None,
        }
    }

    /// Seeds the FK-parent connection and authorizes the sample user on it.
    async fn seed_user(repo: &SqliteChannelRepository) {
        seed_connection(repo, OWNER_A).await;
        repo.create_user(OWNER_A, &sample_user()).await.unwrap();
    }

    /// `owner_user_id`/`connection_id` are left empty on purpose: the INSERT
    /// derives both from the active `channel_users` row, so a caller-supplied
    /// value is never trusted.
    fn sample_session(user_id: &str) -> ChannelConversationBindingRow {
        let now = aionui_common::now_ms();
        ChannelConversationBindingRow {
            id: "sess-1".into(),
            owner_user_id: String::new(),
            connection_id: String::new(),
            user_id: user_id.into(),
            chat_id: Some("chat-abc".into()),
            conversation_id: None,
            created_at: now,
            last_activity: now,
        }
    }

    fn sample_pairing() -> ChannelPairingRequestRow {
        let now = aionui_common::now_ms();
        ChannelPairingRequestRow {
            id: "pair-1".into(),
            owner_user_id: OWNER_A.into(),
            connection_id: "tg-1".into(),
            platform_user_id: "tg_99".into(),
            platform_type: "telegram".into(),
            display_name: Some("Bob".into()),
            code_hash: "hash-123456".into(),
            status: "pending".into(),
            requested_at: now,
            expires_at: now + 600_000,
            approved_channel_user_id: None,
        }
    }

    // ── Plugin tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_all_connections_empty() {
        let (repo, _db) = setup().await;
        let plugins = repo.get_all_connections(OWNER_A).await.unwrap();
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_get_plugin() {
        let (repo, _db) = setup().await;
        let plugin = sample_connection();
        repo.upsert_connection(OWNER_A, &plugin).await.unwrap();

        let found = repo.get_connection(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.id, "tg-1");
        assert_eq!(found.plugin_key, "telegram");
        assert_eq!(found.name, "My Telegram Bot");
        assert!(!found.enabled);
    }

    #[tokio::test]
    async fn upsert_plugin_updates_existing() {
        let (repo, _db) = setup().await;
        let plugin = sample_connection();
        repo.upsert_connection(OWNER_A, &plugin).await.unwrap();

        let updated = ChannelConnectionRow {
            name: "Updated Bot".into(),
            enabled: true,
            updated_at: aionui_common::now_ms(),
            ..plugin
        };
        repo.upsert_connection(OWNER_A, &updated).await.unwrap();

        let found = repo.get_connection(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Bot");
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn get_all_connections_returns_multiple() {
        let (repo, _db) = setup().await;
        repo.upsert_connection(OWNER_A, &sample_connection()).await.unwrap();

        let now = aionui_common::now_ms();
        let lark = ChannelConnectionRow {
            id: "lark-1".into(),
            owner_user_id: OWNER_A.into(),
            plugin_key: "lark".into(),
            name: "Lark Bot".into(),
            enabled: true,
            config: "{}".into(),
            status: Some("running".into()),
            last_connected: Some(now),
            created_at: now,
            updated_at: now,
        };
        repo.upsert_connection(OWNER_A, &lark).await.unwrap();

        let all = repo.get_all_connections(OWNER_A).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn plugins_are_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        repo.upsert_connection(OWNER_A, &sample_connection()).await.unwrap();

        assert!(repo.get_connection(OWNER_B, "tg-1").await.unwrap().is_none());
        assert!(repo.get_all_connections(OWNER_B).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_plugin_id_can_exist_for_different_owners() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        let owner_a_plugin = sample_connection();
        let owner_b_plugin = ChannelConnectionRow {
            owner_user_id: OWNER_B.into(),
            name: "Owner B Telegram Bot".into(),
            enabled: true,
            ..sample_connection()
        };

        repo.upsert_connection(OWNER_A, &owner_a_plugin).await.unwrap();
        repo.upsert_connection(OWNER_B, &owner_b_plugin).await.unwrap();

        let owner_a_found = repo.get_connection(OWNER_A, "tg-1").await.unwrap().unwrap();
        let owner_b_found = repo.get_connection(OWNER_B, "tg-1").await.unwrap().unwrap();
        assert_eq!(owner_a_found.id, "tg-1");
        assert_eq!(owner_b_found.id, "tg-1");
        assert_eq!(owner_a_found.name, "My Telegram Bot");
        assert_eq!(owner_b_found.name, "Owner B Telegram Bot");

        repo.update_connection_status(
            OWNER_B,
            "tg-1",
            &UpdateConnectionStatusParams {
                status: Some("running".into()),
                last_connected: None,
                enabled: None,
            },
        )
        .await
        .unwrap();

        let owner_a_after = repo.get_connection(OWNER_A, "tg-1").await.unwrap().unwrap();
        let owner_b_after = repo.get_connection(OWNER_B, "tg-1").await.unwrap().unwrap();
        assert_eq!(owner_a_after.status, None);
        assert_eq!(owner_b_after.status, Some("running".into()));
    }

    #[tokio::test]
    async fn update_connection_status_sets_fields() {
        let (repo, _db) = setup().await;
        repo.upsert_connection(OWNER_A, &sample_connection()).await.unwrap();

        let now = aionui_common::now_ms();
        repo.update_connection_status(
            OWNER_A,
            "tg-1",
            &UpdateConnectionStatusParams {
                status: Some("running".into()),
                last_connected: Some(now),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();

        let found = repo.get_connection(OWNER_A, "tg-1").await.unwrap().unwrap();
        assert_eq!(found.status.as_deref(), Some("running"));
        assert_eq!(found.last_connected, Some(now));
        assert!(found.enabled);
    }

    #[tokio::test]
    async fn update_connection_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_connection_status(
                OWNER_A,
                "nope",
                &UpdateConnectionStatusParams {
                    status: Some("error".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_connection_status_empty_params_is_noop() {
        let (repo, _db) = setup().await;
        repo.upsert_connection(OWNER_A, &sample_connection()).await.unwrap();
        // No fields to update → no-op, no error.
        repo.update_connection_status(OWNER_A, "tg-1", &UpdateConnectionStatusParams::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_plugin_removes_row() {
        let (repo, _db) = setup().await;
        repo.upsert_connection(OWNER_A, &sample_connection()).await.unwrap();
        repo.delete_connection(OWNER_A, "tg-1").await.unwrap();
        assert!(repo.get_connection(OWNER_A, "tg-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_plugin_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete_connection(OWNER_A, "nope").await.unwrap_err();
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
        seed_connection(&repo, OWNER_A).await;
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
        seed_user(&repo).await;

        let dup = ChannelUserRow {
            id: "usr-2".into(),
            ..sample_user()
        };
        let err = repo.create_user(OWNER_A, &dup).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn create_user_reactivates_revoked_row() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;
        repo.revoke_user(OWNER_A, "usr-1").await.unwrap();

        // Re-authorizing the same identity reuses the revoked audit row
        // instead of inserting a second one.
        let again = ChannelUserRow {
            id: "usr-2".into(),
            display_name: Some("Alice Again".into()),
            ..sample_user()
        };
        repo.create_user(OWNER_A, &again).await.unwrap();

        let found = repo
            .get_user_by_platform(OWNER_A, "tg_12345", "telegram")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "usr-1");
        assert_eq!(found.status, "active");
        assert_eq!(found.revoked_at, None);
        assert_eq!(found.display_name.as_deref(), Some("Alice Again"));

        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM channel_users WHERE owner_user_id = ? AND external_user_id = ?")
                .bind(OWNER_A)
                .bind("tg_12345")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(row_count, 1);
    }

    #[tokio::test]
    async fn platform_users_are_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;

        seed_user(&repo).await;
        seed_connection(&repo, OWNER_B).await;
        let other = ChannelUserRow {
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

        seed_user(&repo).await;
        seed_connection(&repo, OWNER_B).await;
        let other_owner_user = ChannelUserRow {
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
        seed_user(&repo).await;

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
    async fn revoke_user_hides_user_but_keeps_audit_row() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;
        repo.revoke_user(OWNER_A, "usr-1").await.unwrap();

        // Revoked users disappear from every read path.
        assert!(
            repo.get_user_by_platform(OWNER_A, "tg_12345", "telegram")
                .await
                .unwrap()
                .is_none()
        );
        assert!(repo.get_all_users(OWNER_A).await.unwrap().is_empty());

        // The authorization history survives as an audit row.
        let (status, revoked_at): (String, Option<i64>) =
            sqlx::query_as("SELECT status, revoked_at FROM channel_users WHERE id = ?")
                .bind("usr-1")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(status, "revoked");
        assert!(revoked_at.is_some());
    }

    #[tokio::test]
    async fn revoke_user_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.revoke_user(OWNER_A, "nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn revoke_user_twice_is_not_found() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;
        repo.revoke_user(OWNER_A, "usr-1").await.unwrap();

        // Only an ACTIVE row can be revoked.
        let err = repo.revoke_user(OWNER_A, "usr-1").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn revoke_user_deletes_sessions() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;

        let session = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &session)
            .await
            .unwrap();

        // Sessions exist before revocation.
        assert_eq!(repo.get_all_sessions(OWNER_A).await.unwrap().len(), 1);

        repo.revoke_user(OWNER_A, "usr-1").await.unwrap();

        // Soft delete keeps the user row, but message routing stops: the
        // sessions are removed outright, not merely hidden behind the join.
        assert!(repo.get_all_sessions(OWNER_A).await.unwrap().is_empty());
        let session_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM channel_conversation_bindings WHERE channel_user_id = ?")
                .bind("usr-1")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(session_count, 0);
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
        seed_user(&repo).await;

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
        seed_user(&repo).await;

        let new = sample_session("usr-1");
        let first = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        // Second call with different new_row id should still return the first.
        let another = ChannelConversationBindingRow {
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
        seed_user(&repo).await;

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = ChannelConversationBindingRow {
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
        seed_user(&repo).await;

        let new = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.user_id, "usr-1");
        assert_eq!(found.chat_id.as_deref(), Some("chat-abc"));
        // Owner and connection are derived from the channel user, not taken
        // from the caller-supplied row (which left both empty).
        assert_eq!(found.owner_user_id, OWNER_A);
        assert_eq!(found.connection_id, "tg-1");
    }

    #[tokio::test]
    async fn get_session_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_session(OWNER_A, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_session_activity_updates_timestamp() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;

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
        seed_user(&repo).await;

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = ChannelConversationBindingRow {
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
        seed_user(&repo).await;

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
        seed_user(&repo).await;

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

    /// Replaces the pre-A3 `update_session_agent_type` coverage: agent
    /// configuration is no longer a binding column, so what the binding must
    /// now guarantee is that its owner/connection identity comes from the
    /// channel user rather than from the caller.
    #[tokio::test]
    async fn get_or_create_session_derives_owner_and_connection_ignoring_caller_values() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;

        // A caller that lies about owner/connection must not be believed.
        let new = ChannelConversationBindingRow {
            owner_user_id: "attacker".into(),
            connection_id: "forged-connection".into(),
            ..sample_session("usr-1")
        };
        let created = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &new)
            .await
            .unwrap();
        assert_eq!(created.owner_user_id, OWNER_A);
        assert_eq!(created.connection_id, "tg-1");

        let found = repo.get_session(OWNER_A, "sess-1").await.unwrap().unwrap();
        assert_eq!(found.owner_user_id, OWNER_A);
        assert_eq!(found.connection_id, "tg-1");
    }

    /// A revoked channel user is no longer routable: the derive-side INSERT
    /// filters on `status = 'active'`, so no new binding can be created.
    #[tokio::test]
    async fn get_or_create_session_rejects_revoked_channel_user() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;

        repo.revoke_user(OWNER_A, "usr-1").await.unwrap();

        let err = repo
            .get_or_create_session(OWNER_A, "usr-1", "chat-abc", &sample_session("usr-1"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, DbError::NotFound(_)),
            "revoked user must not get a binding, got: {err:?}"
        );
        assert!(repo.get_all_sessions(OWNER_A).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_session_by_user_chat_removes_only_target() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;

        let s1 = sample_session("usr-1");
        repo.get_or_create_session(OWNER_A, "usr-1", "chat-abc", &s1)
            .await
            .unwrap();

        let s2 = ChannelConversationBindingRow {
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
        seed_connection(&repo, OWNER_A).await;
        let pairing = sample_pairing();
        repo.create_pairing(OWNER_A, &pairing).await.unwrap();

        // Addressable by surrogate id …
        let found = repo.get_pairing(OWNER_A, "pair-1").await.unwrap().unwrap();
        assert_eq!(found.platform_user_id, "tg_99");
        assert_eq!(found.status, "pending");
        // … and platform_type is derived from the joined connection.
        assert_eq!(found.platform_type, "telegram");

        // … and by code hash, while pending.
        let by_hash = repo
            .get_pending_pairing_by_code_hash(OWNER_A, "hash-123456")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_hash.id, "pair-1");
    }

    /// The plaintext code must never reach the database: only `code_hash`
    /// is stored, and the table has no column that could hold the code.
    #[tokio::test]
    async fn pairing_stores_only_the_code_hash() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('channel_pairing_requests')")
            .fetch_all(&repo.pool)
            .await
            .unwrap();
        assert!(
            !columns.iter().any(|c| c == "code"),
            "pairing table must not carry a plaintext code column: {columns:?}"
        );

        let stored: String = sqlx::query_scalar("SELECT code_hash FROM channel_pairing_requests WHERE id = ?")
            .bind("pair-1")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        assert_eq!(stored, "hash-123456");
        assert_ne!(stored, "123456");
    }

    #[tokio::test]
    async fn create_duplicate_pairing_returns_conflict() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();
        let second = ChannelPairingRequestRow {
            id: "pair-2".into(),
            ..sample_pairing()
        };
        // One pending request per (owner, connection, external user).
        let err = repo.create_pairing(OWNER_A, &second).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn pairing_lookup_is_filtered_by_owner() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;
        seed_connection(&repo, OWNER_A).await;

        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        assert!(repo.get_pairing(OWNER_B, "pair-1").await.unwrap().is_none());
        assert!(
            repo.get_pending_pairing_by_code_hash(OWNER_B, "hash-123456")
                .await
                .unwrap()
                .is_none()
        );
        assert!(repo.get_pending_pairings(OWNER_B).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_code_hash_can_exist_for_different_owners() {
        let (repo, db) = setup().await;
        create_owner(db.pool(), OWNER_B).await;
        seed_connection(&repo, OWNER_A).await;
        seed_connection(&repo, OWNER_B).await;

        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();
        let owner_b_pairing = ChannelPairingRequestRow {
            id: "pair-2".into(),
            owner_user_id: OWNER_B.into(),
            platform_user_id: "tg_owner_b".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_B, &owner_b_pairing).await.unwrap();

        let owner_a = repo
            .get_pending_pairing_by_code_hash(OWNER_A, "hash-123456")
            .await
            .unwrap()
            .unwrap();
        let owner_b = repo
            .get_pending_pairing_by_code_hash(OWNER_B, "hash-123456")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner_a.platform_user_id, "tg_99");
        assert_eq!(owner_b.platform_user_id, "tg_owner_b");
    }

    #[tokio::test]
    async fn get_pending_pairings_filters_by_status() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        let p1 = sample_pairing();
        repo.create_pairing(OWNER_A, &p1).await.unwrap();

        let p2 = ChannelPairingRequestRow {
            id: "pair-2".into(),
            platform_user_id: "tg_100".into(),
            code_hash: "hash-654321".into(),
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &p2).await.unwrap();

        let pending = repo.get_pending_pairings(OWNER_A).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "pair-1");
        assert_eq!(pending[0].code_hash, "hash-123456");
    }

    #[tokio::test]
    async fn pairing_lookups_not_found() {
        let (repo, _db) = setup().await;
        assert!(repo.get_pairing(OWNER_A, "nope").await.unwrap().is_none());
        assert!(
            repo.get_pending_pairing_by_code_hash(OWNER_A, "hash-000000")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A non-pending request is invisible to the code-hash lookup, so an
    /// already-used code cannot be replayed.
    #[tokio::test]
    async fn code_hash_lookup_ignores_non_pending() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();
        repo.update_pairing_status(OWNER_A, "pair-1", "rejected", None)
            .await
            .unwrap();

        assert!(
            repo.get_pending_pairing_by_code_hash(OWNER_A, "hash-123456")
                .await
                .unwrap()
                .is_none()
        );
        // The row itself is still addressable by id.
        assert_eq!(
            repo.get_pairing(OWNER_A, "pair-1").await.unwrap().unwrap().status,
            "rejected"
        );
    }

    #[tokio::test]
    async fn update_pairing_status_records_approved_user() {
        let (repo, _db) = setup().await;
        seed_user(&repo).await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        repo.update_pairing_status(OWNER_A, "pair-1", "approved", Some("usr-1"))
            .await
            .unwrap();

        let found = repo.get_pairing(OWNER_A, "pair-1").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
        assert_eq!(found.approved_channel_user_id.as_deref(), Some("usr-1"));
    }

    #[tokio::test]
    async fn update_pairing_status_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_pairing_status(OWNER_A, "nope", "approved", None)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn expire_pending_pairings_for_user_targets_one_user() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        repo.create_pairing(OWNER_A, &sample_pairing()).await.unwrap();

        let other = ChannelPairingRequestRow {
            id: "pair-2".into(),
            platform_user_id: "tg_100".into(),
            code_hash: "hash-654321".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &other).await.unwrap();

        let expired = repo
            .expire_pending_pairings_for_user(OWNER_A, "tg-1", "tg_99")
            .await
            .unwrap();
        assert_eq!(expired, 1);

        assert_eq!(
            repo.get_pairing(OWNER_A, "pair-1").await.unwrap().unwrap().status,
            "expired"
        );
        assert_eq!(
            repo.get_pairing(OWNER_A, "pair-2").await.unwrap().unwrap().status,
            "pending"
        );
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_marks_expired() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        let now = aionui_common::now_ms();

        // Create an already-expired pairing.
        let expired = ChannelPairingRequestRow {
            id: "pair-expired".into(),
            platform_user_id: "tg_expired".into(),
            code_hash: "hash-111111".into(),
            expires_at: now - 1000,
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &expired).await.unwrap();

        // Create a still-valid pairing.
        let valid = ChannelPairingRequestRow {
            id: "pair-valid".into(),
            platform_user_id: "tg_valid".into(),
            code_hash: "hash-222222".into(),
            expires_at: now + 600_000,
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &valid).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(OWNER_A, now).await.unwrap();
        assert_eq!(cleaned, 1);

        let found_expired = repo.get_pairing(OWNER_A, "pair-expired").await.unwrap().unwrap();
        assert_eq!(found_expired.status, "expired");

        let found_valid = repo.get_pairing(OWNER_A, "pair-valid").await.unwrap().unwrap();
        assert_eq!(found_valid.status, "pending");
    }

    #[tokio::test]
    async fn cleanup_expired_pairings_skips_non_pending() {
        let (repo, _db) = setup().await;
        seed_connection(&repo, OWNER_A).await;
        let now = aionui_common::now_ms();

        // Create an expired pairing that is already approved.
        let approved = ChannelPairingRequestRow {
            id: "pair-approved".into(),
            code_hash: "hash-333333".into(),
            expires_at: now - 1000,
            status: "approved".into(),
            ..sample_pairing()
        };
        repo.create_pairing(OWNER_A, &approved).await.unwrap();

        let cleaned = repo.cleanup_expired_pairings(OWNER_A, now).await.unwrap();
        assert_eq!(cleaned, 0);

        let found = repo.get_pairing(OWNER_A, "pair-approved").await.unwrap().unwrap();
        assert_eq!(found.status, "approved");
    }
}
