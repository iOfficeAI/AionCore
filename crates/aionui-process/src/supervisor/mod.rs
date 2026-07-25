//! Supervisor — the runner around the pure [`core::reconcile`] kernel.
//!
//! Gathers `ObservedState` (read registry + probe each row's live identity +
//! lock state), calls the pure kernel, and executes the returned `Action`s
//! (group-kill + container-kill + prune). This is the impure shell; all the
//! dangerous decisions live in the pure kernel so they stay exhaustible.

mod core;

pub use core::{Action, LockState, ObservedState, reconcile, reconcile_with_capability};

use uuid::Uuid;

use crate::Liveness;
use crate::proc_control::{self, ObservedLiveness, current_process_group};
use crate::registry_store::{ProcessIdentity, RegistryStore};

/// Gather the observed state for a startup reap: read every registry row and
/// probe its live identity against the recorded start-time + epoch.
pub fn gather_observed<S: RegistryStore + ?Sized>(
    registry: &S,
    lock_state: LockState,
    current_epoch: Uuid,
    current_machine: &str,
) -> Result<ObservedState, crate::ProcessError> {
    let rows = registry.read_all()?;
    let liveness = rows
        .iter()
        .map(|r| {
            let observed: ObservedLiveness = proc_control::probe(r.pid);
            proc_control::classify_liveness(r.start_time_ticks, observed, r.instance_epoch, current_epoch)
        })
        .collect::<Vec<Liveness>>();
    Ok(ObservedState {
        rows,
        liveness,
        lock_state,
        current_epoch,
        current_machine: current_machine.to_owned(),
    })
}

/// Execute the reconcile actions: kill confirmed orphans, prune stale rows.
/// Best-effort and warn-logged — a single failure never aborts the rest.
pub fn execute_actions<S: RegistryStore + ?Sized>(actions: &[Action], registry: &S) {
    for action in actions {
        match action {
            Action::ReapOrphanGroup {
                pid,
                pgid,
                recorded_start_ticks,
            } => {
                // F42: RE-VERIFY identity immediately before the kill. reconcile
                // classified this row as Match using a start-time read back in
                // gather_observed; the pid could have exited and been recycled
                // onto an innocent process in the window. Re-probe NOW and only
                // kill if the live start-time still matches what we recorded.
                let observed = proc_control::probe(*pid);
                let still_ours = matches!(
                    observed,
                    ObservedLiveness::Alive { start_ticks } if start_ticks == *recorded_start_ticks && start_ticks.is_some()
                );
                // Self-group guard (critic category): never group-kill a pgid
                // the REAPER itself belongs to — that would SIGKILL ourselves.
                let our_pgid = current_process_group();
                let targets_own_group = matches!((*pgid, our_pgid), (Some(p), Some(o)) if p == o);
                if !still_ours {
                    tracing::warn!(
                        pid,
                        "skipping reap: identity changed between gather and kill (pid exited/recycled) — never kill on doubt (F42)"
                    );
                } else if targets_own_group {
                    tracing::error!(
                        pid,
                        ?pgid,
                        "refusing to reap: target pgid is the reaper's OWN process group (would self-SIGKILL)"
                    );
                } else if let Err(e) = proc_control::force_kill(*pid, *pgid) {
                    tracing::warn!(pid, ?pgid, error = %e, "reap orphan group failed");
                } else {
                    // Post-reap verification (critic category): SIGKILL is async;
                    // confirm the group is actually gone rather than asserting
                    // success. A still-alive group after the kill = an escaped
                    // (setsid) grandchild — log it honestly (DegradedBestEffort),
                    // do not pretend it was fully reaped.
                    if proc_control::process_group_alive(*pgid) {
                        tracing::warn!(
                            pid,
                            ?pgid,
                            "reap issued but group still alive (likely a setsid-escaped grandchild) — degraded, not confirmed gone"
                        );
                    }
                }
            }
            Action::ReapContainment { containment_id } => {
                // Containment teardown for grandchildren is owned by the
                // caller that created it; here we only have the id recorded.
                // The group-kill above already covers same-group descendants;
                // a future cgroup/JobObject tier would act on this id.
                tracing::debug!(
                    containment_id,
                    "reap containment (process-group tier: covered by group kill)"
                );
            }
            Action::PruneRegistryEntry { pid } => {
                // Prune by pid is sufficient here: the row is stale/foreign/
                // dead. We remove every identity sharing this pid in our own
                // registry (there can only be our own rows).
                if let Err(e) = prune_pid(registry, *pid) {
                    tracing::warn!(pid, error = %e, "prune registry entry failed");
                }
            }
        }
    }
}

fn prune_pid<S: RegistryStore + ?Sized>(registry: &S, pid: u32) -> Result<(), crate::ProcessError> {
    // Find the row(s) with this pid and unregister by full identity.
    for r in registry.read_all()? {
        if r.pid == pid {
            registry.unregister(&ProcessIdentity {
                pid: r.pid,
                start_time_ticks: r.start_time_ticks,
                instance_epoch: r.instance_epoch,
            })?;
        }
    }
    Ok(())
}

/// One-shot startup reap: gather → reconcile → execute. Returns the actions
/// taken (for logging/testing).
pub fn run_startup_reap<S: RegistryStore + ?Sized>(
    registry: &S,
    lock_state: LockState,
    current_epoch: Uuid,
    current_machine: &str,
) -> Result<Vec<Action>, crate::ProcessError> {
    let observed = gather_observed(registry, lock_state, current_epoch, current_machine)?;
    let actions = reconcile(&observed);
    execute_actions(&actions, registry);
    Ok(actions)
}
