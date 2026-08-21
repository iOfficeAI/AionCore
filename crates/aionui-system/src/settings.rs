use std::sync::Arc;

use aionui_api_types::{SystemSettingsResponse, UpdateSettingsRequest};
use aionui_db::{IClientPreferenceRepository, ISettingsRepository};
use tracing::warn;

use crate::error::SystemError;

/// Supported BCP 47 language codes.
const SUPPORTED_LANGUAGES: &[&str] = &[
    "en-US", "zh-CN", "zh-TW", "ja-JP", "ko-KR", "fr-FR", "de-DE", "es-ES", "pt-BR", "ru-RU", "ar-SA", "it-IT",
    "nl-NL", "pl-PL", "tr-TR", "vi-VN", "th-TH", "id-ID",
];

/// Client-preference key that is the single source of truth for the UI
/// language. `system_settings.language` is a legacy read fallback only;
/// writes always land here (settings-dedup B1).
const LANGUAGE_PREF_KEY: &str = "language";

/// Account-scope preference keys that are the single source of truth for the
/// four boolean switches; the matching `system_settings` columns are legacy
/// read fallbacks only (settings-dedup B2, migration 031 materialized them).
const NOTIFICATION_ENABLED_PREF_KEY: &str = "system.notificationEnabled";
const CRON_NOTIFICATION_ENABLED_PREF_KEY: &str = "cron.notificationEnabled";
const COMMAND_QUEUE_ENABLED_PREF_KEY: &str = "system.commandQueueEnabled";
const SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY: &str = "system.saveUploadToWorkspace";

/// Every preference key this service owns, in one read batch.
const SETTINGS_PREF_KEYS: &[&str] = &[
    LANGUAGE_PREF_KEY,
    NOTIFICATION_ENABLED_PREF_KEY,
    CRON_NOTIFICATION_ENABLED_PREF_KEY,
    COMMAND_QUEUE_ENABLED_PREF_KEY,
    SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY,
];

/// Business logic for system settings (language, notifications, etc.).
///
/// Every field is proxied to `client_preferences` — the same keys the frontend
/// reads/writes via `/api/settings/client` — so there is exactly one stored
/// truth. The `system_settings` columns are kept convergent on write and serve
/// as the read fallback for rows that predate the preference materialization;
/// they go away when B3 drops the table.
#[derive(Clone)]
pub struct SettingsService {
    repo: Arc<dyn ISettingsRepository>,
    pref_repo: Arc<dyn IClientPreferenceRepository>,
}

impl SettingsService {
    pub fn new(repo: Arc<dyn ISettingsRepository>, pref_repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { repo, pref_repo }
    }

    /// Get current system settings, falling back to defaults if not yet persisted.
    pub async fn get_settings(&self, user_id: &str) -> Result<SystemSettingsResponse, SystemError> {
        let row = self
            .repo
            .get_settings(user_id)
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get settings: {e}")))?;

        let mut settings = row.map_or_else(SystemSettingsResponse::default, |s| SystemSettingsResponse {
            language: s.language,
            notification_enabled: s.notification_enabled,
            cron_notification_enabled: s.cron_notification_enabled,
            command_queue_enabled: s.command_queue_enabled,
            save_upload_to_workspace: s.save_upload_to_workspace,
        });

        // Preferences are the truth; the columns read above are the fallback
        // for anything a preference does not (yet) cover.
        let prefs = self.get_settings_preferences(user_id).await?;
        for (key, raw) in &prefs {
            match key.as_str() {
                LANGUAGE_PREF_KEY => {
                    if let Some(language) = parse_language_preference(raw) {
                        settings.language = language;
                    }
                }
                NOTIFICATION_ENABLED_PREF_KEY => {
                    if let Some(enabled) = parse_bool_preference(key, raw) {
                        settings.notification_enabled = enabled;
                    }
                }
                CRON_NOTIFICATION_ENABLED_PREF_KEY => {
                    if let Some(enabled) = parse_bool_preference(key, raw) {
                        settings.cron_notification_enabled = enabled;
                    }
                }
                COMMAND_QUEUE_ENABLED_PREF_KEY => {
                    if let Some(enabled) = parse_bool_preference(key, raw) {
                        settings.command_queue_enabled = enabled;
                    }
                }
                SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY => {
                    if let Some(enabled) = parse_bool_preference(key, raw) {
                        settings.save_upload_to_workspace = enabled;
                    }
                }
                _ => {}
            }
        }
        Ok(settings)
    }

