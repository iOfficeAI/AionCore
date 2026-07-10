use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberRuntimeFailure {
    pub(crate) classification: &'static str,
    pub(crate) public_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttachOutcome {
    Ready,
    Failed(MemberRuntimeFailure),
    Removed,
    SessionStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachSignal {
    Pending,
    Terminal(AttachOutcome),
}

#[derive(Debug, Clone)]
pub(crate) struct AttachWaiter {
    operation_id: u64,
    outcome_rx: watch::Receiver<AttachSignal>,
}

impl AttachWaiter {
    pub(crate) fn operation_id(&self) -> u64 {
        self.operation_id
    }

    pub(crate) async fn wait(mut self) -> AttachOutcome {
        loop {
            let terminal = match &*self.outcome_rx.borrow() {
                AttachSignal::Pending => None,
                AttachSignal::Terminal(outcome) => Some(outcome.clone()),
            };
            if let Some(outcome) = terminal {
                return outcome;
            }
            if self.outcome_rx.changed().await.is_err() {
                return AttachOutcome::SessionStopped;
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct AttachLease {
    session_generation: String,
    agent_id: String,
    waiter: AttachWaiter,
}

impl AttachLease {
    pub(crate) fn operation_id(&self) -> u64 {
        self.waiter.operation_id()
    }

    pub(crate) fn waiter(&self) -> AttachWaiter {
        self.waiter.clone()
    }
}

#[derive(Debug)]
pub(crate) struct RemoveLease {
    session_generation: String,
    agent_id: String,
    operation_id: u64,
}

impl RemoveLease {
    pub(crate) fn operation_id(&self) -> u64 {
        self.operation_id
    }
}

#[derive(Debug)]
pub(crate) enum ReserveAttach {
    Start(AttachLease),
    Join(AttachWaiter),
    AlreadyReady,
    Removing(AttachWaiter),
    SessionStopped,
}

#[derive(Debug)]
pub(crate) enum BeginRemove {
    Start(RemoveLease),
    Join(AttachWaiter),
    Absent,
    SessionStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRuntimeSnapshot {
    Absent,
    Attaching {
        operation_id: u64,
    },
    Ready,
    Failed {
        operation_id: u64,
        failure: MemberRuntimeFailure,
    },
    Removing {
        operation_id: u64,
    },
    SessionStopped,
}

#[derive(Debug)]
enum MemberRuntimeEntry {
    Attaching {
        operation_id: u64,
        outcome_tx: watch::Sender<AttachSignal>,
    },
    Ready,
    Failed {
        operation_id: u64,
        failure: MemberRuntimeFailure,
        outcome_tx: watch::Sender<AttachSignal>,
    },
    Removing {
        operation_id: u64,
        outcome_tx: watch::Sender<AttachSignal>,
    },
}

#[derive(Debug)]
pub(crate) struct MemberRuntimeRegistry {
    session_generation: String,
    next_operation_id: AtomicU64,
    entries: Mutex<HashMap<String, MemberRuntimeEntry>>,
    stopped: AtomicBool,
}

impl MemberRuntimeRegistry {
    pub(crate) fn new(session_generation: impl ToString) -> Self {
        Self {
            session_generation: session_generation.to_string(),
            next_operation_id: AtomicU64::new(1),
            entries: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    pub(crate) fn generation(&self) -> &str {
        &self.session_generation
    }

    pub(crate) fn seed_ready(&self, agent_id: impl Into<String>) -> bool {
        let agent_id = agent_id.into();
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        match entries.get(&agent_id) {
            None => {
                entries.insert(agent_id, MemberRuntimeEntry::Ready);
                true
            }
            Some(MemberRuntimeEntry::Ready) => true,
            Some(
                MemberRuntimeEntry::Attaching { .. }
                | MemberRuntimeEntry::Failed { .. }
                | MemberRuntimeEntry::Removing { .. },
            ) => false,
        }
    }

    pub(crate) fn reserve_attach(&self, agent_id: &str, retry_failed: bool) -> ReserveAttach {
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return ReserveAttach::SessionStopped;
        }

        match entries.get(agent_id) {
            Some(MemberRuntimeEntry::Attaching {
                operation_id,
                outcome_tx,
            }) => ReserveAttach::Join(waiter(*operation_id, outcome_tx.subscribe())),
            Some(MemberRuntimeEntry::Ready) => ReserveAttach::AlreadyReady,
            Some(MemberRuntimeEntry::Failed {
                operation_id,
                outcome_tx,
                ..
            }) if !retry_failed => ReserveAttach::Join(waiter(*operation_id, outcome_tx.subscribe())),
            Some(MemberRuntimeEntry::Removing {
                operation_id,
                outcome_tx,
            }) => ReserveAttach::Removing(waiter(*operation_id, outcome_tx.subscribe())),
            Some(MemberRuntimeEntry::Failed { .. }) | None => {
                let operation_id = self.next_operation_id();
                let (outcome_tx, outcome_rx) = watch::channel(AttachSignal::Pending);
                entries.insert(
                    agent_id.to_owned(),
                    MemberRuntimeEntry::Attaching {
                        operation_id,
                        outcome_tx,
                    },
                );
                ReserveAttach::Start(AttachLease {
                    session_generation: self.session_generation.clone(),
                    agent_id: agent_id.to_owned(),
                    waiter: waiter(operation_id, outcome_rx),
                })
            }
        }
    }

    /// Atomically converts a previously-ready slot into a repair attach.
    ///
    /// This is only for reconciliation after the task manager confirms the
    /// underlying runtime disappeared. Ordinary attaches must use
    /// [`Self::reserve_attach`].
    pub(crate) fn reserve_repair(&self, agent_id: &str) -> ReserveAttach {
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return ReserveAttach::SessionStopped;
        }

        match entries.get(agent_id) {
            Some(MemberRuntimeEntry::Attaching {
                operation_id,
                outcome_tx,
            }) => ReserveAttach::Join(waiter(*operation_id, outcome_tx.subscribe())),
            Some(MemberRuntimeEntry::Ready) => {
                let operation_id = self.next_operation_id();
                let (outcome_tx, outcome_rx) = watch::channel(AttachSignal::Pending);
                entries.insert(
                    agent_id.to_owned(),
                    MemberRuntimeEntry::Attaching {
                        operation_id,
                        outcome_tx,
                    },
                );
                ReserveAttach::Start(AttachLease {
                    session_generation: self.session_generation.clone(),
                    agent_id: agent_id.to_owned(),
                    waiter: waiter(operation_id, outcome_rx),
                })
            }
            Some(MemberRuntimeEntry::Failed {
                operation_id,
                outcome_tx,
                ..
            }) => ReserveAttach::Join(waiter(*operation_id, outcome_tx.subscribe())),
            Some(MemberRuntimeEntry::Removing {
                operation_id,
                outcome_tx,
            }) => ReserveAttach::Removing(waiter(*operation_id, outcome_tx.subscribe())),
            None => {
                let operation_id = self.next_operation_id();
                let (outcome_tx, outcome_rx) = watch::channel(AttachSignal::Pending);
                entries.insert(
                    agent_id.to_owned(),
                    MemberRuntimeEntry::Attaching {
                        operation_id,
                        outcome_tx,
                    },
                );
                ReserveAttach::Start(AttachLease {
                    session_generation: self.session_generation.clone(),
                    agent_id: agent_id.to_owned(),
                    waiter: waiter(operation_id, outcome_rx),
                })
            }
        }
    }

    pub(crate) fn owns_attach(&self, lease: &AttachLease) -> bool {
        if lease.session_generation != self.session_generation || self.stopped.load(Ordering::Acquire) {
            return false;
        }
        matches!(
            self.lock_entries().get(&lease.agent_id),
            Some(MemberRuntimeEntry::Attaching { operation_id, .. }) if *operation_id == lease.operation_id()
        )
    }

    pub(crate) fn commit_ready(&self, lease: &AttachLease) -> bool {
        if lease.session_generation != self.session_generation {
            return false;
        }
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Some(MemberRuntimeEntry::Attaching {
            operation_id: current_operation_id,
            outcome_tx,
        }) = entries.get(&lease.agent_id)
        else {
            return false;
        };
        if *current_operation_id != lease.operation_id() {
            return false;
        }
        let outcome_tx = outcome_tx.clone();
        entries.insert(lease.agent_id.clone(), MemberRuntimeEntry::Ready);
        outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::Ready));
        true
    }

    pub(crate) fn commit_failed(&self, lease: &AttachLease, failure: MemberRuntimeFailure) -> bool {
        if lease.session_generation != self.session_generation {
            return false;
        }
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Some(MemberRuntimeEntry::Attaching {
            operation_id: current_operation_id,
            outcome_tx,
        }) = entries.get(&lease.agent_id)
        else {
            return false;
        };
        if *current_operation_id != lease.operation_id() {
            return false;
        }
        let outcome_tx = outcome_tx.clone();
        entries.insert(
            lease.agent_id.clone(),
            MemberRuntimeEntry::Failed {
                operation_id: lease.operation_id(),
                failure: failure.clone(),
                outcome_tx: outcome_tx.clone(),
            },
        );
        outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::Failed(failure)));
        true
    }

    pub(crate) fn begin_remove(&self, agent_id: &str) -> BeginRemove {
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return BeginRemove::SessionStopped;
        }
        if let Some(MemberRuntimeEntry::Removing {
            operation_id,
            outcome_tx,
        }) = entries.get(agent_id)
        {
            return BeginRemove::Join(waiter(*operation_id, outcome_tx.subscribe()));
        }

        let previous = entries.remove(agent_id);
        let cancelled_outcome_tx = match previous {
            Some(MemberRuntimeEntry::Attaching {
                operation_id: _,
                outcome_tx,
            }) => Some(outcome_tx),
            Some(MemberRuntimeEntry::Failed { .. }) => None,
            Some(MemberRuntimeEntry::Ready) => None,
            Some(MemberRuntimeEntry::Removing { .. }) => unreachable!("handled above"),
            None => return BeginRemove::Absent,
        };

        let operation_id = self.next_operation_id();
        let (outcome_tx, _outcome_rx) = watch::channel(AttachSignal::Pending);
        entries.insert(
            agent_id.to_owned(),
            MemberRuntimeEntry::Removing {
                operation_id,
                outcome_tx,
            },
        );
        if let Some(cancelled_outcome_tx) = cancelled_outcome_tx {
            cancelled_outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::Removed));
        }
        BeginRemove::Start(RemoveLease {
            session_generation: self.session_generation.clone(),
            agent_id: agent_id.to_owned(),
            operation_id,
        })
    }

    pub(crate) fn finish_remove(&self, lease: &RemoveLease) -> bool {
        if lease.session_generation != self.session_generation {
            return false;
        }
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Some(MemberRuntimeEntry::Removing {
            operation_id: current_operation_id,
            outcome_tx,
        }) = entries.get(&lease.agent_id)
        else {
            return false;
        };
        if *current_operation_id != lease.operation_id() {
            return false;
        }
        let outcome_tx = outcome_tx.clone();
        entries.remove(&lease.agent_id);
        outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::Removed));
        true
    }

    /// Marks a removed runtime as requiring a fresh attach after persistence fails.
    ///
    /// Call this only after runtime cancellation and join cleanup have completed,
    /// so no live runtime remains. A later reconcile with `retry_failed = true`
    /// starts a new attach operation.
    pub(crate) fn restore_attach_required_after_remove_persist_error(
        &self,
        lease: &RemoveLease,
        failure: MemberRuntimeFailure,
    ) -> bool {
        if lease.session_generation != self.session_generation {
            return false;
        }
        let mut entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Some(MemberRuntimeEntry::Removing {
            operation_id: current_operation_id,
            outcome_tx,
        }) = entries.get(&lease.agent_id)
        else {
            return false;
        };
        if *current_operation_id != lease.operation_id() {
            return false;
        }
        let outcome_tx = outcome_tx.clone();
        entries.insert(
            lease.agent_id.clone(),
            MemberRuntimeEntry::Failed {
                operation_id: lease.operation_id(),
                failure: failure.clone(),
                outcome_tx: outcome_tx.clone(),
            },
        );
        outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::Failed(failure)));
        true
    }

    pub(crate) fn snapshot(&self, agent_id: &str) -> MemberRuntimeSnapshot {
        let entries = self.lock_entries();
        if self.stopped.load(Ordering::Acquire) {
            return MemberRuntimeSnapshot::SessionStopped;
        }
        match entries.get(agent_id) {
            None => MemberRuntimeSnapshot::Absent,
            Some(MemberRuntimeEntry::Attaching { operation_id, .. }) => MemberRuntimeSnapshot::Attaching {
                operation_id: *operation_id,
            },
            Some(MemberRuntimeEntry::Ready) => MemberRuntimeSnapshot::Ready,
            Some(MemberRuntimeEntry::Failed {
                operation_id, failure, ..
            }) => MemberRuntimeSnapshot::Failed {
                operation_id: *operation_id,
                failure: failure.clone(),
            },
            Some(MemberRuntimeEntry::Removing { operation_id, .. }) => MemberRuntimeSnapshot::Removing {
                operation_id: *operation_id,
            },
        }
    }

    pub(crate) fn stop(&self) -> bool {
        let mut entries = self.lock_entries();
        if self.stopped.swap(true, Ordering::AcqRel) {
            return false;
        }
        for entry in entries.drain().map(|(_, entry)| entry) {
            match entry {
                MemberRuntimeEntry::Attaching { outcome_tx, .. } | MemberRuntimeEntry::Removing { outcome_tx, .. } => {
                    outcome_tx.send_replace(AttachSignal::Terminal(AttachOutcome::SessionStopped));
                }
                MemberRuntimeEntry::Ready | MemberRuntimeEntry::Failed { .. } => {}
            }
        }
        true
    }

    fn next_operation_id(&self) -> u64 {
        self.next_operation_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Critical sections contain only short in-memory transitions with no await,
    /// I/O, or user callbacks, so retaining the map after poisoning is safe.
    fn lock_entries(&self) -> MutexGuard<'_, HashMap<String, MemberRuntimeEntry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn waiter(operation_id: u64, outcome_rx: watch::Receiver<AttachSignal>) -> AttachWaiter {
    AttachWaiter {
        operation_id,
        outcome_rx,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::{mpsc, oneshot};

    use super::*;

    fn failure(classification: &'static str, public_reason: &str) -> MemberRuntimeFailure {
        MemberRuntimeFailure {
            classification,
            public_reason: public_reason.to_owned(),
        }
    }

    #[tokio::test]
    async fn concurrent_reservations_join_the_same_attach_operation() {
        let registry = Arc::new(MemberRuntimeRegistry::new(41));
        let (ready_tx, mut ready_rx) = mpsc::channel(2);
        let (release_first_tx, release_first_rx) = oneshot::channel();
        let (release_second_tx, release_second_rx) = oneshot::channel();

        let first_registry = Arc::clone(&registry);
        let first_ready_tx = ready_tx.clone();
        let first = tokio::spawn(async move {
            first_ready_tx.send(()).await.unwrap();
            release_first_rx.await.unwrap();
            first_registry.reserve_attach("worker-1", false)
        });
        let second_registry = Arc::clone(&registry);
        let second = tokio::spawn(async move {
            ready_tx.send(()).await.unwrap();
            release_second_rx.await.unwrap();
            second_registry.reserve_attach("worker-1", false)
        });

        ready_rx.recv().await.unwrap();
        ready_rx.recv().await.unwrap();
        release_first_tx.send(()).unwrap();
        release_second_tx.send(()).unwrap();

        let first = first.await.unwrap();
        let second = second.await.unwrap();
        let (lease, waiter) = match (first, second) {
            (ReserveAttach::Start(lease), ReserveAttach::Join(waiter))
            | (ReserveAttach::Join(waiter), ReserveAttach::Start(lease)) => (lease, waiter),
            _ => panic!("expected one starter and one joiner"),
        };

        assert_eq!(lease.operation_id(), 1);
        assert_eq!(waiter.operation_id(), 1);
        assert!(registry.commit_ready(&lease));
        assert_eq!(lease.waiter().wait().await, AttachOutcome::Ready);
        assert_eq!(waiter.wait().await, AttachOutcome::Ready);
        assert_eq!(registry.snapshot("worker-1"), MemberRuntimeSnapshot::Ready);
    }

    #[tokio::test]
    async fn failed_operation_is_retried_only_by_a_later_reconcile() {
        let registry = MemberRuntimeRegistry::new(42);
        assert_eq!(registry.generation(), "42");
        assert!(registry.seed_ready("lead-1"));
        assert!(matches!(
            registry.reserve_attach("lead-1", false),
            ReserveAttach::AlreadyReady
        ));

        let first = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        assert_eq!(first.operation_id(), 1);
        let first_failure = failure("provider_unavailable", "Agent is temporarily unavailable");
        assert!(registry.commit_failed(&first, first_failure.clone()));
        assert_eq!(
            first.waiter().wait().await,
            AttachOutcome::Failed(first_failure.clone())
        );

        let failed_waiter = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Join(waiter) => waiter,
            _ => panic!("failed operation must not retry without reconciliation"),
        };
        assert_eq!(failed_waiter.operation_id(), 1);
        assert_eq!(failed_waiter.wait().await, AttachOutcome::Failed(first_failure));

        let retry = match registry.reserve_attach("worker-1", true) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("later reconciliation must start a retry"),
        };
        assert_eq!(retry.operation_id(), 2);
        assert_eq!(
            registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Attaching { operation_id: 2 }
        );
        assert!(registry.commit_ready(&retry));
        assert_eq!(retry.waiter().wait().await, AttachOutcome::Ready);
    }

    #[tokio::test]
    async fn removing_cancels_attach_and_rejects_late_ready_commit() {
        let registry = MemberRuntimeRegistry::new(43);
        let attach = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        assert_eq!(attach.operation_id(), 1);

        let remove = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("remove must start"),
        };
        assert_eq!(remove.operation_id(), 2);
        assert_eq!(
            registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Removing { operation_id: 2 }
        );
        assert_eq!(attach.waiter().wait().await, AttachOutcome::Removed);
        assert!(!registry.commit_ready(&attach));
        assert!(!registry.commit_failed(&attach, failure("late_provider_error", "Agent attach did not complete")));

        let removing_waiter = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Removing(waiter) => waiter,
            _ => panic!("reservation must wait while removal is active"),
        };
        assert_eq!(removing_waiter.operation_id(), 2);
        assert!(registry.finish_remove(&remove));
        assert_eq!(removing_waiter.wait().await, AttachOutcome::Removed);
        assert_eq!(registry.snapshot("worker-1"), MemberRuntimeSnapshot::Absent);

        assert!(registry.seed_ready("worker-2"));
        let second_remove = match registry.begin_remove("worker-2") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("seeded runtime removal must start"),
        };
        assert_eq!(second_remove.operation_id(), 3);
        let second_remove_waiter = match registry.reserve_attach("worker-2", false) {
            ReserveAttach::Removing(waiter) => waiter,
            _ => panic!("attach must wait for the active removal"),
        };
        let remove_failure = failure("remove_rejected", "Agent removal could not be completed");
        assert!(registry.restore_attach_required_after_remove_persist_error(&second_remove, remove_failure.clone()));
        assert_eq!(
            second_remove_waiter.wait().await,
            AttachOutcome::Failed(remove_failure.clone())
        );
        assert_eq!(
            registry.snapshot("worker-2"),
            MemberRuntimeSnapshot::Failed {
                operation_id: 3,
                failure: remove_failure.clone(),
            }
        );

        let retry = match registry.reserve_attach("worker-2", true) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("persistence failure must require a fresh attach"),
        };
        assert_eq!(retry.operation_id(), 4);
        assert!(registry.commit_ready(&retry));
    }

    #[tokio::test]
    async fn session_stop_cancels_all_attaches_and_unblocks_waiters() {
        let registry = Arc::new(MemberRuntimeRegistry::new(44));
        let first = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        let second = match registry.reserve_attach("worker-2", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("second reservation must start"),
        };
        assert_eq!(first.operation_id(), 1);
        assert_eq!(second.operation_id(), 2);

        let (waiting_tx, mut waiting_rx) = mpsc::channel(2);
        let first_wait = first.waiter();
        let first_waiting_tx = waiting_tx.clone();
        let first_task = tokio::spawn(async move {
            first_waiting_tx.send(()).await.unwrap();
            first_wait.wait().await
        });
        let second_wait = second.waiter();
        let second_task = tokio::spawn(async move {
            waiting_tx.send(()).await.unwrap();
            second_wait.wait().await
        });
        waiting_rx.recv().await.unwrap();
        waiting_rx.recv().await.unwrap();

        assert!(registry.stop());
        assert!(!registry.stop());
        assert_eq!(first_task.await.unwrap(), AttachOutcome::SessionStopped);
        assert_eq!(second_task.await.unwrap(), AttachOutcome::SessionStopped);
        assert!(!registry.commit_ready(&first));
        assert!(!registry.commit_failed(&second, failure("provider_stopped", "Agent session stopped")));
        assert_eq!(registry.snapshot("worker-1"), MemberRuntimeSnapshot::SessionStopped);

        assert!(matches!(
            registry.reserve_attach("worker-3", false),
            ReserveAttach::SessionStopped
        ));
    }

    #[tokio::test]
    async fn stale_operation_id_cannot_commit_over_a_newer_operation() {
        let registry = MemberRuntimeRegistry::new(45);
        let first = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        let first_failure = failure("transport", "Agent connection failed");
        assert!(registry.commit_failed(&first, first_failure.clone()));
        assert_eq!(first.waiter().wait().await, AttachOutcome::Failed(first_failure));

        let second = match registry.reserve_attach("worker-1", true) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("retry reservation must start"),
        };
        assert_eq!(second.operation_id(), 2);
        assert!(!registry.commit_ready(&first));
        assert!(!registry.commit_failed(&first, failure("stale", "Stale attach failed")));
        assert_eq!(
            registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Attaching { operation_id: 2 }
        );
        assert!(registry.commit_ready(&second));
        assert_eq!(second.waiter().wait().await, AttachOutcome::Ready);
        assert_eq!(registry.snapshot("worker-1"), MemberRuntimeSnapshot::Ready);
    }

    #[tokio::test]
    async fn failed_attach_outcome_is_not_rewritten_when_remove_begins() {
        let registry = MemberRuntimeRegistry::new(46);
        let attach = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        let attach_failure = failure("transport", "Agent connection failed");
        assert!(registry.commit_failed(&attach, attach_failure.clone()));

        let remove = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("failed runtime removal must start"),
        };

        assert_eq!(remove.operation_id(), 2);
        assert_eq!(attach.waiter().wait().await, AttachOutcome::Failed(attach_failure));
    }

    #[tokio::test]
    async fn failed_attach_outcome_is_not_rewritten_when_session_stops() {
        let registry = MemberRuntimeRegistry::new(47);
        let attach = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("first reservation must start"),
        };
        let attach_failure = failure("transport", "Agent connection failed");
        assert!(registry.commit_failed(&attach, attach_failure.clone()));

        assert!(registry.stop());

        assert_eq!(attach.waiter().wait().await, AttachOutcome::Failed(attach_failure));
    }

    #[tokio::test]
    async fn concurrent_remove_requests_join_the_same_remove_operation() {
        let registry = MemberRuntimeRegistry::new(48);
        assert!(registry.seed_ready("worker-1"));
        let owner = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("first remove must start"),
        };
        let joiner = match registry.begin_remove("worker-1") {
            BeginRemove::Join(waiter) => waiter,
            _ => panic!("second remove must join"),
        };

        assert_eq!(owner.operation_id(), 1);
        assert_eq!(joiner.operation_id(), 1);
        assert!(registry.finish_remove(&owner));
        assert_eq!(joiner.wait().await, AttachOutcome::Removed);
    }

    #[tokio::test]
    async fn session_stop_unblocks_waiters_for_an_active_remove() {
        let registry = MemberRuntimeRegistry::new(49);
        assert!(registry.seed_ready("worker-1"));
        let owner = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("remove must start"),
        };
        let reserve_waiter = match registry.reserve_attach("worker-1", false) {
            ReserveAttach::Removing(waiter) => waiter,
            _ => panic!("attach must wait for active remove"),
        };

        assert!(registry.stop());

        assert_eq!(reserve_waiter.wait().await, AttachOutcome::SessionStopped);
        assert!(!registry.finish_remove(&owner));
    }

    #[test]
    fn stale_remove_owner_cannot_finish_or_restore_a_newer_remove() {
        let registry = MemberRuntimeRegistry::new(50);
        assert!(registry.seed_ready("worker-1"));
        let first = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("first remove must start"),
        };
        assert!(registry.finish_remove(&first));

        assert!(registry.seed_ready("worker-1"));
        let second = match registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("second remove must start"),
        };

        assert_eq!(first.operation_id(), 1);
        assert_eq!(second.operation_id(), 2);
        assert!(!registry.finish_remove(&first));
        assert!(
            !registry
                .restore_attach_required_after_remove_persist_error(&first, failure("stale", "Stale remove failed"))
        );
        assert_eq!(
            registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Removing { operation_id: 2 }
        );
        assert!(registry.finish_remove(&second));
    }

    #[test]
    fn attach_owner_from_another_session_generation_cannot_commit() {
        let old_registry = MemberRuntimeRegistry::new(51);
        let old_owner = match old_registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("old session attach must start"),
        };
        let current_registry = MemberRuntimeRegistry::new(52);
        let current_owner = match current_registry.reserve_attach("worker-1", false) {
            ReserveAttach::Start(lease) => lease,
            _ => panic!("current session attach must start"),
        };

        assert_eq!(old_owner.operation_id(), 1);
        assert_eq!(current_owner.operation_id(), 1);
        assert!(!current_registry.commit_ready(&old_owner));
        assert_eq!(
            current_registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Attaching { operation_id: 1 }
        );
        assert!(current_registry.commit_ready(&current_owner));
    }

    #[test]
    fn remove_owner_from_another_session_generation_cannot_finish_or_restore() {
        let old_registry = MemberRuntimeRegistry::new(53);
        assert!(old_registry.seed_ready("worker-1"));
        let old_owner = match old_registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("old session remove must start"),
        };
        let current_registry = MemberRuntimeRegistry::new(54);
        assert!(current_registry.seed_ready("worker-1"));
        let current_owner = match current_registry.begin_remove("worker-1") {
            BeginRemove::Start(lease) => lease,
            _ => panic!("current session remove must start"),
        };

        assert_eq!(old_owner.operation_id(), 1);
        assert_eq!(current_owner.operation_id(), 1);
        assert!(!current_registry.finish_remove(&old_owner));
        assert!(!current_registry.restore_attach_required_after_remove_persist_error(
            &old_owner,
            failure("stale_session", "Stale session remove failed")
        ));
        assert_eq!(
            current_registry.snapshot("worker-1"),
            MemberRuntimeSnapshot::Removing { operation_id: 1 }
        );
        assert!(current_registry.finish_remove(&current_owner));
    }
}
