use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::warn;

use crate::error::FileError;
use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;

use crate::types::{FileWatchEvent, OfficeFileAddedEvent};

/// Debounce duration for file watch events.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(200);

/// Office file extensions to match (lowercase).
const OFFICE_EXTENSIONS: &[&str] = &["pptx", "docx", "xlsx"];

// ---------------------------------------------------------------------------
// Pure helpers (testable without I/O)
// ---------------------------------------------------------------------------

/// Returns `true` if the file path has an Office document extension.
fn is_office_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        let lower = ext.to_ascii_lowercase();
        OFFICE_EXTENSIONS.contains(&lower.as_str())
    })
}

/// Maps a `notify::EventKind` to a human-readable event type string.
/// Returns `None` for events that should be silently skipped (e.g. access).
fn event_kind_to_str(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Modify(_) => Some("change"),
        EventKind::Create(_) => Some("create"),
        EventKind::Remove(_) => Some("remove"),
        EventKind::Any | EventKind::Other => Some("change"),
        EventKind::Access(_) => None,
    }
}

/// Returns `true` if enough time has elapsed since the last event for `key`.
/// Updates the timestamp when returning `true`.
fn should_emit(debounce: &DashMap<String, Instant>, key: &str) -> bool {
    let now = Instant::now();
    if let Some(last) = debounce.get(key)
        && now.duration_since(*last) < DEBOUNCE_DURATION
    {
        return false;
    }
    debounce.insert(key.to_owned(), now);
    true
}

// ---------------------------------------------------------------------------
// FileWatchService
// ---------------------------------------------------------------------------

/// File-system watcher implementing [`crate::traits::IFileWatchService`].
///
/// Internally uses the `notify` crate for cross-platform file-system events.
///
/// - **Single-file watches** share one [`RecommendedWatcher`] instance; each
///   path is registered via `watch()` with [`RecursiveMode::NonRecursive`].
/// - **Workspace Office watches** each get their own watcher running in
///   [`RecursiveMode::Recursive`], filtering for `.pptx`/`.docx`/`.xlsx`
///   creation events.
pub struct FileWatchService {
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Shared watcher for all single-file watches.
    file_watcher: Mutex<RecommendedWatcher>,
    /// Canonical watched paths and the users subscribed to each path.
    watched_files: Arc<DashMap<String, BTreeSet<String>>>,
    /// Per-workspace Office watchers, keyed by canonical workspace path.
    office_watchers: Mutex<HashMap<String, RecommendedWatcher>>,
    /// Canonical Office workspaces and the users subscribed to each workspace.
    office_watch_users: Arc<DashMap<String, BTreeSet<String>>>,
    /// Debounce timestamps shared with watcher callbacks.
    debounce: Arc<DashMap<String, Instant>>,
}

impl FileWatchService {
    /// Create a new watch service backed by the platform's recommended watcher.
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Result<Self, FileError> {
        let watched_files: Arc<DashMap<String, BTreeSet<String>>> = Arc::new(DashMap::new());
        let office_watch_users: Arc<DashMap<String, BTreeSet<String>>> = Arc::new(DashMap::new());
        let debounce: Arc<DashMap<String, Instant>> = Arc::new(DashMap::new());

        let bc = broadcaster.clone();
        let wf = watched_files.clone();
        let db = debounce.clone();

        let file_watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "file watcher error");
                    return;
                }
            };

            let event_type = match event_kind_to_str(&event.kind) {
                Some(t) => t,
                None => return,
            };

            for path in &event.paths {
                let path_str = path.to_string_lossy().into_owned();
                let Some(users) = wf.get(&path_str).map(|entry| entry.clone()) else {
                    continue;
                };
                if !should_emit(&db, &path_str) {
                    continue;
                }
                for user_id in users {
                    let payload = FileWatchEvent {
                        file_path: path_str.clone(),
                        event_type: event_type.to_owned(),
                    };
                    let mut json = serde_json::to_value(&payload).unwrap_or_default();
                    json["user_id"] = serde_json::Value::String(user_id);
                    bc.broadcast(WebSocketMessage::new("fileWatch.fileChanged", json));
                }
            }
        })
        .map_err(|e| FileError::Internal(format!("failed to create file watcher: {e}")))?;

        Ok(Self {
            broadcaster,
            file_watcher: Mutex::new(file_watcher),
            watched_files,
            office_watchers: Mutex::new(HashMap::new()),
            office_watch_users,
            debounce,
        })
    }
}

#[async_trait::async_trait]
impl crate::traits::IFileWatchService for FileWatchService {
    async fn start_watch(&self, file_path: &str) -> Result<(), FileError> {
        self.start_watch_for_user("system_default_user", file_path).await
    }

