//! Per-platform capability descriptor (feature 005, WORKFLOW discipline 7).
//!
//! Turns "what this crate can actually do on this OS" from scattered, silent
//! `cfg` branches into a single TYPED, ASSERTABLE value. The matrix in the 005
//! design doc maps 1:1 to these fields; the `capabilities_matrix_per_platform`
//! test pins each platform's row, so a capability regression (e.g. someone
//! re-stubs macOS start-time to `None`) turns a test RED instead of silently
//! degrading reap safety.
//!
//! "Hot" vs "cold" kill is the load-bearing distinction (design I-9): while a
//! live `ManagedProcess` handle is held (normal exit / explicit kill / Drop)
//! the whole subtree is torn down on every platform; only the post-CRASH
//! cold-reap (reconstruct from a persisted pid, no live handle) degrades — and
//! only on Windows, where the Job handle does not survive the owner's death.

use serde::{Deserialize, Serialize};

/// What kind of OS primitive contains a spawned subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainmentKind {
    /// No subtree containment (grandchildren are not corralled).
    None,
    /// POSIX process group (`setpgid` + `kill(-pgid)`); a `setsid` grandchild escapes.
    ProcessGroup,
    /// Windows Job Object (`KILL_ON_JOB_CLOSE` + `TerminateJobObject`); stronger than a group.
    JobObject,
}

/// How well crash-recovery reap (from a persisted pid, no live handle) works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapSupport {
    /// No cross-restart reap on this platform.
    None,
    /// Full subtree reap survives restart (Unix: persisted pgid → `kill(-pgid)`).
    Full,
    /// Single-process kill after identity gating, plus a best-effort `taskkill /T`
    /// sweep (Windows: the Job handle does not persist across the owner's death,
    /// so the subtree guarantee degrades — design I-9).
    SingleProcessGated,
}

/// The subprocess-mechanism capabilities of the platform this binary was
/// compiled for. A `const fn` per-platform value — no runtime probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Can actively kill a process we spawned.
    pub can_kill: bool,
    /// What contains a spawned subtree.
    pub subtree_containment: ContainmentKind,
    /// `probe` can truthfully report liveness.
    pub liveness_probe: bool,
    /// `read_process_start_time` yields a real value (the reap-safety identity gate).
    pub identity_gate: bool,
    /// While a live handle is held (normal exit / kill / Drop), the WHOLE subtree
    /// is torn down. True on every supported platform — no degradation here.
    pub hot_kill_subtree: bool,
    /// Crash-recovery reap quality (no live handle, from a persisted pid).
    pub cold_reap: ReapSupport,
    /// The kernel auto-kills our children when the parent dies (Linux
    /// `PR_SET_PDEATHSIG` / Windows `KILL_ON_JOB_CLOSE`); shrinks crash orphans.
    /// macOS has no equivalent.
    pub parent_death_signal: bool,
    /// Dropping a `ManagedProcess` reaps its subtree.
    pub drop_reaps: bool,
}

impl Capabilities {
    /// The capabilities of the current compile target.
    pub const fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                can_kill: true,
                subtree_containment: ContainmentKind::ProcessGroup,
                liveness_probe: true,
                identity_gate: true, // /proc/<pid>/stat field 22
                hot_kill_subtree: true,
                cold_reap: ReapSupport::Full, // persisted pgid → kill(-pgid)
                parent_death_signal: true,    // PR_SET_PDEATHSIG (R9)
                drop_reaps: true,
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                can_kill: true,
                subtree_containment: ContainmentKind::ProcessGroup,
                liveness_probe: true,
                identity_gate: true, // proc_pidinfo PROC_PIDTBSDINFO (R1)
                hot_kill_subtree: true,
                cold_reap: ReapSupport::Full, // persisted pgid → kill(-pgid)
                parent_death_signal: false,   // no PDEATHSIG equivalent; reaper is load-bearing
                drop_reaps: true,
            }
        }
        #[cfg(target_os = "windows")]
        {
            // BATCH B implemented (feature 005). Windows now has real:
            //   - probe + identity gate: OpenProcess + WaitForSingleObject +
            //     GetProcessTimes creation-FILETIME (proc_control windows_impl);
            //   - hot-kill subtree: Job Object (CREATE_SUSPENDED → assign →
            //     resume) + TerminateJobObject / KILL_ON_JOB_CLOSE on Drop;
            //   - parent-death: KILL_ON_JOB_CLOSE (job dies with the last handle).
            // cold-reap stays SingleProcessGated (I-9): the Job handle does NOT
            // persist across the owner's death, so a from-disk pid is terminated
            // as a single process (TerminateProcess), not the whole subtree.
            // ⚠️ Verified by cross-compile (cargo-xwin) + must be run on a real
            // Windows host / UTM VM (no x86 CI lane) — until then treat the
            // RUNTIME behavior as LocalVerifiedOnly in spirit.
            Self {
                can_kill: true,                                  // TerminateJobObject / TerminateProcess
                subtree_containment: ContainmentKind::JobObject, // Job Object
                liveness_probe: true,                            // OpenProcess + WaitForSingleObject
                identity_gate: true,                             // GetProcessTimes creation FILETIME
                hot_kill_subtree: true,                          // Job terminate while handle held
                cold_reap: ReapSupport::SingleProcessGated,      // Job doesn't persist (I-9)
                parent_death_signal: true,                       // KILL_ON_JOB_CLOSE
                drop_reaps: true,                                // Drop terminates the Job
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            // Unknown platform: claim nothing (safe defaults — never kill on doubt).
            Self {
                can_kill: false,
                subtree_containment: ContainmentKind::None,
                liveness_probe: false,
                identity_gate: false,
                hot_kill_subtree: false,
                cold_reap: ReapSupport::None,
                parent_death_signal: false,
                drop_reaps: false,
            }
        }
    }
}
