//! Pure reconcile kernel — the reap decision brain (Tier-A exhaustible).
//!
//! No `.await`, no clock, no syscall: it takes an already-gathered
//! [`ObservedState`] and emits [`Action`]s. Every IC-1 safety rule lives here
//! as a pure predicate, so the dangerous "what do we kill" logic is unit-test
//! exhaustible and mutation-guardable.
//!
//! The whole point: a reap is emitted ONLY for a registry row that is a
//! prior-run orphan, on THIS machine, whose live process identity MATCHES what
//! we recorded — and only when we hold the single-instance lock. Everything
//! else prunes (removes the stale row) without killing.

use uuid::Uuid;

use crate::Liveness;
use crate::registry_store::RegisteredProcess;

/// Whether the single-instance lock is held this run. Reaping is gated on it:
/// if a sibling instance holds it, we must NOT kill (its processes are live).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Acquired,
    HeldBySibling,
}

/// Everything the kernel needs, gathered impurely by the caller.
pub struct ObservedState {
    /// Registry rows read back from disk (this crate's own registry).
    pub rows: Vec<RegisteredProcess>,
    /// Liveness/identity classification per row pid (from proc_control::probe
    /// + classify_liveness), keyed by index into `rows`.
    pub liveness: Vec<Liveness>,
    pub lock_state: LockState,
    pub current_epoch: Uuid,
    pub current_machine: String,
}

/// A decision the runner must execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Group-SIGKILL a confirmed prior-run orphan, then prune its row.
    /// `recorded_start_ticks` is carried so the runner can RE-VERIFY identity
    /// immediately before the kill (F42 TOCTOU close): between `gather_observed`
    /// (which classified this as Match) and the actual `force_kill`, the pid
    /// could exit and be recycled. The runner re-probes and only kills if the
    /// live start-time still matches this recorded value.
    ReapOrphanGroup {
        pid: u32,
        pgid: Option<u32>,
        recorded_start_ticks: Option<u64>,
    },
    /// Kill the containment fence of a confirmed orphan (covers grandchildren).
    ReapContainment { containment_id: String },
    /// Remove a stale/unkillable/foreign row without killing anything.
    PruneRegistryEntry { pid: u32 },
}

/// The pure reap decision. For each row, decide reap-and-prune vs prune-only.
///
/// Safety rules enforced here (IC-1):
/// - Lock not Acquired => emit NOTHING (never touch a live sibling's procs).
/// - Foreign machine_id => prune-only (cloud-synced data dir, design I-5).
/// - Own-epoch row => never a reap target (it's one of THIS run's, I-4).
/// - Liveness != Match => prune-only (recycled/unknown/eperm/gone, I-1).
/// - Only Match + prior-epoch + same-machine => ReapOrphanGroup (+ContainmentReap).
pub fn reconcile(observed: &ObservedState) -> Vec<Action> {
    reconcile_with_capability(observed, crate::Capabilities::current().can_kill)
}

