use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use aionui_api_types::TeamRunTargetRole;
use aionui_common::{generate_id, now_ms};
use tracing::{debug, info, warn};

use super::model::*;
use crate::TeamError;
use crate::work_source::WorkSource;

#[derive(Debug, Clone)]
pub(super) struct EnqueueLeaseRecord {
    pub(super) lease: EnqueueLease,
    binding: RunBinding,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveBatch {
    pub(super) batch: WorkBatch,
    pub(super) turn_id: Option<String>,
    pub(super) started_at_ms: Option<aionui_common::TimestampMs>,
}

#[derive(Debug)]
pub(super) struct SlotState {
    pub(super) role: TeamRunTargetRole,
    // Declared in claim order; see `SlotWorkCoordinator::next`.
    pub(super) foreground: VecDeque<String>,
    pub(super) control: VecDeque<String>,
    pub(super) directed: VecDeque<String>,
    pub(super) background: VecDeque<String>,
    pub(super) active: Option<ActiveBatch>,
    pub(super) paused: bool,
    pub(super) runtime_constraint: RuntimeConstraint,
    known_unread_message_ids: HashSet<String>,
    delivery_failure_counts: HashMap<String, u8>,
    applied_mcp_fingerprint: Option<String>,
    pending_mcp_fingerprint: Option<String>,
    mcp_refresh_in_progress: bool,
    removed: bool,
}

impl SlotState {
    fn new(role: TeamRunTargetRole) -> Self {
        Self {
            role,
            foreground: VecDeque::new(),
            control: VecDeque::new(),
            directed: VecDeque::new(),
            background: VecDeque::new(),
            active: None,
            paused: false,
            runtime_constraint: RuntimeConstraint::Starting { operation_id: 0 },
            known_unread_message_ids: HashSet::new(),
            delivery_failure_counts: HashMap::new(),
            applied_mcp_fingerprint: None,
            pending_mcp_fingerprint: None,
            mcp_refresh_in_progress: false,
            removed: false,
        }
    }

    fn queue(&self, priority: WorkPriority) -> &VecDeque<String> {
        match priority {
            WorkPriority::Foreground => &self.foreground,
            WorkPriority::Control => &self.control,
            WorkPriority::Directed => &self.directed,
            WorkPriority::Background => &self.background,
        }
    }

    fn queue_mut(&mut self, priority: WorkPriority) -> &mut VecDeque<String> {
        match priority {
            WorkPriority::Foreground => &mut self.foreground,
            WorkPriority::Control => &mut self.control,
            WorkPriority::Directed => &mut self.directed,
            WorkPriority::Background => &mut self.background,
        }
    }

    pub(super) fn queued_ids(&self) -> impl Iterator<Item = &String> {
        self.foreground
            .iter()
            .chain(self.control.iter())
            .chain(self.directed.iter())
            .chain(self.background.iter())
    }

    fn remove_queued(&mut self, intent_id: &str) {
        for queue in [
            &mut self.foreground,
            &mut self.control,
            &mut self.directed,
            &mut self.background,
        ] {
            queue.retain(|candidate| candidate != intent_id);
        }
    }
}

#[derive(Default)]
pub(super) struct CoordinatorState {
    pub(super) slots: BTreeMap<String, SlotState>,
    pub(super) intents: HashMap<String, WorkIntent>,
    pub(super) enqueue_leases: HashMap<String, EnqueueLeaseRecord>,
    interrupted_batches: HashMap<String, BatchInterruptMetadata>,
    next_operation_id: u64,
}

pub(crate) struct SlotWorkCoordinator {
    team_id: String,
    pub(super) session_generation: String,
    pub(super) run_causality: Arc<dyn RunCausalityPort>,
    pub(super) state: Mutex<CoordinatorState>,
}

impl SlotWorkCoordinator {
    pub(crate) fn new(team_id: String, session_generation: String, run_causality: Arc<dyn RunCausalityPort>) -> Self {
        Self {
            team_id,
            session_generation,
            run_causality,
            state: Mutex::new(CoordinatorState::default()),
        }
    }

    pub(crate) fn acquire_enqueue(&self, request: EnqueueRequest) -> Result<EnqueueLease, TeamError> {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(request.slot_id.clone())
            .or_insert_with(|| SlotState::new(request.role.clone()));
        slot.role = request.role.clone();
        Self::validate_enqueue_constraint(&request.slot_id, slot)?;

        let binding = match &request.binding {
            CausalBinding::Background => RunBinding {
                team_run_id: None,
                created_new_run: false,
                user_intervention: false,
            },
            CausalBinding::InheritRunningBatch { caller_slot_id } => {
                let inherited = state
                    .slots
                    .get(caller_slot_id)
                    .and_then(|caller| caller.active.as_ref())
                    .and_then(|active| active.batch.team_run_ids.first().cloned());
                match inherited {
                    // Inherit the caller's active run when there is one.
                    Some(team_run_id) => RunBinding {
                        team_run_id: Some(team_run_id),
                        created_new_run: false,
                        user_intervention: false,
                    },
                    // No inheritable run: a run-scoped wake (send/shutdown) is a
                    // legitimate work initiation and must live in a run, so fall
                    // back to attach-or-create a SystemLifecycle run (same as
                    // SystemInitiated). Closes "no active team run for
                    // run-scoped wake" at the single enqueue choke-point.
                    None => self.run_causality.bind_system_enqueue(&request),
                }
            }
            CausalBinding::UserVisible | CausalBinding::ActiveRunOrBackground => {
                self.run_causality.bind_enqueue(&request)
            }
            CausalBinding::SystemInitiated { inherit_from } => {
                let inherited = inherit_from.as_ref().and_then(|caller_slot_id| {
                    state
                        .slots
                        .get(caller_slot_id)
                        .and_then(|caller| caller.active.as_ref())
                        .and_then(|active| active.batch.team_run_ids.first().cloned())
                });
                match inherited {
                    Some(team_run_id) => RunBinding {
                        team_run_id: Some(team_run_id),
                        created_new_run: false,
                        user_intervention: false,
                    },
                    None => self.run_causality.bind_system_enqueue(&request),
                }
            }
        };
        let lease = EnqueueLease {
            lease_id: generate_id(),
            session_generation: self.session_generation.clone(),
            slot_id: request.slot_id,
            role: request.role,
            source: request.source,
            team_run_id: binding.team_run_id.clone(),
        };
        state.enqueue_leases.insert(
            lease.lease_id.clone(),
            EnqueueLeaseRecord {
                lease: lease.clone(),
                binding,
            },
        );
        Ok(lease)
    }