    async fn start_watch_for_user(&self, user_id: &str, file_path: &str) -> Result<(), FileError> {
        let canonical = std::fs::canonicalize(file_path)
            .map_err(|e| FileError::NotFound(format!("cannot resolve path {file_path}: {e}")))?;
        let key = canonical.to_string_lossy().into_owned();

        if let Some(mut users) = self.watched_files.get_mut(&key) {
            users.insert(user_id.to_owned());
            return Ok(());
        }

        let mut watcher = self
            .file_watcher
            .lock()
            .map_err(|e| FileError::Internal(format!("file watcher lock poisoned: {e}")))?;
        watcher
            .watch(&canonical, RecursiveMode::NonRecursive)
            .map_err(|e| FileError::Internal(format!("failed to watch {file_path}: {e}")))?;
        self.watched_files.insert(key, BTreeSet::from([user_id.to_owned()]));
        Ok(())
    }

    async fn stop_watch(&self, file_path: &str) -> Result<(), FileError> {
        self.stop_watch_for_user("system_default_user", file_path).await
    }

    async fn stop_watch_for_user(&self, user_id: &str, file_path: &str) -> Result<(), FileError> {
        let canonical = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.into());
        let key = canonical.to_string_lossy().into_owned();

        let should_unwatch = if let Some(mut users) = self.watched_files.get_mut(&key) {
            users.remove(user_id);
            users.is_empty()
        } else {
            false
        };

        if should_unwatch {
            self.watched_files.remove(&key);
            let mut watcher = self
                .file_watcher
                .lock()
                .map_err(|e| FileError::Internal(format!("file watcher lock poisoned: {e}")))?;
            // Ignore unwatch errors because the file may have been deleted.
            let _ = watcher.unwatch(&canonical);
            self.debounce.remove(&key);
        }