    /// Partially update system settings. Only fields present in the request are changed.
    pub async fn update_settings(
        &self,
        user_id: &str,
        req: UpdateSettingsRequest,
    ) -> Result<SystemSettingsResponse, SystemError> {
        if let Some(ref lang) = req.language {
            validate_language(lang)?;
        }

        // Merge with current settings (or defaults)
        let current = self.get_settings(user_id).await?;

        let language = req.language.unwrap_or(current.language);
        let notification_enabled = req.notification_enabled.unwrap_or(current.notification_enabled);
        let cron_notification_enabled = req
            .cron_notification_enabled
            .unwrap_or(current.cron_notification_enabled);
        let command_queue_enabled = req.command_queue_enabled.unwrap_or(current.command_queue_enabled);
        let save_upload_to_workspace = req.save_upload_to_workspace.unwrap_or(current.save_upload_to_workspace);

        // The truth lives in client_preferences; the column write below only
        // keeps the legacy fallback convergent for pre-migration readers.
        let language_value = serde_json::Value::String(language.clone()).to_string();
        let entries = [
            (LANGUAGE_PREF_KEY, language_value.as_str()),
            (NOTIFICATION_ENABLED_PREF_KEY, bool_pref_value(notification_enabled)),
            (
                CRON_NOTIFICATION_ENABLED_PREF_KEY,
                bool_pref_value(cron_notification_enabled),
            ),
            (COMMAND_QUEUE_ENABLED_PREF_KEY, bool_pref_value(command_queue_enabled)),
            (
                SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY,
                bool_pref_value(save_upload_to_workspace),
            ),
        ];
        self.pref_repo
            .upsert_batch(user_id, &entries)
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to update settings preferences: {e}")))?;

        self.repo
            .upsert_settings(
                user_id,
                &language,
                notification_enabled,
                cron_notification_enabled,
                command_queue_enabled,
                save_upload_to_workspace,
            )
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to update settings: {e}")))?;

        Ok(SystemSettingsResponse {
            language,
            notification_enabled,
            cron_notification_enabled,
            command_queue_enabled,
            save_upload_to_workspace,
        })
    }

    /// Reads this service's preference keys as raw stored values, keyed by
    /// preference key. Missing keys are simply absent.
    async fn get_settings_preferences(&self, user_id: &str) -> Result<Vec<(String, String)>, SystemError> {
        let rows = self
            .pref_repo
            .get_by_keys(user_id, SETTINGS_PREF_KEYS)
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get settings preferences: {e}")))?;
        Ok(rows.into_iter().map(|row| (row.key, row.value)).collect())
    }
}

/// Parse a stored language preference, tolerating both JSON-encoded and raw
/// string storage. Non-string or empty values are ignored (the legacy column
/// then serves as the fallback).
fn parse_language_preference(raw: &str) -> Option<String> {
    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(_) => {
            warn!(
                key = LANGUAGE_PREF_KEY,
                "Ignoring non-string stored language preference"
            );
            return None;
        }
        // Raw (non-JSON) storage from older writers.
        Err(_) => raw.to_owned(),
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Parse a stored boolean switch preference. JSON booleans are the canonical
/// encoding; bare `true`/`false` text from older writers is tolerated. Anything
/// else is ignored with a warning so the legacy column stays the fallback.
fn parse_bool_preference(key: &str, raw: &str) -> Option<bool> {
    if let Ok(serde_json::Value::Bool(enabled)) = serde_json::from_str::<serde_json::Value>(raw) {
        return Some(enabled);
    }
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => {
            warn!(key, "Ignoring non-boolean stored settings preference");
            None
        }
    }
}

/// Canonical stored encoding for a boolean switch preference.
fn bool_pref_value(enabled: bool) -> &'static str {
    if enabled { "true" } else { "false" }
}

