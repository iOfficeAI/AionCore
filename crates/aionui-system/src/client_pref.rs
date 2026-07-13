use std::collections::BTreeSet;
use std::sync::Arc;

use aionui_api_types::{ClientPreferencesResponse, UpdateClientPreferencesRequest};
use aionui_db::IClientPreferenceRepository;
use tracing::{debug, info};

use crate::error::SystemError;

/// Maximum allowed key length for client preferences.
const MAX_KEY_LENGTH: usize = 255;

/// Business logic for client preferences (generic key-value store).
#[derive(Clone)]
pub struct ClientPrefService {
    repo: Arc<dyn IClientPreferenceRepository>,
}

impl ClientPrefService {
    pub fn new(repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { repo }
    }

    /// Get all client preferences, or only the specified keys.
    pub async fn get_preferences(&self, keys: Option<&[&str]>) -> Result<ClientPreferencesResponse, SystemError> {
        let rows = match keys {
            Some(k) if !k.is_empty() => self.repo.get_by_keys(k).await,
            _ => self.repo.get_all().await,
        }
        .map_err(|e| SystemError::Internal(format!("Failed to get preferences: {e}")))?;

        let mut found_keys = BTreeSet::new();
        let mut map = ClientPreferencesResponse::new();
        for row in rows {
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
    pub async fn update_preferences(&self, req: UpdateClientPreferencesRequest) -> Result<(), SystemError> {
        let mut upserts: Vec<(String, String)> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();

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

        if !upserts.is_empty() {
            let entries: Vec<(&str, &str)> = upserts.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            self.repo
                .upsert_batch(&entries)
                .await
                .map_err(|e| SystemError::Internal(format!("Failed to upsert preferences: {e}")))?;
        }

        if !deletes.is_empty() {
            let keys: Vec<&str> = deletes.iter().map(|k| k.as_str()).collect();
            self.repo
                .delete_keys(&keys)
                .await
                .map_err(|e| SystemError::Internal(format!("Failed to delete preferences: {e}")))?;
        }

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteClientPreferenceRepository, init_database_memory};
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Mutex, Once, OnceLock};
    use tracing::Level;
    use tracing_subscriber::fmt;

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

    async fn setup() -> ClientPrefService {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        std::mem::forget(db);
        ClientPrefService::new(repo)
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
        let prefs = svc.get_preferences(None).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn update_and_get_boolean() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("system.closeToTray".into(), json!(true));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["system.closeToTray"], json!(true));
    }

    #[tokio::test]
    async fn update_and_get_number() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("pet.size".into(), json!(360));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["pet.size"], json!(360));
    }

    #[tokio::test]
    async fn update_and_get_string() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["theme"], json!("dark"));
    }

    #[tokio::test]
    async fn null_deletes_key() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(req).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("theme".into(), json!(null));
        svc.update_preferences(req2).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert!(!prefs.contains_key("theme"));
    }

    #[tokio::test]
    async fn get_by_keys_filters() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("a".into(), json!(1));
        req.insert("b".into(), json!(2));
        req.insert("c".into(), json!(3));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(Some(&["a", "c"])).await.unwrap();
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
                svc.update_preferences(req).await.unwrap();

                let _ = svc
                    .get_preferences(Some(&["appearance.secretTheme", "appearance.missing"]))
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
        svc.update_preferences(req1).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("k".into(), json!("v2"));
        svc.update_preferences(req2).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["k"], json!("v2"));
    }

    #[tokio::test]
    async fn empty_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("".into(), json!(true));
        let err = svc.update_preferences(req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn long_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("x".repeat(256), json!(true));
        let err = svc.update_preferences(req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn batch_mixed_upsert_and_delete() {
        let svc = setup().await;

        let mut setup_req = UpdateClientPreferencesRequest::new();
        setup_req.insert("keep".into(), json!(1));
        setup_req.insert("remove".into(), json!(2));
        svc.update_preferences(setup_req).await.unwrap();

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("remove".into(), json!(null));
        req.insert("new".into(), json!(3));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["keep"], json!(1));
        assert_eq!(prefs["new"], json!(3));
    }
}
