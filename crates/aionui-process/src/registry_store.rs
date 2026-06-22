//! Persisted registry of processes THIS crate spawned (IC-4).
//!
//! Lives in its own subdir `{data_dir}/runtime/aionui-process/registry.json`,
//! never touching the existing `agent-process-registry.json`. Written via a
//! durable atomic write whose temp file is namespaced to this crate + pid, so
//! it can never clobber another mechanism's temp. Accessed by exact path only
//! — never by directory scan/glob.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ProcessError;

/// Subdir + filenames, namespaced so they are provably disjoint from the
/// existing mechanism's artifacts and from bun's `runtime.lock` (IC-3/IC-4).
pub const SUBDIR: &str = "runtime/aionui-process";
pub const REGISTRY_FILE: &str = "registry.json";
pub const LOCK_FILE: &str = "instance.lock";
/// Cross-process advisory lock for the registry read-modify-write (F49).
/// DELIBERATELY a SEPARATE file from `LOCK_FILE` (`instance.lock`): the
/// single-instance lock is held NON-BLOCKING for the whole process lifetime
/// (it gates reap), whereas this one is taken BLOCKING for the duration of a
/// single millisecond-scale RMW. Reusing the same file would self-deadlock —
/// the process already holds `instance.lock` exclusively for its whole life.
pub const REGISTRY_LOCK_FILE: &str = "registry.json.lock";

/// One process this crate spawned. Identity fields (`start_time_ticks` +
/// `instance_epoch`) back the IC-1 kill gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredProcess {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<u32>,
    /// Kernel start-time in clock ticks (identity gate). None if unobtainable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<u64>,
    /// This-run UUID; a row whose epoch != current run is a prior-run orphan.
    pub instance_epoch: Uuid,
    /// Host identity; a row from another machine (cloud-synced data dir) is
    /// prune-only, never killed (IC-1 cross-machine guard, design I-5).
    pub machine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_id: Option<String>,
    /// Opaque owner tag (e.g. a conversation id); this layer never parses it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub opaque_owner_tag: String,
    pub registered_at_ms: i64,
}

/// Identity key for `unregister` — by (pid, start_time, epoch), not bare pid,
/// so a recycled-pid row is never accidentally removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ticks: Option<u64>,
    pub instance_epoch: Uuid,
}

impl RegisteredProcess {
    fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_time_ticks: self.start_time_ticks,
            instance_epoch: self.instance_epoch,
        }
    }
}

/// The on-disk registry schema version this build writes and understands.
const CURRENT_REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    /// `#[serde(default)]` so a registry missing the field (older/hand-edited)
    /// is NOT a hard parse error that aborts the reap (F55 + ties into F39);
    /// it defaults to the current version instead.
    #[serde(default = "default_registry_version")]
    version: u32,
    processes: Vec<RegisteredProcess>,
}

fn default_registry_version() -> u32 {
    CURRENT_REGISTRY_VERSION
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: CURRENT_REGISTRY_VERSION,
            processes: Vec::new(),
        }
    }
}

/// Persisted store of this crate's spawned processes.
pub trait RegistryStore: Send + Sync {
    fn record(&self, entry: RegisteredProcess) -> Result<(), ProcessError>;
    /// Remove by full identity (recycled-pid rows with a different identity
    /// are left intact).
    fn unregister(&self, id: &ProcessIdentity) -> Result<(), ProcessError>;
    /// The production reader — read all rows back (used by startup reap).
    fn read_all(&self) -> Result<Vec<RegisteredProcess>, ProcessError>;
}

/// File-backed registry under `{data_dir}/runtime/aionui-process/registry.json`.
pub struct FileRegistryStore {
    path: PathBuf,
    /// Sidecar path for the cross-process RMW lock (F49). See
    /// [`REGISTRY_LOCK_FILE`]; a separate file from `instance.lock`.
    lock_path: PathBuf,
    /// Serializes read-modify-write within this process. `atomic_write` makes
    /// only the final rename atomic; without this, two concurrent `record`s
    /// in one process could lose a row (last-writer-wins) — leaking a spawned-
    /// but-unrecorded, un-reapable orphan. Cross-process writers are serialized
    /// by the additional fs lock in [`Self::with_rmw_flock`] (F49).
    rmw: std::sync::Mutex<()>,
}

