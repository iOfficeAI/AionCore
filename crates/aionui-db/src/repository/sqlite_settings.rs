use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::SystemSettings;
use crate::repository::ISettingsRepository;

/// SQLite-backed implementation of [`ISettingsRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSettingsRepository {
    pool: SqlitePool,
}

impl SqliteSettingsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISettingsRepository for SqliteSettingsRepository {
    async fn get_settings(&self, user_id: &str) -> Result<Option<SystemSettings>, DbError> {
        let row = sqlx::query_as::<_, SystemSettings>("SELECT * FROM system_settings WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_settings(
        &self,
        user_id: &str,
        language: &str,
        notification_enabled: bool,
        cron_notification_enabled: bool,
        command_queue_enabled: bool,
        save_upload_to_workspace: bool,
        cross_session_message_enabled: bool,
    ) -> Result<SystemSettings, DbError> {
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO system_settings \
                (user_id, language, notification_enabled, cron_notification_enabled, \
                 command_queue_enabled, save_upload_to_workspace, \
                 cross_session_message_enabled, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
                language = excluded.language, \
                notification_enabled = excluded.notification_enabled, \
                cron_notification_enabled = excluded.cron_notification_enabled, \
                command_queue_enabled = excluded.command_queue_enabled, \
                save_upload_to_workspace = excluded.save_upload_to_workspace, \
                cross_session_message_enabled = excluded.cross_session_message_enabled, \
                updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(language)
        .bind(notification_enabled)
        .bind(cron_notification_enabled)
        .bind(command_queue_enabled)
        .bind(save_upload_to_workspace)
        .bind(cross_session_message_enabled)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(SystemSettings {
            user_id: user_id.to_string(),
            language: language.to_string(),
            notification_enabled,
            cron_notification_enabled,
            command_queue_enabled,
            save_upload_to_workspace,
            cross_session_message_enabled,
            updated_at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "user_b";

    async fn setup() -> (SqliteSettingsRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(USER_B)
        .bind(USER_B)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteSettingsRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_settings_returns_none_when_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get_settings(USER_A).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_creates_settings() {
        let (repo, _db) = setup().await;
        let s = repo
            .upsert_settings(USER_A, "zh-CN", false, true, true, false, true)
            .await
            .unwrap();

        assert_eq!(s.user_id, USER_A);
        assert_eq!(s.language, "zh-CN");
        assert!(!s.notification_enabled);
        assert!(s.cron_notification_enabled);
        assert!(s.command_queue_enabled);
        assert!(!s.save_upload_to_workspace);
        assert!(s.updated_at > 0);
    }

    #[tokio::test]
    async fn upsert_then_get_returns_same() {
        let (repo, _db) = setup().await;
        repo.upsert_settings(USER_A, "en-US", true, false, false, true, true)
            .await
            .unwrap();

        let s = repo.get_settings(USER_A).await.unwrap().unwrap();
        assert_eq!(s.language, "en-US");
        assert!(s.notification_enabled);
        assert!(!s.cron_notification_enabled);
        assert!(!s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() {
        let (repo, _db) = setup().await;
        repo.upsert_settings(USER_A, "en-US", true, false, false, false, true)
            .await
            .unwrap();
        let s = repo
            .upsert_settings(USER_A, "ja-JP", false, true, true, true, true)
            .await
            .unwrap();

        assert_eq!(s.language, "ja-JP");
        assert!(!s.notification_enabled);
        assert!(s.cron_notification_enabled);
        assert!(s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);

        // Verify persisted via get
        let fetched = repo.get_settings(USER_A).await.unwrap().unwrap();
        assert_eq!(fetched.language, "ja-JP");
    }

    #[tokio::test]
    async fn settings_are_scoped_by_user() {
        let (repo, _db) = setup().await;
        repo.upsert_settings(USER_A, "en-US", true, false, false, false, true)
            .await
            .unwrap();
        repo.upsert_settings(USER_B, "zh-CN", false, true, true, true, true)
            .await
            .unwrap();

        let a = repo.get_settings(USER_A).await.unwrap().unwrap();
        let b = repo.get_settings(USER_B).await.unwrap().unwrap();
        assert_eq!(a.language, "en-US");
        assert_eq!(b.language, "zh-CN");
        assert!(a.notification_enabled);
        assert!(!b.notification_enabled);
    }

    #[tokio::test]
    async fn cross_session_flag_round_trips() {
        let (repo, _db) = setup().await;
        repo.upsert_settings(USER_A, "en-US", true, false, false, false, false)
            .await
            .unwrap();
        let fetched = repo.get_settings(USER_A).await.unwrap().unwrap();
        assert!(!fetched.cross_session_message_enabled);
    }

    /// Migration 040 is `NOT NULL DEFAULT 1`, so a row written by pre-040 code
    /// (i.e. without the column) must read back as enabled rather than as
    /// `false` — a silently-disabled feature after upgrade would look like a
    /// bug in the delivery path, not in the schema.
    #[tokio::test]
    async fn a_row_inserted_without_the_column_defaults_to_enabled() {
        let (repo, _db) = setup().await;
        sqlx::query(
            "INSERT INTO system_settings \
                (user_id, language, notification_enabled, cron_notification_enabled, \
                 command_queue_enabled, save_upload_to_workspace, updated_at) \
             VALUES (?, 'en-US', 1, 0, 0, 0, 1)",
        )
        .bind(USER_B)
        .execute(&repo.pool)
        .await
        .unwrap();

        let fetched = repo.get_settings(USER_B).await.unwrap().unwrap();
        assert!(fetched.cross_session_message_enabled);
    }
}
