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

    /// Every descendant of the contained pid, each paired with its start-time
    /// so it can be re-identified after the kill.
    ///
    /// Not filtered to "escaped" ones: a descendant still inside the group is
    /// already dead by the time we sweep, and `SIGKILL`ing a gone pid is a
    /// no-op (`ESRCH` is success). Filtering would need a second per-pid
    /// accessor to buy nothing.
    fn descendant_snapshot(&self) -> Vec<(u32, Option<u64>)> {
        let table = crate::proc_tree::parent_table();
        crate::proc_tree::collect_descendants(self.pid, &table)
            .into_iter()
            .map(|pid| (pid, crate::read_process_start_time(pid)))
            .collect()
    }

    /// `SIGKILL` snapshot members that are still the same process, and report
    /// how many are STILL alive afterwards.
    ///
    /// Identity is start-time equality: between the snapshot and this sweep the
    /// pid may have been recycled onto something unrelated, and killing that
    /// would be far worse than leaking. Anything we cannot positively identify
    /// — no recorded start-time, none observable, or a mismatch — is left
    /// alone, matching this crate's standing "never kill on doubt" rule.
    fn reap(&self, snapshot: &[(u32, Option<u64>)]) -> usize {
        let own_pid = std::process::id();
        let mut still_alive = 0usize;
        for &(pid, recorded) in snapshot {
            if pid == own_pid {
                continue; // never signal the reaper itself
            }
            let observed = match crate::probe(pid) {
                crate::ObservedLiveness::Alive { start_ticks } => start_ticks,
                // Gone: nothing to do. EpermAlive: alive but not provably ours.
                crate::ObservedLiveness::Gone => continue,
                crate::ObservedLiveness::EpermAlive => {
                    still_alive += 1;
                    continue;
                }
            };
            let same_process = match (recorded, observed) {
                // `0` is this crate's non-identity sentinel (see
                // `classify_liveness`): corrupt data, not a discriminator.
                (Some(0), _) | (_, Some(0)) | (None, _) | (_, None) => false,
                (Some(rec), Some(obs)) => rec == obs,
            };
            if !same_process {
                still_alive += 1;
                continue;
            }
            if crate::force_kill(pid, None).is_err() {
                still_alive += 1;
            }
        }
        still_alive
    }
}

impl Containment for ProcessGroupContainment {
    fn kill_all(&self) -> Result<ContainmentKillOutcome, ProcessError> {
        // BEFORE the kill, not after: the parent link is the only thing tying
        // an escaped child to this tree, and the kernel reparents it to init
        // the moment the CLI dies. Looked up afterwards, the set is empty and
        // the child is unreachable forever.
        let snapshot = self.descendant_snapshot();

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

        let strays = self.reap(&snapshot);
        if group_gone && strays == 0 {
            return Ok(ContainmentKillOutcome::ProbedGone);
        }
        Ok(ContainmentKillOutcome::DegradedBestEffort)
    }

    fn guarantee(&self) -> ReapGuarantee {
        ReapGuarantee::BestEffort
    }
}
