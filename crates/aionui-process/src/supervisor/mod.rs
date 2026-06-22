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

#[cfg(test)]
mod tests {
    //! Tests for the impure reap shell. The pure decision kernel
    //! (`core::reconcile`) is exhaustively table-tested in `core.rs`; here we
    //! cover the SHELL that executes its `Action`s — specifically the registry
    //! mutation paths that need no real OS process and so are testable with an
    //! in-memory mock store.
    //!
    //! The `ReapOrphanGroup` kill path (F42 TOCTOU re-verify + self-group guard)
    //! calls `proc_control::probe` / `current_process_group` directly (no seam),
    //! so it is real-process-only and lives elsewhere (see audit REAP-C8/C9).

    use std::sync::Mutex;

    use uuid::Uuid;

    use super::*;
    use crate::ProcessError;
    use crate::registry_store::{ProcessIdentity, RegisteredProcess, RegistryStore};

    /// In-memory `RegistryStore` for shell tests: records every call and can be
    /// armed to fail a chosen method, so best-effort/error-propagation contracts
    /// are observable without touching disk.
    #[derive(Default)]
    struct MockRegistry {
        rows: Mutex<Vec<RegisteredProcess>>,
        unregister_calls: Mutex<Vec<ProcessIdentity>>,
        fail_read_all: bool,
        fail_unregister: bool,
    }

    impl MockRegistry {
        fn with_rows(rows: Vec<RegisteredProcess>) -> Self {
            Self {
                rows: Mutex::new(rows),
                ..Self::default()
            }
        }
    }

    impl RegistryStore for MockRegistry {
        fn record(&self, entry: RegisteredProcess) -> Result<(), ProcessError> {
            self.rows.lock().unwrap().push(entry);
            Ok(())
        }

        fn unregister(&self, id: &ProcessIdentity) -> Result<(), ProcessError> {
            self.unregister_calls.lock().unwrap().push(id.clone());
            if self.fail_unregister {
                return Err(ProcessError::internal("injected unregister failure"));
            }
            self.rows.lock().unwrap().retain(|r| {
                r.pid != id.pid || r.start_time_ticks != id.start_time_ticks || r.instance_epoch != id.instance_epoch
            });
            Ok(())
        }

        fn read_all(&self) -> Result<Vec<RegisteredProcess>, ProcessError> {
            if self.fail_read_all {
                return Err(ProcessError::internal("injected read_all failure"));
            }
            Ok(self.rows.lock().unwrap().clone())
        }
    }

    fn row(pid: u32, start: u64, epoch: u128) -> RegisteredProcess {
        RegisteredProcess {
            pid,
            pgid: Some(pid),
            start_time_ticks: Some(start),
            instance_epoch: Uuid::from_u128(epoch),
            machine_id: "machine-this".to_string(),
            containment_id: None,
            opaque_owner_tag: String::new(),
            registered_at_ms: 0,
        }
    }

    /// REAP-C13: `PruneRegistryEntry` unregisters ONLY the row(s) matching the
    /// action's pid, by FULL identity (pid + start_time + epoch), leaving every
    /// other row untouched.
    #[test]
    fn prune_action_unregisters_only_the_matching_pid_by_full_identity() {
        let reg = MockRegistry::with_rows(vec![row(10, 1000, 0xA), row(20, 2000, 0xA), row(30, 3000, 0xA)]);

        execute_actions(&[Action::PruneRegistryEntry { pid: 20 }], &reg);

        // Exactly one unregister, carrying pid 20's full identity.
        let calls = reg.unregister_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "prune must unregister exactly the matching row");
        assert_eq!(
            calls[0],
            ProcessIdentity {
                pid: 20,
                start_time_ticks: Some(2000),
                instance_epoch: Uuid::from_u128(0xA),
            },
            "unregister must carry the matched row's FULL identity, not a bare pid"
        );
        // Rows 10 and 30 survive; 20 is gone.
        let remaining: Vec<u32> = reg.rows.lock().unwrap().iter().map(|r| r.pid).collect();
        assert_eq!(remaining, vec![10, 30], "only the pruned pid is removed");
    }

    /// REAP-C14: `execute_actions` is best-effort — a failing `unregister` is
    /// warn-logged and the loop CONTINUES to the next action (no panic, no early
    /// abort). Here a failing prune for pid 10 must not stop the prune for pid 20.
    #[test]
    fn execute_actions_continues_after_a_failing_prune() {
        let reg = MockRegistry {
            rows: Mutex::new(vec![row(10, 1000, 0xA), row(20, 2000, 0xA)]),
            fail_unregister: true,
            ..MockRegistry::default()
        };

        // Two prune actions; the first fails (injected). The call must not panic
        // and must still attempt the second.
        execute_actions(
            &[
                Action::PruneRegistryEntry { pid: 10 },
                Action::PruneRegistryEntry { pid: 20 },
            ],
            &reg,
        );

        let calls = reg.unregister_calls.lock().unwrap();
        let pids: Vec<u32> = calls.iter().map(|c| c.pid).collect();
        assert!(
            pids.contains(&10) && pids.contains(&20),
            "both prunes attempted despite the first failing (best-effort, no abort); saw {pids:?}"
        );
    }

    /// REAP-C16: `run_startup_reap` propagates a gather failure. If `read_all`
    /// (inside `gather_observed`) errors, the function returns `Err` and NEVER
    /// reaches `execute_actions` (no rows are mutated).
    #[test]
    fn run_startup_reap_propagates_gather_failure_without_executing() {
        let reg = MockRegistry {
            rows: Mutex::new(vec![row(10, 1000, 0xA)]),
            fail_read_all: true,
            ..MockRegistry::default()
        };

        let result = run_startup_reap(&reg, LockState::Acquired, Uuid::from_u128(0xB), "machine-this");

        assert!(result.is_err(), "a read_all failure during gather must surface as Err");
        // execute_actions never ran → no unregister attempted.
        assert!(
            reg.unregister_calls.lock().unwrap().is_empty(),
            "gather failure must short-circuit BEFORE execute_actions (no mutation)"
        );
    }

    /// Companion to REAP-C16: when the lock is NOT held, `reconcile` emits no
    /// actions, so a successful gather still mutates nothing — `run_startup_reap`
    /// returns an empty action list and the registry is untouched.
    #[test]
    fn run_startup_reap_without_lock_takes_no_actions() {
        let reg = MockRegistry::with_rows(vec![row(10, 1000, 0xA)]);

        let actions = run_startup_reap(&reg, LockState::HeldBySibling, Uuid::from_u128(0xB), "machine-this")
            .expect("gather succeeds");

        assert!(actions.is_empty(), "no lock → no actions (gate 1)");
        assert!(
            reg.unregister_calls.lock().unwrap().is_empty(),
            "no actions → no registry mutation"
        );
    }
}
