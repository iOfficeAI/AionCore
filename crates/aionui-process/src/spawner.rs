//! `Spawner` — the assembly entry that wires spawn → containment → registry.
//!
//! Record-once (design Decision 2): we spawn FIRST, get the real pid + identity,
//! then `record` a single complete row — no placeholder pid, no backfill window.
//! The crash window before `record` is acceptable: a process spawned but not yet
//! recorded is simply not reaped on the next run (it leaks at most one), which is
//! strictly safer than recording a placeholder we might group-kill.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_common::CommandSpec;
use uuid::Uuid;

use crate::registry_store::{ProcessIdentity, RegisteredProcess, RegistryStore};
use crate::{ManagedProcess, ProcessError};

/// Spawns subprocesses and registers them so the supervisor can reap our own
/// orphans after a crash.
#[async_trait::async_trait]
pub trait Spawner: Send + Sync {
    /// Spawn `spec`, place it under containment, and record it. `extra_env` is
    /// applied per-child (IC-5). `opaque_owner_tag` is stored verbatim and
    /// never parsed by this layer (it's the caller's conversation id etc).
    async fn spawn(
        &self,
        spec: CommandSpec,
        extra_env: &[(String, String)],
        opaque_owner_tag: &str,
    ) -> Result<Arc<ManagedProcess>, ProcessError>;
}

/// Production spawner backed by the file registry. Holds this run's identity
/// (epoch + machine_id) so every recorded row is identity-gateable on reap.
pub struct RealSpawner<S: RegistryStore> {
    registry: Arc<S>,
    instance_epoch: Uuid,
    machine_id: String,
}

impl<S: RegistryStore> RealSpawner<S> {
    pub fn new(registry: Arc<S>, instance_epoch: Uuid, machine_id: impl Into<String>) -> Self {
        Self {
            registry,
            instance_epoch,
            machine_id: machine_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl<S: RegistryStore + 'static> Spawner for RealSpawner<S> {
    async fn spawn(
        &self,
        spec: CommandSpec,
        extra_env: &[(String, String)],
        opaque_owner_tag: &str,
    ) -> Result<Arc<ManagedProcess>, ProcessError> {
        // Spawn first — the child is already its own process-group leader
        // (Builder process_group(0)), so it is contained from birth.
        let proc = ManagedProcess::spawn(spec, extra_env).await?;
        let pid = proc.pid();
        let pgid = proc.process_group_id();

        // Read start-time ONCE, while the child is freshly alive, and reuse it
        // for BOTH the recorded row and the dereg identity below. Re-reading it
        // at dereg time would race the process's exit (→ None / different value)
        // and the identity would no longer match the recorded row, so dereg
        // would silently fail (the bug the F38 test caught).
        let start_time_ticks = crate::read_process_start_time(pid);

        // record-once with the real pid + identity (no placeholder).
        let entry = RegisteredProcess {
            pid,
            pgid,
            start_time_ticks,
            instance_epoch: self.instance_epoch,
            machine_id: self.machine_id.clone(),
            // The pgid IS the containment id for the process-group tier.
            containment_id: pgid.map(|g| g.to_string()),
            opaque_owner_tag: opaque_owner_tag.to_owned(),
            registered_at_ms: aionui_common::now_ms(),
        };
        if let Err(e) = self.registry.record(entry) {
            // If we cannot durably record it, we must not leave a live child we
            // can never reap: tear it down before surfacing the error
            // (no live-child-without-durable-row). Surface a kill failure rather
            // than silently swallowing it (F59) — but still return the original
            // record error (that is the root cause the caller must see).
            if let Err(kill_err) = proc.kill(std::time::Duration::from_millis(200)).await {
                tracing::warn!(pid, error = %kill_err, "record failed AND teardown of the un-recorded child failed");
            }
            return Err(e);
        }

        // Steady-state dereg (F38): when the handle is dropped (turn end / clean
        // exit), remove this registry row so registry.json does not grow
        // unbounded for the parent's whole lifetime. The hook is opaque to
        // ManagedProcess and offloads the file I/O to a blocking pool so Drop
        // never stalls the (tokio worker) thread that releases the last Arc.
        let registry = Arc::clone(&self.registry);
        let identity = ProcessIdentity {
            pid,
            start_time_ticks, // SAME value recorded above — must match for dereg
            instance_epoch: self.instance_epoch,
        };
        proc.set_on_drop(Box::new(move || {
            // We may be inside a tokio runtime (consumer worker) or not (tests).
            // If a runtime is present, offload; otherwise do it inline (cheap in
            // tests / non-async drop sites).
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn_blocking(move || {
                        if let Err(e) = registry.unregister(&identity) {
                            tracing::warn!(error = %e, "failed to deregister process from registry on drop");
                        }
                    });
                }
                Err(_) => {
                    if let Err(e) = registry.unregister(&identity) {
                        tracing::warn!(error = %e, "failed to deregister process from registry on drop");
                    }
                }
            }
        }));

        Ok(Arc::new(proc))
    }
}

/// A stable, NON-synced machine identifier for the cross-machine reap guard
/// (IC-5). Persisted in the OS-LOCAL cache dir (NOT `data_dir`, which may be
/// iCloud/Dropbox-synced — a synced id would defeat the guard). First call
/// mints + persists a random UUID; later calls read it back.
pub fn local_machine_id(os_local_cache_dir: &std::path::Path) -> String {
    let path: PathBuf = os_local_cache_dir.join("aionui-process").join("machine-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_owned();
        }
        // Non-empty file existed but was blank/whitespace → a prior torn write.
        // Re-minting here orphans every prior-machine registry row as foreign,
        // silently disabling reaping (F40). Warn loudly so this is observable.
        tracing::warn!(
            path = %path.display(),
            "machine-id file present but empty (likely a torn write); minting a NEW id — \
             prior registry rows from the old id will be treated as foreign and only pruned"
        );
    }
    let id = Uuid::new_v4().to_string();
    // Durable atomic write (F40): temp + fsync + rename, so a kill/disk-full
    // mid-write can never leave the empty file that triggers the silent re-mint
    // above. A bare `std::fs::write` is NOT atomic and was the root cause.
    if let Err(e) = crate::registry_store::atomic_write(&path, id.as_bytes()) {
        tracing::warn!(error = %e, "failed to durably persist machine-id; reap identity may not be stable across runs");
    }
    id
}
