use std::sync::{Arc, Mutex, MutexGuard};

use aionui_api_types::{
    TeamRunPayload, TeamRunSource, TeamRunStatus, TeamRunTargetRole, TeamSlotBlockedReason, TeamSlotWorkPayload,
    TeamSlotWorkState,
};
use aionui_common::{TimestampMs, generate_id, now_ms};
use tracing::info;

use crate::TeamError;
use crate::events::{
    TEAM_RUN_ACCEPTED_EVENT, TEAM_RUN_CANCELLED_EVENT, TEAM_RUN_COMPLETED_EVENT, TEAM_RUN_FAILED_EVENT,
    TEAM_RUN_STARTED_EVENT, TEAM_RUN_UPDATED_EVENT, TeamEventEmitter,
};
use crate::work_coordinator::{
    CausalBinding, CoordinatorSnapshot, EnqueueRequest, RunBinding, RunCausalityPort, RunWorkSummary,
    RuntimeConstraint, SlotPhase, SlotWorkSnapshot,
};

const ACTIVE_TURN_SLOW_THRESHOLD_MS: u64 = 10 * 60 * 1000;

#[derive(Clone)]
struct TeamRunRecord {
    team_run_id: String,
    source: TeamRunSource,
    has_user_intervention: bool,
    target_slot_id: String,
    target_role: TeamRunTargetRole,
    status: TeamRunStatus,
    started_at_ms: Option<TimestampMs>,
    completed_at_ms: Option<TimestampMs>,
    cancel_reason: Option<String>,
    summary: Option<RunWorkSummary>,
    accepted_emitted: bool,
}

impl TeamRunRecord {
    fn is_active(&self) -> bool {
        matches!(
            self.status,
            TeamRunStatus::Accepted | TeamRunStatus::Running | TeamRunStatus::Cancelling
        )
    }
}

pub struct TeamRunManager {
    team_id: String,
    emitter: Arc<TeamEventEmitter>,
    state: Mutex<Option<TeamRunRecord>>,
}

impl TeamRunManager {
    pub(crate) fn new(team_id: String, emitter: Arc<TeamEventEmitter>) -> Self {
        Self {
            team_id,
            emitter,
            state: Mutex::new(None),
        }
    }

    pub(crate) fn bind_user_enqueue(&self, target_slot_id: &str, target_role: TeamRunTargetRole) -> RunBinding {
        let mut state = self.lock_state();
        if let Some(run) = state.as_mut().filter(|run| run.is_active()) {
            run.has_user_intervention = true;
            return RunBinding {
                team_run_id: Some(run.team_run_id.clone()),
                created_new_run: false,
                user_intervention: true,
            };
        }

        let run = TeamRunRecord {
            team_run_id: generate_id(),
            source: TeamRunSource::UserMessage,
            has_user_intervention: false,
            target_slot_id: target_slot_id.to_owned(),
            target_role,
            status: TeamRunStatus::Accepted,
            started_at_ms: None,
            completed_at_ms: None,
            cancel_reason: None,
            summary: None,
            accepted_emitted: false,
        };
        let run_id = run.team_run_id.clone();
        *state = Some(run);
        drop(state);
        info!(
            team_id = %self.team_id,
            team_run_id = %run_id,
            target_slot_id,
            "team run accepted"
        );
        RunBinding {
            team_run_id: Some(run_id),
            created_new_run: true,
            user_intervention: false,
        }
    }

    pub fn current_active_run_id(&self) -> Option<String> {
        self.lock_state()
            .as_ref()
            .filter(|run| run.is_active())
            .map(|run| run.team_run_id.clone())
    }

    pub(crate) fn apply_work_summary(&self, summary: RunWorkSummary) {
        let event = {
            let mut state = self.lock_state();
            let Some(run) = state.as_mut().filter(|run| run.team_run_id == summary.team_run_id) else {
                return;
            };
            let previous_status = run.status.clone();
            run.summary = Some(summary.clone());
            let no_work = summary.queued_intent_count == 0
                && summary.starting_batch_count == 0
                && summary.running_batch_count == 0
                && summary.active_enqueue_lease_count == 0
                && summary.paused_intent_count == 0;
            if summary.failed_intent_count > 0 {
                run.status = TeamRunStatus::Failed;
                run.completed_at_ms = Some(now_ms());
            } else if no_work {
                run.status = if previous_status == TeamRunStatus::Cancelling {
                    TeamRunStatus::Cancelled
                } else {
                    TeamRunStatus::Completed
                };
                run.completed_at_ms = Some(now_ms());
            } else if summary.running_batch_count > 0 {
                run.status = TeamRunStatus::Running;
                run.started_at_ms.get_or_insert_with(now_ms);
            }
            let event_name = if !run.accepted_emitted {
                run.accepted_emitted = true;
                TEAM_RUN_ACCEPTED_EVENT
            } else {
                match (&previous_status, &run.status) {
                    (_, TeamRunStatus::Failed) if previous_status != TeamRunStatus::Failed => TEAM_RUN_FAILED_EVENT,
                    (_, TeamRunStatus::Cancelled) if previous_status != TeamRunStatus::Cancelled => {
                        TEAM_RUN_CANCELLED_EVENT
                    }
                    (_, TeamRunStatus::Completed) if previous_status != TeamRunStatus::Completed => {
                        TEAM_RUN_COMPLETED_EVENT
                    }
                    (_, TeamRunStatus::Running) if previous_status != TeamRunStatus::Running => TEAM_RUN_STARTED_EVENT,
                    _ => TEAM_RUN_UPDATED_EVENT,
                }
            };
            Some((event_name, self.payload_locked(run, &summary)))
        };
        if let Some((event_name, payload)) = event {
            self.emitter.broadcast_team_run(event_name, payload);
        }
    }