impl FileRegistryStore {
    pub fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join(SUBDIR);
        let store = Self {
            path: dir.join(REGISTRY_FILE),
            lock_path: dir.join(REGISTRY_LOCK_FILE),
            rmw: std::sync::Mutex::new(()),
        };
        // F51: best-effort sweep of stray atomic-write temp files left by a
        // crash/SIGKILL between fsync and rename. We scan ONLY this crate's own
        // subdir and match ONLY our own exact temp prefix (`.registry.json.` +
        // `.corrupt.`) — never a broad `*.tmp`/`*.json` glob (IC-4: never touch
        // another mechanism's artifacts). Without this the design's
        // "by-exact-path-only" rule means strays can NEVER be GC'd.
        store.sweep_stray_temps();
        store
    }

    /// Remove THIS PROCESS's own orphaned `.registry.json.<our_pid>.<n>.tmp`
    /// strays from a prior crash. PID-SCOPED (F51 review fix): the temp name is
    /// `.{stem}.{pid}.{counter}.tmp`, so we only sweep temps carrying OUR pid —
    /// never a SIBLING instance's in-flight atomic-write temp (deleting that
    /// would make the sibling's `rename` fail NotFound and silently lose a row →
    /// an un-reapable orphan). A stale temp from an OLD run that happens to share
    /// our recycled pid is the only (harmless) over-match, and it is genuinely a
    /// stray. Deliberately does NOT touch `.corrupt.` quarantine files (forensics,
    /// F39).
    fn sweep_stray_temps(&self) {
        let Some(parent) = self.path.parent() else { return };
        let stem = self.path.file_name().and_then(|n| n.to_str()).unwrap_or(REGISTRY_FILE);
        // Pid-scoped prefix: only our own process's temps.
        let our_prefix = format!(".{stem}.{}.", std::process::id());
        let Ok(entries) = std::fs::read_dir(parent) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&our_prefix) && name.ends_with(".tmp") && !name.contains(".corrupt.") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the registry, FAIL-SAFE on corruption (F39). A truncated / malformed
    /// registry.json (e.g. an interrupted prior `atomic_write` that never reached
    /// rename, or a stray byte) must NOT abort the whole reap — aborting would
    /// leak every real orphan from the prior crash. Instead we QUARANTINE the bad
    /// file (rename it aside for forensics, non-destructively) and degrade to an
    /// empty registry: this run reaps nothing (safe — killing pids read from
    /// corrupt data is strictly more dangerous than skipping a round), and the
    /// next `write_file` starts clean (self-heal). A genuine I/O error (not a
    /// parse error) still propagates — that is an environment fault, not data we
    /// can safely ignore.
    fn read_file(&self) -> Result<RegistryFile, ProcessError> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RegistryFile::default()),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str::<RegistryFile>(&contents) {
            Ok(reg) if reg.version > CURRENT_REGISTRY_VERSION => {
                // F55: a registry written by a FUTURE build may have renamed /
                // repurposed fields. Feeding it into reconcile could group-kill on
                // stale field meanings. Degrade to empty (reap nothing this round
                // — safe) rather than trust forward-incompatible data. Do NOT
                // quarantine: a newer sibling owns that file legitimately.
                tracing::warn!(
                    path = %self.path.display(),
                    found = reg.version,
                    understood = CURRENT_REGISTRY_VERSION,
                    "registry schema version is newer than this build understands; degrading to empty (reap skipped)"
                );
                Ok(RegistryFile::default())
            }
            Ok(reg) if reg.version < CURRENT_REGISTRY_VERSION => {
                // F55 (asymmetry made explicit, per review): a registry written
                // by an OLDER build. This build understands all prior schemas by
                // construction (fields are serde-default-tolerant + only additive
                // changes are allowed across versions), so an older registry is
                // SAFE to trust and reap from — unlike a newer one (above) whose
                // field meanings we can't know. We log it for observability but
                // proceed. If a future version ever makes a BREAKING change, this
                // arm must change to a migration/degrade instead of blind trust.
                tracing::debug!(
                    path = %self.path.display(),
                    found = reg.version,
                    understood = CURRENT_REGISTRY_VERSION,
                    "registry schema version is older than current; trusting (backward-compatible additive schema)"
                );
                Ok(reg)
            }
            Ok(reg) => Ok(reg),
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "registry is corrupt/unparseable; quarantining and degrading to empty (reap skipped this round)"
                );
                self.quarantine_corrupt();
                Ok(RegistryFile::default())
            }
        }
    }

    /// Best-effort rename the corrupt registry aside so it is preserved for
    /// forensics and a fresh one can be written. Namespaced to this crate's
    /// subdir + pid/counter so it never collides; failure here is non-fatal
    /// (we still degrade to empty).
    fn quarantine_corrupt(&self) {
        let stem = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("registry.json");
        if let Some(parent) = self.path.parent() {
            let dst = parent.join(format!(".{stem}.corrupt.{}.{}", std::process::id(), next_counter()));
            if let Err(e) = std::fs::rename(&self.path, &dst) {
                tracing::warn!(error = %e, "failed to quarantine corrupt registry (will be overwritten on next write)");
            }
        }
    }

    fn write_file(&self, reg: &RegistryFile) -> Result<(), ProcessError> {
        let bytes =
            serde_json::to_vec_pretty(reg).map_err(|e| ProcessError::internal(format!("serialize registry: {e}")))?;
        atomic_write(&self.path, &bytes)
    }

    /// Run `f` while holding the CROSS-PROCESS registry lock (F49), so a
    /// concurrent sibling instance cannot interleave its own read-modify-write
    /// and lose a row (last-writer-wins). `atomic_write` only makes the final
    /// rename atomic — it does NOT prevent two processes each reading the same
    /// N rows, each appending one, and the second `rename` clobbering the
    /// first's row. A BLOCKING `flock` around the whole RMW serializes that.
    ///
    /// Degrade-not-fail: if the lock file cannot be opened or locked (rare I/O
    /// fault), we WARN and still run `f` — the cross-process guard is an
    /// enhancement over the always-present in-process `rmw` Mutex; failing the
    /// `record`/`unregister` outright would be a worse regression than the
    /// single-instance behavior we had before F49.
    fn with_rmw_flock<T>(&self, f: impl FnOnce() -> Result<T, ProcessError>) -> Result<T, ProcessError> {
        use fs2::FileExt;
        if let Some(parent) = self.lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // `File::create` sets O_CLOEXEC on unix by default, so this advisory-lock
        // fd is NOT inherited by spawned children (same reasoning as
        // instance_lock.rs) — an inherited lock fd could keep the lock "held"
        // after we exit.
        let lock_file = match std::fs::File::create(&self.lock_path) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(
                    path = %self.lock_path.display(),
                    error = %e,
                    "could not open registry lock file; proceeding without cross-process guard (F49 degraded)"
                );
                return f();
            }
        };
        // BLOCKING exclusive lock for the whole RMW (≠ instance.lock's
        // non-blocking try_lock): we WANT to wait out a sibling's in-flight RMW,
        // not bail. RMW is millisecond-scale so contention is brief.
        if let Err(e) = lock_file.lock_exclusive() {
            tracing::warn!(
                path = %self.lock_path.display(),
                error = %e,
                "could not acquire registry lock; proceeding without cross-process guard (F49 degraded)"
            );
            return f();
        }
        let result = f();
        // Explicit unlock for deterministic release (closing the File would also
        // release it, but on some platforms the close-triggered release can lag).
        let _ = fs2::FileExt::unlock(&lock_file);
        result
    }
}