    pub(crate) fn commit_enqueue(
        &self,
        lease: &EnqueueLease,
        mailbox_message_id: Option<String>,
    ) -> Result<EnqueueCommit, TeamError> {
        let mut state = self.lock_state();
        let record = state
            .enqueue_leases
            .get(&lease.lease_id)
            .cloned()
            .ok_or_else(|| TeamError::InvalidRequest("enqueue lease is no longer active".into()))?;
        if record.lease != *lease || lease.session_generation != self.session_generation {
            return Err(TeamError::InvalidRequest("stale enqueue lease owner".into()));
        }
        if lease.source.requires_mailbox_message() && mailbox_message_id.is_none() {
            return Err(TeamError::InvalidRequest(format!(
                "{:?} requires a mailbox message",
                lease.source
            )));
        }

        let disposition = {
            let slot = state
                .slots
                .get(&lease.slot_id)
                .ok_or_else(|| TeamError::AgentNotFound(lease.slot_id.clone()))?;
            Self::validate_enqueue_constraint(&lease.slot_id, slot)?;
            match slot.runtime_constraint {
                RuntimeConstraint::Starting { .. } => EnqueueDisposition::BlockedRuntimeStarting,
                RuntimeConstraint::Ready if slot.active.is_none() && slot.queued_ids().next().is_none() => {
                    EnqueueDisposition::Accepted
                }
                RuntimeConstraint::Ready => EnqueueDisposition::Queued,
                RuntimeConstraint::Failed { .. }
                | RuntimeConstraint::Removing { .. }
                | RuntimeConstraint::SessionStopped => unreachable!("validated above"),
            }
        };

        state.enqueue_leases.remove(&lease.lease_id);
        let intent_id = generate_id();
        let intent = WorkIntent {
            intent_id: intent_id.clone(),
            session_generation: self.session_generation.clone(),
            slot_id: lease.slot_id.clone(),
            role: lease.role.clone(),
            source: lease.source,
            priority: lease.source.priority(),
            mailbox_message_id: mailbox_message_id.clone(),
            team_run_id: lease.team_run_id.clone(),
            created_at_ms: now_ms(),
            state: WorkIntentState::Queued,
        };
        state.intents.insert(intent_id.clone(), intent.clone());
        let slot = state.slots.get_mut(&lease.slot_id).expect("validated slot exists");
        if lease.source.resumes_paused_slot() {
            slot.paused = false;
        }
        if let Some(message_id) = mailbox_message_id {
            slot.known_unread_message_ids.insert(message_id);
        }
        if lease.source == WorkSource::LeadIntervention {
            slot.queue_mut(intent.priority).push_front(intent_id.clone());
        } else {
            slot.queue_mut(intent.priority).push_back(intent_id.clone());
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, &lease.slot_id).expect("committed slot exists");
        let summaries = Self::run_summaries_locked(&state, lease.team_run_id.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(Some(slot_snapshot.clone()));

        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %lease.slot_id,
            intent_id = %intent_id,
            team_run_id = lease.team_run_id.as_deref().unwrap_or("background"),
            source = ?lease.source,
            priority = ?lease.source.priority(),
            "team work enqueue committed"
        );
        Ok(EnqueueCommit {
            intent_id,
            team_run_id: lease.team_run_id.clone(),
            disposition,
            slot: slot_snapshot,
        })
    }

    pub(crate) fn abort_enqueue(&self, lease: &EnqueueLease, classification: &'static str) -> CommitResult {
        let mut state = self.lock_state();
        let Some(record) = state.enqueue_leases.remove(&lease.lease_id) else {
            return CommitResult::StaleOwner;
        };
        if record.lease != *lease || lease.session_generation != self.session_generation {
            state.enqueue_leases.insert(record.lease.lease_id.clone(), record);
            return CommitResult::StaleOwner;
        }
        let remove_empty_run = lease.team_run_id.as_ref().is_some_and(|team_run_id| {
            !state
                .intents
                .values()
                .any(|intent| intent.team_run_id.as_ref() == Some(team_run_id))
                && !state
                    .enqueue_leases
                    .values()
                    .any(|candidate| candidate.lease.team_run_id.as_ref() == Some(team_run_id))
        });
        let slot_snapshot = Self::slot_snapshot_locked(&state, &lease.slot_id);
        let summaries = Self::run_summaries_locked(&state, lease.team_run_id.iter().cloned());
        drop(state);
        if remove_empty_run {
            self.run_causality.abort_binding(&record.binding);
        }
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        debug!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %lease.slot_id,
            lease_id = %lease.lease_id,
            classification,
            "team work enqueue aborted"
        );
        CommitResult::Committed
    }

    #[cfg(test)]
    pub(crate) fn reconcile_mailbox(
        &self,
        slot_id: &str,
        unread_message_ids: &[String],
        role: TeamRunTargetRole,
    ) -> ReconcileProjection {
        let stale_candidates = self.mailbox_reconcile_candidates(slot_id);
        self.reconcile_mailbox_snapshot(slot_id, unread_message_ids, role, &stale_candidates)
    }

    pub(crate) fn reconcile_mailbox_snapshot(
        &self,
        slot_id: &str,
        unread_message_ids: &[String],
        role: TeamRunTargetRole,
        stale_candidates: &HashSet<String>,
    ) -> ReconcileProjection {
        let mut state = self.lock_state();
        let unread = unread_message_ids.iter().cloned().collect::<HashSet<_>>();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(role.clone()));
        slot.role = role.clone();
        slot.known_unread_message_ids = unread.clone();
        slot.delivery_failure_counts
            .retain(|message_id, _| unread.contains(message_id));

        let candidates = slot.queued_ids().cloned().collect::<Vec<_>>();
        let mut retained_intent_ids = Vec::new();
        let mut cleared_stale_intent_ids = Vec::new();
        for intent_id in candidates {
            let Some(intent) = state.intents.get_mut(&intent_id) else {
                continue;
            };
            let Some(message_id) = intent.mailbox_message_id.as_ref() else {
                retained_intent_ids.push(intent_id);
                continue;
            };
            if unread.contains(message_id) || !stale_candidates.contains(&intent_id) {
                retained_intent_ids.push(intent_id);
            } else {
                intent.state = WorkIntentState::Completed;
                cleared_stale_intent_ids.push(intent_id);
            }
        }
        if let Some(slot) = state.slots.get_mut(slot_id) {
            for intent_id in &cleared_stale_intent_ids {
                slot.remove_queued(intent_id);
            }
        }