fn validate_language(lang: &str) -> Result<(), SystemError> {
    if SUPPORTED_LANGUAGES.contains(&lang) {
        Ok(())
    } else {
        Err(SystemError::BadRequest(format!("Unsupported language code: '{lang}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "user-1";
    use aionui_db::{SqliteClientPreferenceRepository, SqliteSettingsRepository, init_database_memory};

    async fn setup() -> SettingsService {
        setup_with_prefs().await.0
    }

    async fn setup_with_prefs() -> (SettingsService, Arc<SqliteClientPreferenceRepository>) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, '', 'active', 0, 1, 1)",
        )
        .bind(TEST_USER_ID)
        .bind(TEST_USER_ID)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = Arc::new(SqliteSettingsRepository::new(db.pool().clone()));
        let pref_repo = Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        // Leak the db handle so the pool stays alive for the test
        std::mem::forget(db);
        (SettingsService::new(repo, pref_repo.clone()), pref_repo)
    }

    #[test]
    fn validate_language_accepts_supported() {
        assert!(validate_language("en-US").is_ok());
        assert!(validate_language("zh-CN").is_ok());
        assert!(validate_language("ja-JP").is_ok());
    }

    #[test]
    fn validate_language_rejects_unsupported() {
        assert!(validate_language("invalid").is_err());
        assert!(validate_language("").is_err());
        assert!(validate_language("xx-YY").is_err());
    }

    #[tokio::test]
    async fn get_settings_returns_defaults_when_empty() {
        let svc = setup().await;
        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_single_field() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("zh-CN".into()),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert_eq!(result.language, "zh-CN");
        // Other fields stay at defaults
        assert!(result.notification_enabled);
        assert!(!result.cron_notification_enabled);
    }

    #[tokio::test]
    async fn update_multiple_fields() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            notification_enabled: Some(false),
            command_queue_enabled: Some(true),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert!(!result.notification_enabled);
        assert!(result.command_queue_enabled);
        assert_eq!(result.language, "en-US");
    }

    #[tokio::test]
    async fn update_empty_request_returns_current() {
        let svc = setup().await;
        let result = svc
            .update_settings(TEST_USER_ID, UpdateSettingsRequest::default())
            .await
            .unwrap();
        assert_eq!(result, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_invalid_language_rejected() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("invalid-lang".into()),
            ..Default::default()
        };
        let err = svc.update_settings(TEST_USER_ID, req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn language_preference_wins_over_legacy_column() {
        let (svc, _prefs) = setup_with_prefs().await;
        // Legacy column says zh-CN…
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-CN", true, false, false, false)
            .await
            .unwrap();
        // …but the preference (single truth) says ja-JP.
        svc.pref_repo
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "\"ja-JP\"")])
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "ja-JP");
    }

    #[tokio::test]
    async fn language_falls_back_to_legacy_column_without_preference() {
        let (svc, _prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-TW", true, false, false, false)
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "zh-TW");
    }

    #[tokio::test]
    async fn update_language_writes_the_preference_truth() {
        let (svc, prefs) = setup_with_prefs().await;
        let req = UpdateSettingsRequest {
            language: Some("ko-KR".into()),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert_eq!(result.language, "ko-KR");

        // The preference row is the stored truth (JSON-encoded string).
        let rows = prefs.get_by_keys(TEST_USER_ID, &[LANGUAGE_PREF_KEY]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "\"ko-KR\"");
        // And reads agree with it.
        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "ko-KR");
    }

    #[tokio::test]
    async fn raw_string_preference_storage_is_tolerated() {
        let (svc, prefs) = setup_with_prefs().await;
        // Older writers stored the raw string without JSON encoding.
        prefs
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "fr-FR")])
            .await
            .unwrap();

        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "fr-FR");
    }

    #[tokio::test]
    async fn non_string_language_preference_is_ignored() {
        let (svc, prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-CN", true, false, false, false)
            .await
            .unwrap();
        prefs
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "123")])
            .await
            .unwrap();

        // Falls back to the legacy column.
        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "zh-CN");
    }

    #[tokio::test]
    async fn switch_preferences_win_over_legacy_columns() {
        let (svc, prefs) = setup_with_prefs().await;
        // Legacy columns say: notification on, cron off, queue off, save off…
        svc.repo
            .upsert_settings(TEST_USER_ID, "en-US", true, false, false, false)
            .await
            .unwrap();
        // …but the preferences (single truth) say the exact opposite.
        prefs
            .upsert_batch(
                TEST_USER_ID,
                &[
                    (NOTIFICATION_ENABLED_PREF_KEY, "false"),
                    (CRON_NOTIFICATION_ENABLED_PREF_KEY, "true"),
                    (COMMAND_QUEUE_ENABLED_PREF_KEY, "true"),
                    (SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY, "true"),
                ],
            )
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert!(!settings.notification_enabled);
        assert!(settings.cron_notification_enabled);
        assert!(settings.command_queue_enabled);
        assert!(settings.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn switches_fall_back_to_legacy_columns_without_preferences() {
        let (svc, _prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "en-US", false, true, true, true)
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert!(!settings.notification_enabled);
        assert!(settings.cron_notification_enabled);
        assert!(settings.command_queue_enabled);
        assert!(settings.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn a_single_switch_preference_overlays_only_itself() {
        let (svc, prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "en-US", true, true, false, false)
            .await
            .unwrap();
        prefs
            .upsert_batch(TEST_USER_ID, &[(CRON_NOTIFICATION_ENABLED_PREF_KEY, "false")])
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert!(!settings.cron_notification_enabled, "pref wins for its own key");
        assert!(settings.notification_enabled, "other switches keep the column value");
        assert!(!settings.command_queue_enabled);
        assert!(!settings.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn non_boolean_switch_preference_is_ignored() {
        let (svc, prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "en-US", false, false, false, false)
            .await
            .unwrap();
        prefs
            .upsert_batch(TEST_USER_ID, &[(NOTIFICATION_ENABLED_PREF_KEY, "\"yes\"")])
            .await
            .unwrap();

        // Falls back to the legacy column.
        assert!(!svc.get_settings(TEST_USER_ID).await.unwrap().notification_enabled);
    }

    #[tokio::test]
    async fn update_writes_every_switch_as_a_preference() {
        let (svc, prefs) = setup_with_prefs().await;
        let result = svc
            .update_settings(
                TEST_USER_ID,
                UpdateSettingsRequest {
                    notification_enabled: Some(false),
                    command_queue_enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!result.notification_enabled);
        assert!(result.command_queue_enabled);

        // All five keys are written, including the ones the request omitted —
        // preferences must describe the full effective state.
        let stored: std::collections::BTreeMap<String, String> = prefs
            .get_by_keys(TEST_USER_ID, SETTINGS_PREF_KEYS)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect();
        assert_eq!(stored.len(), SETTINGS_PREF_KEYS.len());
        assert_eq!(stored[NOTIFICATION_ENABLED_PREF_KEY], "false");
        assert_eq!(stored[COMMAND_QUEUE_ENABLED_PREF_KEY], "true");
        assert_eq!(stored[CRON_NOTIFICATION_ENABLED_PREF_KEY], "false");
        assert_eq!(stored[SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY], "false");
        assert_eq!(stored[LANGUAGE_PREF_KEY], "\"en-US\"");

        // And reads agree with the stored truth.
        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert!(!settings.notification_enabled);
        assert!(settings.command_queue_enabled);
    }

    #[tokio::test]
    async fn switch_preference_survives_an_unrelated_update() {
        let (svc, prefs) = setup_with_prefs().await;
        prefs
            .upsert_batch(TEST_USER_ID, &[(SAVE_UPLOAD_TO_WORKSPACE_PREF_KEY, "true")])
            .await
            .unwrap();

        // Updating only the language must carry the switch's effective value
        // forward, not reset it to the column default.
        let result = svc
            .update_settings(
                TEST_USER_ID,
                UpdateSettingsRequest {
                    language: Some("ja-JP".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(result.save_upload_to_workspace);
        assert!(svc.get_settings(TEST_USER_ID).await.unwrap().save_upload_to_workspace);
    }

    #[test]
    fn parse_bool_preference_accepts_json_and_raw_booleans() {
        assert_eq!(parse_bool_preference("k", "true"), Some(true));
        assert_eq!(parse_bool_preference("k", "false"), Some(false));
        assert_eq!(parse_bool_preference("k", " true "), Some(true));
        assert_eq!(parse_bool_preference("k", "\"true\""), None);
        assert_eq!(parse_bool_preference("k", "1"), None);
        assert_eq!(parse_bool_preference("k", "yes"), None);
        assert_eq!(parse_bool_preference("k", ""), None);
    }

    #[tokio::test]
    async fn update_then_get_reflects_changes() {
        let svc = setup().await;
        svc.update_settings(
            TEST_USER_ID,
            UpdateSettingsRequest {
                language: Some("ja-JP".into()),
                save_upload_to_workspace: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "ja-JP");
        assert!(settings.save_upload_to_workspace);
    }
}
