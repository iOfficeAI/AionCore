//! Single-instance advisory lock + per-run epoch (IC-1 defense / IC-3 naming).
//!
//! The lock file is a dedicated sidecar `{data_dir}/runtime/aionui-process/
//! instance.lock` — provably disjoint from bun's `runtime.lock`, the node
//! install lock, the db migrate lock, and the builtin-skills lock (IC-3). It
//! is acquired NON-BLOCKING (try-lock) and fails fast on contention, so a
//! second overlapping instance (auto-update) never silently hangs and — more
//! importantly — never reaps the live sibling's processes (the reap pass is
//! gated on holding this lock).
//!
//! Each successful acquisition also mints a fresh per-run `instance_epoch`
//! (random v4 UUID). Equality-only: a registry row whose epoch differs from
//! the current run is a prior-run entry. No persistence, no ordering — so NTP
//! / reinstall can never make a stale epoch look current (IC-1, design Dec 1).

use std::fs::File;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use uuid::Uuid;

use crate::registry_store::{LOCK_FILE, SUBDIR};

/// Held single-instance lock. Held for the whole process lifetime by keeping
/// this value alive; dropping it releases the advisory lock.
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Explicit unlock for deterministic release. Closing the File would
        // also release the flock, but on some platforms (macOS) the close-
        // triggered release can lag; an explicit unlock() makes re-acquire by
        // a successor deterministic.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Returned when the lock is already held by another instance.
#[derive(Debug)]
pub struct LockHeld {
    pub path: PathBuf,
}

impl std::fmt::Display for LockHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "instance lock already held: {}", self.path.display())
    }
}

/// Try to acquire the single-instance lock under `data_dir`. On success
/// returns the held lock + a freshly minted per-run epoch. On contention
/// returns `Err(LockHeld)` — the caller must NOT run any reap (IC-1: never
/// reap a live sibling instance's processes).
pub fn acquire_instance_lock(data_dir: &Path) -> Result<(InstanceLock, Uuid), LockHeld> {
    let dir = data_dir.join(SUBDIR);
    let path = dir.join(LOCK_FILE);
    // create_dir_all is idempotent + benign under races (IC: already-isolated).
    if std::fs::create_dir_all(&dir).is_err() {
        return Err(LockHeld { path });
    }
    // `File::create` sets `O_CLOEXEC` by default on unix (Rust std since 1.0),
    // so this advisory-lock fd is NOT inherited by spawned children / MCP
    // grandchildren. That matters: an inherited flock fd held by a surviving
    // grandchild would keep the lock "held" after the parent dies, making the
    // NEXT instance see contention and refuse to reap — defeating cold-reap.
    // CLOEXEC closes that hole for free; the `cloexec_lock_fd_not_inherited`
    // test guards against a future switch to a non-CLOEXEC open path.
    let file = match File::create(&path) {
        Ok(f) => f,
        Err(_) => return Err(LockHeld { path }),
    };
    // NON-BLOCKING: fail fast on contention, never block (IC-3).
    match file.try_lock_exclusive() {
        Ok(()) => Ok((InstanceLock { file, path }, Uuid::new_v4())),
        Err(_) => Err(LockHeld { path }),
    }
}
