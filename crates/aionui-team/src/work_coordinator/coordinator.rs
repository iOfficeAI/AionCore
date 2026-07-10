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
    pub(super) foreground: VecDeque<String>,
    pub(super) control: VecDeque<String>,
    pub(super) background: VecDeque<String>,
    pub(super) active: Option<ActiveBatch>,
    pub(super) paused: bool,
    pub(super) runtime_constraint: RuntimeConstraint,
    known_unread_message_ids: HashSet<String>,
    removed: bool,
}

impl SlotState {
    fn new(role: TeamRunTargetRole) -> Self {
        Self {
            role,
            foreground: VecDeque::new(),
            control: VecDeque::new(),
            background: VecDeque::new(),
            active: None,
            paused: false,
            runtime_constraint: RuntimeConstraint::Starting { operation_id: 0 },
            known_unread_message_ids: HashSet::new(),
            removed: false,
        }
    }

    fn queue(&self, priority: WorkPriority) -> &VecDeque<String> {
        match priority {
            WorkPriority::Foreground => &self.foreground,
            WorkPriority::Control => &self.control,
            WorkPriority::Background => &self.background,
        }
    }

    fn queue_mut(&mut self, priority: WorkPriority) -> &mut VecDeque<String> {
        match priority {
            WorkPriority::Foreground => &mut self.foreground,
            WorkPriority::Control => &mut self.control,
            WorkPriority::Background => &mut self.background,
        }
    }

    pub(super) fn queued_ids(&self) -> impl Iterator<Item = &String> {
        self.foreground
            .iter()
            .chain(self.control.iter())
            .chain(self.background.iter())
    }

    fn remove_queued(&mut self, intent_id: &str) {
        for queue in [&mut self.foreground, &mut self.control, &mut self.background] {
            queue.retain(|candidate| candidate != intent_id);
        }
    }
}

#[derive(Default)]
pub(super) struct CoordinatorState {
    pub(super) slots: BTreeMap<String, SlotState>,
    pub(super) intents: HashMap<String, WorkIntent>,
    pub(super) enqueue_leases: HashMap<String, EnqueueLeaseRecord>,
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
            CausalBinding::InheritRunningBatch { caller_slot_id } => RunBinding {
                team_run_id: state
                    .slots
                    .get(caller_slot_id)
                    .and_then(|caller| caller.active.as_ref())
                    .and_then(|active| active.batch.team_run_ids.first().cloned()),
                created_new_run: false,
                user_intervention: false,
            },
            CausalBinding::UserVisible | CausalBinding::ActiveRunOrBackground => {
                self.run_causality.bind_enqueue(&request)
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
        slot.queue_mut(intent.priority).push_back(intent_id.clone());
        let slot_snapshot = Self::slot_snapshot_locked(&state, &lease.slot_id).expect("committed slot exists");
        let summaries = Self::run_summaries_locked(&state, lease.team_run_id.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);

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
        let summaries = Self::run_summaries_locked(&state, lease.team_run_id.iter().cloned());
        drop(state);
        if remove_empty_run {
            self.run_causality.abort_binding(&record.binding);
        }
        self.publish_run_summaries(summaries);
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

    pub(crate) fn reconcile_mailbox(
        &self,
        slot_id: &str,
        unread_message_ids: &[String],
        role: TeamRunTargetRole,
    ) -> ReconcileProjection {
        let mut state = self.lock_state();
        let unread = unread_message_ids.iter().cloned().collect::<HashSet<_>>();
        let slot = state
            .slots
            .entry(slot_id.to_owned())
            .or_insert_with(|| SlotState::new(role.clone()));
        slot.role = role.clone();
        slot.known_unread_message_ids = unread.clone();

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
            if unread.contains(message_id) {
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

        let selected_priority = [
            WorkPriority::Foreground,
            WorkPriority::Control,
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
        let message_intent_ids = queued_ids
            .iter()
            .filter(|intent_id| {
                state
                    .intents
                    .get(*intent_id)
                    .is_some_and(|intent| intent.mailbox_message_id.is_some())
            })
            .cloned()
            .collect::<Vec<_>>();
        if message_intent_ids.is_empty() {
            return ReconcileDecision::SettleSignals(queued_ids);
        }

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
            highest_priority: priority,
            team_run_ids: team_run_ids.clone(),
            operation_id,
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
        let summaries = Self::run_summaries_locked(&state, team_run_ids);
        drop(state);
        self.publish_run_summaries(summaries);
        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id,
            batch_id = %batch.batch_id,
            operation_id,
            intent_count = batch.intent_ids.len(),
            message_count = batch.mailbox_message_ids.len(),
            priority = ?batch.highest_priority,
            "team work batch claimed"
        );
        ReconcileDecision::Claim(batch)
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
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
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
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
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

    pub(crate) fn complete_batch(&self, batch: &WorkBatch) -> CommitResult {
        self.terminalize_batch(batch, WorkIntentState::Completed, "completed")
    }

    pub(crate) fn fail_batch(&self, batch: &WorkBatch, classification: &'static str) -> CommitResult {
        self.terminalize_batch(batch, WorkIntentState::Failed { classification }, classification)
    }

    pub(crate) fn cancel_batch(&self, batch: &WorkBatch, classification: &'static str) -> CommitResult {
        self.terminalize_batch(batch, WorkIntentState::Cancelled { classification }, classification)
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
        RuntimeConstraintUpdate {
            slot,
            terminal_message_ids,
            affected_run_summaries,
        }
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

    fn terminalize_batch(
        &self,
        batch: &WorkBatch,
        terminal_state: WorkIntentState,
        classification: &'static str,
    ) -> CommitResult {
        let mut state = self.lock_state();
        if !self.is_current_batch(&state, batch) {
            self.log_stale_batch(batch, "terminalize_batch");
            return CommitResult::StaleOwner;
        }
        for intent_id in &batch.intent_ids {
            if let Some(intent) = state.intents.get_mut(intent_id) {
                intent.state = terminal_state.clone();
            }
        }
        state
            .slots
            .get_mut(&batch.slot_id)
            .expect("current batch slot exists")
            .active = None;
        let summaries = Self::run_summaries_locked(&state, batch.team_run_ids.iter().cloned());
        drop(state);
        self.publish_run_summaries(summaries);
        info!(
            team_id = %self.team_id,
            session_generation = %self.session_generation,
            slot_id = %batch.slot_id,
            batch_id = %batch.batch_id,
            operation_id = batch.operation_id,
            classification,
            "team work batch terminal"
        );
        CommitResult::Committed
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