impl RegistryStore for FileRegistryStore {
    fn record(&self, entry: RegisteredProcess) -> Result<(), ProcessError> {
        // Lock order (fixed, both RMW methods): in-process `rmw` Mutex (outer)
        // → cross-process flock (inner). This crate takes both locks at exactly
        // these two sites only, so there is no opposite-order acquisition and
        // thus no cross-lock deadlock risk.
        let _guard = self.rmw.lock().unwrap_or_else(|e| e.into_inner());
        self.with_rmw_flock(|| {
            let mut reg = self.read_file()?;
            reg.processes.retain(|p| p.identity() != entry.identity());
            reg.processes.push(entry);
            self.write_file(&reg)
        })
    }

    fn unregister(&self, id: &ProcessIdentity) -> Result<(), ProcessError> {
        let _guard = self.rmw.lock().unwrap_or_else(|e| e.into_inner());
        self.with_rmw_flock(|| {
            let mut reg = self.read_file()?;
            let before = reg.processes.len();
            reg.processes.retain(|p| &p.identity() != id);
            if reg.processes.len() == before {
                return Ok(()); // nothing matched — idempotent
            }
            self.write_file(&reg)
        })
    }

    fn read_all(&self) -> Result<Vec<RegisteredProcess>, ProcessError> {
        Ok(self.read_file()?.processes)
    }
}

