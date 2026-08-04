use sqlx::{Row, SqlitePool};

use aionui_common::PaginatedResult;

use crate::error::DbError;
use crate::models::{
    ConversationArtifactRow, ConversationAssistantSnapshotRow, ConversationRow, MessageRow,
    UpsertConversationAssistantSnapshotParams,
};
use crate::repository::conversation::{
    ConversationFilters, ConversationRowUpdate, IConversationRepository, MessagePageCursor, MessagePageDirection,
    MessagePageParams, MessagePageResult, MessageRowUpdate, MessageSearchRow, StaleRuntimeMessageRow,
};

/// Bump `conversations.updated_at` so the conversation-list sort
/// (ORDER BY conversations.updated_at DESC) floats a conversation with fresh
/// activity to the top. Persisting a message never used to touch this column,
/// so a conversation receiving new messages stayed frozen at its last
/// create/rename/reset time. `MAX(updated_at, ?)` keeps recency monotonic: an
/// out-of-order streaming upsert (older event time) can never move a
/// conversation backward in the list. Runs inside the caller's transaction so
/// the message write and the bump commit atomically.
/// SQLite-backed implementation of [`IConversationRepository`].
#[derive(Clone, Debug)]
pub struct SqliteConversationRepository {
    pool: SqlitePool,
}

impl SqliteConversationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn conversation_exists_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE user_id = ? AND id = ?)")
            .bind(user_id)
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await?;

        Ok(exists != 0)
    }

    async fn ensure_conversation_for_user(&self, user_id: &str, conversation_id: &str) -> Result<(), DbError> {
        if self.conversation_exists_for_user(user_id, conversation_id).await? {
            Ok(())
        } else {
            Err(DbError::NotFound(format!("Conversation '{conversation_id}' not found")))
        }
    }

    async fn insert_message_once(&self, user_id: &str, message: &MessageRow) -> Result<(), DbError> {
        // BEGIN IMMEDIATE claims the writer lock up front (same pattern as
        // `claim_run`) so concurrent inserters queue on SQLite's busy handler
        // instead of a read-then-write transaction failing with "database is
        // locked" when it tries to upgrade to the write lock.
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result: Result<(), DbError> = async {
            // Ownership check inside the same transaction as the insert +
            // bump, so parent-chain authorization and the write are atomic.
            let exists: i64 =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE user_id = ? AND id = ?)")
                    .bind(user_id)
                    .bind(&message.conversation_id)
                    .fetch_one(&mut *connection)
                    .await?;
            if exists == 0 {
                return Err(DbError::NotFound(format!(
                    "Conversation '{}' not found",
                    message.conversation_id
                )));
            }
            sqlx::query(
                "INSERT INTO messages \
                    (id, conversation_id, msg_id, type, content, position, \
                     status, hidden, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&message.id)
            .bind(&message.conversation_id)
            .bind(&message.msg_id)
            .bind(&message.r#type)
            .bind(&message.content)
            .bind(&message.position)
            .bind(&message.status)
            .bind(message.hidden)
            .bind(message.created_at)
            .execute(&mut *connection)
            .await?;

            // Persisting a message bumps the parent conversation's recency so
            // the conversation-list sort floats fresh activity to the top;
            // MAX() keeps recency monotonic under out-of-order upserts.
            sqlx::query("UPDATE conversations SET updated_at = MAX(updated_at, ?) WHERE id = ?")
                .bind(message.created_at)
                .bind(&message.conversation_id)
                .execute(&mut *connection)
                .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn upsert_message_once(&self, user_id: &str, message: &MessageRow) -> Result<(), DbError> {
        // Writer-lock-first transaction; see `insert_message_once` for why.
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result: Result<(), DbError> = async {
            let result = sqlx::query(
                "INSERT INTO messages \
                (id, conversation_id, msg_id, type, content, position, \
                 status, hidden, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                content = CASE \
                    WHEN messages.status IN ('finish', 'error') AND excluded.status = 'work' THEN \
                        CASE messages.type \
                            WHEN 'acp_tool_call' THEN json_set( \
                                json_patch(messages.content, excluded.content), \
                                '$.update.status', \
                                json_extract(messages.content, '$.update.status') \
                            ) \
                            ELSE json_set( \
                                json_patch(messages.content, excluded.content), \
                                '$.status', \
                                json_extract(messages.content, '$.status') \
                            ) \
                        END \
                    ELSE json_patch(messages.content, excluded.content) \
                END, \
                status = CASE \
                    WHEN messages.status IN ('finish', 'error') AND excluded.status = 'work' THEN messages.status \
                    ELSE excluded.status \
                END, \
                position = COALESCE(messages.position, excluded.position), \
                hidden = excluded.hidden, \
                created_at = MIN(messages.created_at, excluded.created_at) \
             WHERE messages.conversation_id = excluded.conversation_id \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = messages.conversation_id AND c.user_id = ? \
               )",
            )
            .bind(&message.id)
            .bind(&message.conversation_id)
            .bind(&message.msg_id)
            .bind(&message.r#type)
            .bind(&message.content)
            .bind(&message.position)
            .bind(&message.status)
            .bind(message.hidden)
            .bind(message.created_at)
            .bind(user_id)
            .execute(&mut *connection)
            .await?;

            // 0 rows: either the id exists under another conversation, or the
            // user-scope EXISTS guard rejected a foreign owner. The rollback
            // below means the timestamp bump never applies either way.
            if result.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "Message with id '{}' already exists outside the requested conversation",
                    message.id
                )));
            }

            sqlx::query("UPDATE conversations SET updated_at = MAX(updated_at, ?) WHERE id = ?")
                .bind(message.created_at)
                .bind(&message.conversation_id)
                .execute(&mut *connection)
                .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn visible_message_exists_before(
        &self,
        user_id: &str,
        conv_id: &str,
        cursor: &MessagePageCursor,
    ) -> Result<bool, DbError> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS( \
                SELECT 1 FROM messages m \
                INNER JOIN conversations c ON c.id = m.conversation_id \
                WHERE c.user_id = ? \
                  AND m.conversation_id = ? \
                  AND (m.created_at < ? OR (m.created_at = ? AND m.id < ?)) \
                  AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
             )",
        )
        .bind(user_id)
        .bind(conv_id)
        .bind(cursor.created_at)
        .bind(cursor.created_at)
        .bind(&cursor.id)
        .fetch_one(&self.pool)
        .await?;

        Ok(exists != 0)
    }

    async fn visible_message_exists_after(
        &self,
        user_id: &str,
        conv_id: &str,
        cursor: &MessagePageCursor,
    ) -> Result<bool, DbError> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS( \
                SELECT 1 FROM messages m \
                INNER JOIN conversations c ON c.id = m.conversation_id \
                WHERE c.user_id = ? \
                  AND m.conversation_id = ? \
                  AND (m.created_at > ? OR (m.created_at = ? AND m.id > ?)) \
                  AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
             )",
        )
        .bind(user_id)
        .bind(conv_id)
        .bind(cursor.created_at)
        .bind(cursor.created_at)
        .bind(&cursor.id)
        .fetch_one(&self.pool)
        .await?;

        Ok(exists != 0)
    }

    async fn page_with_flags(
        &self,
        user_id: &str,
        conv_id: &str,
        items: Vec<MessageRow>,
    ) -> Result<MessagePageResult, DbError> {
        let Some(first) = items.first() else {
            return Ok(MessagePageResult {
                items,
                has_more_before: false,
                has_more_after: false,
            });
        };
        let first_cursor = MessagePageCursor::from(first);
        let last_cursor = items
            .last()
            .map(MessagePageCursor::from)
            .unwrap_or(first_cursor.clone());
        let has_more_before = self
            .visible_message_exists_before(user_id, conv_id, &first_cursor)
            .await?;
        let has_more_after = self
            .visible_message_exists_after(user_id, conv_id, &last_cursor)
            .await?;

        Ok(MessagePageResult {
            items,
            has_more_before,
            has_more_after,
        })
    }
}

