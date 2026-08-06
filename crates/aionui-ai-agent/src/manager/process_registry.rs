use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use aionui_common::{AgentType, ErrorChain};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::capability::cli_process::CliAgentProcess;
use crate::error::AgentError;

pub(crate) const AGENT_PROCESS_REGISTRY_RELATIVE_PATH: &str = "runtime/agent-process-registry.json";
const AGENT_PROCESS_REGISTRY_LOCK_RELATIVE_PATH: &str = "runtime/agent-process-registry.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisteredAgentProcess {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    pub conversation_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_preview: Option<String>,
    pub registered_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessRegistry {
    version: u32,
    processes: Vec<RegisteredAgentProcess>,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self {
            version: 1,
            processes: Vec::new(),
        }
    }
}

static REGISTRY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn agent_process_registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join(AGENT_PROCESS_REGISTRY_RELATIVE_PATH)
}

pub(crate) fn register_session_process(
    data_dir: &Path,
    process: Arc<CliAgentProcess>,
    conversation_id: impl Into<String>,
    agent_type: AgentType,
    backend: Option<String>,
    command_preview: Option<String>,
) -> Result<(), AgentError> {
    let pid = process.pid();
    let process_group_id = process.process_group_id();
    let entry = RegisteredAgentProcess {
        pid,
        process_group_id,
        conversation_id: conversation_id.into(),
        agent_type: agent_type.serde_name().to_owned(),
        backend,
        command_preview,
        registered_at_ms: now_ms(),
    };

    register_agent_process(data_dir, entry).map_err(|e| {
        AgentError::internal(format!(
            "Failed to register agent process {pid} in runtime registry: {e}"
        ))
    })?;

    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        let _ = process.wait_for_exit().await;
        wait_for_process_tree_exit(pid, process_group_id).await;
        if let Err(e) = unregister_agent_process(&data_dir, pid) {
            warn!(
                pid,
                path = %agent_process_registry_path(&data_dir).display(),
                error = %ErrorChain(&e),
                "Failed to unregister exited agent process from runtime registry"
            );
        }
    });

    Ok(())
}

fn register_agent_process(data_dir: &Path, entry: RegisteredAgentProcess) -> io::Result<()> {
    with_registry_lock(data_dir, || {
        let path = agent_process_registry_path(data_dir);
        let mut registry = read_registry_file(&path)?;
        registry.processes.retain(|existing| existing.pid != entry.pid);
        registry.processes.push(entry);
        write_registry_file(&path, &registry)
    })
}

pub(crate) fn unregister_agent_process(data_dir: &Path, pid: u32) -> io::Result<()> {
    with_registry_lock(data_dir, || {
        let path = agent_process_registry_path(data_dir);
        let mut registry = read_registry_file(&path)?;
        let original_len = registry.processes.len();
        registry.processes.retain(|existing| existing.pid != pid);
        if registry.processes.len() == original_len {
            return Ok(());
        }
        write_registry_file(&path, &registry)
    })
}

fn read_registry_file(path: &Path) -> io::Result<ProcessRegistry> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ProcessRegistry::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_str(&contents) {
        Ok(registry) => Ok(registry),
        Err(e) => {
            // Fail-safe on corruption: this registry is pure bookkeeping for
            // orphan reaping, so a torn/empty file must not abort agent
            // startup. Quarantine the bad file for forensics and degrade to
            // an empty registry; the next successful write self-heals.
            warn!(
                path = %path.display(),
                error = %e,
                "agent process registry is corrupt; quarantining and degrading to empty registry"
            );
            quarantine_corrupt_registry(path);
            Ok(ProcessRegistry::default())
        }
    }
}

fn write_registry_file(path: &Path, registry: &ProcessRegistry) -> io::Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "registry path has no parent"))?;
    fs::create_dir_all(parent)?;

    let payload = serde_json::to_vec_pretty(registry).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize process registry {}: {e}", path.display()),
        )
    })?;

    // Temp file is namespaced to pid + counter so concurrent writers (two
    // aioncore backends sharing one data-dir) can never clobber each other's
    // in-flight temp — the fixed-name variant was one of the corruption
    // sources behind ELECTRON-3WN.
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent-process-registry.json");
    let tmp_path = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), next_counter()));

    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp_path, path).or_else(|e| {
        if cfg!(windows) {
            // Windows can refuse to rename over a concurrently open target;
            // retry once after a best-effort removal.
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
        } else {
            Err(e)
        }
    }) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Best-effort rename the corrupt registry aside so it is preserved for
/// forensics and a fresh one can be written. Failure is non-fatal — we still
/// degrade to an empty registry.
fn quarantine_corrupt_registry(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent-process-registry.json");
    let dst = parent.join(format!(".{stem}.corrupt.{}.{}", std::process::id(), next_counter()));
    if let Err(e) = fs::rename(path, &dst) {
        warn!(
            path = %path.display(),
            error = %e,
            "failed to quarantine corrupt agent process registry (will be overwritten on next write)"
        );
    }
}

fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn with_registry_lock<T>(data_dir: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    // Lock order (fixed): in-process Mutex (outer) → cross-process flock
    // (inner). Both locks are only taken here, so no opposite-order
    // acquisition and no cross-lock deadlock risk.
    let _guard = REGISTRY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    with_registry_flock(data_dir, f)
}