/// Durable atomic write into this crate's subdir. Temp file is namespaced to
/// the final path + pid + counter so it cannot collide with another
/// mechanism's temp or with concurrent writers (IC-4). Best-effort dir fsync.
/// `pub(crate)` so other durable artifacts (the machine-id, F40) reuse the same
/// temp+fsync+rename discipline instead of a torn `std::fs::write`.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProcessError> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| ProcessError::internal("registry path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("registry.json");
    let tmp = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), next_counter()));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path).or_else(|e| {
        if cfg!(windows) {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
        } else {
            Err(e)
        }
    }) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    //! Persistence-layer tests against a real `FileRegistryStore` rooted in a
    //! `tempfile::TempDir` (single-process; the cross-process RMW flock race is
    //! a concurrency-harness gap, see audit PERS-P10/P20). Covers identity-keyed
    //! record/unregister, version compatibility, corruption quarantine, and the
    //! atomic-write durability discipline.

    use std::path::Path;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;

    fn store() -> (TempDir, FileRegistryStore) {
        let tmp = TempDir::new().unwrap();
        let store = FileRegistryStore::new(tmp.path());
        (tmp, store)
    }

    fn proc(pid: u32, start: u64, epoch: u128) -> RegisteredProcess {
        RegisteredProcess {
            pid,
            pgid: Some(pid),
            start_time_ticks: Some(start),
            instance_epoch: Uuid::from_u128(epoch),
            machine_id: "m".into(),
            containment_id: None,
            opaque_owner_tag: String::new(),
            registered_at_ms: 0,
        }
    }

    /// PERS-P9: `record` is idempotent on FULL identity — recording the same
    /// (pid, start_time, epoch) twice leaves exactly one row (retain-then-push
    /// dedups), never two.
    #[test]
    fn record_is_idempotent_on_identical_identity() {
        let (_tmp, store) = store();
        store.record(proc(100, 1000, 0xA)).unwrap();
        store.record(proc(100, 1000, 0xA)).unwrap();
        assert_eq!(
            store.read_all().unwrap().len(),
            1,
            "identical identity dedups to one row"
        );
    }

    /// PERS-P9 (identity gate, the load-bearing half): a SAME-pid row with a
    /// DIFFERENT start_time is a DIFFERENT identity (a recycled pid = a genuinely
    /// different process), so BOTH rows are kept. This is the whole point of
    /// keying on start_time, not bare pid — recording must NOT collapse them.
    #[test]
    fn record_keeps_both_rows_for_recycled_pid_different_start() {
        let (_tmp, store) = store();
        store.record(proc(100, 1000, 0xA)).unwrap();
        store.record(proc(100, 2000, 0xA)).unwrap();
        let rows = store.read_all().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "recycled pid (diff start_time) is a distinct identity → both kept"
        );
        let starts: Vec<Option<u64>> = rows.iter().map(|r| r.start_time_ticks).collect();
        assert!(starts.contains(&Some(1000)) && starts.contains(&Some(2000)));
    }

    /// PERS-P8: `unregister` matches by FULL identity. A mismatched start_time
    /// must NOT remove the row (defends against recycled-pid mis-deletion);
    /// an exact identity match removes it. `unregister` is idempotent when
    /// nothing matches.
    #[test]
    fn unregister_requires_exact_identity_match() {
        let (_tmp, store) = store();
        store.record(proc(100, 1000, 0xA)).unwrap();

        // Wrong start_time → no-op (row survives).
        store
            .unregister(&ProcessIdentity {
                pid: 100,
                start_time_ticks: Some(999),
                instance_epoch: Uuid::from_u128(0xA),
            })
            .unwrap();
        assert_eq!(
            store.read_all().unwrap().len(),
            1,
            "start_time mismatch must NOT remove the row"
        );

        // Wrong epoch → no-op.
        store
            .unregister(&ProcessIdentity {
                pid: 100,
                start_time_ticks: Some(1000),
                instance_epoch: Uuid::from_u128(0xB),
            })
            .unwrap();
        assert_eq!(
            store.read_all().unwrap().len(),
            1,
            "epoch mismatch must NOT remove the row"
        );

        // Exact identity → removed.
        store
            .unregister(&ProcessIdentity {
                pid: 100,
                start_time_ticks: Some(1000),
                instance_epoch: Uuid::from_u128(0xA),
            })
            .unwrap();
        assert!(
            store.read_all().unwrap().is_empty(),
            "exact identity match removes the row"
        );
    }

    /// PERS-P6: schema version compatibility. A FUTURE version degrades to empty
    /// (never trust forward-incompatible field meanings); an OLDER version is
    /// trusted (additive-only schema, backward compatible).
    #[test]
    fn read_degrades_on_future_version_and_trusts_older() {
        let (_tmp, store) = store();
        let parent = store.path().parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();

        // Future version → empty (reap nothing).
        std::fs::write(
            store.path(),
            serde_json::json!({ "version": CURRENT_REGISTRY_VERSION + 99, "processes": [proc(7, 70, 0xA)] })
                .to_string(),
        )
        .unwrap();
        assert!(
            store.read_all().unwrap().is_empty(),
            "future schema version must degrade to empty, not be trusted"
        );

        // Older version → trusted.
        std::fs::write(
            store.path(),
            serde_json::json!({ "version": 0, "processes": [proc(7, 70, 0xA)] }).to_string(),
        )
        .unwrap();
        assert_eq!(
            store.read_all().unwrap().len(),
            1,
            "older schema version is trusted (additive-only)"
        );
    }

    /// PERS-P12: the `version` field defaults to `CURRENT_REGISTRY_VERSION` when
    /// omitted (a hand-edited / older file with no version key parses, not errors).
    #[test]
    fn registry_file_version_defaults_when_omitted() {
        let json = r#"{ "processes": [] }"#;
        let reg: RegistryFile = serde_json::from_str(json).expect("missing version must default, not error");
        assert_eq!(reg.version, CURRENT_REGISTRY_VERSION);
    }

    /// PERS-P5: a corrupt/unparseable registry is quarantined aside (forensics,
    /// `.{stem}.corrupt.*`) and read degrades to empty — corruption never aborts
    /// the reap, and the original bytes are preserved (not deleted).
    #[test]
    fn corrupt_registry_is_quarantined_and_degrades_to_empty() {
        let (_tmp, store) = store();
        let parent = store.path().parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(store.path(), b"{ this is not json ]").unwrap();

        assert!(store.read_all().unwrap().is_empty(), "corrupt file degrades to empty");
        // Original is gone (renamed) and a .corrupt.* sibling now holds the bytes.
        assert!(
            !store.path().exists(),
            "corrupt original is renamed aside, not left in place"
        );
        let quarantined: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt."))
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file for forensics");
        let saved = std::fs::read_to_string(quarantined[0].path()).unwrap();
        assert!(
            saved.contains("not json"),
            "quarantine preserves the original corrupt bytes"
        );
    }

    /// PERS-P3: `next_counter` yields strictly-increasing, collision-free values
    /// (it is the temp-name disambiguator preventing concurrent atomic_write
    /// collisions). It is a process-global static, so we assert monotonicity over
    /// a captured window rather than absolute values.
    #[test]
    fn next_counter_is_strictly_increasing_and_unique() {
        let seq: Vec<u64> = (0..100).map(|_| next_counter()).collect();
        for w in seq.windows(2) {
            assert!(w[1] > w[0], "counter must strictly increase: {} !> {}", w[1], w[0]);
        }
        let uniq: std::collections::HashSet<_> = seq.iter().collect();
        assert_eq!(uniq.len(), seq.len(), "no duplicate counter values");
    }

    /// PERS-P1/P17: `atomic_write` writes the exact bytes and creates missing
    /// parent directories (temp+fsync+rename discipline), leaving no `.tmp` stray.
    #[test]
    fn atomic_write_persists_bytes_and_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a/b/c/file.json");
        atomic_write(&target, b"hello-atomic").unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"hello-atomic",
            "exact bytes persisted"
        );
        // No leftover temp in the final dir.
        let strays: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "rename consumes the temp; no .tmp stray remains");
    }

    /// PERS-P18: `atomic_write` rejects a path with no parent directory.
    #[test]
    fn atomic_write_rejects_path_without_parent() {
        let err = atomic_write(Path::new("/"), b"x").expect_err("root path has no parent");
        assert!(
            matches!(err, ProcessError::Internal(ref m) if m.contains("no parent")),
            "expected internal 'no parent', got {err:?}"
        );
    }

    /// PERS-P19: `RegisteredProcess` equality is field-wise (backs identity
    /// comparison + dedup). Identical structs are equal; a single field change
    /// makes them unequal.
    #[test]
    fn registered_process_equality_is_fieldwise() {
        let a = proc(1, 10, 0xA);
        let b = proc(1, 10, 0xA);
        assert_eq!(a, b);
        let mut c = a.clone();
        c.start_time_ticks = Some(11);
        assert_ne!(a, c, "differing start_time makes rows unequal");
    }

    /// PERS-P4: `read_file` on a missing registry returns the empty default
    /// (NotFound is the normal first-run case, never an error).
    #[test]
    fn read_file_missing_returns_empty_default() {
        let (_tmp, store) = store();
        // FileRegistryStore::new does not create the file; read_all → empty.
        assert!(
            store.read_all().unwrap().is_empty(),
            "absent registry reads as empty, not error"
        );
    }

    /// PERS-P7: serde tolerance — a row JSON omitting the optional fields
    /// (pgid / start_time_ticks / containment_id / opaque_owner_tag) deserializes
    /// with the documented defaults, and re-serializing skips the empty ones
    /// (skip_serializing_if). This is what lets older/hand-edited registries load.
    #[test]
    fn registered_process_serde_defaults_and_skips() {
        let json = r#"{ "pid": 42, "instance_epoch": "00000000-0000-0000-0000-00000000000a", "machine_id": "m", "registered_at_ms": 0 }"#;
        let row: RegisteredProcess = serde_json::from_str(json).expect("minimal row must deserialize");
        assert_eq!(row.pid, 42);
        assert_eq!(row.pgid, None);
        assert_eq!(row.start_time_ticks, None);
        assert_eq!(row.containment_id, None);
        assert_eq!(row.opaque_owner_tag, "", "missing owner tag defaults empty");

        // Re-serialize: the None/empty optionals must be omitted from output.
        let out = serde_json::to_string(&row).unwrap();
        assert!(!out.contains("pgid"), "None pgid is skipped");
        assert!(!out.contains("start_time_ticks"), "None start_time is skipped");
        assert!(!out.contains("containment_id"), "None containment is skipped");
        assert!(!out.contains("opaque_owner_tag"), "empty owner tag is skipped");
        // Required fields always present.
        assert!(out.contains("\"pid\":42") && out.contains("instance_epoch") && out.contains("machine_id"));
    }

    /// PERS-P2 / PERS-P16: `FileRegistryStore::new` creates its subdir and sweeps
    /// THIS process's stray atomic-write temps (`.registry.json.<our_pid>.*.tmp`)
    /// while leaving a sibling's in-flight temp and `.corrupt.` forensics intact.
    #[test]
    fn new_creates_subdir_and_sweeps_only_own_pid_temps() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let our = std::process::id();
        let own_temp = dir.join(format!(".{REGISTRY_FILE}.{our}.7.tmp"));
        let sibling_temp = dir.join(format!(".{REGISTRY_FILE}.{}.3.tmp", our.wrapping_add(1)));
        let corrupt = dir.join(format!(".{REGISTRY_FILE}.corrupt.{our}.1"));
        std::fs::write(&own_temp, b"stale").unwrap();
        std::fs::write(&sibling_temp, b"in-flight").unwrap();
        std::fs::write(&corrupt, b"forensics").unwrap();

        // new() runs the sweep on construction.
        let _store = FileRegistryStore::new(tmp.path());

        assert!(!own_temp.exists(), "our own stray temp is swept");
        assert!(
            sibling_temp.exists(),
            "a SIBLING's in-flight temp is NEVER swept (would lose its row)"
        );
        assert!(corrupt.exists(), "a .corrupt. quarantine file is preserved (forensics)");
    }

    /// PERS-P16: `new` is cheap and side-effect-light — it does NOT eagerly
    /// create the subdir (the sweep tolerates an absent dir), and `path()` points
    /// into the namespaced subdir. The dir is created lazily on the first write
    /// (`atomic_write` → `create_dir_all`), which `record` then materializes.
    #[test]
    fn new_is_lazy_then_record_materializes_subdir() {
        let tmp = TempDir::new().unwrap();
        let store = FileRegistryStore::new(tmp.path());
        assert_eq!(
            store.path(),
            tmp.path().join(SUBDIR).join(REGISTRY_FILE),
            "path() is under the namespaced subdir"
        );
        // The first durable write creates the subdir + file.
        store.record(proc(1, 10, 0xA)).unwrap();
        assert!(tmp.path().join(SUBDIR).is_dir(), "subdir materialized on first record");
        assert!(store.path().is_file(), "registry.json written on first record");
    }
}