        let mut created_recovery_intent_ids = Vec::new();
        for message_id in unread_message_ids {
            let already_owned = state.intents.values().any(|intent| {
                intent.slot_id == slot_id
                    && intent.mailbox_message_id.as_deref() == Some(message_id.as_str())
                    && !intent.state.is_terminal()
            });
            if already_owned {
                continue;
            }
            let intent_id = generate_id();
            state.intents.insert(
                intent_id.clone(),
                WorkIntent {
                    intent_id: intent_id.clone(),
                    session_generation: self.session_generation.clone(),
                    slot_id: slot_id.to_owned(),
                    role: role.clone(),
                    source: WorkSource::RecoveryDrain,
                    priority: WorkPriority::Background,
                    mailbox_message_id: Some(message_id.clone()),
                    team_run_id: None,
                    created_at_ms: now_ms(),
                    state: WorkIntentState::Queued,
                },
            );
            state
                .slots
                .get_mut(slot_id)
                .expect("reconciled slot exists")
                .background
                .push_back(intent_id.clone());
            created_recovery_intent_ids.push(intent_id);
        }
        debug!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id,
            unread_count = unread_message_ids.len(),
            created_recovery_count = created_recovery_intent_ids.len(),
            cleared_stale_count = cleared_stale_intent_ids.len(),
            "team work mailbox reconciled"
        );
        ReconcileProjection {
            created_recovery_intent_ids,
            retained_intent_ids,
            cleared_stale_intent_ids,
        }
    }

    pub(crate) fn mailbox_reconcile_candidates(&self, slot_id: &str) -> HashSet<String> {
        let state = self.lock_state();
        state
            .slots
            .get(slot_id)
            .into_iter()
            .flat_map(SlotState::queued_ids)
            .filter(|intent_id| {
                state
                    .intents
                    .get(*intent_id)
                    .is_some_and(|intent| intent.mailbox_message_id.is_some())
            })
            .cloned()
            .collect()
    }

    pub(crate) fn next(&self, slot_id: &str) -> ReconcileDecision {
        let mut state = self.lock_state();
        let Some(slot) = state.slots.get(slot_id) else {
            return ReconcileDecision::Quiescent;
        };
        if slot.active.is_some() {
            return ReconcileDecision::WaitingForCompletion;
        }
        if !matches!(slot.runtime_constraint, RuntimeConstraint::Ready) {
            return ReconcileDecision::Blocked(slot.runtime_constraint.clone());
        }
        if slot.paused {
            return ReconcileDecision::Quiescent;
        }

        // Control outranks Directed. Control carries only shutdown request /
        // rejection — at most a couple of messages per slot — so putting it first
        // cannot starve the directed lane, while the reverse is not true: teammate
        // traffic arrives continuously and would keep pushing a shutdown request
        // behind another round of work. A teammate that wants to finish what it is
        // holding can still answer `shutdown_rejected` with a reason; that is the
        // right place for that decision, not the lane order.
        let selected_priority = [
            WorkPriority::Foreground,
            WorkPriority::Control,
            WorkPriority::Directed,
            WorkPriority::Background,
        ]
        .into_iter()
        .find(|priority| !slot.queue(*priority).is_empty());
        let Some(priority) = selected_priority else {
            info!(
                team_id = %self.team_id,
                session_generation = %self.session_generation,
                slot_id,
                "team work slot quiescent"
            );
            return ReconcileDecision::Quiescent;
        };

        let queued_ids = slot.queue(priority).iter().cloned().collect::<Vec<_>>();
        // Message intents (those carrying a mailbox row), in FIFO order.
        let fifo_message_intent_ids = queued_ids
            .iter()
            .filter(|intent_id| {
                state
                    .intents
                    .get(*intent_id)
                    .is_some_and(|intent| intent.mailbox_message_id.is_some())
            })
            .cloned()
            .collect::<Vec<_>>();
        if fifo_message_intent_ids.is_empty() {
            return ReconcileDecision::SettleSignals(queued_ids);
        }

        // ELECTRON-3RN: isolate recognized slash commands into single-message
        // batches (FIFO, no preemption). The queue head decides:
        // - head is a `UserCommand` → this batch is exactly that one command, so
        //   the native op is not fed the rest of the turn (the flaw that ruled
        //   out option B);
        // - head is an ordinary message → merge the leading run of ordinary
        //   messages but STOP at the first `UserCommand` so a command is never
        //   folded into a plain batch (existing multi-message merge, bounded).
        let head_source = state
            .intents
            .get(&fifo_message_intent_ids[0])
            .map(|intent| intent.source);
        let head_is_isolated = matches!(
            head_source,
            Some(WorkSource::UserCommand | WorkSource::LeadIntervention)
        );
        let (message_intent_ids, is_command) = if head_is_isolated {
            (
                vec![fifo_message_intent_ids[0].clone()],
                head_source == Some(WorkSource::UserCommand),
            )
        } else {
            let mut selected = Vec::new();
            for intent_id in &fifo_message_intent_ids {
                let is_batch_barrier = state.intents.get(intent_id).is_some_and(|intent| {
                    matches!(intent.source, WorkSource::UserCommand | WorkSource::LeadIntervention)
                });
                if is_batch_barrier {
                    break;
                }
                selected.push(intent_id.clone());
            }
            (selected, false)
        };

        state.next_operation_id = state.next_operation_id.saturating_add(1);
        let operation_id = state.next_operation_id;
        let batch_id = generate_id();
        let mut mailbox_message_ids = Vec::new();
        let mut team_run_ids = Vec::new();
        for intent_id in &message_intent_ids {
            let intent = state.intents.get_mut(intent_id).expect("queued intent exists");
            intent.state = WorkIntentState::Starting {
                batch_id: batch_id.clone(),
                operation_id,
            };
            if let Some(message_id) = &intent.mailbox_message_id {
                mailbox_message_ids.push(message_id.clone());
            }
            if let Some(team_run_id) = &intent.team_run_id
                && !team_run_ids.contains(team_run_id)
            {
                team_run_ids.push(team_run_id.clone());
            }
        }
        let batch = WorkBatch {
            batch_id,
            session_generation: self.session_generation.clone(),
            slot_id: slot_id.to_owned(),
            intent_ids: message_intent_ids.clone(),
            mailbox_message_ids,
            observed_message_ids: Vec::new(),
            highest_priority: priority,
            team_run_ids: team_run_ids.clone(),
            operation_id,
            is_command,
        };
        let slot = state.slots.get_mut(slot_id).expect("selected slot exists");
        for intent_id in &message_intent_ids {
            slot.remove_queued(intent_id);
        }
        slot.active = Some(ActiveBatch {
            batch: batch.clone(),
            turn_id: None,
            started_at_ms: None,
        });
        let slot_snapshot = Self::slot_snapshot_locked(&state, slot_id);
        let summaries = Self::run_summaries_locked(&state, team_run_ids);
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id,
            batch_id = %batch.batch_id,
            operation_id,
            intent_count = batch.intent_ids.len(),
            message_count = batch.mailbox_message_ids.len(),
            priority = ?batch.highest_priority,
            is_command = batch.is_command,
            "team work batch claimed"
        );
        ReconcileDecision::Claim(batch)
    }

    /// The batch that currently owns `slot_id`'s turn, if any. Captured at peek
    /// time so a later `observe_messages` can prove it is still talking about
    /// the same turn.
    pub(crate) fn active_batch_id(&self, slot_id: &str) -> Option<String> {
        self.lock_state()
            .slots
            .get(slot_id)
            .and_then(|slot| slot.active.as_ref())
            .map(|active| active.batch.batch_id.clone())
    }

    /// Bind mailbox rows observed through `team_read_messages` to the active
    /// turn. The binding is coordinator-local until the turn succeeds, so a
    /// failed, cancelled, or interrupted turn leaves every observed row unread
    /// and available to the existing retry/recovery path.
    ///
    /// `expected_batch_id` is the batch that owned the turn when the rows were
    /// read. A mismatch means the turn was replaced (cancelled, interrupted, or
    /// completed) while the tool call was in flight, so the rows must not be
    /// attributed to whatever batch happens to own the slot now — dropping the
    /// binding leaves them unread for the normal delivery path.
    pub(crate) fn observe_messages(
        &self,
        slot_id: &str,
        expected_batch_id: &str,
        message_ids: &[String],
    ) -> ObserveMessagesResult {
        let mut state = self.lock_state();
        let Some(active) = state.slots.get_mut(slot_id).and_then(|slot| slot.active.as_mut()) else {
            return ObserveMessagesResult {
                batch_id: None,
                observed_count: 0,
            };
        };
        if active.batch.batch_id != expected_batch_id {
            let current_batch_id = active.batch.batch_id.clone();
            drop(state);
            warn!(
                team_id = %self.team_id,
                session_generation = %self.session_generation,
                slot_id,
                expected_batch_id,
                current_batch_id,
                "team mailbox observation dropped; active turn was replaced"
            );
            return ObserveMessagesResult {
                batch_id: None,
                observed_count: 0,
            };
        }
        let original_ids = active.batch.mailbox_message_ids.iter().cloned().collect::<HashSet<_>>();
        let mut known_ids = active
            .batch
            .observed_message_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut observed_count = 0;
        for message_id in message_ids {
            if original_ids.contains(message_id) || !known_ids.insert(message_id.clone()) {
                continue;
            }
            active.batch.observed_message_ids.push(message_id.clone());
            observed_count += 1;
        }
        let batch_id = active.batch.batch_id.clone();
        drop(state);
        debug!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id,
            batch_id,
            observed_count,
            "team mailbox messages bound to active turn"
        );
        ObserveMessagesResult {
            batch_id: Some(batch_id),
            observed_count,
        }
    }

    pub(crate) fn mark_started(&self, batch: &WorkBatch, turn_id: &str) -> StartCommitResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            let was_cancelled = batch.intent_ids.iter().all(|intent_id| {
                state
                    .intents
                    .get(intent_id)
                    .is_some_and(|intent| matches!(intent.state, WorkIntentState::Cancelled { .. }))
            });
            if was_cancelled {
                return StartCommitResult::CancelImmediately;
            }
            self.log_stale_batch(batch, "mark_started");
            return StartCommitResult::StaleOwner;
        }
        let cancel_immediately = state
            .slots
            .get(&batch.slot_id)
            .is_some_and(|slot| slot.paused || !matches!(slot.runtime_constraint, RuntimeConstraint::Ready));
        for intent_id in &batch.intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = WorkIntentState::Running {
                    batch_id: batch.batch_id.clone(),
                    operation_id: batch.operation_id,
                    turn_id: turn_id.to_owned(),
                };
            }
        }
        if let Some(active) = state
            .slots
            .get_mut(&batch.slot_id)
            .and_then(|slot| slot.active.as_mut())
        {
            active.turn_id = Some(turn_id.to_owned());
            active.started_at_ms = Some(now_ms());
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, &batch.slot_id);
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        if cancel_immediately {
            StartCommitResult::CancelImmediately
        } else {
            StartCommitResult::Accepted
        }
    }

    pub(crate) fn retry_start(&self, batch: &WorkBatch, classification: &'static str) -> CommitResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            self.log_stale_batch(batch, "retry_start");
            return CommitResult::StaleOwner;
        }
        for intent_id in &batch.intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = WorkIntentState::Queued;
            }
        }
        let slot = state.slots.get_mut(&batch.slot_id).expect("current batch slot exists");
        slot.active = None;
        for intent_id in batch.intent_ids.iter().rev() {
            slot.queue_mut(batch.highest_priority).push_front(intent_id.clone());
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, &batch.slot_id);
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        debug!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %batch.slot_id,
            batch_id = %batch.batch_id,
            operation_id = batch.operation_id,
            classification,
            "team work batch start will retry"
        );
        CommitResult::Committed
    }

    #[cfg(test)]
    pub(crate) fn complete_batch(&self, batch: &WorkBatch) -> CommitResult {
        self.complete_batch_with_ack(batch).commit_result
    }

    pub(crate) fn complete_batch_with_ack(&self, batch: &WorkBatch) -> BatchCompletionResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            self.log_stale_batch(batch, "complete_batch_with_ack");
            return BatchCompletionResult {
                commit_result: CommitResult::StaleOwner,
                ack_message_ids: Vec::new(),
                team_run_ids: Vec::new(),
            };
        }

        let active_batch = state
            .slots
            .get(&batch.slot_id)
            .and_then(|slot| slot.active.as_ref())
            .expect("current batch slot has active ownership")
            .batch
            .clone();
        let mut ack_message_ids = active_batch.mailbox_message_ids.clone();
        for message_id in &active_batch.observed_message_ids {
            if !ack_message_ids.contains(message_id) {
                ack_message_ids.push(message_id.clone());
            }
        }
        let observed_ids = active_batch
            .observed_message_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut completed_intent_ids = active_batch.intent_ids.iter().cloned().collect::<HashSet<_>>();
        let mut team_run_ids = active_batch.team_run_ids.clone();

        for intent in state.intents.values_mut() {
            let belongs_to_observed_message = intent.slot_id == batch.slot_id
                && intent
                    .mailbox_message_id
                    .as_ref()
                    .is_some_and(|message_id| observed_ids.contains(message_id));
            if completed_intent_ids.contains(&intent.intent_id)
                || (belongs_to_observed_message && !intent.state.is_terminal())
            {
                completed_intent_ids.insert(intent.intent_id.clone());
                if let Some(team_run_id) = &intent.team_run_id
                    && !team_run_ids.contains(team_run_id)
                {
                    team_run_ids.push(team_run_id.clone());
                }
                intent.state = WorkIntentState::Completed;
            }
        }

        let slot = state.slots.get_mut(&batch.slot_id).expect("current batch slot exists");
        for intent_id in &completed_intent_ids {
            slot.remove_queued(intent_id);
        }
        slot.active = None;
        for message_id in &ack_message_ids {
            slot.delivery_failure_counts.remove(message_id);
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, &batch.slot_id);
        let summaries = Self::run_summaries_locked(&state, team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %batch.slot_id,
            batch_id = %batch.batch_id,
            operation_id = batch.operation_id,
            ack_count = ack_message_ids.len(),
            observed_count = active_batch.observed_message_ids.len(),
            "team work batch terminal"
        );
        BatchCompletionResult {
            commit_result: CommitResult::Committed,
            ack_message_ids,
            team_run_ids,
        }
    }

    pub(crate) fn fail_batch(&self, batch: &WorkBatch, classification: &'static str) -> BatchFailureResult {
        self.terminalize_batch(
            batch,
            WorkIntentState::Failed { classification },
            classification,
            DeliveryOutcome::Failed,
        )
    }

    pub(crate) fn cancel_batch(&self, batch: &WorkBatch, classification: &'static str) -> CommitResult {
        // A cancelled batch never reached the agent, so it is not a failed delivery
        // attempt and must not consume the message's retry budget.
        self.terminalize_batch(
            batch,
            WorkIntentState::Cancelled { classification },
            classification,
            DeliveryOutcome::NotFailed,
        )
        .commit_result
    }

    pub(crate) fn interrupt_batch(
        &self,
        batch: &WorkBatch,
        reason: Option<String>,
        replacement_message_id: String,
    ) -> InterruptBatchResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            self.log_stale_batch(batch, "interrupt_batch");
            return InterruptBatchResult {
                commit_result: CommitResult::StaleOwner,
                terminal_message_ids: Vec::new(),
            };
        }
        for intent_id in &batch.intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = WorkIntentState::Cancelled {
                    classification: "lead_interrupted",
                };
            }
        }
        let slot = state.slots.get_mut(&batch.slot_id).expect("current batch slot exists");
        slot.active = None;
        for message_id in &batch.mailbox_message_ids {
            slot.delivery_failure_counts.remove(message_id);
        }
        state.interrupted_batches.insert(
            batch.batch_id.clone(),
            BatchInterruptMetadata {
                reason,
                replacement_message_id,
            },
        );
        let slot_snapshot = Self::slot_snapshot_locked(&state, &batch.slot_id);
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        InterruptBatchResult {
            commit_result: CommitResult::Committed,
            terminal_message_ids: batch.mailbox_message_ids.clone(),
        }
    }

    pub(crate) fn take_interrupt_metadata(&self, batch_id: &str) -> Option<BatchInterruptMetadata> {
        self.lock_state().interrupted_batches.remove(batch_id)
    }

    /// Pending interrupt metadata. Every entry must eventually be taken by the
    /// interrupting caller or by the event loop, so a non-zero residue after a
    /// batch is finished is a leak.
    #[cfg(test)]
    pub(crate) fn interrupted_batch_count(&self) -> usize {
        self.lock_state().interrupted_batches.len()
    }

    pub(crate) fn is_batch_cancelled(&self, batch: &WorkBatch) -> bool {
        let state = self.lock_state();
        batch.intent_ids.iter().all(|intent_id| {
            state
                .intents
                .get(intent_id)
                .is_some_and(|intent| matches!(intent.state, WorkIntentState::Cancelled { .. }))
        })
    }

    pub(crate) fn is_active_batch(&self, batch: &WorkBatch, turn_id: Option<&str>) -> bool {
        let state = self.lock_state();
        self.is_current_batch(&state, batch)
            && state
                .slots
                .get(&batch.slot_id)
                .and_then(|slot| slot.active.as_ref())
                .is_some_and(|active| active.turn_id.as_deref() == turn_id)
    }

    /// Terminalize the slot's queued work so only `retained_message_id` survives,
    /// backing `TeamQueuedPolicy::Discard`. Returns the mailbox rows the caller
    /// must mark read.
    ///
    /// Control-lane work is exempt. Discard means "supersede the queued
    /// instructions", but the Control lane carries lifecycle protocol
    /// (`team_shutdown_agent` handshakes) rather than instructions, and dropping
    /// a queued shutdown request would strand that handshake forever with no
    /// retry path.
    pub(crate) fn discard_queued_except(&self, slot_id: &str, retained_message_id: &str) -> Vec<String> {
        let mut state = self.lock_state();
        let queued_ids = state
            .slots
            .get(slot_id)
            .map(|slot| slot.queued_ids().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut terminal_message_ids = Vec::new();
        let mut discard_ids = Vec::new();
        for intent_id in queued_ids {
            let Some(intent) = state.intents.get_mut(&intent_id) else {
                continue;
            };
            if intent.mailbox_message_id.as_deref() == Some(retained_message_id) {
                continue;
            }
            if intent.source.priority() == WorkPriority::Control {
                continue;
            }
            if let Some(message_id) = &intent.mailbox_message_id {
                terminal_message_ids.push(message_id.clone());
            }
            intent.state = WorkIntentState::Cancelled {
                classification: "lead_interrupt_discarded",
            };
            discard_ids.push(intent_id);
        }
        if let Some(slot) = state.slots.get_mut(slot_id) {
            for intent_id in discard_ids {
                slot.remove_queued(&intent_id);
            }
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, slot_id);
        drop(state);
        self.publish_slot_work_snapshot(slot_snapshot);
        terminal_message_ids
    }

    pub(crate) fn cancel_run(&self, team_run_id: &str) -> CancelRunWorkResult {
        let mut state = self.lock_state();
        let mut cancel_targets = Vec::new();
        let mut terminal_message_ids = Vec::new();
        let active_slots = state
            .slots
            .iter()
            .filter_map(|(slot_id, slot)| {
                let active = slot.active.as_ref()?;
                active
                    .batch
                    .intent_ids
                    .iter()
                    .any(|intent_id| {
                        state
                            .intents
                            .get(intent_id)
                            .is_some_and(|intent| intent.team_run_id.as_deref() == Some(team_run_id))
                    })
                    .then(|| (slot_id.clone(), active.clone()))
            })
            .collect::<Vec<_>>();
        for (slot_id, active) in active_slots {
            cancel_targets.push(BatchCancelTarget {
                batch: active.batch.clone(),
                turn_id: active.turn_id,
            });
            let mut retained = Vec::new();
            for intent_id in &active.batch.intent_ids {
                let intent = state.intents.get_mut(intent_id).expect("active intent exists");
                if intent.team_run_id.as_deref() == Some(team_run_id) {
                    if let Some(message_id) = &intent.mailbox_message_id {
                        terminal_message_ids.push(message_id.clone());
                    }
                    intent.state = WorkIntentState::Cancelled {
                        classification: "run_cancelled",
                    };
                } else {
                    intent.state = WorkIntentState::Queued;
                    retained.push((intent.priority, intent_id.clone()));
                }
            }
            let slot = state.slots.get_mut(&slot_id).expect("active slot exists");
            slot.active = None;
            for (priority, intent_id) in retained.into_iter().rev() {
                slot.queue_mut(priority).push_front(intent_id);
            }
        }

        let queued_ids = state
            .intents
            .values()
            .filter(|intent| {
                intent.team_run_id.as_deref() == Some(team_run_id) && matches!(intent.state, WorkIntentState::Queued)
            })
            .map(|intent| intent.intent_id.clone())
            .collect::<Vec<_>>();
        for intent_id in queued_ids {
            let slot_id = {
                let intent = state.intents.get_mut(&intent_id).expect("queued intent exists");
                if let Some(message_id) = &intent.mailbox_message_id {
                    terminal_message_ids.push(message_id.clone());
                }
                intent.state = WorkIntentState::Cancelled {
                    classification: "run_cancelled",
                };
                intent.slot_id.clone()
            };
            if let Some(slot) = state.slots.get_mut(&slot_id) {
                slot.remove_queued(&intent_id);
            }
        }
        let lease_ids = state
            .enqueue_leases
            .values()
            .filter(|record| record.lease.team_run_id.as_deref() == Some(team_run_id))
            .map(|record| record.lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in lease_ids {
            state.enqueue_leases.remove(&lease_id);
        }
        let summary = Self::run_summary_locked(&state, team_run_id);
        drop(state);
        self.run_causality.apply_work_summary(summary.clone());
        CancelRunWorkResult {
            cancel_targets,
            terminal_message_ids,
            summary,
        }
    }

    pub(crate) fn complete_signals(&self, slot_id: &str, intent_ids: &[String]) -> CommitResult {
        let mut state = self.lock_state();
        let valid = intent_ids.iter().all(|intent_id| {
            state.intents.get(intent_id).is_some_and(|intent| {
                intent.slot_id == slot_id
                    && intent.mailbox_message_id.is_none()
                    && intent.state == WorkIntentState::Queued
            })
        });
        if !valid {
            return CommitResult::Rejected;
        }
        let run_ids = intent_ids
            .iter()
            .filter_map(|intent_id| state.intents.get(intent_id)?.team_run_id.clone())
            .collect::<BTreeSet<_>>();
        for intent_id in intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = WorkIntentState::Completed;
            }
            if let Some(slot) = state.slots.get_mut(slot_id) {
                slot.remove_queued(intent_id);
            }
        }
        let summaries = Self::run_summaries_locked(&state, run_ids);
        drop(state);
        self.publish_run_summaries(summaries);
        CommitResult::Committed
    }

    pub(crate) fn pause_slot(&self, slot_id: &str) -> PauseWorkResult {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(TeamRunTargetRole::Teammate));
        slot.paused = true;
        let cancel_target = slot.active.as_ref().map(|active| BatchCancelTarget {
            batch: active.batch.clone(),
            turn_id: active.turn_id.clone(),
        });
        let snapshot = Self::slot_snapshot_locked(&state, slot_id).expect("paused slot exists");
        drop(state);
        self.publish_slot_work_snapshot(Some(snapshot.clone()));
        PauseWorkResult {
            cancel_target,
            slot: snapshot,
        }
    }

    pub(crate) fn set_runtime_constraint(
        &self,
        slot_id: &str,
        constraint: RuntimeConstraint,
    ) -> RuntimeConstraintUpdate {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(TeamRunTargetRole::Teammate));
        slot.runtime_constraint = constraint.clone();

        let mut terminal_message_ids = Vec::new();
        let mut affected_run_ids = BTreeSet::new();
        if matches!(
            constraint,
            RuntimeConstraint::Failed { .. } | RuntimeConstraint::SessionStopped
        ) {
            let intent_ids = state
                .intents
                .values()
                .filter(|intent| intent.slot_id == slot_id && !intent.state.is_terminal())
                .map(|intent| intent.intent_id.clone())
                .collect::<Vec<_>>();
            let classification = match constraint {
                RuntimeConstraint::Failed { classification, .. } => classification,
                RuntimeConstraint::SessionStopped => "session_stopped",
                _ => unreachable!(),
            };
            let mut requeue_background = Vec::new();
            for intent_id in intent_ids {
                let intent = state.intents.get_mut(&intent_id).expect("selected intent exists");
                if intent.team_run_id.is_none() && !matches!(constraint, RuntimeConstraint::SessionStopped) {
                    intent.state = WorkIntentState::Queued;
                    requeue_background.push((intent.priority, intent_id));
                    continue;
                }
                if let Some(team_run_id) = &intent.team_run_id {
                    affected_run_ids.insert(team_run_id.clone());
                }
                if let Some(message_id) = &intent.mailbox_message_id {
                    terminal_message_ids.push(message_id.clone());
                }
                intent.state = if matches!(constraint, RuntimeConstraint::SessionStopped) {
                    WorkIntentState::Cancelled { classification }
                } else {
                    WorkIntentState::Failed { classification }
                };
            }
            let slot = state.slots.get_mut(slot_id).expect("constrained slot exists");
            slot.foreground.clear();
            slot.directed.clear();
            slot.control.clear();
            slot.background.clear();
            slot.active = None;
            for (priority, intent_id) in requeue_background {
                slot.queue_mut(priority).push_back(intent_id);
            }
        }
        let affected_run_summaries = affected_run_ids
            .iter()
            .map(|run_id| Self::run_summary_locked(&state, run_id))
            .collect::<Vec<_>>();
        let slot = Self::slot_snapshot_locked(&state, slot_id).expect("constrained slot exists");
        drop(state);
        self.publish_run_summaries(affected_run_summaries.clone());
        self.publish_slot_work_snapshot(Some(slot.clone()));
        RuntimeConstraintUpdate {
            slot,
            terminal_message_ids,
            affected_run_summaries,
        }
    }

    /// Atomically blocks new work for a runtime restart.
    ///
    /// The active-work check and `Starting` transition share the coordinator
    /// state lock with [`Self::next`]. Therefore either an existing batch wins
    /// and the restart is rejected, or the restart gate wins and no new batch
    /// can be claimed until the runtime becomes ready again.
    pub(crate) fn begin_runtime_restart(&self, slot_id: &str) -> Result<RuntimeRestartGate, RuntimeRestartRejection> {
        self.begin_runtime_restart_inner(slot_id, false)
    }

    pub(crate) fn begin_mcp_runtime_restart(
        &self,
        slot_id: &str,
    ) -> Result<RuntimeRestartGate, RuntimeRestartRejection> {
        self.begin_runtime_restart_inner(slot_id, true)
    }

    fn begin_runtime_restart_inner(
        &self,
        slot_id: &str,
        allow_queued: bool,
    ) -> Result<RuntimeRestartGate, RuntimeRestartRejection> {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(TeamRunTargetRole::Teammate));
        if slot.active.is_some() || (!allow_queued && slot.queued_ids().next().is_some()) {
            return Err(RuntimeRestartRejection::Busy);
        }
        match slot.runtime_constraint {
            RuntimeConstraint::Removing { .. } => return Err(RuntimeRestartRejection::Removing),
            RuntimeConstraint::SessionStopped => return Err(RuntimeRestartRejection::SessionStopped),
            RuntimeConstraint::Ready | RuntimeConstraint::Starting { .. } | RuntimeConstraint::Failed { .. } => {}
        }
        let previous_constraint = slot.runtime_constraint.clone();
        state.next_operation_id = state.next_operation_id.saturating_add(1);
        let operation_id = state.next_operation_id;
        state
            .slots
            .get_mut(slot_id)
            .expect("restart-gated slot exists")
            .runtime_constraint = RuntimeConstraint::Starting { operation_id };
        let snapshot = Self::slot_snapshot_locked(&state, slot_id);
        drop(state);
        self.publish_slot_work_snapshot(snapshot);
        Ok(RuntimeRestartGate {
            operation_id,
            previous_constraint,
        })
    }

    pub(crate) fn request_mcp_refresh(&self, slot_id: &str, fingerprint: &str) -> McpRefreshDisposition {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(TeamRunTargetRole::Teammate));
        if slot.applied_mcp_fingerprint.as_deref() == Some(fingerprint)
            || slot.pending_mcp_fingerprint.as_deref() == Some(fingerprint)
        {
            return McpRefreshDisposition::Unchanged;
        }
        if slot.active.is_some()
            || slot.queued_ids().next().is_some()
            || !matches!(slot.runtime_constraint, RuntimeConstraint::Ready)
        {
            slot.pending_mcp_fingerprint = Some(fingerprint.to_owned());
            slot.mcp_refresh_in_progress = false;
            return McpRefreshDisposition::Deferred;
        }
        McpRefreshDisposition::RestartNow
    }

    pub(crate) fn defer_mcp_refresh(&self, slot_id: &str, fingerprint: &str) {
        let mut state = self.lock_state();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(TeamRunTargetRole::Teammate));
        if slot.applied_mcp_fingerprint.as_deref() != Some(fingerprint) {
            slot.pending_mcp_fingerprint = Some(fingerprint.to_owned());
            slot.mcp_refresh_in_progress = false;
        }
    }

    pub(crate) fn claim_pending_mcp_refresh(&self, slot_id: &str) -> Option<String> {
        let mut state = self.lock_state();
        let slot = state.slots.get_mut(slot_id)?;
        if slot.active.is_some() || slot.mcp_refresh_in_progress {
            return None;
        }
        let fingerprint = slot.pending_mcp_fingerprint.clone()?;
        slot.mcp_refresh_in_progress = true;
        Some(fingerprint)
    }

    pub(crate) fn complete_mcp_refresh(&self, slot_id: &str, fingerprint: &str) {
        let mut state = self.lock_state();
        let Some(slot) = state.slots.get_mut(slot_id) else {
            return;
        };
        slot.applied_mcp_fingerprint = Some(fingerprint.to_owned());
        if slot.pending_mcp_fingerprint.as_deref() == Some(fingerprint) {
            slot.pending_mcp_fingerprint = None;
        }
        slot.mcp_refresh_in_progress = false;
    }

    pub(crate) fn release_mcp_refresh(&self, slot_id: &str, fingerprint: &str) {
        let mut state = self.lock_state();
        let Some(slot) = state.slots.get_mut(slot_id) else {
            return;
        };
        if slot.pending_mcp_fingerprint.as_deref() == Some(fingerprint) {
            slot.mcp_refresh_in_progress = false;
        }
    }

    pub(crate) fn settle_mcp_refresh_claim(&self, slot_id: &str, fingerprint: &str) {
        let mut state = self.lock_state();
        let Some(slot) = state.slots.get_mut(slot_id) else {
            return;
        };
        if slot.pending_mcp_fingerprint.as_deref() == Some(fingerprint) {
            slot.pending_mcp_fingerprint = None;
            slot.mcp_refresh_in_progress = false;
        }
    }

    /// Roll back an unused restart gate without overwriting a newer runtime
    /// transition that may have taken ownership of the slot.
    pub(crate) fn abort_runtime_restart(&self, slot_id: &str, gate: &RuntimeRestartGate) {
        let mut state = self.lock_state();
        let Some(slot) = state.slots.get_mut(slot_id) else {
            return;
        };
        if slot.runtime_constraint
            != (RuntimeConstraint::Starting {
                operation_id: gate.operation_id,
            })
        {
            return;
        }
        slot.runtime_constraint = gate.previous_constraint.clone();
        let snapshot = Self::slot_snapshot_locked(&state, slot_id);
        drop(state);
        self.publish_slot_work_snapshot(snapshot);
    }

    pub(crate) fn remove_slot(&self, slot_id: &str) -> RemoveWorkResult {
        let mut state = self.lock_state();
        let cancel_target = state
            .slots
            .get(slot_id)
            .and_then(|slot| slot.active.as_ref())
            .map(|active| BatchCancelTarget {
                batch: active.batch.clone(),
                turn_id: active.turn_id.clone(),
            });
        let mut intent_ids = state
            .slots
            .get(slot_id)
            .into_iter()
            .flat_map(|slot| {
                slot.active
                    .iter()
                    .flat_map(|active| active.batch.intent_ids.iter())
                    .chain(slot.queued_ids())
            })
            .cloned()
            .collect::<Vec<_>>();
        let already_ordered = intent_ids.iter().cloned().collect::<HashSet<_>>();
        let mut remaining = state
            .intents
            .values()
            .filter(|intent| {
                intent.slot_id == slot_id && !intent.state.is_terminal() && !already_ordered.contains(&intent.intent_id)
            })
            .collect::<Vec<_>>();
        remaining.sort_by_key(|intent| (intent.created_at_ms, intent.intent_id.as_str()));
        intent_ids.extend(remaining.into_iter().map(|intent| intent.intent_id.clone()));
        let mut terminal_message_ids = Vec::new();
        let mut affected_run_ids = BTreeSet::new();
        for intent_id in intent_ids {
            let intent = state.intents.get_mut(&intent_id).expect("selected intent exists");
            if let Some(message_id) = &intent.mailbox_message_id {
                terminal_message_ids.push(message_id.clone());
            }
            if let Some(team_run_id) = &intent.team_run_id {
                affected_run_ids.insert(team_run_id.clone());
            }
            intent.state = WorkIntentState::Cancelled {
                classification: "slot_removed",
            };
        }
        if let Some(slot) = state.slots.get_mut(slot_id) {
            slot.foreground.clear();
            slot.directed.clear();
            slot.control.clear();
            slot.background.clear();
            slot.active = None;
            slot.removed = true;
        }
        let affected_run_summaries = affected_run_ids
            .iter()
            .map(|run_id| Self::run_summary_locked(&state, run_id))
            .collect::<Vec<_>>();
        drop(state);
        self.publish_run_summaries(affected_run_summaries.clone());
        RemoveWorkResult {
            cancel_target,
            terminal_message_ids,
            affected_run_summaries,
        }
    }

    pub(crate) fn stop(&self) -> Vec<RunWorkSummary> {
        let mut state = self.lock_state();
        let run_ids = Self::all_run_ids(&state);
        for intent in state.intents.values_mut().filter(|intent| !intent.state.is_terminal()) {
            intent.state = WorkIntentState::Cancelled {
                classification: "session_stopped",
            };
        }
        for slot in state.slots.values_mut() {
            slot.foreground.clear();
            slot.directed.clear();
            slot.control.clear();
            slot.background.clear();
            slot.active = None;
            slot.runtime_constraint = RuntimeConstraint::SessionStopped;
        }
        state.enqueue_leases.clear();
        let summaries = run_ids
            .iter()
            .map(|run_id| Self::run_summary_locked(&state, run_id))
            .collect::<Vec<_>>();
        drop(state);
        self.publish_run_summaries(summaries.clone());
        summaries
    }

    pub(crate) fn slot_snapshot(&self, slot_id: &str) -> Option<SlotWorkSnapshot> {
        Self::slot_snapshot_locked(&self.lock_state(), slot_id)
    }

    pub(crate) fn snapshot(&self) -> CoordinatorSnapshot {
        let state = self.lock_state();
        let slots = state
            .slots
            .keys()
            .filter_map(|slot_id| Self::slot_snapshot_locked(&state, slot_id))
            .collect();
        let active_run_summary = Self::all_run_ids(&state)
            .into_iter()
            .map(|run_id| Self::run_summary_locked(&state, &run_id))
            .find(|summary| {
                summary.queued_intent_count > 0
                    || summary.starting_batch_count > 0
                    || summary.running_batch_count > 0
                    || summary.active_enqueue_lease_count > 0
                    || summary.paused_intent_count > 0
            });
        CoordinatorSnapshot {
            session_generation: self.session_generation.clone(),
            slots,
            active_run_summary,
        }
    }

    #[cfg(test)]
    pub(super) fn intents_for_slot(&self, slot_id: &str) -> Vec<WorkIntent> {
        let state = self.lock_state();
        let mut intents = state
            .intents
            .values()
            .filter(|intent| intent.slot_id == slot_id)
            .cloned()
            .collect::<Vec<_>>();
        intents.sort_by_key(|intent| intent.created_at_ms);
        intents
    }

    /// Retire the current batch: mark its intents terminal, release the slot, and
    /// publish the resulting snapshots.
    ///
    /// `delivery` decides what happens to the per-message delivery retry counters,
    /// which is the ONLY thing that differs between a batch that ended cleanly and
    /// one that failed.
    fn terminalize_batch(
        &self,
        batch: &WorkBatch,
        terminal_state: WorkIntentState,
        classification: &'static str,
        delivery: DeliveryOutcome,
    ) -> BatchFailureResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            self.log_stale_batch(batch, "terminalize_batch");
            return BatchFailureResult {
                commit_result: CommitResult::StaleOwner,
                exhausted_message_ids: Vec::new(),
            };
        }
        for intent_id in &batch.intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = terminal_state.clone();
            }
        }
        let slot = state.slots.get_mut(&batch.slot_id).expect("current batch slot exists");
        slot.active = None;
        let mut exhausted_message_ids = Vec::new();
        match delivery {
            DeliveryOutcome::NotFailed => {
                for message_id in &batch.mailbox_message_ids {
                    slot.delivery_failure_counts.remove(message_id);
                }
            }
            DeliveryOutcome::Failed => {
                // Only messages this batch actually tried to deliver count toward
                // the retry limit. Rows merely observed through
                // `team_read_messages` were never a delivery attempt, so they keep
                // their counter and stay unread — they accrue failures later if a
                // batch genuinely claims and fails to deliver them.
                for message_id in &batch.mailbox_message_ids {
                    let failure_count = slot.delivery_failure_counts.entry(message_id.clone()).or_default();
                    *failure_count = failure_count.saturating_add(1);
                    if *failure_count >= MAX_MESSAGE_DELIVERY_FAILURES {
                        exhausted_message_ids.push(message_id.clone());
                    }
                }
                if !exhausted_message_ids.is_empty() {
                    slot.paused = true;
                }
            }
        }
        let slot_snapshot = Self::slot_snapshot_locked(&state, &batch.slot_id);
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        self.publish_slot_work_snapshot(slot_snapshot);
        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %batch.slot_id,
            batch_id = %batch.batch_id,
            operation_id = batch.operation_id,
            classification,
            exhausted_message_count = exhausted_message_ids.len(),
            "team work batch terminal"
        );
        BatchFailureResult {
            commit_result: CommitResult::Committed,
            exhausted_message_ids,
        }
    }

    fn validate_enqueue_constraint(slot_id: &str, slot: &SlotState) -> Result<(), TeamError> {
        if slot.removed {
            return Err(TeamError::InvalidRequest(format!("Team slot was removed: {slot_id}")));
        }
        match slot.runtime_constraint {
            RuntimeConstraint::Ready | RuntimeConstraint::Starting { .. } => Ok(()),
            RuntimeConstraint::Failed { classification, .. } => Err(TeamError::InvalidRequest(format!(
                "Team slot runtime failed ({classification}): {slot_id}"
            ))),
            RuntimeConstraint::Removing { .. } => Err(TeamError::InvalidRequest(format!(
                "Team slot is being removed: {slot_id}"
            ))),
            RuntimeConstraint::SessionStopped => Err(TeamError::InvalidRequest(format!(
                "Team session stopped for slot: {slot_id}"
            ))),
        }
    }

    fn is_current_batch(&self, state: &CoordinatorState, batch: &WorkBatch) -> bool {
        batch.session_generation == self.session_generation
            && state
                .slots
                .get(&batch.slot_id)
                .and_then(|slot| slot.active.as_ref())
                .is_some_and(|active| {
                    active.batch.batch_id == batch.batch_id
                        && active.batch.operation_id == batch.operation_id
                        && active.batch.session_generation == batch.session_generation
                })
    }

    fn log_stale_batch(&self, batch: &WorkBatch, operation: &'static str) {
        warn!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            submitted_generation = %batch.session_generation,
            slot_id = %batch.slot_id,
            batch_id = %batch.batch_id,
            operation_id = batch.operation_id,
            operation,
            "stale team work ownership rejected"
        );
    }
}
