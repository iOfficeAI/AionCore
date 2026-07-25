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