        Ok(())
    }

    async fn stop_all_watches(&self) -> Result<(), FileError> {
        let mut watcher = self
            .file_watcher
            .lock()
            .map_err(|e| FileError::Internal(format!("file watcher lock poisoned: {e}")))?;

        for entry in self.watched_files.iter() {
            let path = std::path::PathBuf::from(entry.key().as_str());
            let _ = watcher.unwatch(&path);
        }
        self.watched_files.clear();
        // Clean file-watch debounce entries only (keep office ones).
        self.debounce.retain(|k, _| k.starts_with("office:"));
        Ok(())
    }

    async fn stop_all_watches_for_user(&self, user_id: &str) -> Result<(), FileError> {
        let keys: Vec<String> = self.watched_files.iter().map(|entry| entry.key().clone()).collect();
        for key in keys {
            self.stop_watch_for_user(user_id, &key).await?;
        }
        Ok(())
    }

    async fn start_office_watch(&self, workspace: &str) -> Result<(), FileError> {
        self.start_office_watch_for_user("system_default_user", workspace).await
    }

    async fn start_office_watch_for_user(&self, user_id: &str, workspace: &str) -> Result<(), FileError> {
        let canonical = std::fs::canonicalize(workspace)
            .map_err(|e| FileError::NotFound(format!("cannot resolve workspace {workspace}: {e}")))?;
        let key = canonical.to_string_lossy().into_owned();

        {
            let watchers = self
                .office_watchers
                .lock()
                .map_err(|e| FileError::Internal(format!("office watcher lock poisoned: {e}")))?;
            if watchers.contains_key(&key) {
                self.office_watch_users
                    .entry(key)
                    .or_default()
                    .insert(user_id.to_owned());
                return Ok(());
            }
        }

        let bc = self.broadcaster.clone();
        let db = self.debounce.clone();
        let ws = key.clone();
        let office_users = self.office_watch_users.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "office watcher error");
                    return;
                }
            };

            if !matches!(event.kind, EventKind::Create(_)) {
                return;
            }

            for path in &event.paths {
                if !is_office_file(path) {
                    continue;
                }
                let path_str = path.to_string_lossy().into_owned();
                let debounce_key = format!("office:{path_str}");
                let Some(users) = office_users.get(&ws).map(|entry| entry.clone()) else {
                    continue;
                };
                if !should_emit(&db, &debounce_key) {
                    continue;
                }
                for user_id in users {
                    let payload = OfficeFileAddedEvent {
                        file_path: path_str.clone(),
                        workspace: ws.clone(),
                    };
                    let mut json = serde_json::to_value(&payload).unwrap_or_default();
                    json["user_id"] = serde_json::Value::String(user_id);
                    bc.broadcast(WebSocketMessage::new("workspaceOfficeWatch.fileAdded", json));
                }
            }
        })
        .map_err(|e| FileError::Internal(format!("failed to create office watcher: {e}")))?;

        watcher
            .watch(&canonical, RecursiveMode::Recursive)
            .map_err(|e| FileError::Internal(format!("failed to watch workspace {workspace}: {e}")))?;

        let mut watchers = self
            .office_watchers
            .lock()
            .map_err(|e| FileError::Internal(format!("office watcher lock poisoned: {e}")))?;
        watchers.insert(key.clone(), watcher);
        self.office_watch_users
            .insert(key, BTreeSet::from([user_id.to_owned()]));
        Ok(())
    }

    async fn stop_office_watch(&self, workspace: &str) -> Result<(), FileError> {
        self.stop_office_watch_for_user("system_default_user", workspace).await
    }

    async fn stop_office_watch_for_user(&self, user_id: &str, workspace: &str) -> Result<(), FileError> {
        let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.into());
        let key = canonical.to_string_lossy().into_owned();

        let should_stop = if let Some(mut users) = self.office_watch_users.get_mut(&key) {
            users.remove(user_id);
            users.is_empty()
        } else {
            false
        };

        if !should_stop {
            return Ok(());
        }

        self.office_watch_users.remove(&key);
        let mut watchers = self
            .office_watchers
            .lock()
            .map_err(|e| FileError::Internal(format!("office watcher lock poisoned: {e}")))?;
        // Dropping the watcher stops watching.
        watchers.remove(&key);
        Ok(())
    }

    async fn stop_all_office_watches_for_user(&self, user_id: &str) -> Result<(), FileError> {
        let keys: Vec<String> = self
            .office_watch_users
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for key in keys {
            self.stop_office_watch_for_user(user_id, &key).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};
    use std::path::PathBuf;

    // -- is_office_file --

    #[test]
    fn office_file_pptx() {
        assert!(is_office_file(Path::new("/ws/slides.pptx")));
    }

    #[test]
    fn office_file_docx() {
        assert!(is_office_file(Path::new("/ws/report.docx")));
    }

    #[test]
    fn office_file_xlsx() {
        assert!(is_office_file(Path::new("/ws/data.xlsx")));
    }

    #[test]
    fn office_file_case_insensitive() {
        assert!(is_office_file(Path::new("/ws/FILE.PPTX")));
        assert!(is_office_file(Path::new("/ws/Doc.Docx")));
    }

    #[test]
    fn non_office_file_txt() {
        assert!(!is_office_file(Path::new("/ws/readme.txt")));
    }

    #[test]
    fn non_office_file_pdf() {
        assert!(!is_office_file(Path::new("/ws/paper.pdf")));
    }

    #[test]
    fn no_extension() {
        assert!(!is_office_file(Path::new("/ws/Makefile")));
    }

    // -- event_kind_to_str --

    #[test]
    fn modify_event_maps_to_change() {
        assert_eq!(
            event_kind_to_str(&EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content))),
            Some("change")
        );
    }

    #[test]
    fn create_event_maps_to_create() {
        assert_eq!(event_kind_to_str(&EventKind::Create(CreateKind::File)), Some("create"));
    }

    #[test]
    fn remove_event_maps_to_remove() {
        assert_eq!(event_kind_to_str(&EventKind::Remove(RemoveKind::File)), Some("remove"));
    }

    #[test]
    fn any_event_maps_to_change() {
        assert_eq!(event_kind_to_str(&EventKind::Any), Some("change"));
    }

    #[test]
    fn other_event_maps_to_change() {
        assert_eq!(event_kind_to_str(&EventKind::Other), Some("change"));
    }

    #[test]
    fn access_event_is_skipped() {
        assert_eq!(event_kind_to_str(&EventKind::Access(AccessKind::Read)), None);
    }

    // -- should_emit (debounce) --

    #[test]
    fn first_emit_returns_true() {
        let db = DashMap::new();
        assert!(should_emit(&db, "/tmp/a.txt"));
    }

    #[test]
    fn immediate_second_emit_returns_false() {
        let db = DashMap::new();
        assert!(should_emit(&db, "/tmp/a.txt"));
        assert!(!should_emit(&db, "/tmp/a.txt"));
    }

    #[test]
    fn different_keys_are_independent() {
        let db = DashMap::new();
        assert!(should_emit(&db, "/tmp/a.txt"));
        assert!(should_emit(&db, "/tmp/b.txt"));
    }

    #[test]
    fn emit_after_debounce_duration() {
        let db = DashMap::new();
        assert!(should_emit(&db, "/tmp/a.txt"));

        // Simulate time passing by manually backdating the entry.
        db.insert(
            "/tmp/a.txt".to_owned(),
            Instant::now() - DEBOUNCE_DURATION - Duration::from_millis(1),
        );
        assert!(should_emit(&db, "/tmp/a.txt"));
    }

    // -- is_office_file edge cases --

    #[test]
    fn dotfile_with_office_ext() {
        assert!(is_office_file(Path::new("/ws/.hidden.docx")));
    }

    #[test]
    fn nested_path_office_file() {
        assert!(is_office_file(Path::new("/ws/deep/nested/dir/report.xlsx")));
    }

    #[test]
    fn empty_path() {
        assert!(!is_office_file(&PathBuf::new()));
    }
}
