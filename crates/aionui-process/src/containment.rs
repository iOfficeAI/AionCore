//! Per-platform lifecycle fence (Containment). Tears down a whole subprocess
//! subtree (agent CLI + grandchildren like MCP servers), not just the direct
//! child. Lifecycle-only — orthogonal to any security sandbox.
//!
//! Single tier ships: [`ProcessGroupContainment`] (best-effort Unix process
//! group). Job Object / cgroup tiers are intentionally not built (no CI lane;
//! they collapse to the process-group kill on testable platforms). The seam
//! lets them land later without touching callers.

use crate::ProcessError;

/// Strength of a containment's teardown guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapGuarantee {
    /// Process-group SIGKILL, plus a sweep of descendants captured before the
    /// kill — so a child that left the group via `setsid`/`setpgid` is still
    /// reaped. Best-effort because anything spawned between the snapshot and
    /// the kill, or whose identity cannot be confirmed, is left alone.
    BestEffort,
}

/// Outcome of [`Containment::kill_all`] — never a bare `Ok(())` the caller can
/// misread as "tree definitely gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentKillOutcome {
    /// Post-kill liveness probe confirmed the group is gone.
    ProbedGone,
    /// Kill issued but not confirmed gone (e.g. a member escaped the group).
    DegradedBestEffort,
}

/// A lifecycle fence around a spawned subprocess subtree.
pub trait Containment: Send + Sync {
    fn kill_all(&self) -> Result<ContainmentKillOutcome, ProcessError>;
    fn guarantee(&self) -> ReapGuarantee;
}

/// Best-effort containment via the Unix process group captured at spawn.
pub struct ProcessGroupContainment {
    pid: u32,
    process_group_id: Option<u32>,
}

impl ProcessGroupContainment {
    pub fn new(pid: u32, process_group_id: Option<u32>) -> Self {
        Self { pid, process_group_id }
    }
}

impl Containment for ProcessGroupContainment {
    fn kill_all(&self) -> Result<ContainmentKillOutcome, ProcessError> {
        // BEFORE the kill, not after: the parent link is the only thing tying
        // an escaped child to this tree, and the kernel reparents it to init
        // the moment the CLI dies. Looked up afterwards, the set is empty and
        // the child is unreachable forever.
        let snapshot = crate::proc_tree::escaped_descendants(self.pid, self.process_group_id);

        crate::force_kill(self.pid, self.process_group_id)?;
        // SIGKILL is async; give the kernel a brief bounded settle before the
        // confirmation probe, else a clean kill almost always reads alive and
        // ProbedGone would be unreachable. Still alive after settle => honest
        // Degraded (escaped grandchild) rather than a false "gone".
        const ATTEMPTS: u32 = 20;
        const STEP: std::time::Duration = std::time::Duration::from_millis(25);
        let mut group_gone = false;
        for _ in 0..ATTEMPTS {
            if !crate::process_group_alive(self.process_group_id) {
                group_gone = true;
                break;
            }
            std::thread::sleep(STEP);
        }

        let strays = crate::proc_tree::reap(&snapshot);
        if group_gone && strays == 0 {
            return Ok(ContainmentKillOutcome::ProbedGone);
        }
        Ok(ContainmentKillOutcome::DegradedBestEffort)
    }

    fn guarantee(&self) -> ReapGuarantee {
        ReapGuarantee::BestEffort
    }
}