/// Capability-gated reconcile (F58). `can_kill` is normally
/// `Capabilities::current().can_kill`; the seam exists so the "unknown platform
/// must never emit a kill it cannot perform" invariant is ENFORCED (not merely
/// emergent from `probe`→Unknown), and is unit-testable by passing `false`.
pub fn reconcile_with_capability(observed: &ObservedState, can_kill: bool) -> Vec<Action> {
    // Gate 1: no lock => do not reap anything (IC-1 / I-3 lock-gate).
    if observed.lock_state != LockState::Acquired {
        return Vec::new();
    }

    let mut actions = Vec::new();
    for (row, &live) in observed.rows.iter().zip(observed.liveness.iter()) {
        // Gate 2: foreign machine => prune-only (I-5). A pgid on machine B is
        // meaningless/dangerous here.
        if row.machine_id != observed.current_machine {
            actions.push(Action::PruneRegistryEntry { pid: row.pid });
            continue;
        }
        // Gate 3: identity. Only an identity-MATCHED, prior-epoch, live process
        // is a reap target. classify_liveness already encodes "Match == alive
        // && start-time matches && prior epoch"; DiffEpoch == our own run.
        match live {
            // Gate 0 (F58): only emit a kill if the platform can actually
            // perform it. On a platform without a real `force_kill`, emitting
            // ReapOrphanGroup would warn-and-swallow while the row is pruned and
            // the live process forgotten — prune-and-forget. If we cannot kill,
            // prune-only (safe), enforced here rather than relying on probe
            // happening to return Unknown.
            Liveness::Match if can_kill => {
                // Reap the containment first (covers grandchildren), then the
                // group, then prune. The runner executes in order.
                if let Some(cid) = &row.containment_id {
                    actions.push(Action::ReapContainment {
                        containment_id: cid.clone(),
                    });
                }
                actions.push(Action::ReapOrphanGroup {
                    pid: row.pid,
                    pgid: row.pgid,
                    recorded_start_ticks: row.start_time_ticks,
                });
                actions.push(Action::PruneRegistryEntry { pid: row.pid });
            }
            // Match but platform cannot kill → prune-only (F58 safe degrade).
            Liveness::Match => {
                actions.push(Action::PruneRegistryEntry { pid: row.pid });
            }
            // Everything else: prune the stale row, never kill (I-1 negative).
            Liveness::RecycledPid | Liveness::DiffEpoch | Liveness::Gone | Liveness::Unknown | Liveness::EpermAlive => {
                actions.push(Action::PruneRegistryEntry { pid: row.pid });
            }
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE: &str = "machine-this";

    // The reconcile-level reap decision keys on `liveness` (already classified
    // upstream by classify_liveness, which is itself table-tested); the row's
    // own epoch is not re-read here. A prior-run epoch value keeps the row
    // realistic, but the gate that matters is the passed-in Liveness.
    const PRIOR_EPOCH: u128 = 0xA;
    const CURRENT_EPOCH: u128 = 0xB;

    fn row(pid: u32) -> RegisteredProcess {
        RegisteredProcess {
            pid,
            pgid: Some(pid),
            start_time_ticks: Some(1000),
            instance_epoch: Uuid::from_u128(PRIOR_EPOCH),
            machine_id: MACHINE.to_string(),
            containment_id: None,
            opaque_owner_tag: String::new(),
            registered_at_ms: 0,
        }
    }

    fn observed(
        rows: Vec<RegisteredProcess>,
        liveness: Vec<Liveness>,
        lock: LockState,
        machine: &str,
    ) -> ObservedState {
        ObservedState {
            rows,
            liveness,
            lock_state: lock,
            current_epoch: Uuid::from_u128(CURRENT_EPOCH),
            current_machine: machine.to_string(),
        }
    }

    fn is_kill(a: &Action) -> bool {
        matches!(a, Action::ReapOrphanGroup { .. })
    }

    /// Gate 1 (IC-1/I-3): no lock acquired → emit NOTHING (never touch a live
    /// sibling's processes). Even a perfect Match row must produce zero actions.
    #[test]
    fn gate1_no_lock_emits_nothing() {
        let r = row(100);
        let st = observed(vec![r], vec![Liveness::Match], LockState::HeldBySibling, MACHINE);
        assert!(
            reconcile_with_capability(&st, true).is_empty(),
            "no lock → no actions at all"
        );
    }

    /// Gate 2 (I-5): foreign machine_id → prune-only, NEVER kill (cloud-synced
    /// data dir; a pgid from machine B is meaningless/dangerous here).
    #[test]
    fn gate2_foreign_machine_prune_only() {
        let mut r = row(100);
        r.machine_id = "machine-OTHER".into();
        let st = observed(vec![r], vec![Liveness::Match], LockState::Acquired, MACHINE);
        let acts = reconcile_with_capability(&st, true);
        assert_eq!(
            acts,
            vec![Action::PruneRegistryEntry { pid: 100 }],
            "foreign machine → prune-only"
        );
        assert!(!acts.iter().any(is_kill), "must NOT kill a foreign-machine row");
    }

    /// Gate 0 (F58): platform cannot kill → Match degrades to prune-only (never
    /// emit a ReapOrphanGroup the platform will warn-and-swallow = prune-and-forget).
    #[test]
    fn gate0_cannot_kill_match_degrades_to_prune() {
        let st = observed(vec![row(100)], vec![Liveness::Match], LockState::Acquired, MACHINE);
        let acts = reconcile_with_capability(&st, false);
        assert_eq!(
            acts,
            vec![Action::PruneRegistryEntry { pid: 100 }],
            "can_kill=false → prune-only even on Match"
        );
    }

    /// Gate 3 (I-1 negative): every non-Match liveness → prune-only, never kill.
    #[test]
    fn gate3_non_match_never_kills() {
        for live in [
            Liveness::RecycledPid,
            Liveness::DiffEpoch,
            Liveness::Gone,
            Liveness::Unknown,
            Liveness::EpermAlive,
        ] {
            let st = observed(vec![row(100)], vec![live], LockState::Acquired, MACHINE);
            let acts = reconcile_with_capability(&st, true);
            assert!(!acts.iter().any(is_kill), "{live:?} must never produce a kill");
            assert_eq!(
                acts,
                vec![Action::PruneRegistryEntry { pid: 100 }],
                "{live:?} → prune-only"
            );
        }
    }

    /// The ONLY reapable path: lock acquired + same machine + Match + can_kill →
    /// ReapOrphanGroup (carrying recorded_start_ticks for the runner's F42
    /// re-verify) THEN prune, in order.
    #[test]
    fn only_match_same_machine_locked_reaps() {
        let st = observed(vec![row(100)], vec![Liveness::Match], LockState::Acquired, MACHINE);
        let acts = reconcile_with_capability(&st, true);
        assert_eq!(
            acts,
            vec![
                Action::ReapOrphanGroup {
                    pid: 100,
                    pgid: Some(100),
                    recorded_start_ticks: Some(1000),
                },
                Action::PruneRegistryEntry { pid: 100 },
            ],
            "reap-then-prune in order, start-ticks carried for TOCTOU re-verify"
        );
    }

    /// Containment fence is reaped BEFORE the group (covers grandchildren).
    #[test]
    fn match_with_containment_reaps_fence_first() {
        let mut r = row(100);
        r.containment_id = Some("job-xyz".into());
        let st = observed(vec![r], vec![Liveness::Match], LockState::Acquired, MACHINE);
        let acts = reconcile_with_capability(&st, true);
        assert_eq!(
            acts[0],
            Action::ReapContainment {
                containment_id: "job-xyz".into()
            },
            "containment first"
        );
        assert!(matches!(acts[1], Action::ReapOrphanGroup { .. }), "then group");
        assert_eq!(acts[2], Action::PruneRegistryEntry { pid: 100 }, "then prune");
    }
}
