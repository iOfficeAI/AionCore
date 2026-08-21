use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `client_preferences` table.
///
/// Generic key-value store. Values are stored as JSON-serialized TEXT.
///
/// Rows carry a scope (migration 031): `'account'` rows are per-user and
/// always have a `user_id`; `'device'` rows are machine-level (one value per
/// key for the whole machine) and always have `user_id = NULL`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ClientPreference {
    /// `'account'` or `'device'` — enforced by a CHECK constraint together
    /// with the nullability of `user_id`.
    pub scope: String,
    /// Owning user for account-scope rows; `None` for device-scope rows.
    pub user_id: Option<String>,
    pub key: String,
    pub value: String,
    pub updated_at: TimestampMs,
}
