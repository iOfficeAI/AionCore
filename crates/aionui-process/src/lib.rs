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
//! design doc §Isolation-Contract (IC-1..6).

mod capabilities;
mod containment;
mod error;
mod instance_lock;
mod proc_control;
mod proc_tree;
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
