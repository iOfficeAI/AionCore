use std::collections::BTreeSet;
use std::sync::Arc;

use aionui_api_types::{ClientPreferencesResponse, UpdateClientPreferencesRequest};
use aionui_db::IClientPreferenceRepository;
use tracing::{debug, info, warn};

use crate::error::SystemError;
use crate::keep_awake::{DynKeepAwakeController, KEEP_AWAKE_KEY, NoopKeepAwakeController};

/// Maximum allowed key length for client preferences.
const MAX_KEY_LENGTH: usize = 255;

/// Preference keys that describe the machine, not the account. They are stored
/// once per device (`scope = 'device'`, no owning user) so every account on the
/// machine reads and writes the same value. Confirmed by the settings-dedup B1
/// inventory and promoted by migration 031.
const DEVICE_SCOPED_KEYS: &[&str] = &["system.closeToTray", KEEP_AWAKE_KEY, "autoPreviewOfficeFiles"];

/// Key prefixes that are device-scoped as a family (desktop pet geometry and
/// visibility describe the screen, not the account).
const DEVICE_SCOPED_KEY_PREFIXES: &[&str] = &["pet."];

/// Whether a preference key is stored machine-level rather than per-account.
fn is_device_scoped_key(key: &str) -> bool {
    DEVICE_SCOPED_KEYS.contains(&key) || DEVICE_SCOPED_KEY_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Business logic for client preferences (generic key-value store).
#[derive(Clone)]
pub struct ClientPrefService {
    repo: Arc<dyn IClientPreferenceRepository>,
    keep_awake_controller: DynKeepAwakeController,
}

impl ClientPrefService {
    pub fn new(repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self {
            repo,
            keep_awake_controller: Arc::new(NoopKeepAwakeController),
        }
    }

    /// Build the service and restore the persisted keep-awake assertion.
    ///
    /// `keepAwake` is device-scoped, so the restore needs no user: whatever the
    /// machine last stored is reapplied at startup regardless of who logs in.
    pub fn with_keep_awake_controller(
        repo: Arc<dyn IClientPreferenceRepository>,
        keep_awake_controller: DynKeepAwakeController,
    ) -> Self {
        let service = Self {
            repo,
            keep_awake_controller,
        };
        service.restore_keep_awake_from_preferences();
        service
    }

    /// Get all client preferences, or only the specified keys.
    ///
    /// The response merges the caller's account-scope rows with the machine's
    /// device-scope rows; the two sets never share a key (write routing sends
    /// each key to exactly one scope, and migration 031 moved the historical
    /// per-user copies of device keys into the single device row).
    pub async fn get_preferences(
        &self,
        user_id: &str,
        keys: Option<&[&str]>,
    ) -> Result<ClientPreferencesResponse, SystemError> {
        let (account_rows, device_rows) = match keys {
            Some(requested) if !requested.is_empty() => {
                let (device_keys, account_keys): (Vec<&str>, Vec<&str>) =
                    requested.iter().copied().partition(|key| is_device_scoped_key(key));
                (
                    self.repo
                        .get_by_keys(user_id, &account_keys)
                        .await
                        .map_err(|e| SystemError::Internal(format!("Failed to get preferences: {e}")))?,
                    self.repo
                        .get_device_by_keys(&device_keys)
                        .await
                        .map_err(|e| SystemError::Internal(format!("Failed to get device preferences: {e}")))?,
                )
            }
            _ => (
                self.repo
                    .get_all(user_id)
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to get preferences: {e}")))?,
                self.repo
                    .get_all_device()
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to get device preferences: {e}")))?,
            ),
        };

        let mut found_keys = BTreeSet::new();
        let mut map = ClientPreferencesResponse::new();
        for row in account_rows.into_iter().chain(device_rows) {
            let value: serde_json::Value =
                serde_json::from_str(&row.value).unwrap_or(serde_json::Value::String(row.value));
            found_keys.insert(row.key.clone());
            debug!(
                target: "aionui_feedback_diagnostics",
                diagnostic_event = "feedback.runtime.client_preference_read",
                key = %row.key,
                found = true,
                "feedback.runtime.client_preference_read"
            );
            map.insert(row.key, value);
        }
        if let Some(keys) = keys {
            for key in keys.iter().filter(|key| !found_keys.contains(**key)) {
                debug!(
                    target: "aionui_feedback_diagnostics",
                    diagnostic_event = "feedback.runtime.client_preference_read",
                    key = %key,
                    found = false,
                    "feedback.runtime.client_preference_read"
                );
            }
        }
        Ok(map)
    }

    /// Batch update client preferences. Null values delete the key.
    pub async fn update_preferences(
        &self,
        user_id: &str,
        req: UpdateClientPreferencesRequest,
    ) -> Result<(), SystemError> {
        let mut upserts: Vec<(String, String)> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();
        let keep_awake_update = resolve_keep_awake_update(&req)?;

        for (key, value) in req {
            validate_key(&key)?;

            if value.is_null() {
                info!(
                    target: "aionui_feedback_diagnostics",
                    diagnostic_event = "feedback.runtime.client_preference_write",
                    key = %key,
                    value_type = %"null",
                    value_bytes = 0,
                    "feedback.runtime.client_preference_write"
                );
                deletes.push(key);
            } else {
                let serialized = serde_json::to_string(&value)
                    .map_err(|e| SystemError::Internal(format!("Failed to serialize value: {e}")))?;
                info!(
                    target: "aionui_feedback_diagnostics",
                    diagnostic_event = "feedback.runtime.client_preference_write",
                    key = %key,
                    value_type = %json_value_type(&value),
                    value_bytes = serialized.len(),
                    "feedback.runtime.client_preference_write"
                );
                upserts.push((key, serialized));
            }
        }

        let previous_keep_awake = if keep_awake_update.is_some() {
            Some(self.get_stored_keep_awake().await?)
        } else {
            None
        };

        if let Some(enabled) = keep_awake_update {
            self.apply_keep_awake(enabled).await?;
        }

        // Route each key to the scope it is stored in.
        let (device_upserts, account_upserts): (Vec<_>, Vec<_>) = upserts
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .partition(|(key, _)| is_device_scoped_key(key));
        let (device_deletes, account_deletes): (Vec<&str>, Vec<&str>) = deletes
            .iter()
            .map(|key| key.as_str())
            .partition(|key| is_device_scoped_key(key));

        let stored = async {
            if !account_upserts.is_empty() {
                self.repo
                    .upsert_batch(user_id, &account_upserts)
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to upsert preferences: {e}")))?;
            }
            if !device_upserts.is_empty() {
                self.repo
                    .upsert_device_batch(&device_upserts)
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to upsert device preferences: {e}")))?;
            }
            if !account_deletes.is_empty() {
                self.repo
                    .delete_keys(user_id, &account_deletes)
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to delete preferences: {e}")))?;
            }
            if !device_deletes.is_empty() {
                self.repo
                    .delete_device_keys(&device_deletes)
                    .await
                    .map_err(|e| SystemError::Internal(format!("Failed to delete device preferences: {e}")))?;
            }
            Ok::<(), SystemError>(())
        }
        .await;

        if let Err(error) = stored {
            // Persistence failed after the assertion was already flipped —
            // put the machine back where it was.
            if let Some(previous) = previous_keep_awake {
                let _ = self.apply_keep_awake(previous).await;
            }
            return Err(error);
        }

        Ok(())
    }

    pub async fn release_keep_awake_for_shutdown(&self) -> Result<(), SystemError> {
        self.keep_awake_controller.set_enabled(false).await.map_err(|error| {
            warn!(error = %error, "Failed to release system keep-awake assertion during shutdown");
            error
        })?;
        info!("System keep-awake assertion released during shutdown");
        Ok(())
    }

    /// Reads the machine's stored keep-awake value. Device-scoped: there is one
    /// value per machine, so no user is involved.
    async fn get_stored_keep_awake(&self) -> Result<bool, SystemError> {
        let rows = self
            .repo
            .get_device_by_keys(&[KEEP_AWAKE_KEY])
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get keep-awake preference: {e}")))?;

        if let Some(row) = rows.iter().find(|row| row.key == KEEP_AWAKE_KEY) {
            let value: serde_json::Value =
                serde_json::from_str(&row.value).unwrap_or(serde_json::Value::String(row.value.clone()));
            match parse_keep_awake_value(&value) {
                Ok(enabled) => return Ok(enabled),
                Err(error) => {
                    warn!(key = KEEP_AWAKE_KEY, error = %error, "Ignoring invalid stored keep-awake preference")
                }
            }
        }
        Ok(false)
    }

    async fn apply_keep_awake(&self, enabled: bool) -> Result<(), SystemError> {
        self.keep_awake_controller.set_enabled(enabled).await.map_err(|error| {
            warn!(enabled, error = %error, "Failed to update system keep-awake assertion");
            error
        })?;
        info!(enabled, "System keep-awake preference applied");
        Ok(())
    }

    fn restore_keep_awake_from_preferences(&self) {
        let service = self.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("Cannot restore system keep-awake preference without a Tokio runtime");
            return;
        };
        handle.spawn(async move {
            match service.get_stored_keep_awake().await {
                Ok(true) => {
                    if let Err(error) = service.apply_keep_awake(true).await {
                        warn!(error = %error, "Failed to restore system keep-awake assertion");
                    }
                }
                Ok(false) => {}
                Err(error) => warn!(error = %error, "Failed to read system keep-awake preference for restore"),
            }
        });
    }
}

