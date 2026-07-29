use crate::error::DbError;
use crate::models::{AppOperationsModelSettingRow, SystemSettings};

/// System settings data access abstraction.
///
/// The `system_settings` table holds a single row (id=1).
/// `get_settings` returns `None` if no row exists yet (caller uses defaults).
/// `upsert_settings` inserts or replaces the single row.
#[async_trait::async_trait]
pub trait ISettingsRepository: Send + Sync {
    /// Returns the settings row, or `None` if no settings have been persisted.
    async fn get_settings(&self) -> Result<Option<SystemSettings>, DbError>;

    /// Returns the App Operations model setting, defaulting to Auto when settings are absent.
    async fn get_app_operations_model(&self) -> Result<AppOperationsModelSettingRow, DbError>;

    /// Inserts or replaces the single settings row.
    async fn upsert_settings(
        &self,
        language: &str,
        notification_enabled: bool,
        cron_notification_enabled: bool,
        command_queue_enabled: bool,
        save_upload_to_workspace: bool,
    ) -> Result<SystemSettings, DbError>;

    /// Inserts or updates the App Operations model setting without overwriting other settings.
    async fn upsert_app_operations_model(
        &self,
        mode: &str,
        provider_id: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<AppOperationsModelSettingRow, DbError>;
}
