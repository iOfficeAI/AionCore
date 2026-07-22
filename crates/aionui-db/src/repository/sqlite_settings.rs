use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{AppOperationsModelSettingRow, SystemSettings};
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
    async fn get_settings(&self) -> Result<Option<SystemSettings>, DbError> {
        let row = sqlx::query_as::<_, SystemSettings>("SELECT * FROM system_settings WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn get_app_operations_model(&self) -> Result<AppOperationsModelSettingRow, DbError> {
        let row = sqlx::query_as::<_, AppOperationsModelSettingRow>(
            "SELECT app_operations_model_mode AS mode, \
                    app_operations_provider_id AS provider_id, \
                    app_operations_model_id AS model_id \
             FROM system_settings \
             WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.unwrap_or(AppOperationsModelSettingRow {
            mode: "auto".to_string(),
            provider_id: None,
            model_id: None,
        }))
    }

    async fn upsert_settings(
        &self,
        language: &str,
        notification_enabled: bool,
        cron_notification_enabled: bool,
        command_queue_enabled: bool,
        save_upload_to_workspace: bool,
    ) -> Result<SystemSettings, DbError> {
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO system_settings \
                (id, language, notification_enabled, cron_notification_enabled, \
                 command_queue_enabled, save_upload_to_workspace, updated_at) \
             VALUES (1, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                language = excluded.language, \
                notification_enabled = excluded.notification_enabled, \
                cron_notification_enabled = excluded.cron_notification_enabled, \
                command_queue_enabled = excluded.command_queue_enabled, \
                save_upload_to_workspace = excluded.save_upload_to_workspace, \
                updated_at = excluded.updated_at",
        )
        .bind(language)
        .bind(notification_enabled)
        .bind(cron_notification_enabled)
        .bind(command_queue_enabled)
        .bind(save_upload_to_workspace)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, SystemSettings>("SELECT * FROM system_settings WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;

        Ok(row)
    }

    async fn upsert_app_operations_model(
        &self,
        mode: &str,
        provider_id: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<AppOperationsModelSettingRow, DbError> {
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO system_settings \
                (id, app_operations_model_mode, app_operations_provider_id, \
                 app_operations_model_id, updated_at) \
             VALUES (1, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                app_operations_model_mode = excluded.app_operations_model_mode, \
                app_operations_provider_id = excluded.app_operations_provider_id, \
                app_operations_model_id = excluded.app_operations_model_id, \
                updated_at = excluded.updated_at",
        )
        .bind(mode)
        .bind(provider_id)
        .bind(model_id)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(AppOperationsModelSettingRow {
            mode: mode.to_string(),
            provider_id: provider_id.map(str::to_string),
            model_id: model_id.map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteSettingsRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteSettingsRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_settings_returns_none_when_empty() {
        let (repo, _db) = setup().await;
        assert!(repo.get_settings().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn app_operations_defaults_to_auto_when_settings_are_empty() {
        let (repo, _db) = setup().await;
        let setting = repo.get_app_operations_model().await.unwrap();
        assert_eq!(setting.mode, "auto");
        assert_eq!(setting.provider_id, None);
        assert_eq!(setting.model_id, None);
    }

    #[tokio::test]
    async fn fixed_app_operations_setting_round_trips_without_overwriting_language() {
        let (repo, _db) = setup().await;
        repo.upsert_settings("ja-JP", true, false, false, false).await.unwrap();
        repo.upsert_app_operations_model("fixed", Some("provider-1"), Some("model-1"))
            .await
            .unwrap();

        let app_model = repo.get_app_operations_model().await.unwrap();
        let system = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(app_model.mode, "fixed");
        assert_eq!(app_model.provider_id.as_deref(), Some("provider-1"));
        assert_eq!(app_model.model_id.as_deref(), Some("model-1"));
        assert_eq!(system.language, "ja-JP");
    }

    #[tokio::test]
    async fn upsert_settings_returns_existing_fixed_app_operations_model() {
        let (repo, _db) = setup().await;
        repo.upsert_app_operations_model("fixed", Some("provider-1"), Some("model-1"))
            .await
            .unwrap();

        let system = repo.upsert_settings("ja-JP", true, false, false, false).await.unwrap();

        assert_eq!(system.app_operations_model_mode, "fixed");
        assert_eq!(system.app_operations_provider_id.as_deref(), Some("provider-1"));
        assert_eq!(system.app_operations_model_id.as_deref(), Some("model-1"));
    }

    #[tokio::test]
    async fn upsert_creates_settings() {
        let (repo, _db) = setup().await;
        let s = repo.upsert_settings("zh-CN", false, true, true, false).await.unwrap();

        assert_eq!(s.id, 1);
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
        repo.upsert_settings("en-US", true, false, false, true).await.unwrap();

        let s = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(s.language, "en-US");
        assert!(s.notification_enabled);
        assert!(!s.cron_notification_enabled);
        assert!(!s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() {
        let (repo, _db) = setup().await;
        repo.upsert_settings("en-US", true, false, false, false).await.unwrap();
        let s = repo.upsert_settings("ja-JP", false, true, true, true).await.unwrap();

        assert_eq!(s.language, "ja-JP");
        assert!(!s.notification_enabled);
        assert!(s.cron_notification_enabled);
        assert!(s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);

        // Verify persisted via get
        let fetched = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(fetched.language, "ja-JP");
    }
}