    pub(crate) fn begin_cancel(&self, team_run_id: &str, reason: Option<String>) -> Result<(), TeamError> {
        let payload = {
            let mut state = self.lock_state();
            let run = state
                .as_mut()
                .filter(|run| run.team_run_id == team_run_id && run.is_active())
                .ok_or_else(|| TeamError::InvalidRequest(format!("team run is not active: {team_run_id}")))?;
            run.status = TeamRunStatus::Cancelling;
            run.cancel_reason = reason;
            run.summary.as_ref().map(|summary| self.payload_locked(run, summary))
        };
        if let Some(payload) = payload {
            self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        }
        Ok(())
    }

    pub(crate) fn current_payload(&self, coordinator: &CoordinatorSnapshot) -> Option<TeamRunPayload> {
        let state = self.lock_state();
        let run = state.as_ref()?;
        let summary = run.summary.as_ref().or_else(|| {
            coordinator
                .active_run_summary
                .as_ref()
                .filter(|summary| summary.team_run_id == run.team_run_id)
        });
        let empty_summary;
        let summary = match summary {
            Some(summary) => summary,
            None => {
                empty_summary = RunWorkSummary {
                    team_run_id: run.team_run_id.clone(),
                    queued_intent_count: 0,
                    starting_batch_count: 0,
                    running_batch_count: 0,
                    active_enqueue_lease_count: 0,
                    paused_intent_count: 0,
                    failed_intent_count: 0,
                    slots: Vec::new(),
                };
                &empty_summary
            }
        };
        Some(self.payload_locked(run, summary))
    }

    pub(crate) fn publish_snapshot_update(&self, coordinator: &CoordinatorSnapshot) {
        if let Some(payload) = self.current_payload(coordinator)
            && matches!(payload.status, TeamRunStatus::Accepted | TeamRunStatus::Running)
        {
            self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        }
    }

    fn payload_locked(&self, run: &TeamRunRecord, summary: &RunWorkSummary) -> TeamRunPayload {
        TeamRunPayload {
            team_id: self.team_id.clone(),
            team_run_id: run.team_run_id.clone(),
            source: run.source.clone(),
            has_user_intervention: run.has_user_intervention,
            target_slot_id: run.target_slot_id.clone(),
            target_role: run.target_role.clone(),
            status: run.status.clone(),
            queued_intent_count: summary.queued_intent_count,
            starting_batch_count: summary.starting_batch_count,
            running_batch_count: summary.running_batch_count,
            active_enqueue_lease_count: summary.active_enqueue_lease_count,
            slot_work: summary.slots.iter().map(Self::slot_payload).collect(),
        }
    }

    pub(crate) fn slot_payload(slot: &SlotWorkSnapshot) -> TeamSlotWorkPayload {
        let active_turn_elapsed_ms = slot
            .active_turn_started_at_ms
            .map(|started_at| now_ms().saturating_sub(started_at).max(0) as u64);
        TeamSlotWorkPayload {
            slot_id: slot.slot_id.clone(),
            role: slot.role.clone(),
            state: match slot.state {
                SlotPhase::Idle => TeamSlotWorkState::Idle,
                SlotPhase::Queued => TeamSlotWorkState::Queued,
                SlotPhase::Starting => TeamSlotWorkState::Starting,
                SlotPhase::Running => TeamSlotWorkState::Running,
                SlotPhase::Paused => TeamSlotWorkState::Paused,
                SlotPhase::Blocked => TeamSlotWorkState::Blocked,
            },
            queued_foreground_count: slot.queued_foreground_count,
            queued_background_count: slot.queued_background_count,
            active_turn_id: slot.active_turn_id.clone(),
            active_turn_started_at_ms: slot.active_turn_started_at_ms,
            active_turn_elapsed_ms,
            active_turn_slow: active_turn_elapsed_ms.map(|elapsed| elapsed >= ACTIVE_TURN_SLOW_THRESHOLD_MS),
            active_turn_slow_threshold_ms: slot.active_turn_id.as_ref().map(|_| ACTIVE_TURN_SLOW_THRESHOLD_MS),
            blocked_reason: match slot.runtime_constraint {
                RuntimeConstraint::Ready => None,
                RuntimeConstraint::Starting { .. } => Some(TeamSlotBlockedReason::RuntimeStarting),
                RuntimeConstraint::Failed { .. } => Some(TeamSlotBlockedReason::RuntimeFailed),
                RuntimeConstraint::Removing { .. } => Some(TeamSlotBlockedReason::Removing),
                RuntimeConstraint::SessionStopped => Some(TeamSlotBlockedReason::SessionStopped),
            },
            team_run_id: slot.team_run_id.clone(),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, Option<TeamRunRecord>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RunCausalityPort for TeamRunManager {
    fn bind_enqueue(&self, request: &EnqueueRequest) -> RunBinding {
        match &request.binding {
            CausalBinding::UserVisible => self.bind_user_enqueue(&request.slot_id, request.role.clone()),
            CausalBinding::ActiveRunOrBackground => RunBinding {
                team_run_id: self.current_active_run_id(),
                created_new_run: false,
                user_intervention: false,
            },
            CausalBinding::InheritRunningBatch { .. } | CausalBinding::Background => RunBinding {
                team_run_id: None,
                created_new_run: false,
                user_intervention: false,
            },
        }
    }

    fn abort_binding(&self, binding: &RunBinding) {
        let mut state = self.lock_state();
        if state
            .as_ref()
            .is_some_and(|run| Some(&run.team_run_id) == binding.team_run_id.as_ref())
        {
            *state = None;
        }
    }

    fn apply_work_summary(&self, summary: RunWorkSummary) {
        TeamRunManager::apply_work_summary(self, summary);
    }
}
