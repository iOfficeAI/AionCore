#![warn(clippy::disallowed_types)]

//! Bundled runtime (bun) resolver for aioncore.
//!
//! Embeds the bun runtime at build time (zstd-compressed) and extracts it
//! to the user's OS cache directory on first call. Callers use
//! [`resolve_bun`] to obtain a usable executable path and [`bun_bin_dir`]
//! to prepend the runtime directory to child-process `PATH`.

pub mod acp_tool_runtime;
mod agent_env;
mod cache;
mod embed;
mod extract;
mod http_client;
pub mod managed_resources;
pub mod managed_resources_contract;
pub mod node_runtime;
mod resolver;
mod shell_env;

pub use acp_tool_runtime::{
    ManagedAcpToolError, ManagedAcpToolFailureKind, ManagedAcpToolId, ManagedAcpToolProgress,
    ManagedAcpToolProgressPhase, ManagedAcpToolProgressReporter, ManagedAcpToolSupport, ResolvedManagedAcpTool,
    SharedManagedAcpToolProgressReporter, doctor_snapshot as acp_tool_doctor_snapshot, ensure_managed_acp_tool,
    ensure_managed_acp_tool_with_reporter, prepare_managed_acp_tool_to_root, probe_managed_acp_tool_supported,
};
pub use agent_env::agent_process_env;
pub use cache::init;
pub use managed_resources::{ManagedResourcesMode, managed_resources_mode, set_managed_resources_mode};
pub use node_runtime::{
    DoctorRow, NodeRuntimeError, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase,
    NodeRuntimeProgressReporter, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe, SharedNodeRuntimeProgressReporter, doctor_snapshot,
    doctor_snapshot_for_test, ensure_node_runtime, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, probe_node_runtime_supported, probe_runtime_command,
};
pub use resolver::{ResolveError, bun_bin_dir, resolve_bun, resolve_command_in, resolve_command_path};
pub use shell_env::enhance_process_path;
mod spawn;
pub use spawn::{Builder, kill_process_tree};

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support_tests;

#[cfg(test)]
mod test_support;
