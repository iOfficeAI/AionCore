use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `system_settings` table.
///
/// Per-user settings table. Boolean fields are stored as INTEGER
/// in SQLite (0/1) and mapped to `bool` via sqlx.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SystemSettings {
    pub user_id: String,
    pub language: String,
    pub notification_enabled: bool,
    pub cron_notification_enabled: bool,
    pub command_queue_enabled: bool,
    pub save_upload_to_workspace: bool,
    /// Cross-session messaging master switch. `NOT NULL DEFAULT 1` (migration
    /// 040), so "default on" is expressed by the schema rather than re-derived
    /// at every read site.
    pub cross_session_message_enabled: bool,
    pub updated_at: TimestampMs,
}
