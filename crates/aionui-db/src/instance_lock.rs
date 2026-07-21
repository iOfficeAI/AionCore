//! Data-dir process-level instance guard (Sentry 135525166 Option A).
//!
//! A long-lived, non-blocking exclusive `flock` on a lock file next to the
//! database file. The winning aioncore holds it for its whole lifetime so a
//! second aioncore yields structurally before touching the database or binding
//! a port, instead of racing the assistant bootstrap over the same data dir.
//!
//! This is distinct from the migration lock (`*.migrate.lock`, acquired and
//! released within a single migration run). The kernel releases `flock`
//! automatically on process death, so no pid file, staleness GC, or liveness
//! probe is needed (see the fix design notes).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// Path of the data-dir process-level instance lock file.
///
/// Placed next to the DB file so it lives on the same filesystem (avoids odd
/// `flock` semantics across mount points) and is cleaned up alongside the DB if
/// the user resets their data directory. Distinct from `*.migrate.lock`.
pub fn instance_lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let new_name = match p.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.instance.lock"),
        None => "aionui.instance.lock".to_string(),
    };
    p.set_file_name(new_name);
    p
}

/// RAII guard holding an exclusive `flock` on the data-dir instance lock file
/// for the lifetime of the process. Drop unlocks (best-effort) and closes the
/// handle; the kernel also releases the lock automatically on process exit.
#[derive(Debug)]
pub struct DataDirInstanceGuard {
    file: std::fs::File,
}

impl DataDirInstanceGuard {
    /// Attempt to acquire the guard without blocking.
    ///
    /// - `Ok(Some(guard))` — this process is the winner and now owns the data dir.
    /// - `Ok(None)` — a peer already holds the lock (would-block).
    /// - `Err(e)` — `flock` is unavailable (e.g. some network filesystems). The
    ///   caller proceeds without the structural guard and relies on the
    ///   bootstrap concurrency safety (Option B) as the last line of defence.
    pub fn try_acquire(db_path: &Path) -> std::io::Result<Option<Self>> {
        let lock_path = instance_lock_path(db_path);
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(e) if is_would_block(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for DataDirInstanceGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Whether a `try_lock_exclusive` error means "another process holds the lock"
/// (contention) rather than a real I/O failure. Rust maps EWOULDBLOCK/EAGAIN to
/// `ErrorKind::WouldBlock`; `fs2::lock_contended_error()` gives the exact
/// platform-specific contention error (e.g. ERROR_LOCK_VIOLATION on Windows).
fn is_would_block(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_lock_path_appends_instance_lock_suffix() {
        // Mirrors migrate_lock_path: the suffix is appended to the full file
        // name (kept next to the DB, on the same filesystem).
        let path = instance_lock_path(Path::new("/data/aionui/aionui-backend.db"));
        assert_eq!(path, PathBuf::from("/data/aionui/aionui-backend.db.instance.lock"));
    }

    #[test]
    fn guard_is_exclusive_and_releases_on_drop() {
        let dir = std::env::temp_dir().join(format!("aionui-instance-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aionui-backend.db");

        // First acquisition wins.
        let guard = DataDirInstanceGuard::try_acquire(&db_path)
            .expect("try_acquire should not error on a local filesystem")
            .expect("first acquisition should win");

        // While held, a second acquisition yields (would-block).
        let contended =
            DataDirInstanceGuard::try_acquire(&db_path).expect("try_acquire should not error while contended");
        assert!(
            contended.is_none(),
            "second acquisition must yield while the guard is held"
        );

        // After dropping the guard, acquisition succeeds again (auto-release).
        drop(guard);
        let reacquired = DataDirInstanceGuard::try_acquire(&db_path)
            .expect("try_acquire should not error after release")
            .expect("acquisition should succeed after the guard is dropped");
        drop(reacquired);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
