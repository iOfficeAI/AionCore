use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `system_settings` table.
///
/// Single-row table (id is always 1). Boolean fields are stored as INTEGER
/// in SQLite (0/1) and mapped to `bool` via sqlx.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SystemSettings {
    pub id: i64,
    pub language: String,
    pub notification_enabled: bool,
    pub cron_notification_enabled: bool,
    pub command_queue_enabled: bool,
    pub save_upload_to_workspace: bool,
    pub app_operations_model_mode: String,
    pub app_operations_provider_id: Option<String>,
    pub app_operations_model_id: Option<String>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AppOperationsModelSettingRow {
    pub mode: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
}