fn json_value_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn validate_key(key: &str) -> Result<(), SystemError> {
    if key.is_empty() {
        return Err(SystemError::BadRequest("Preference key must not be empty".into()));
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(SystemError::BadRequest(format!(
            "Preference key exceeds maximum length of {MAX_KEY_LENGTH} characters"
        )));
    }
    Ok(())
}

fn resolve_keep_awake_update(req: &UpdateClientPreferencesRequest) -> Result<Option<bool>, SystemError> {
    req.get(KEEP_AWAKE_KEY).map(parse_keep_awake_value).transpose()
}

fn parse_keep_awake_value(value: &serde_json::Value) -> Result<bool, SystemError> {
    match value {
        serde_json::Value::Bool(enabled) => Ok(*enabled),
        serde_json::Value::Null => Ok(false),
        _ => Err(SystemError::BadRequest(format!(
            "{KEEP_AWAKE_KEY} must be a boolean or null"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteClientPreferenceRepository, init_database_memory};
    use async_trait::async_trait;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Mutex, Once, OnceLock};
    use tracing::Level;
    use tracing_subscriber::fmt;

    const TEST_USER_ID: &str = "user-1";
    const OTHER_USER_ID: &str = "user-2";

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    static LOG_CAPTURE_INIT: Once = Once::new();
    static LOG_CAPTURE_LOCK: Mutex<()> = Mutex::new(());
    static LOG_CAPTURE_BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

    fn capture_logs(max_level: Level, f: impl FnOnce()) -> String {
        let _capture_guard = LOG_CAPTURE_LOCK.lock().unwrap();
        let buffer = Arc::clone(LOG_CAPTURE_BUFFER.get_or_init(|| Arc::new(Mutex::new(Vec::<u8>::new()))));
        buffer.lock().unwrap().clear();
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };
        LOG_CAPTURE_INIT.call_once(|| {
            let subscriber = fmt::Subscriber::builder()
                .with_max_level(max_level)
                .with_writer(make_writer)
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("set global test subscriber");
        });

        f();
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    async fn setup_repo() -> Arc<SqliteClientPreferenceRepository> {
        let db = init_database_memory().await.unwrap();
        for user_id in [TEST_USER_ID, OTHER_USER_ID] {
            sqlx::query(
                "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
                 VALUES (?, 'local', ?, '', 'active', 0, 1, 1)",
            )
            .bind(user_id)
            .bind(user_id)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let repo = Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        // Leak the db handle so the pool stays alive for the test
        std::mem::forget(db);
        repo
    }

    async fn setup() -> ClientPrefService {
        ClientPrefService::new(setup_repo().await)
    }

    async fn setup_with_keep_awake_controller(controller: DynKeepAwakeController) -> ClientPrefService {
        ClientPrefService {
            repo: setup_repo().await,
            keep_awake_controller: controller,
        }
    }

    #[derive(Default)]
    struct RecordingKeepAwakeController {
        calls: Mutex<Vec<bool>>,
        fail: bool,
    }

    #[async_trait]
    impl crate::keep_awake::KeepAwakeController for RecordingKeepAwakeController {
        async fn set_enabled(&self, enabled: bool) -> Result<(), SystemError> {
            self.calls.lock().unwrap().push(enabled);
            if self.fail {
                Err(SystemError::Internal("keep-awake failed".into()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn validate_key_accepts_valid() {
        assert!(validate_key("theme").is_ok());
        assert!(validate_key("system.closeToTray").is_ok());
        assert!(validate_key("a").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn validate_key_rejects_too_long() {
        let long_key = "x".repeat(MAX_KEY_LENGTH + 1);
        assert!(validate_key(&long_key).is_err());
    }

    #[tokio::test]
    async fn get_empty_returns_empty_map() {
        let svc = setup().await;
        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn update_and_get_boolean() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("system.closeToTray".into(), json!(true));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs["system.closeToTray"], json!(true));
    }

    #[tokio::test]
    async fn update_and_get_number() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("pet.size".into(), json!(360));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs["pet.size"], json!(360));
    }

    #[tokio::test]
    async fn update_and_get_string() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs["theme"], json!("dark"));
    }

    #[tokio::test]
    async fn null_deletes_key() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("theme".into(), json!(null));
        svc.update_preferences(TEST_USER_ID, req2).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert!(!prefs.contains_key("theme"));
    }

    #[tokio::test]
    async fn get_by_keys_filters() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("a".into(), json!(1));
        req.insert("b".into(), json!(2));
        req.insert("c".into(), json!(3));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, Some(&["a", "c"])).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["a"], json!(1));
        assert_eq!(prefs["c"], json!(3));
    }

    #[test]
    fn client_preference_diagnostic_logs_do_not_include_values() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let captured = capture_logs(Level::DEBUG, || {
            runtime.block_on(async {
                let svc = setup().await;
                let mut req = UpdateClientPreferencesRequest::new();
                req.insert("appearance.secretTheme".into(), json!("super-secret-value"));
                req.insert("appearance.deleted".into(), json!(null));
                svc.update_preferences(TEST_USER_ID, req).await.unwrap();

                let _ = svc
                    .get_preferences(TEST_USER_ID, Some(&["appearance.secretTheme", "appearance.missing"]))
                    .await
                    .unwrap();
            });
        });

        assert!(captured.contains("aionui_feedback_diagnostics"), "{captured}");
        assert!(
            captured.contains("feedback.runtime.client_preference_write"),
            "{captured}"
        );
        assert!(
            captured.contains("feedback.runtime.client_preference_read"),
            "{captured}"
        );
        assert!(captured.contains("key=appearance.secretTheme"), "{captured}");
        assert!(captured.contains("key=appearance.missing"), "{captured}");
        assert!(captured.contains("value_type=string"), "{captured}");
        assert!(captured.contains("value_type=null"), "{captured}");
        assert!(captured.contains("found=true"), "{captured}");
        assert!(captured.contains("found=false"), "{captured}");
        assert!(!captured.contains("super-secret-value"), "{captured}");
    }

    #[tokio::test]
    async fn overwrite_existing_value() {
        let svc = setup().await;

        let mut req1 = UpdateClientPreferencesRequest::new();
        req1.insert("k".into(), json!("v1"));
        svc.update_preferences(TEST_USER_ID, req1).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("k".into(), json!("v2"));
        svc.update_preferences(TEST_USER_ID, req2).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs["k"], json!("v2"));
    }

    #[tokio::test]
    async fn empty_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("".into(), json!(true));
        let err = svc.update_preferences(TEST_USER_ID, req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn long_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("x".repeat(256), json!(true));
        let err = svc.update_preferences(TEST_USER_ID, req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn batch_mixed_upsert_and_delete() {
        let svc = setup().await;

        let mut setup_req = UpdateClientPreferencesRequest::new();
        setup_req.insert("keep".into(), json!(1));
        setup_req.insert("remove".into(), json!(2));
        svc.update_preferences(TEST_USER_ID, setup_req).await.unwrap();

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("remove".into(), json!(null));
        req.insert("new".into(), json!(3));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["keep"], json!(1));
        assert_eq!(prefs["new"], json!(3));
    }

    #[tokio::test]
    async fn keep_awake_true_enables_controller_and_persists_value() {
        let controller = Arc::new(RecordingKeepAwakeController::default());
        let svc = setup_with_keep_awake_controller(controller.clone()).await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(true));

        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        assert_eq!(*controller.calls.lock().unwrap(), vec![true]);
        let prefs = svc
            .get_preferences(TEST_USER_ID, Some(&[KEEP_AWAKE_KEY]))
            .await
            .unwrap();
        assert_eq!(prefs[KEEP_AWAKE_KEY], json!(true));
    }

    #[tokio::test]
    async fn keep_awake_null_disables_controller_and_deletes_value() {
        let controller = Arc::new(RecordingKeepAwakeController::default());
        let svc = setup_with_keep_awake_controller(controller.clone()).await;
        let mut setup_req = UpdateClientPreferencesRequest::new();
        setup_req.insert(KEEP_AWAKE_KEY.into(), json!(true));
        svc.update_preferences(TEST_USER_ID, setup_req).await.unwrap();

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(null));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        assert_eq!(*controller.calls.lock().unwrap(), vec![true, false]);
        let prefs = svc
            .get_preferences(TEST_USER_ID, Some(&[KEEP_AWAKE_KEY]))
            .await
            .unwrap();
        assert!(!prefs.contains_key(KEEP_AWAKE_KEY));
    }

    #[tokio::test]
    async fn keep_awake_shutdown_release_does_not_clear_persisted_preference() {
        let controller = Arc::new(RecordingKeepAwakeController::default());
        let svc = setup_with_keep_awake_controller(controller.clone()).await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(true));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        svc.release_keep_awake_for_shutdown().await.unwrap();

        assert_eq!(*controller.calls.lock().unwrap(), vec![true, false]);
        let prefs = svc
            .get_preferences(TEST_USER_ID, Some(&[KEEP_AWAKE_KEY]))
            .await
            .unwrap();
        assert_eq!(prefs[KEEP_AWAKE_KEY], json!(true));
    }

    #[tokio::test]
    async fn keep_awake_rejects_non_boolean_values() {
        let controller = Arc::new(RecordingKeepAwakeController::default());
        let svc = setup_with_keep_awake_controller(controller.clone()).await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!("yes"));

        let err = svc.update_preferences(TEST_USER_ID, req).await.unwrap_err();

        assert!(matches!(err, SystemError::BadRequest(_)));
        assert!(controller.calls.lock().unwrap().is_empty());
        let prefs = svc
            .get_preferences(TEST_USER_ID, Some(&[KEEP_AWAKE_KEY]))
            .await
            .unwrap();
        assert!(!prefs.contains_key(KEEP_AWAKE_KEY));
    }

    #[tokio::test]
    async fn keep_awake_controller_failure_does_not_persist_value() {
        let controller = Arc::new(RecordingKeepAwakeController {
            fail: true,
            ..Default::default()
        });
        let svc = setup_with_keep_awake_controller(controller.clone()).await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(true));

        let err = svc.update_preferences(TEST_USER_ID, req).await.unwrap_err();

        assert!(matches!(err, SystemError::Internal(_)));
        assert_eq!(*controller.calls.lock().unwrap(), vec![true]);
        let prefs = svc
            .get_preferences(TEST_USER_ID, Some(&[KEEP_AWAKE_KEY]))
            .await
            .unwrap();
        assert!(!prefs.contains_key(KEEP_AWAKE_KEY));
    }

    #[tokio::test]
    async fn keep_awake_restore_applies_persisted_value() {
        let initial = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(true));
        initial.update_preferences(TEST_USER_ID, req).await.unwrap();

        let controller = Arc::new(RecordingKeepAwakeController::default());
        let service = ClientPrefService::with_keep_awake_controller(initial.repo.clone(), controller.clone());

        for _ in 0..50 {
            if !controller.calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(*controller.calls.lock().unwrap(), vec![true]);
        drop(service);
    }

    #[tokio::test]
    async fn keep_awake_restore_reads_the_machine_value_written_by_any_account() {
        // `keepAwake` is device-scoped: the value another account stored is the
        // machine's value, and startup restore must pick it up without knowing
        // which user wrote it.
        let initial = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert(KEEP_AWAKE_KEY.into(), json!(true));
        initial.update_preferences(OTHER_USER_ID, req).await.unwrap();

        let controller = Arc::new(RecordingKeepAwakeController::default());
        let service = ClientPrefService::with_keep_awake_controller(initial.repo.clone(), controller.clone());

        for _ in 0..50 {
            if !controller.calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(*controller.calls.lock().unwrap(), vec![true]);
        drop(service);
    }

    #[tokio::test]
    async fn keep_awake_restore_leaves_controller_untouched_without_a_stored_value() {
        let initial = setup().await;
        let controller = Arc::new(RecordingKeepAwakeController::default());
        let service = ClientPrefService::with_keep_awake_controller(initial.repo.clone(), controller.clone());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            controller.calls.lock().unwrap().is_empty(),
            "restore must not assert keep-awake when the machine stored nothing"
        );
        drop(service);
    }

    // -- device vs account scope --

    #[test]
    fn device_scoped_keys_are_the_machine_level_set() {
        for key in ["system.closeToTray", "keepAwake", "autoPreviewOfficeFiles"] {
            assert!(is_device_scoped_key(key), "{key} must be device-scoped");
        }
        for key in ["pet.", "pet.size", "pet.visible", "pet.anything.nested"] {
            assert!(is_device_scoped_key(key), "{key} must be device-scoped");
        }
    }

    #[test]
    fn account_scoped_keys_are_not_device_scoped() {
        for key in [
            "theme",
            "language",
            "system.notificationEnabled",
            "system.saveUploadToWorkspace",
            "assistant.telegram.agent",
            // Near-misses: the prefix rule must not swallow these.
            "petals",
            "pet",
            "appearance.pet.size",
            "system.closeToTrayExtra",
        ] {
            assert!(!is_device_scoped_key(key), "{key} must be account-scoped");
        }
    }

    #[tokio::test]
    async fn device_key_written_by_one_user_is_visible_to_another() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("system.closeToTray".into(), json!(true));
        req.insert("pet.size".into(), json!(360));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        // Full read for the other account sees the machine's values…
        let prefs = svc.get_preferences(OTHER_USER_ID, None).await.unwrap();
        assert_eq!(prefs["system.closeToTray"], json!(true));
        assert_eq!(prefs["pet.size"], json!(360));

        // …and so does a keyed read.
        let prefs = svc
            .get_preferences(OTHER_USER_ID, Some(&["system.closeToTray", "pet.size"]))
            .await
            .unwrap();
        assert_eq!(prefs["system.closeToTray"], json!(true));
        assert_eq!(prefs["pet.size"], json!(360));
    }

    #[tokio::test]
    async fn device_key_write_by_second_user_overwrites_the_machine_value() {
        let svc = setup().await;
        let mut first = UpdateClientPreferencesRequest::new();
        first.insert("autoPreviewOfficeFiles".into(), json!(true));
        svc.update_preferences(TEST_USER_ID, first).await.unwrap();

        let mut second = UpdateClientPreferencesRequest::new();
        second.insert("autoPreviewOfficeFiles".into(), json!(false));
        svc.update_preferences(OTHER_USER_ID, second).await.unwrap();

        for user in [TEST_USER_ID, OTHER_USER_ID] {
            let prefs = svc.get_preferences(user, None).await.unwrap();
            assert_eq!(prefs["autoPreviewOfficeFiles"], json!(false), "for {user}");
        }
    }

    #[tokio::test]
    async fn device_key_delete_removes_it_for_every_account() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("pet.size".into(), json!(360));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        let mut delete = UpdateClientPreferencesRequest::new();
        delete.insert("pet.size".into(), json!(null));
        svc.update_preferences(OTHER_USER_ID, delete).await.unwrap();

        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert!(!prefs.contains_key("pet.size"));
    }

    #[tokio::test]
    async fn account_keys_stay_isolated_between_users() {
        let svc = setup().await;
        let mut mine = UpdateClientPreferencesRequest::new();
        mine.insert("theme".into(), json!("dark"));
        svc.update_preferences(TEST_USER_ID, mine).await.unwrap();

        let mut theirs = UpdateClientPreferencesRequest::new();
        theirs.insert("theme".into(), json!("light"));
        svc.update_preferences(OTHER_USER_ID, theirs).await.unwrap();

        assert_eq!(
            svc.get_preferences(TEST_USER_ID, None).await.unwrap()["theme"],
            json!("dark")
        );
        assert_eq!(
            svc.get_preferences(OTHER_USER_ID, None).await.unwrap()["theme"],
            json!("light")
        );

        // Deleting one account's key leaves the other's alone.
        let mut delete = UpdateClientPreferencesRequest::new();
        delete.insert("theme".into(), json!(null));
        svc.update_preferences(OTHER_USER_ID, delete).await.unwrap();
        assert!(
            !svc.get_preferences(OTHER_USER_ID, None)
                .await
                .unwrap()
                .contains_key("theme")
        );
        assert_eq!(
            svc.get_preferences(TEST_USER_ID, None).await.unwrap()["theme"],
            json!("dark")
        );
    }

    #[tokio::test]
    async fn mixed_scope_batch_routes_each_key_to_its_own_store() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("pet.size".into(), json!(360));
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(TEST_USER_ID, req).await.unwrap();

        // The device key lives in the device store only…
        let device_rows = svc.repo.get_all_device().await.unwrap();
        let device_keys: Vec<&str> = device_rows.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(device_keys, vec!["pet.size"]);
        assert!(device_rows[0].user_id.is_none());

        // …and the account key in the account store only.
        let account_rows = svc.repo.get_all(TEST_USER_ID).await.unwrap();
        let account_keys: Vec<&str> = account_rows.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(account_keys, vec!["theme"]);

        // The merged read still returns both.
        let prefs = svc.get_preferences(TEST_USER_ID, None).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["pet.size"], json!(360));
        assert_eq!(prefs["theme"], json!("dark"));
    }
}