/// Run `f` while holding a BLOCKING cross-process exclusive lock, so a
/// sibling aioncore instance sharing the same data-dir cannot interleave its
/// read-modify-write and lose an entry. Degrade-not-fail: if the lock file
/// cannot be opened or locked, warn and run `f` unguarded — a lock fault must
/// not turn back into a send-blocking registration failure.
fn with_registry_flock<T>(data_dir: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    use fs2::FileExt;

    let lock_path = data_dir.join(AGENT_PROCESS_REGISTRY_LOCK_RELATIVE_PATH);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock_file = match fs::File::create(&lock_path) {
        Ok(file) => file,
        Err(e) => {
            warn!(
                path = %lock_path.display(),
                error = %e,
                "could not open agent process registry lock file; proceeding without cross-process guard"
            );
            return f();
        }
    };
    if let Err(e) = lock_file.lock_exclusive() {
        warn!(
            path = %lock_path.display(),
            error = %e,
            "could not lock agent process registry; proceeding without cross-process guard"
        );
        return f();
    }
    let result = f();
    // Explicit unlock for deterministic release (close-triggered release can
    // lag on some platforms).
    let _ = fs2::FileExt::unlock(&lock_file);
    result
}

async fn wait_for_process_tree_exit(pid: u32, process_group_id: Option<u32>) {
    while is_registered_process_tree_alive(pid, process_group_id) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn is_registered_process_tree_alive(pid: u32, process_group_id: Option<u32>) -> bool {
    process_group_id
        .filter(|group_id| *group_id > 1)
        .is_some_and(is_unix_process_group_alive)
        || is_unix_process_alive(pid)
}

#[cfg(unix)]
fn is_unix_process_group_alive(process_group_id: u32) -> bool {
    signal_zero(-(process_group_id as i32))
}

#[cfg(not(unix))]
fn is_unix_process_group_alive(_process_group_id: u32) -> bool {
    false
}

#[cfg(unix)]
fn is_unix_process_alive(pid: u32) -> bool {
    signal_zero(pid as i32)
}

#[cfg(not(unix))]
fn is_unix_process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn signal_zero(target: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let result = unsafe { kill(target, 0) };
    if result == 0 {
        return true;
    }

    !matches!(io::Error::last_os_error().raw_os_error(), Some(3))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(pid: u32) -> RegisteredAgentProcess {
        RegisteredAgentProcess {
            pid,
            process_group_id: None,
            conversation_id: format!("conv-{pid}"),
            agent_type: AgentType::Acp.serde_name().into(),
            backend: None,
            command_preview: None,
            registered_at_ms: 1,
        }
    }

    fn quarantine_file_names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .filter(|name| name.contains(".corrupt."))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn registry_path_is_scoped_under_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        assert_eq!(path, dir.path().join("runtime/agent-process-registry.json"));
    }

    #[test]
    fn unregister_is_idempotent_for_missing_pid() {
        let dir = tempfile::tempdir().unwrap();
        unregister_agent_process(dir.path(), 42).unwrap();
        let registry = read_registry_file(&agent_process_registry_path(dir.path())).unwrap();
        assert!(registry.processes.is_empty());
    }

    #[test]
    fn register_then_unregister_updates_registry_file() {
        let dir = tempfile::tempdir().unwrap();
        let entry = RegisteredAgentProcess {
            pid: 42,
            process_group_id: Some(42),
            conversation_id: "conv-1".into(),
            agent_type: AgentType::Acp.serde_name().into(),
            backend: Some("codex".into()),
            command_preview: Some("codex-acp".into()),
            registered_at_ms: 123,
        };

        register_agent_process(dir.path(), entry.clone()).unwrap();
        let path = agent_process_registry_path(dir.path());
        let registry = read_registry_file(&path).unwrap();
        assert_eq!(registry.processes, vec![entry]);

        unregister_agent_process(dir.path(), 42).unwrap();
        let registry = read_registry_file(&path).unwrap();
        assert!(registry.processes.is_empty());
    }

    #[test]
    fn register_degrades_and_quarantines_on_empty_registry_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"").unwrap();

        register_agent_process(dir.path(), test_entry(42)).unwrap();

        let registry = read_registry_file(&path).unwrap();
        assert_eq!(registry.processes.len(), 1);
        assert_eq!(registry.processes[0].pid, 42);
        assert_eq!(quarantine_file_names(path.parent().unwrap()).len(), 1);
    }

    #[test]
    fn register_degrades_and_quarantines_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":"#).unwrap();

        register_agent_process(dir.path(), test_entry(7)).unwrap();

        let registry = read_registry_file(&path).unwrap();
        assert_eq!(registry.processes.len(), 1);
        assert_eq!(quarantine_file_names(path.parent().unwrap()).len(), 1);
    }

    #[test]
    fn unregister_degrades_on_corrupt_registry_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":"#).unwrap();

        unregister_agent_process(dir.path(), 42).unwrap();
        assert_eq!(quarantine_file_names(path.parent().unwrap()).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn read_propagates_real_io_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":1,"processes":[]}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let err = read_registry_file(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn write_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        register_agent_process(dir.path(), test_entry(1)).unwrap();

        let path = agent_process_registry_path(dir.path());
        let leftovers: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
        // Namespaced temp naming: the fixed-name variant `path.with_extension("tmp")`
        // must not be produced anymore.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn concurrent_register_unregister_keeps_registry_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..8u32)
            .map(|t| {
                let data_dir = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    for i in 0..25u32 {
                        let pid = t * 100 + i + 1;
                        register_agent_process(&data_dir, test_entry(pid)).unwrap();
                        if i % 2 == 0 {
                            unregister_agent_process(&data_dir, pid).unwrap();
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let path = agent_process_registry_path(dir.path());
        let registry = read_registry_file(&path).unwrap();
        assert_eq!(registry.processes.len(), 8 * 12); // per thread: 13 even-i pids unregistered, 12 odd-i pids survive
        assert!(
            quarantine_file_names(path.parent().unwrap()).is_empty(),
            "concurrent writes corrupted the registry"
        );
    }
}