#[async_trait::async_trait]
impl IConversationRepository for SqliteConversationRepository {
    // ── Conversation CRUD ───────────────────────────────────────────

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<ConversationRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationRow>("SELECT * FROM conversations WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn owner_user_id(&self, id: &str) -> Result<Option<String>, DbError> {
        let user_id = sqlx::query_scalar::<_, String>("SELECT user_id FROM conversations WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user_id)
    }

    async fn create(&self, row: &ConversationRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, model, status, source, \
                 channel_chat_id, pinned, pinned_at, created_at, updated_at, project_id, folder_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.r#type)
        .bind(&row.extra)
        .bind(&row.model)
        .bind(&row.status)
        .bind(&row.source)
        .bind(&row.channel_chat_id)
        .bind(row.pinned)
        .bind(row.pinned_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(&row.project_id)
        .bind(&row.folder_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn update(&self, user_id: &str, id: &str, updates: &ConversationRowUpdate) -> Result<(), DbError> {
        // Build dynamic SET clause
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        if let Some(ref name) = updates.name {
            set_parts.push("name = ?".to_string());
            binds.push(BindValue::Str(name.clone()));
        }
        if let Some(pinned) = updates.pinned {
            set_parts.push("pinned = ?".to_string());
            binds.push(BindValue::Bool(pinned));
        }
        if let Some(ref pinned_at) = updates.pinned_at {
            set_parts.push("pinned_at = ?".to_string());
            binds.push(BindValue::OptI64(*pinned_at));
        }
        if let Some(ref model) = updates.model {
            set_parts.push("model = ?".to_string());
            binds.push(BindValue::OptStr(model.clone()));
        }
        if let Some(ref extra) = updates.extra {
            set_parts.push("extra = ?".to_string());
            binds.push(BindValue::Str(extra.clone()));
        }
        if let Some(ref status) = updates.status {
            set_parts.push("status = ?".to_string());
            binds.push(BindValue::Str(status.clone()));
        }
        if let Some(updated_at) = updates.updated_at {
            set_parts.push("updated_at = ?".to_string());
            binds.push(BindValue::I64(updated_at));
        }
        if let Some(ref project_id) = updates.project_id {
            set_parts.push("project_id = ?".to_string());
            binds.push(BindValue::Str(project_id.clone()));
        }
        if let Some(ref folder_id) = updates.folder_id {
            set_parts.push("folder_id = ?".to_string());
            binds.push(BindValue::Str(folder_id.clone()));
        }

        if set_parts.is_empty() {
            return Ok(());
        }

        let sql = format!(
            "UPDATE conversations SET {} WHERE user_id = ? AND id = ?",
            set_parts.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(user_id);
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Conversation '{id}' not found")));
        }

        Ok(())
    }

    async fn delete(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM conversations WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Conversation '{id}' not found")));
        }

        Ok(())
    }

    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError> {
        let limit = filters.effective_limit();
        // Fetch one extra row to determine hasMore
        let fetch_limit = limit + 1;

        let mut where_parts = vec!["c.user_id = ?".to_string()];
        let mut binds: Vec<BindValue> = vec![BindValue::Str(user_id.to_string())];

        // Cursor-based pagination: use updated_at of the cursor row
        if let Some(ref cursor_id) = filters.cursor {
            where_parts.push(
                "(c.updated_at < (SELECT updated_at FROM conversations WHERE id = ?) \
                 OR (c.updated_at = (SELECT updated_at FROM conversations WHERE id = ?) \
                     AND c.id < ?))"
                    .to_string(),
            );
            binds.push(BindValue::Str(cursor_id.clone()));
            binds.push(BindValue::Str(cursor_id.clone()));
            binds.push(BindValue::Str(cursor_id.clone()));
        }

        append_filter_conditions(filters, &mut where_parts, &mut binds);

        let where_clause = where_parts.join(" AND ");

        // Count total matching rows (without cursor filter for total)
        let count_sql = build_count_sql(user_id, filters);
        let total = execute_count(&self.pool, &count_sql.0, &count_sql.1).await?;

        // Fetch page
        let sql = format!(
            "SELECT c.* FROM conversations c \
             WHERE {where_clause} \
             ORDER BY c.updated_at DESC, c.id DESC \
             LIMIT ?"
        );

        let mut query = sqlx::query_as::<_, ConversationRow>(&sql);
        for bind in &binds {
            query = bind_value_as(query, bind);
        }
        query = query.bind(fetch_limit);

        let mut rows = query.fetch_all(&self.pool).await?;

        let has_more = rows.len() as u32 > limit;
        if has_more {
            rows.pop();
        }

        Ok(PaginatedResult {
            items: rows,
            total,
            has_more,
        })
    }

    // ── Extended queries ────────────────────────────────────────────

    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? AND source = ? AND channel_chat_id = ? AND type = ? \
             ORDER BY updated_at DESC \
             LIMIT 1",
        )
        .bind(user_id)
        .bind(source)
        .bind(chat_id)
        .bind(agent_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn list_by_cron_job(&self, user_id: &str, cron_job_id: &str) -> Result<Vec<ConversationRow>, DbError> {
        let rows = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? \
             AND (json_extract(extra, '$.cronJobId') = ? OR json_extract(extra, '$.cron_job_id') = ?) \
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(cron_job_id)
        .bind(cron_job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn list_associated(&self, user_id: &str, conversation_id: &str) -> Result<Vec<ConversationRow>, DbError> {
        // First get the target conversation's workspace
        let target = sqlx::query_as::<_, ConversationRow>("SELECT * FROM conversations WHERE id = ? AND user_id = ?")
            .bind(conversation_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Conversation '{conversation_id}' not found")))?;

        // Extract workspace from extra JSON
        let workspace: Option<String> = serde_json::from_str::<serde_json::Value>(&target.extra)
            .ok()
            .and_then(|v: serde_json::Value| v.get("workspace")?.as_str().map(String::from));

        let Some(ref workspace) = workspace else {
            return Ok(Vec::new());
        };

        if workspace.is_empty() {
            return Ok(Vec::new());
        }

        // Find other conversations with the same workspace
        let rows = sqlx::query_as::<_, ConversationRow>(
            "SELECT * FROM conversations \
             WHERE user_id = ? \
               AND id != ? \
               AND json_extract(extra, '$.workspace') = ? \
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(conversation_id)
        .bind(workspace)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn get_assistant_snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationAssistantSnapshotRow>(
            "SELECT s.* FROM conversation_assistant_snapshots s \
             INNER JOIN conversations c ON c.id = s.conversation_id \
             WHERE c.user_id = ? AND s.conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn upsert_assistant_snapshot(
        &self,
        user_id: &str,
        params: &UpsertConversationAssistantSnapshotParams<'_>,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, DbError> {
        self.ensure_conversation_for_user(user_id, params.conversation_id)
            .await?;
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO conversation_assistant_snapshots (
                conversation_id,
                assistant_definition_id,
                assistant_id,
                assistant_source,
                agent_id,
                rules_content,
                default_model_mode,
                resolved_model_id,
                default_permission_mode,
                resolved_permission_value,
                default_thought_level_mode,
                resolved_thought_level_value,
                default_skills_mode,
                resolved_skill_ids,
                resolved_disabled_builtin_skill_ids,
                default_mcps_mode,
                resolved_mcp_ids,
                created_at,
                updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(conversation_id) DO UPDATE SET
                assistant_definition_id = excluded.assistant_definition_id,
                assistant_id = excluded.assistant_id,
                assistant_source = excluded.assistant_source,
                agent_id = excluded.agent_id,
                rules_content = excluded.rules_content,
                default_model_mode = excluded.default_model_mode,
                resolved_model_id = excluded.resolved_model_id,
                default_permission_mode = excluded.default_permission_mode,
                resolved_permission_value = excluded.resolved_permission_value,
                default_thought_level_mode = excluded.default_thought_level_mode,
                resolved_thought_level_value = excluded.resolved_thought_level_value,
                default_skills_mode = excluded.default_skills_mode,
                resolved_skill_ids = excluded.resolved_skill_ids,
                resolved_disabled_builtin_skill_ids = excluded.resolved_disabled_builtin_skill_ids,
                default_mcps_mode = excluded.default_mcps_mode,
                resolved_mcp_ids = excluded.resolved_mcp_ids,
                updated_at = excluded.updated_at",
        )
        .bind(params.conversation_id)
        .bind(params.assistant_definition_id)
        .bind(params.assistant_id)
        .bind(params.assistant_source)
        .bind(params.agent_id)
        .bind(params.rules_content)
        .bind(params.default_model_mode)
        .bind(params.resolved_model_id)
        .bind(params.default_permission_mode)
        .bind(params.resolved_permission_value)
        .bind(params.default_thought_level_mode)
        .bind(params.resolved_thought_level_value)
        .bind(params.default_skills_mode)
        .bind(params.resolved_skill_ids)
        .bind(params.resolved_disabled_builtin_skill_ids)
        .bind(params.default_mcps_mode)
        .bind(params.resolved_mcp_ids)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get_assistant_snapshot(user_id, params.conversation_id).await
    }

    async fn delete_assistant_snapshot(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query(
            "DELETE FROM conversation_assistant_snapshots \
             WHERE conversation_id = ? \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = conversation_assistant_snapshots.conversation_id \
                      AND c.user_id = ? \
               )",
        )
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    // ── Message operations ──────────────────────────────────────────

    async fn list_messages_page(
        &self,
        user_id: &str,
        conv_id: &str,
        params: &MessagePageParams,
    ) -> Result<MessagePageResult, DbError> {
        self.ensure_conversation_for_user(user_id, conv_id).await?;
        let limit = params.limit.max(1) as i64;
        let fetch_limit = limit + 1;

        let mut rows = match &params.direction {
            MessagePageDirection::InitialLatest => {
                let mut rows = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                      INNER JOIN conversations c ON c.id = m.conversation_id \
                      WHERE c.user_id = ? \
                        AND m.conversation_id = ? \
                        AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
                      ORDER BY m.created_at DESC, m.id DESC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await?;
                rows.truncate(limit as usize);
                rows.reverse();
                rows
            }
            MessagePageDirection::Before { cursor } => {
                let mut rows = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                      INNER JOIN conversations c ON c.id = m.conversation_id \
                      WHERE c.user_id = ? \
                        AND m.conversation_id = ? \
                        AND (m.created_at < ? OR (m.created_at = ? AND m.id < ?)) \
                        AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
                      ORDER BY m.created_at DESC, m.id DESC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(cursor.created_at)
                .bind(cursor.created_at)
                .bind(&cursor.id)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await?;
                rows.truncate(limit as usize);
                rows.reverse();
                rows
            }
            MessagePageDirection::After { cursor } => {
                let mut rows = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                      INNER JOIN conversations c ON c.id = m.conversation_id \
                      WHERE c.user_id = ? \
                        AND m.conversation_id = ? \
                        AND (m.created_at > ? OR (m.created_at = ? AND m.id > ?)) \
                        AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
                      ORDER BY m.created_at ASC, m.id ASC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(cursor.created_at)
                .bind(cursor.created_at)
                .bind(&cursor.id)
                .bind(fetch_limit)
                .fetch_all(&self.pool)
                .await?;
                rows.truncate(limit as usize);
                rows
            }
            MessagePageDirection::Anchor { message_id } => {
                let anchor = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                     INNER JOIN conversations c ON c.id = m.conversation_id \
                     WHERE c.user_id = ? \
                       AND m.conversation_id = ? \
                       AND m.id = ? \
                       AND m.type NOT IN ('cron_trigger', 'skill_suggest')",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| DbError::NotFound(format!("Message '{message_id}' not found")))?;

                let side_limit = limit;
                let mut before = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                      INNER JOIN conversations c ON c.id = m.conversation_id \
                      WHERE c.user_id = ? \
                        AND m.conversation_id = ? \
                        AND (m.created_at < ? OR (m.created_at = ? AND m.id < ?)) \
                        AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
                      ORDER BY m.created_at DESC, m.id DESC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(anchor.created_at)
                .bind(anchor.created_at)
                .bind(&anchor.id)
                .bind(side_limit)
                .fetch_all(&self.pool)
                .await?;
                before.reverse();

                let after = sqlx::query_as::<_, MessageRow>(
                    "SELECT m.* FROM messages m \
                      INNER JOIN conversations c ON c.id = m.conversation_id \
                      WHERE c.user_id = ? \
                        AND m.conversation_id = ? \
                        AND (m.created_at > ? OR (m.created_at = ? AND m.id > ?)) \
                        AND m.type NOT IN ('cron_trigger', 'skill_suggest') \
                      ORDER BY m.created_at ASC, m.id ASC \
                      LIMIT ?",
                )
                .bind(user_id)
                .bind(conv_id)
                .bind(anchor.created_at)
                .bind(anchor.created_at)
                .bind(&anchor.id)
                .bind(side_limit)
                .fetch_all(&self.pool)
                .await?;

                let before_target = ((limit - 1) / 2).max(0) as usize;
                let after_target = (limit as usize).saturating_sub(1 + before_target);
                let mut before_take = before.len().min(before_target);
                let mut after_take = after.len().min(after_target);
                let mut remaining = (limit as usize).saturating_sub(1 + before_take + after_take);
                if remaining > 0 {
                    let extra_after = (after.len() - after_take).min(remaining);
                    after_take += extra_after;
                    remaining -= extra_after;
                }
                if remaining > 0 {
                    before_take += (before.len() - before_take).min(remaining);
                }

                let before_start = before.len().saturating_sub(before_take);
                let mut rows = before[before_start..].to_vec();
                rows.push(anchor);
                rows.extend(after.into_iter().take(after_take));
                rows
            }
        };
        rows.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        self.page_with_flags(user_id, conv_id, rows).await
    }

    async fn get_message(&self, user_id: &str, conv_id: &str, message_id: &str) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT m.* FROM messages m \
             INNER JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.user_id = ? \
               AND m.conversation_id = ? \
               AND m.id = ? \
               AND m.type NOT IN ('cron_trigger', 'skill_suggest')",
        )
        .bind(user_id)
        .bind(conv_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn insert_message(&self, user_id: &str, message: &MessageRow) -> Result<(), DbError> {
        self.insert_message_once(user_id, message).await
    }

    async fn upsert_message(&self, user_id: &str, message: &MessageRow) -> Result<(), DbError> {
        self.upsert_message_once(user_id, message).await
    }

    async fn update_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        id: &str,
        updates: &MessageRowUpdate,
    ) -> Result<(), DbError> {
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        if let Some(ref content) = updates.content {
            set_parts.push("content = ?".to_string());
            binds.push(BindValue::Str(content.clone()));
        }
        if let Some(ref status) = updates.status {
            set_parts.push("status = ?".to_string());
            binds.push(BindValue::OptStr(status.clone()));
        }
        if let Some(hidden) = updates.hidden {
            set_parts.push("hidden = ?".to_string());
            binds.push(BindValue::Bool(hidden));
        }

        if set_parts.is_empty() {
            return Ok(());
        }

        let sql = format!(
            "UPDATE messages SET {} \
             WHERE conversation_id = ? AND id = ? \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = messages.conversation_id AND c.user_id = ? \
               )",
            set_parts.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(conversation_id);
        query = query.bind(id);
        query = query.bind(user_id);

        let result = query.execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Message '{id}' not found")));
        }

        Ok(())
    }

    async fn delete_messages_by_conversation(&self, user_id: &str, conv_id: &str) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM messages \
             WHERE conversation_id = ? \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = messages.conversation_id AND c.user_id = ? \
               )",
        )
        .bind(conv_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_message_by_msg_id(
        &self,
        user_id: &str,
        conv_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        let row = sqlx::query_as::<_, MessageRow>(
            "SELECT m.* FROM messages m \
             INNER JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.user_id = ? AND m.conversation_id = ? AND m.msg_id = ? AND m.type = ?",
        )
        .bind(user_id)
        .bind(conv_id)
        .bind(msg_id)
        .bind(msg_type)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn list_stale_runtime_messages(&self) -> Result<Vec<StaleRuntimeMessageRow>, DbError> {
        let rows = sqlx::query(
            "SELECT c.user_id, m.* FROM messages m \
             INNER JOIN conversations c ON c.id = m.conversation_id \
             WHERE m.position = 'left' \
               AND m.status IN ('work', 'pending') \
               AND m.type IN ('text', 'thinking', 'tool_call', 'tool_group') \
             ORDER BY m.created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                Ok(StaleRuntimeMessageRow {
                    user_id: row.try_get("user_id")?,
                    message: MessageRow {
                        id: row.try_get("id")?,
                        conversation_id: row.try_get("conversation_id")?,
                        msg_id: row.try_get("msg_id")?,
                        r#type: row.try_get("type")?,
                        content: row.try_get("content")?,
                        position: row.try_get("position")?,
                        status: row.try_get("status")?,
                        hidden: row.try_get("hidden")?,
                        created_at: row.try_get("created_at")?,
                    },
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?)
    }

    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError> {
        let effective_page = if page == 0 { 1 } else { page };
        let effective_size = if page_size == 0 { 20 } else { page_size };
        let offset = (effective_page - 1) * effective_size;
        let fetch_limit = effective_size + 1;

        let like_pattern = format!("%{keyword}%");

        let count_row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages m \
             INNER JOIN conversations c ON m.conversation_id = c.id \
             WHERE c.user_id = ? AND m.content LIKE ?",
        )
        .bind(user_id)
        .bind(&like_pattern)
        .fetch_one(&self.pool)
        .await?;
        let total = count_row.0 as u64;

        let rows = sqlx::query_as::<_, MessageSearchRow>(
            "SELECT \
                m.id AS message_id, \
                m.type, \
                m.content, \
                m.created_at, \
                c.id AS conversation_id, \
                c.name AS conversation_name, \
                c.type AS conversation_type, \
                c.extra AS conversation_extra, \
                c.model AS conversation_model, \
                c.status AS conversation_status, \
                c.source AS conversation_source, \
                c.channel_chat_id AS conversation_channel_chat_id, \
                c.pinned AS conversation_pinned, \
                c.pinned_at AS conversation_pinned_at, \
                c.created_at AS conversation_created_at, \
                c.updated_at AS conversation_updated_at \
             FROM messages m \
             INNER JOIN conversations c ON m.conversation_id = c.id \
             WHERE c.user_id = ? AND m.content LIKE ? \
             ORDER BY m.created_at DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(&like_pattern)
        .bind(fetch_limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let has_more = rows.len() as u32 > effective_size;
        let items = if has_more {
            rows[..effective_size as usize].to_vec()
        } else {
            rows
        };

        Ok(PaginatedResult { items, total, has_more })
    }

    async fn list_artifacts(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT a.* FROM conversation_artifacts a \
             INNER JOIN conversations c ON c.id = a.conversation_id \
             WHERE c.user_id = ? AND a.conversation_id = ? \
             ORDER BY a.created_at ASC, a.id ASC",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn get_artifact(
        &self,
        user_id: &str,
        conversation_id: &str,
        artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        let row = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT a.* FROM conversation_artifacts a \
             INNER JOIN conversations c ON c.id = a.conversation_id \
             WHERE c.user_id = ? AND a.conversation_id = ? AND a.id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    async fn upsert_artifact(
        &self,
        user_id: &str,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, DbError> {
        self.ensure_conversation_for_user(user_id, &artifact.conversation_id)
            .await?;
        let result = sqlx::query(
            "INSERT INTO conversation_artifacts \
                (id, conversation_id, cron_job_id, kind, status, payload, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                conversation_id = excluded.conversation_id, \
                cron_job_id = excluded.cron_job_id, \
                kind = excluded.kind, \
                status = excluded.status, \
                payload = excluded.payload, \
                updated_at = excluded.updated_at \
             WHERE conversation_artifacts.conversation_id = excluded.conversation_id \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = conversation_artifacts.conversation_id AND c.user_id = ? \
               )",
        )
        .bind(&artifact.id)
        .bind(&artifact.conversation_id)
        .bind(&artifact.cron_job_id)
        .bind(&artifact.kind)
        .bind(&artifact.status)
        .bind(&artifact.payload)
        .bind(artifact.created_at)
        .bind(artifact.updated_at)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Conversation artifact with id '{}' already exists outside the requested conversation",
                artifact.id
            )));
        }

        self.get_artifact(user_id, &artifact.conversation_id, &artifact.id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert artifact did not produce row for id '{}'", artifact.id)))
    }

    async fn update_artifact_status(
        &self,
        user_id: &str,
        conversation_id: &str,
        artifact_id: &str,
        status: &str,
        updated_at: i64,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        let result = sqlx::query(
            "UPDATE conversation_artifacts \
             SET status = ?, updated_at = ? \
             WHERE conversation_id = ? AND id = ? \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = conversation_artifacts.conversation_id AND c.user_id = ? \
               )",
        )
        .bind(status)
        .bind(updated_at)
        .bind(conversation_id)
        .bind(artifact_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_artifact(user_id, conversation_id, artifact_id).await
    }

    async fn mark_skill_suggest_artifacts_saved(
        &self,
        user_id: &str,
        cron_job_id: &str,
        updated_at: i64,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        sqlx::query(
            "UPDATE conversation_artifacts \
             SET status = 'saved', updated_at = ? \
             WHERE kind = 'skill_suggest' AND cron_job_id = ? AND status != 'saved' \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = conversation_artifacts.conversation_id AND c.user_id = ? \
               )",
        )
        .bind(updated_at)
        .bind(cron_job_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, ConversationArtifactRow>(
            "SELECT a.* FROM conversation_artifacts a \
             INNER JOIN conversations c ON c.id = a.conversation_id \
             WHERE c.user_id = ? AND a.kind = 'skill_suggest' AND a.cron_job_id = ? \
             ORDER BY a.created_at ASC, a.id ASC",
        )
        .bind(user_id)
        .bind(cron_job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn delete_artifacts_by_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM conversation_artifacts \
             WHERE conversation_id = ? \
               AND EXISTS ( \
                    SELECT 1 FROM conversations c \
                    WHERE c.id = conversation_artifacts.conversation_id AND c.user_id = ? \
               )",
        )
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn list_legacy_cron_trigger_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<MessageRow>, DbError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT m.* FROM messages m \
             INNER JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.user_id = ? AND m.conversation_id = ? AND m.type = 'cron_trigger' \
             ORDER BY m.created_at ASC, m.id ASC",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

// ── Dynamic bind helpers ────────────────────────────────────────────

/// Tagged union to carry heterogeneous bind values for dynamic SQL.
#[derive(Debug, Clone)]
enum BindValue {
    Str(String),
    OptStr(Option<String>),
    Bool(bool),
    I64(i64),
    OptI64(Option<i64>),
}

/// Binds a `BindValue` to a raw `sqlx::query::Query`.
fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    val: &'q BindValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match val {
        BindValue::Str(s) => query.bind(s.as_str()),
        BindValue::OptStr(s) => query.bind(s.as_deref()),
        BindValue::Bool(b) => query.bind(*b),
        BindValue::I64(n) => query.bind(*n),
        BindValue::OptI64(n) => query.bind(*n),
    }
}

/// Binds a `BindValue` to a `sqlx::query::QueryAs`.
fn bind_value_as<'q, T>(
    query: sqlx::query::QueryAs<'q, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'q>>,
    val: &'q BindValue,
) -> sqlx::query::QueryAs<'q, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'q>> {
    match val {
        BindValue::Str(s) => query.bind(s.as_str()),
        BindValue::OptStr(s) => query.bind(s.as_deref()),
        BindValue::Bool(b) => query.bind(*b),
        BindValue::I64(n) => query.bind(*n),
        BindValue::OptI64(n) => query.bind(*n),
    }
}

/// Appends shared filter conditions (source, cron_job_id, pinned) to WHERE
/// clause parts and bind values. Used by both `list_paginated` and the count
/// query to keep filter logic in one place.
fn append_filter_conditions(filters: &ConversationFilters, where_parts: &mut Vec<String>, binds: &mut Vec<BindValue>) {
    if let Some(ref source) = filters.source {
        where_parts.push("c.source = ?".to_string());
        binds.push(BindValue::Str(source.clone()));
    }
    if let Some(ref cron_job_id) = filters.cron_job_id {
        where_parts.push(
            "(json_extract(c.extra, '$.cronJobId') = ? OR json_extract(c.extra, '$.cron_job_id') = ?)".to_string(),
        );
        binds.push(BindValue::Str(cron_job_id.clone()));
        binds.push(BindValue::Str(cron_job_id.clone()));
    }
    if let Some(pinned) = filters.pinned {
        where_parts.push("c.pinned = ?".to_string());
        binds.push(BindValue::Bool(pinned));
    }
}

/// Builds a count query and bind values for the total (ignoring cursor).
fn build_count_sql(user_id: &str, filters: &ConversationFilters) -> (String, Vec<BindValue>) {
    let mut where_parts = vec!["c.user_id = ?".to_string()];
    let mut binds: Vec<BindValue> = vec![BindValue::Str(user_id.to_string())];

    append_filter_conditions(filters, &mut where_parts, &mut binds);

    let sql = format!(
        "SELECT COUNT(*) FROM conversations c WHERE {}",
        where_parts.join(" AND ")
    );

    (sql, binds)
}

/// Executes a dynamic count query.
async fn execute_count(pool: &SqlitePool, sql: &str, binds: &[BindValue]) -> Result<u64, DbError> {
    let mut query = sqlx::query_as::<_, (i64,)>(sql);
    for bind in binds {
        query = bind_value_as(query, bind);
    }
    let row = query.fetch_one(pool).await?;
    Ok(row.0 as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteConversationRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteConversationRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_conversation(user_id: &str) -> ConversationRow {
        let now = aionui_common::now_ms();
        ConversationRow {
            id: aionui_common::generate_prefixed_id("conv"),
            user_id: user_id.to_string(),
            name: "Test Conversation".to_string(),
            r#type: "gemini".to_string(),
            extra: r#"{"workspace":"/home/user/project"}"#.to_string(),
            model: Some(r#"{"providerId":"prov_1","model":"claude-sonnet-4-20250514"}"#.to_string()),
            status: Some("pending".to_string()),
            source: Some("aionui".to_string()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
            project_id: None,
            folder_id: None,
        }
    }

    fn sample_message(conv_id: &str) -> MessageRow {
        let now = aionui_common::now_ms();
        MessageRow {
            id: aionui_common::generate_prefixed_id("msg"),
            conversation_id: conv_id.to_string(),
            msg_id: Some("client_msg_1".to_string()),
            r#type: "text".to_string(),
            content: r#"{"content":"Hello world"}"#.to_string(),
            position: Some("right".to_string()),
            status: Some("finish".to_string()),
            hidden: false,
            created_at: now,
        }
    }

    const SYSTEM_USER_ID: &str = "system_default_user";

    // ── Conversation CRUD tests ─────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_conversation() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);

        repo.create(&conv).await.unwrap();
        let found = repo.get(&conv.user_id, &conv.id).await.unwrap().unwrap();

        assert_eq!(found.id, conv.id);
        assert_eq!(found.name, "Test Conversation");
        assert_eq!(found.r#type, "gemini");
        assert_eq!(found.status.as_deref(), Some("pending"));
        assert!(!found.pinned);
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let (repo, _db) = setup().await;
        assert!(repo.get("user_1", "no_such_id").await.unwrap().is_none());
    }

    /// Persisting a message must bump the parent conversation's updated_at so the
    /// conversation-list sort (ORDER BY conversations.updated_at DESC) floats a
    /// conversation with fresh activity to the top. Recency is monotonic — an
    /// out-of-order (older) upsert must not move it backward.
    #[tokio::test]
    async fn insert_message_bumps_conversation_updated_at() {
        let (repo, _db) = setup().await;
        let mut conv = sample_conversation(SYSTEM_USER_ID);
        conv.updated_at = 1; // force a known-stale baseline
        repo.create(&conv).await.unwrap();

        // insert_message with a newer event time bumps updated_at forward.
        let mut msg = sample_message(&conv.id);
        msg.created_at = 5_000;
        repo.insert_message(SYSTEM_USER_ID, &msg).await.unwrap();
        assert_eq!(
            repo.get(SYSTEM_USER_ID, &conv.id).await.unwrap().unwrap().updated_at,
            5_000,
            "insert must bump updated_at to the message time"
        );

        // A newer upsert (streaming tool-call update) advances recency.
        let mut newer = sample_message(&conv.id);
        newer.id = "tool-1".to_string();
        newer.created_at = 9_000;
        repo.upsert_message(SYSTEM_USER_ID, &newer).await.unwrap();
        assert_eq!(
            repo.get(SYSTEM_USER_ID, &conv.id).await.unwrap().unwrap().updated_at,
            9_000,
            "newer upsert must advance updated_at"
        );

        // An out-of-order (older) upsert must NOT move recency backward (MAX guard).
        let mut older = sample_message(&conv.id);
        older.id = "tool-2".to_string();
        older.created_at = 3_000;
        repo.upsert_message(SYSTEM_USER_ID, &older).await.unwrap();
        assert_eq!(
            repo.get(SYSTEM_USER_ID, &conv.id).await.unwrap().unwrap().updated_at,
            9_000,
            "older upsert must not move updated_at backward"
        );
    }

    #[tokio::test]
    async fn update_conversation_name() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let now = aionui_common::now_ms();
        repo.update(
            &conv.user_id,
            &conv.id,
            &ConversationRowUpdate {
                name: Some("Updated Name".to_string()),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(&conv.user_id, &conv.id).await.unwrap().unwrap();
        assert_eq!(found.name, "Updated Name");
        assert!(found.updated_at >= conv.updated_at);
    }

    #[tokio::test]
    async fn update_conversation_pinned() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let pin_time = aionui_common::now_ms();
        repo.update(
            &conv.user_id,
            &conv.id,
            &ConversationRowUpdate {
                pinned: Some(true),
                pinned_at: Some(Some(pin_time)),
                updated_at: Some(pin_time),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = repo.get(&conv.user_id, &conv.id).await.unwrap().unwrap();
        assert!(found.pinned);
        assert_eq!(found.pinned_at, Some(pin_time));
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update(
                "user_1",
                "no_id",
                &ConversationRowUpdate {
                    name: Some("x".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_empty_is_noop() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        // Empty update should succeed without error
        repo.update(&conv.user_id, &conv.id, &ConversationRowUpdate::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_conversation() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        repo.delete(&conv.user_id, &conv.id).await.unwrap();
        assert!(repo.get(&conv.user_id, &conv.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_cascades_messages() {
        let (repo, db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let msg = sample_message(&conv.id);
        repo.insert_message(&conv.user_id, &msg).await.unwrap();

        repo.delete(&conv.user_id, &conv.id).await.unwrap();

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(&conv.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete("user_1", "no_id").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    // ── Pagination tests ────────────────────────────────────────────

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
        assert!(!result.has_more);
    }

    #[tokio::test]
    async fn list_ordered_by_updated_at_desc() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.name = "First".to_string();
        c1.updated_at = 1000;
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.name = "Second".to_string();
        c2.updated_at = 2000;
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.name = "Third".to_string();
        c3.updated_at = 3000;
        repo.create(&c3).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 3);
        assert_eq!(result.total, 3);
        assert_eq!(result.items[0].name, "Third");
        assert_eq!(result.items[1].name, "Second");
        assert_eq!(result.items[2].name, "First");
    }

    #[tokio::test]
    async fn list_cursor_pagination() {
        let (repo, _db) = setup().await;

        let mut convs = Vec::new();
        for i in 0..5 {
            let mut c = sample_conversation(SYSTEM_USER_ID);
            c.name = format!("Conv {i}");
            c.updated_at = (i + 1) as i64 * 1000;
            repo.create(&c).await.unwrap();
            convs.push(c);
        }

        // Page 1: limit 2 → items[4,3], hasMore=true
        let page1 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert_eq!(page1.items[0].name, "Conv 4");
        assert_eq!(page1.items[1].name, "Conv 3");

        // Page 2: cursor = last item of page 1
        let cursor = page1.items.last().unwrap().id.clone();
        let page2 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cursor: Some(cursor),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page2.items.len(), 2);
        assert!(page2.has_more);
        assert_eq!(page2.items[0].name, "Conv 2");
        assert_eq!(page2.items[1].name, "Conv 1");

        // Page 3: cursor = last item of page 2
        let cursor = page2.items.last().unwrap().id.clone();
        let page3 = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cursor: Some(cursor),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page3.items.len(), 1);
        assert!(!page3.has_more);
        assert_eq!(page3.items[0].name, "Conv 0");
    }

    #[tokio::test]
    async fn list_filter_by_source() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.source = Some("aionui".to_string());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.source = Some("telegram".to_string());
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    source: Some("telegram".to_string()),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].source.as_deref(), Some("telegram"));
    }

    #[tokio::test]
    async fn list_filter_by_cron_job_id() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.extra = r#"{"cronJobId":"cron_abc","workspace":"/p"}"#.to_string();
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.extra = r#"{"workspace":"/p"}"#.to_string();
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    cron_job_id: Some("cron_abc".to_string()),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].id, c1.id);
    }

    #[tokio::test]
    async fn list_filter_by_pinned() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.pinned = true;
        c1.pinned_at = Some(aionui_common::now_ms());
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.pinned = false;
        repo.create(&c2).await.unwrap();

        let result = repo
            .list_paginated(
                SYSTEM_USER_ID,
                &ConversationFilters {
                    pinned: Some(true),
                    limit: 20,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert!(result.items[0].pinned);
    }

    // ── Extended query tests ────────────────────────────────────────

    #[tokio::test]
    async fn find_by_source_and_chat() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(SYSTEM_USER_ID);
        c.source = Some("telegram".to_string());
        c.channel_chat_id = Some("user:123".to_string());
        c.r#type = "gemini".to_string();
        repo.create(&c).await.unwrap();

        let found = repo
            .find_by_source_and_chat(SYSTEM_USER_ID, "telegram", "user:123", "gemini")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, c.id);

        // Different chat ID → not found
        let not_found = repo
            .find_by_source_and_chat(SYSTEM_USER_ID, "telegram", "user:999", "gemini")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn list_by_cron_job() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.extra = r#"{"cronJobId":"cron_1","workspace":"/p"}"#.to_string();
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.extra = r#"{"cronJobId":"cron_1","workspace":"/q"}"#.to_string();
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.extra = r#"{"cronJobId":"cron_2","workspace":"/r"}"#.to_string();
        repo.create(&c3).await.unwrap();

        let result = repo.list_by_cron_job(SYSTEM_USER_ID, "cron_1").await.unwrap();
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn list_associated_by_workspace() {
        let (repo, _db) = setup().await;

        let mut c1 = sample_conversation(SYSTEM_USER_ID);
        c1.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        repo.create(&c1).await.unwrap();

        let mut c2 = sample_conversation(SYSTEM_USER_ID);
        c2.extra = r#"{"workspace":"/shared/project"}"#.to_string();
        repo.create(&c2).await.unwrap();

        let mut c3 = sample_conversation(SYSTEM_USER_ID);
        c3.extra = r#"{"workspace":"/other/project"}"#.to_string();
        repo.create(&c3).await.unwrap();

        let associated = repo.list_associated(SYSTEM_USER_ID, &c1.id).await.unwrap();
        assert_eq!(associated.len(), 1);
        assert_eq!(associated[0].id, c2.id);
    }

    #[tokio::test]
    async fn list_associated_no_workspace() {
        let (repo, _db) = setup().await;

        let mut c = sample_conversation(SYSTEM_USER_ID);
        c.extra = r#"{}"#.to_string();
        repo.create(&c).await.unwrap();

        let associated = repo.list_associated(SYSTEM_USER_ID, &c.id).await.unwrap();
        assert!(associated.is_empty());
    }

    #[tokio::test]
    async fn list_associated_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.list_associated(SYSTEM_USER_ID, "no_id").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    // ── Message tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn insert_and_get_messages() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let msg = sample_message(&conv.id);
        repo.insert_message(&conv.user_id, &msg).await.unwrap();

        let result = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 50,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, msg.id);
        assert_eq!(result.items[0].created_at, msg.created_at);
    }

    #[tokio::test]
    async fn initial_latest_returns_latest_limit_in_ascending_order() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        for i in 0..10 {
            let mut msg = sample_message(&conv.id);
            msg.id = aionui_common::generate_prefixed_id("msg");
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&conv.user_id, &msg).await.unwrap();
        }

        let page1 = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 3,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        assert_eq!(page1.items.len(), 3);
        assert_eq!(
            page1.items.iter().map(|m| m.created_at).collect::<Vec<_>>(),
            vec![8000, 9000, 10000]
        );
        assert!(page1.has_more_before);
        assert!(!page1.has_more_after);
    }

    #[tokio::test]
    async fn before_pages_walk_history_without_duplicates() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        for i in 0..6 {
            let mut msg = sample_message(&conv.id);
            msg.id = aionui_common::generate_prefixed_id("msg");
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&conv.user_id, &msg).await.unwrap();
        }

        let latest = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 3,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        let older = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 3,
                    direction: MessagePageDirection::Before {
                        cursor: MessagePageCursor::from(&latest.items[0]),
                    },
                },
            )
            .await
            .unwrap();

        assert_eq!(
            older.items.iter().map(|m| m.created_at).collect::<Vec<_>>(),
            vec![1000, 2000, 3000]
        );
        assert!(!older.has_more_before);
        assert!(older.has_more_after);
    }

    #[tokio::test]
    async fn update_message_content() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let msg = sample_message(&conv.id);
        repo.insert_message(&conv.user_id, &msg).await.unwrap();

        repo.update_message(
            &conv.user_id,
            &conv.id,
            &msg.id,
            &MessageRowUpdate {
                content: Some(r#"{"content":"Updated"}"#.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let result = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 50,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.items[0].content, r#"{"content":"Updated"}"#);
    }

    #[tokio::test]
    async fn update_message_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update_message(
                "user_1",
                "conv_1",
                "no_id",
                &MessageRowUpdate {
                    hidden: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_messages_by_conversation() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        for _ in 0..3 {
            let mut msg = sample_message(&conv.id);
            msg.id = aionui_common::generate_prefixed_id("msg");
            repo.insert_message(&conv.user_id, &msg).await.unwrap();
        }

        repo.delete_messages_by_conversation(&conv.user_id, &conv.id)
            .await
            .unwrap();

        let result = repo
            .list_messages_page(
                &conv.user_id,
                &conv.id,
                &MessagePageParams {
                    limit: 50,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn get_message_by_msg_id() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let msg = sample_message(&conv.id);
        repo.insert_message(&conv.user_id, &msg).await.unwrap();

        let found = repo
            .get_message_by_msg_id(&conv.user_id, &conv.id, "client_msg_1", "text")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, msg.id);

        // Wrong type → not found
        let not_found = repo
            .get_message_by_msg_id(&conv.user_id, &conv.id, "client_msg_1", "tips")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn search_messages_by_keyword() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let mut msg1 = sample_message(&conv.id);
        msg1.content = r#"{"content":"Rust 审查报告"}"#.to_string();
        repo.insert_message(&conv.user_id, &msg1).await.unwrap();

        let mut msg2 = sample_message(&conv.id);
        msg2.id = aionui_common::generate_prefixed_id("msg");
        msg2.content = r#"{"content":"Python 测试"}"#.to_string();
        repo.insert_message(&conv.user_id, &msg2).await.unwrap();

        let result = repo.search_messages(SYSTEM_USER_ID, "审查", 1, 20).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].conversation_name, "Test Conversation");
    }

    #[tokio::test]
    async fn search_messages_no_match() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        let msg = sample_message(&conv.id);
        repo.insert_message(&conv.user_id, &msg).await.unwrap();

        let result = repo
            .search_messages(SYSTEM_USER_ID, "xxxxnotexist", 1, 20)
            .await
            .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn search_messages_pagination() {
        let (repo, _db) = setup().await;
        let conv = sample_conversation(SYSTEM_USER_ID);
        repo.create(&conv).await.unwrap();

        for i in 0..5 {
            let mut msg = sample_message(&conv.id);
            msg.id = aionui_common::generate_prefixed_id("msg");
            msg.content = format!(r#"{{"content":"match keyword item {i}"}}"#);
            msg.created_at = (i + 1) * 1000;
            repo.insert_message(&conv.user_id, &msg).await.unwrap();
        }

        let result = repo.search_messages(SYSTEM_USER_ID, "keyword", 1, 2).await.unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.total, 5);
        assert!(result.has_more);
    }

    // ── Filters tests ───────────────────────────────────────────────

    #[test]
    fn effective_limit_default() {
        let f = ConversationFilters::default();
        assert_eq!(f.effective_limit(), 20);
    }

    #[test]
    fn effective_limit_custom() {
        let f = ConversationFilters {
            limit: 50,
            ..Default::default()
        };
        assert_eq!(f.effective_limit(), 50);
    }
}
