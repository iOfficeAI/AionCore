//! `aionui-process` — self-contained subprocess mechanism (feature 001).
//!
//! A Foundation-layer crate that spawns, supervises, and reaps the agent
//! subprocesses **it itself starts** — fully parallel to and unaware of the
//! existing `CliAgentProcess` / process registry in `aionui-ai-agent`.
//!
//! "Bytes not semantics": it never parses agent output, holds no session
//! state, and never mutates `std::env`. It depends only on `aionui-common`
//! and `aionui-runtime`.
//!
//! ## Isolation contract (why two mechanisms coexist without conflict)
//! All shared resources are namespaced under `{data_dir}/runtime/aionui-process/`
//! and every kill is identity-gated against a recorded process start-time so a
//! recycled PID/PGID is never mistaken for one of ours. See the feature
//! design doc §隔离契约 (IC-1..6).
//!
//! ## Usage
//! The standard flow: mint this run's identity (single-instance lock + epoch),
//! build a [`RealSpawner`] over a [`FileRegistryStore`], and spawn. The returned
//! [`ManagedProcess`] owns the child's lifetime — dropping it group-kills the
//! child and deregisters its registry row, so an orphan can never outlive its
//! handle. On the next startup, [`run_startup_reap`] cleans up any rows left by
//! a previous crash (identity-gated: only a confirmed prior-run orphan is killed).
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use aionui_common::CommandSpec;
//! use aionui_process::{
//!     acquire_instance_lock, FileRegistryStore, LockState, RealSpawner, Spawner,
//!     local_machine_id, run_startup_reap,
//! };
//!
//! # async fn example(data_dir: &std::path::Path, cache_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
//! // 1. Take the single-instance lock; it yields this run's fresh epoch.
//! //    Contention (a sibling instance holds it) is NOT fatal — we just mint a
//! //    fresh epoch, skip reap (never touch the live sibling's processes), and
//! //    keep serving. `LockHeld` carries the contended path for logging.
//! let (lock, epoch, lock_state) = match acquire_instance_lock(data_dir) {
//!     Ok((lock, epoch)) => (Some(lock), epoch, LockState::Acquired),
//!     Err(_held) => (None, uuid::Uuid::new_v4(), LockState::HeldBySibling),
//! };
//! let _lock = lock; // hold for the whole process lifetime; drop releases it
//! let machine_id = local_machine_id(cache_dir);
//! let registry = Arc::new(FileRegistryStore::new(data_dir));
//!
//! // 2. Reap orphans left by a prior crash. Gated on the lock: when held by a
//! //    sibling, reconcile emits nothing (no action taken).
//! run_startup_reap(&*registry, lock_state, epoch, &machine_id)?;
//!
//! // 3. Spawn an agent subprocess; it is recorded for crash-recovery.
//! let spawner = RealSpawner::new(Arc::clone(&registry), epoch, machine_id);
//! let spec = CommandSpec { command: "/usr/bin/my-agent".into(), args: vec![], env: vec![], cwd: None };
//! let proc = spawner.spawn(spec, &[], "conversation-123").await?;
//!
//! // 4. Hand the duplex to a transport, then tear down when the turn ends.
//! if let Some((_stdin, _stdout)) = proc.take_stdio().await { /* drive I/O */ }
//! proc.kill(Duration::from_secs(2)).await?; // or just drop(proc) for fire-and-forget teardown
//! # Ok(())
//! # }
//! ```

mod capabilities;
mod containment;
mod error;
mod instance_lock;
mod proc_control;
mod process;
mod registry_store;
mod spawner;
mod supervisor;

pub use capabilities::{Capabilities, ContainmentKind, ReapSupport};
pub use containment::{Containment, ContainmentKillOutcome, ProcessGroupContainment, ReapGuarantee};
pub use error::ProcessError;
pub use instance_lock::{InstanceLock, LockHeld, acquire_instance_lock};
pub use proc_control::{
    Liveness, ObservedLiveness, classify_liveness, force_kill, probe, process_group_alive, read_process_start_time,
};
pub use process::{BoxedStdin, BoxedStdout, ManagedProcess, TerminalExit};
pub use registry_store::{
    FileRegistryStore, LOCK_FILE, ProcessIdentity, REGISTRY_FILE, RegisteredProcess, RegistryStore, SUBDIR,
};
pub use spawner::{RealSpawner, Spawner, local_machine_id};
pub use supervisor::{
    Action, LockState, ObservedState, execute_actions, gather_observed, reconcile, reconcile_with_capability,
    run_startup_reap,
};
