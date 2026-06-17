use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use aionui_api_types::{
    TeamChildTurnPayload, TeamRunAckResponse, TeamRunPayload, TeamRunSource, TeamRunStatus, TeamRunTargetRole,
    TeamSlotRuntimeHealth, TeamSlotWorkPayload,
};
use aionui_common::{TimestampMs, generate_id, now_ms};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::error::TeamError;
use crate::events::{
    TEAM_CHILD_TURN_CANCELLED_EVENT, TEAM_CHILD_TURN_COMPLETED_EVENT, TEAM_CHILD_TURN_STARTED_EVENT,
    TEAM_RUN_ACCEPTED_EVENT, TEAM_RUN_CANCELLED_EVENT, TEAM_RUN_COMPLETED_EVENT, TEAM_RUN_FAILED_EVENT,
    TEAM_RUN_STARTED_EVENT, TEAM_RUN_UPDATED_EVENT, TeamEventEmitter,
};
use crate::slot_wake_gate::SlotWakeGate;
#[cfg(test)]
use crate::slot_wake_gate::WakeGateDecision;
use crate::types::TeammateRole;
use crate::wake::TeamWakeSource;

const ACTIVE_CHILD_SLOW_THRESHOLD_MS: u64 = 10 * 60 * 1000;
const ACTIVE_CHILD_SLOW_REPEAT_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveChildTurn {
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
    pub started_at_ms: TimestampMs,
    pub last_slow_notified_at_ms: Option<TimestampMs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartingReservationState {
    Starting,
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartingChildReservation {
    pub reservation_id: String,
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub(crate) wake_source: TeamWakeSource,
    pub(crate) message_id: Option<String>,
    pub state: StartingReservationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingWake {
    slot_id: String,
    role: TeamRunTargetRole,
    source: TeamWakeSource,
    message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildStartDecision {
    Accepted,
    CancelImmediately(ActiveChildTurn),
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildCancelTarget {
    Active(ActiveChildTurn),
    Starting(StartingChildReservation),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeRecordDecision {
    Recorded,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeamRunOperationLease {
    pub lease_id: String,
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub wake_source: TeamWakeSource,
    pub accepted_as_new_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveOperationLease {
    lease: TeamRunOperationLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeamRunWakeAcquireOutcome {
    Accepted(TeamRunOperationLease),
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamRunSlotState {
    Busy,
    Pending,
    Paused,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquirePolicyDecision {
    Accept,
    Suppress(&'static str),
    RejectSlotBusy,
    RejectInvalid(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamRunWakeIntent {
    ExternalRequest,
    SchedulerWakeTarget,
}

#[derive(Debug, Clone)]
pub struct PauseSlotOutcome {
    pub team_run_id: String,
    pub cancel_target: Option<ChildCancelTarget>,
    pub payload: TeamRunPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingWakeView {
    pub source: TeamWakeSource,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryWakeCandidate {
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub unread_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryBacklogResult {
    pub team_run_id: String,
    pub source: TeamRunSource,
    pub recorded_wakes: Vec<String>,
    pub pending_wake_count: usize,
}

#[derive(Debug, Clone)]
struct TeamRunRecord {
    team_run_id: String,
    team_id: String,
    source: TeamRunSource,
    has_user_intervention: bool,
    target_slot_id: String,
    target_role: TeamRunTargetRole,
    status: TeamRunStatus,
    started_at: Option<TimestampMs>,
    completed_at: Option<TimestampMs>,
    cancelled_at: Option<TimestampMs>,
    cancel_reason: Option<String>,
    active_child_turns: HashMap<String, ActiveChildTurn>,
    starting_reservations: HashMap<String, StartingChildReservation>,
    pending_wakes: HashMap<String, VecDeque<PendingWake>>,
    slot_runtime_health: HashMap<String, TeamSlotRuntimeHealth>,
    slot_wake_gate: SlotWakeGate,
    active_operation_leases: HashMap<String, ActiveOperationLease>,
}

impl TeamRunRecord {
    fn pending_wake_count(&self) -> usize {
        self.pending_wakes.values().map(VecDeque::len).sum()
    }

    fn pending_wake_count_for_slot(&self, slot_id: &str) -> usize {
        self.pending_wakes.get(slot_id).map(VecDeque::len).unwrap_or(0)
    }

    fn starting_child_count_for_slot(&self, slot_id: &str) -> usize {
        self.starting_reservations
            .values()
            .filter(|reservation| reservation.slot_id == slot_id)
            .count()
    }

    fn active_operation_lease_count(&self) -> usize {
        self.active_operation_leases.len()
    }

    fn slot_run_state(&self, slot_id: &str) -> TeamRunSlotState {
        if self.active_child_turns.contains_key(slot_id) || self.starting_child_count_for_slot(slot_id) > 0 {
            return TeamRunSlotState::Busy;
        }
        if self.pending_wake_count_for_slot(slot_id) > 0 {
            return TeamRunSlotState::Pending;
        }
        if self.slot_wake_gate.snapshot_for_slot(slot_id).paused {
            return TeamRunSlotState::Paused;
        }
        TeamRunSlotState::Idle
    }

    fn has_spawn_welcome_for_slot(&self, slot_id: &str) -> bool {
        self.pending_wakes
            .get(slot_id)
            .is_some_and(|wakes| wakes.iter().any(|wake| wake.source == TeamWakeSource::SpawnWelcome))
            || self.active_operation_leases.values().any(|active| {
                active.lease.slot_id == slot_id && active.lease.wake_source == TeamWakeSource::SpawnWelcome
            })
    }

    fn role_for_slot(&self, slot_id: &str) -> Option<TeamRunTargetRole> {
        self.active_child_turns
            .get(slot_id)
            .map(|child| child.role.clone())
            .or_else(|| {
                self.starting_reservations
                    .values()
                    .find(|reservation| reservation.slot_id == slot_id)
                    .map(|reservation| reservation.role.clone())
            })
            .or_else(|| {
                self.pending_wakes
                    .get(slot_id)
                    .and_then(|wakes| wakes.front())
                    .map(|wake| wake.role.clone())
            })
            .or_else(|| self.slot_wake_gate.role_hint(slot_id))
            .or_else(|| (self.target_slot_id == slot_id).then(|| self.target_role.clone()))
    }

    fn slot_work(&self) -> Vec<TeamSlotWorkPayload> {
        let now = now_ms();
        let mut slot_ids = self
            .pending_wakes
            .keys()
            .cloned()
            .chain(
                self.starting_reservations
                    .values()
                    .map(|reservation| reservation.slot_id.clone()),
            )
            .chain(self.active_child_turns.keys().cloned())
            .chain(self.slot_runtime_health.keys().cloned())
            .chain(self.slot_wake_gate.slot_ids().cloned())
            .collect::<Vec<_>>();
        slot_ids.sort();
        slot_ids.dedup();

        slot_ids
            .into_iter()
            .filter_map(|slot_id| {
                let role = self.role_for_slot(&slot_id)?;
                let gate = self.slot_wake_gate.snapshot_for_slot(&slot_id);
                let active_child = self.active_child_turns.get(&slot_id);
                let active_elapsed_ms = active_child.map(|child| {
                    child
                        .last_slow_notified_at_ms
                        .unwrap_or(now)
                        .saturating_sub(child.started_at_ms)
                        .max(0) as u64
                });
                Some(TeamSlotWorkPayload {
                    pending_wake_count: self.pending_wake_count_for_slot(&slot_id),
                    starting_child_count: self.starting_child_count_for_slot(&slot_id),
                    paused: gate.paused,
                    suppressed_wake_count: gate.suppressed_wake_count,
                    active_turn_id: active_child.map(|child| child.turn_id.clone()),
                    active_turn_started_at_ms: active_child.map(|child| child.started_at_ms),
                    active_turn_elapsed_ms: active_elapsed_ms,
                    active_turn_slow: active_elapsed_ms.map(|elapsed| elapsed >= ACTIVE_CHILD_SLOW_THRESHOLD_MS),
                    active_turn_slow_threshold_ms: active_child.map(|_| ACTIVE_CHILD_SLOW_THRESHOLD_MS),
                    runtime_health: self.slot_runtime_health.get(&slot_id).cloned(),
                    slot_id,
                    role,
                })
            })
            .collect()
    }

    fn slot_is_busy(&self, slot_id: &str) -> bool {
        self.pending_wake_count_for_slot(slot_id) > 0
            || self.starting_child_count_for_slot(slot_id) > 0
            || self.active_child_turns.contains_key(slot_id)
    }

    fn has_retained_wake_gate_work(&self) -> bool {
        self.slot_wake_gate.has_retained_work()
    }

    fn payload(&self) -> TeamRunPayload {
        TeamRunPayload {
            team_id: self.team_id.clone(),
            team_run_id: self.team_run_id.clone(),
            source: self.source.clone(),
            has_user_intervention: self.has_user_intervention,
            target_slot_id: self.target_slot_id.clone(),
            target_role: self.target_role.clone(),
            status: self.status.clone(),
            active_child_count: self.active_child_turns.len(),
            pending_wake_count: self.pending_wake_count(),
            starting_child_count: self.starting_reservations.len(),
            slot_work: self.slot_work(),
        }
    }

    fn ack(
        &self,
        accepted_slot_id: &str,
        accepted_role: TeamRunTargetRole,
        message_id: Option<String>,
    ) -> TeamRunAckResponse {
        TeamRunAckResponse {
            team_run_id: self.team_run_id.clone(),
            team_id: self.team_id.clone(),
            source: self.source.clone(),
            has_user_intervention: self.has_user_intervention,
            target_slot_id: self.target_slot_id.clone(),
            target_role: self.target_role.clone(),
            accepted_slot_id: accepted_slot_id.to_owned(),
            accepted_role,
            status: self.status.clone(),
            message_id,
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.status, TeamRunStatus::Accepted | TeamRunStatus::Running)
    }
}

fn new_operation_lease(
    run: &mut TeamRunRecord,
    slot_id: &str,
    role: TeamRunTargetRole,
    wake_source: TeamWakeSource,
    accepted_as_new_run: bool,
) -> TeamRunOperationLease {
    let lease = TeamRunOperationLease {
        lease_id: generate_id(),
        team_run_id: run.team_run_id.clone(),
        slot_id: slot_id.to_owned(),
        role,
        wake_source,
        accepted_as_new_run,
    };
    run.active_operation_leases
        .insert(lease.lease_id.clone(), ActiveOperationLease { lease: lease.clone() });
    lease
}

fn new_team_run_record(
    team_id: String,
    target_slot_id: &str,
    target_role: TeamRunTargetRole,
    source: TeamRunSource,
    has_user_intervention: bool,
) -> TeamRunRecord {
    TeamRunRecord {
        team_run_id: generate_id(),
        team_id,
        source,
        has_user_intervention,
        target_slot_id: target_slot_id.to_owned(),
        target_role,
        status: TeamRunStatus::Accepted,
        started_at: None,
        completed_at: None,
        cancelled_at: None,
        cancel_reason: None,
        active_child_turns: HashMap::new(),
        starting_reservations: HashMap::new(),
        pending_wakes: HashMap::new(),
        slot_runtime_health: HashMap::new(),
        slot_wake_gate: SlotWakeGate::default(),
        active_operation_leases: HashMap::new(),
    }
}

fn push_pending_wake_locked(
    run: &mut TeamRunRecord,
    slot_id: String,
    role: TeamRunTargetRole,
    source: TeamWakeSource,
    message_id: Option<String>,
) {
    let wake = PendingWake {
        slot_id: slot_id.clone(),
        role,
        source,
        message_id,
    };
    let queue = run.pending_wakes.entry(slot_id).or_default();
    if is_foreground_wake(source) {
        let insert_at = queue
            .iter()
            .position(|pending| !is_foreground_wake(pending.source))
            .unwrap_or(queue.len());
        queue.insert(insert_at, wake);
    } else {
        queue.push_back(wake);
    }
}

fn is_foreground_wake(source: TeamWakeSource) -> bool {
    matches!(source, TeamWakeSource::UserMessage | TeamWakeSource::UserIntervention)
}

fn acquire_policy(
    source: TeamWakeSource,
    slot_state: TeamRunSlotState,
    has_spawn_welcome: bool,
    intent: TeamRunWakeIntent,
) -> AcquirePolicyDecision {
    match source {
        TeamWakeSource::UserMessage | TeamWakeSource::UserIntervention => match slot_state {
            TeamRunSlotState::Busy => AcquirePolicyDecision::RejectSlotBusy,
            TeamRunSlotState::Pending | TeamRunSlotState::Paused | TeamRunSlotState::Idle => {
                AcquirePolicyDecision::Accept
            }
        },
        TeamWakeSource::McpSendMessage => match slot_state {
            TeamRunSlotState::Paused => AcquirePolicyDecision::Suppress("paused_slot_background_wake"),
            TeamRunSlotState::Busy | TeamRunSlotState::Pending | TeamRunSlotState::Idle => {
                AcquirePolicyDecision::Accept
            }
        },
        TeamWakeSource::SpawnWelcome => {
            if has_spawn_welcome {
                return AcquirePolicyDecision::Suppress("duplicate_spawn_welcome");
            }
            match slot_state {
                TeamRunSlotState::Busy => AcquirePolicyDecision::RejectInvalid("spawn welcome target is already busy"),
                TeamRunSlotState::Pending => AcquirePolicyDecision::Suppress("spawn_welcome_already_pending"),
                TeamRunSlotState::Paused | TeamRunSlotState::Idle => AcquirePolicyDecision::Accept,
            }
        }
        TeamWakeSource::McpShutdownRequest => AcquirePolicyDecision::Accept,
        TeamWakeSource::IdleNotification | TeamWakeSource::InterruptedNotification => match slot_state {
            TeamRunSlotState::Idle if intent == TeamRunWakeIntent::SchedulerWakeTarget => AcquirePolicyDecision::Accept,
            TeamRunSlotState::Idle => AcquirePolicyDecision::Suppress("background_notification_without_wake_target"),
            TeamRunSlotState::Busy | TeamRunSlotState::Pending | TeamRunSlotState::Paused => {
                AcquirePolicyDecision::Suppress("background_notification_deduped")
            }
        },
        TeamWakeSource::RecoveryDrain
        | TeamWakeSource::CrashNotification
        | TeamWakeSource::InactivityTimeout
        | TeamWakeSource::SpawnAttachFailure
        | TeamWakeSource::ShutdownRejected => match slot_state {
            TeamRunSlotState::Paused => AcquirePolicyDecision::Suppress("paused_slot_recovery_wake"),
            TeamRunSlotState::Busy | TeamRunSlotState::Pending | TeamRunSlotState::Idle => {
                AcquirePolicyDecision::Accept
            }
        },
    }
}

#[derive(Clone)]
pub struct TeamRunManager {
    team_id: String,
    emitter: Arc<TeamEventEmitter>,
    state: Arc<Mutex<Option<TeamRunRecord>>>,
}

impl TeamRunManager {
    pub fn new(team_id: String, emitter: Arc<TeamEventEmitter>) -> Self {
        Self {
            team_id,
            emitter,
            state: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn accept_user_message(
        &self,
        target_slot_id: &str,
        target_role: TeamRunTargetRole,
        allow_active_intervention: bool,
        message_id: Option<String>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        let mut guard = self.state.lock().await;
        if let Some(active) = guard.as_mut().filter(|r| r.is_active()) {
            if allow_active_intervention {
                if active.slot_is_busy(target_slot_id) {
                    info!(
                        team_id = %self.team_id,
                        team_run_id = %active.team_run_id,
                        target_slot_id,
                        target_role = ?target_role,
                        "team_run active intervention rejected because target slot is busy"
                    );
                    return Err(TeamError::SlotBusy(target_slot_id.to_owned()));
                }
                debug!(
                    team_id = %self.team_id,
                    team_run_id = %active.team_run_id,
                    target_slot_id = %target_slot_id,
                    target_role = ?target_role,
                    active_target_slot_id = %active.target_slot_id,
                    active_target_role = ?active.target_role,
                    "team_run active intervention accepted"
                );
                active.has_user_intervention = true;
                return Ok(active.ack(target_slot_id, target_role, message_id));
            }
            return Err(TeamError::InvalidRequest("team run is already active".into()));
        }
        if let Some(cancelling) = guard.as_ref().filter(|r| matches!(r.status, TeamRunStatus::Cancelling)) {
            return Err(TeamError::InvalidRequest(format!(
                "team run {} is cancelling",
                cancelling.team_run_id
            )));
        }

        let record = new_team_run_record(
            self.team_id.clone(),
            target_slot_id,
            target_role.clone(),
            TeamRunSource::UserMessage,
            true,
        );
        let ack = record.ack(target_slot_id, target_role, message_id);
        let payload = record.payload();
        *guard = Some(record);
        drop(guard);

        info!(
            team_id = %self.team_id,
            team_run_id = %ack.team_run_id,
            target_slot_id = %ack.target_slot_id,
            target_role = ?ack.target_role,
            "team_run accepted"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_ACCEPTED_EVENT, payload);
        Ok(ack)
    }

    pub(crate) async fn acquire_user_message_wake(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
    ) -> Result<(TeamRunAckResponse, TeamRunOperationLease), TeamError> {
        let mut guard = self.state.lock().await;

        if let Some(run) = guard.as_mut().filter(|r| r.is_active()) {
            if matches!(run.slot_run_state(slot_id), TeamRunSlotState::Busy) {
                info!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id,
                    role = ?role,
                    "team_run user wake acquire rejected because slot is busy"
                );
                return Err(TeamError::SlotBusy(slot_id.to_owned()));
            }

            let source = TeamWakeSource::UserIntervention;
            let _ = run.slot_wake_gate.before_wake(slot_id, source, None);
            run.has_user_intervention = true;
            let lease = new_operation_lease(run, slot_id, role.clone(), source, false);
            let ack = run.ack(slot_id, role, None);
            info!(
                team_id = %self.team_id,
                team_run_id = %lease.team_run_id,
                lease_id = %lease.lease_id,
                slot_id = %lease.slot_id,
                wake_source = %lease.wake_source,
                accepted_as_new_run = lease.accepted_as_new_run,
                "team_run operation lease acquired"
            );
            return Ok((ack, lease));
        }

        if let Some(cancelling) = guard.as_ref().filter(|r| matches!(r.status, TeamRunStatus::Cancelling)) {
            return Err(TeamError::InvalidRequest(format!(
                "team run {} is cancelling",
                cancelling.team_run_id
            )));
        }

        let mut record = new_team_run_record(
            self.team_id.clone(),
            slot_id,
            role.clone(),
            TeamRunSource::UserMessage,
            true,
        );
        let lease = new_operation_lease(&mut record, slot_id, role.clone(), TeamWakeSource::UserMessage, true);
        let ack = record.ack(slot_id, role, None);
        let payload = record.payload();
        *guard = Some(record);
        drop(guard);

        info!(
            team_id = %self.team_id,
            team_run_id = %ack.team_run_id,
            target_slot_id = %ack.target_slot_id,
            target_role = ?ack.target_role,
            "team_run accepted"
        );
        info!(
            team_id = %self.team_id,
            team_run_id = %lease.team_run_id,
            lease_id = %lease.lease_id,
            slot_id = %lease.slot_id,
            wake_source = %lease.wake_source,
            accepted_as_new_run = lease.accepted_as_new_run,
            "team_run operation lease acquired"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_ACCEPTED_EVENT, payload);
        Ok((ack, lease))
    }

    pub(crate) async fn recover_mailbox_backlog(
        &self,
        candidates: Vec<RecoveryWakeCandidate>,
    ) -> Option<RecoveryBacklogResult> {
        if candidates.is_empty() {
            return None;
        }

        let mut guard = self.state.lock().await;
        if let Some(run) = guard.as_ref().filter(|r| matches!(r.status, TeamRunStatus::Cancelling)) {
            warn!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                source = %TeamWakeSource::RecoveryDrain,
                candidate_count = candidates.len(),
                reason = "active_run_cancelling",
                "team recovery backlog skipped"
            );
            return None;
        }

        if guard.as_ref().is_none_or(|run| !run.is_active()) {
            let first = candidates.first().expect("checked non-empty");
            let mut run = new_team_run_record(
                self.team_id.clone(),
                &first.slot_id,
                first.role.clone(),
                TeamRunSource::RecoveryDrain,
                false,
            );
            for candidate in &candidates {
                push_pending_wake_locked(
                    &mut run,
                    candidate.slot_id.clone(),
                    candidate.role.clone(),
                    TeamWakeSource::RecoveryDrain,
                    None,
                );
            }
            let payload = run.payload();
            let result = RecoveryBacklogResult {
                team_run_id: run.team_run_id.clone(),
                source: run.source.clone(),
                recorded_wakes: candidates.iter().map(|candidate| candidate.slot_id.clone()).collect(),
                pending_wake_count: payload.pending_wake_count,
            };
            *guard = Some(run);
            drop(guard);

            info!(
                team_id = %self.team_id,
                team_run_id = %result.team_run_id,
                source = "recovery_drain",
                slot_count = result.recorded_wakes.len(),
                pending_wake_count = result.pending_wake_count,
                reason = "orphan_mailbox_backlog",
                "team recovery drain accepted"
            );
            self.emitter.broadcast_team_run(TEAM_RUN_ACCEPTED_EVENT, payload);
            return Some(result);
        }

        let run = guard.as_mut().expect("active checked");
        let mut recorded_wakes = Vec::new();
        for candidate in candidates {
            let slot_state = run.slot_run_state(&candidate.slot_id);
            if !matches!(slot_state, TeamRunSlotState::Idle) {
                debug!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id = %candidate.slot_id,
                    source = "recovery_drain",
                    unread_count = candidate.unread_count,
                    slot_state = ?slot_state,
                    reason = "slot_already_has_run_work",
                    "team recovery backlog wake skipped"
                );
                continue;
            }
            push_pending_wake_locked(
                run,
                candidate.slot_id.clone(),
                candidate.role,
                TeamWakeSource::RecoveryDrain,
                None,
            );
            recorded_wakes.push(candidate.slot_id);
        }

        if recorded_wakes.is_empty() {
            return None;
        }

        let payload = run.payload();
        let result = RecoveryBacklogResult {
            team_run_id: run.team_run_id.clone(),
            source: run.source.clone(),
            recorded_wakes,
            pending_wake_count: payload.pending_wake_count,
        };
        info!(
            team_id = %self.team_id,
            team_run_id = %result.team_run_id,
            source = "recovery_drain",
            slot_count = result.recorded_wakes.len(),
            pending_wake_count = result.pending_wake_count,
            reason = "attached_to_active_run",
            "team recovery backlog attached to active run"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Some(result)
    }

    pub(crate) async fn commit_operation_lease(
        &self,
        lease_id: &str,
        trigger_message_id: Option<String>,
    ) -> Result<(), TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            error!(
                team_id = %self.team_id,
                lease_id,
                "team_run operation lease commit failed because no active run exists"
            );
            return Err(TeamError::InvalidRequest(format!(
                "team run operation lease missing: {lease_id}"
            )));
        };
        let Some(active) = run.active_operation_leases.remove(lease_id) else {
            error!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                lease_id,
                "team_run operation lease commit failed because lease is missing"
            );
            return Err(TeamError::InvalidRequest(format!(
                "team run operation lease missing: {lease_id}"
            )));
        };

        let lease = active.lease;
        push_pending_wake_locked(
            run,
            lease.slot_id.clone(),
            lease.role.clone(),
            lease.wake_source,
            trigger_message_id,
        );
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            lease_id = %lease.lease_id,
            slot_id = %lease.slot_id,
            wake_source = %lease.wake_source,
            pending_wake_count = payload.pending_wake_count,
            active_operation_lease_count = run.active_operation_lease_count(),
            "team_run operation lease committed"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Ok(())
    }

    pub(crate) async fn abort_operation_lease(&self, lease_id: &str, reason: &str) -> Result<(), TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            error!(
                team_id = %self.team_id,
                lease_id,
                reason,
                "team_run operation lease abort failed because no active run exists"
            );
            return Err(TeamError::InvalidRequest(format!(
                "team run operation lease missing: {lease_id}"
            )));
        };
        let Some(active) = run.active_operation_leases.remove(lease_id) else {
            error!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                lease_id,
                reason,
                "team_run operation lease abort failed because lease is missing"
            );
            return Err(TeamError::InvalidRequest(format!(
                "team run operation lease missing: {lease_id}"
            )));
        };
        let payload = run.payload();
        warn!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            lease_id = %active.lease.lease_id,
            slot_id = %active.lease.slot_id,
            wake_source = %active.lease.wake_source,
            reason,
            active_operation_lease_count = run.active_operation_lease_count(),
            "team_run operation lease aborted"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Ok(())
    }

    async fn acquire_run_scoped_wake_with_intent(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
        source: TeamWakeSource,
        intent: TeamRunWakeIntent,
    ) -> Result<TeamRunWakeAcquireOutcome, TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            warn!(
                team_id = %self.team_id,
                slot_id,
                role = ?role,
                wake_source = %source,
                "team_run wake acquire rejected because no active run exists"
            );
            return Err(TeamError::InvalidRequest(
                "no active team run for run-scoped wake".into(),
            ));
        };

        let slot_state = run.slot_run_state(slot_id);
        let decision = acquire_policy(source, slot_state, run.has_spawn_welcome_for_slot(slot_id), intent);
        match decision {
            AcquirePolicyDecision::RejectSlotBusy => Err(TeamError::SlotBusy(slot_id.to_owned())),
            AcquirePolicyDecision::RejectInvalid(message) => Err(TeamError::InvalidRequest(message.into())),
            AcquirePolicyDecision::Suppress(reason) => {
                let _ = run.slot_wake_gate.before_wake(slot_id, source, None);
                let payload = run.payload();
                debug!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id,
                    wake_source = %source,
                    slot_state = ?slot_state,
                    reason,
                    "team_run wake acquire suppressed"
                );
                drop(guard);
                self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
                Ok(TeamRunWakeAcquireOutcome::Suppressed)
            }
            AcquirePolicyDecision::Accept => {
                let _ = run.slot_wake_gate.before_wake(slot_id, source, None);
                let lease = new_operation_lease(run, slot_id, role, source, false);
                info!(
                    team_id = %self.team_id,
                    team_run_id = %lease.team_run_id,
                    lease_id = %lease.lease_id,
                    slot_id = %lease.slot_id,
                    wake_source = %lease.wake_source,
                    accepted_as_new_run = lease.accepted_as_new_run,
                    "team_run operation lease acquired"
                );
                Ok(TeamRunWakeAcquireOutcome::Accepted(lease))
            }
        }
    }

    pub(crate) async fn acquire_run_scoped_wake(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
        source: TeamWakeSource,
    ) -> Result<TeamRunWakeAcquireOutcome, TeamError> {
        self.acquire_run_scoped_wake_with_intent(slot_id, role, source, TeamRunWakeIntent::ExternalRequest)
            .await
    }

    pub(crate) async fn acquire_scheduler_wake(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
        source: TeamWakeSource,
    ) -> Result<TeamRunWakeAcquireOutcome, TeamError> {
        self.acquire_run_scoped_wake_with_intent(slot_id, role, source, TeamRunWakeIntent::SchedulerWakeTarget)
            .await
    }

    pub async fn active_run_id(&self) -> Option<String> {
        let guard = self.state.lock().await;
        guard.as_ref().filter(|r| r.is_active()).map(|r| r.team_run_id.clone())
    }

    pub async fn current_run_id(&self) -> Option<String> {
        let guard = self.state.lock().await;
        guard.as_ref().map(|r| r.team_run_id.clone())
    }

    pub async fn active_child_turns(&self) -> Vec<ActiveChildTurn> {
        let guard = self.state.lock().await;
        guard
            .as_ref()
            .map(|run| run.active_child_turns.values().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn current_payload(&self) -> Option<TeamRunPayload> {
        let guard = self.state.lock().await;
        guard.as_ref().filter(|run| run.is_active()).map(TeamRunRecord::payload)
    }

    pub(crate) async fn slot_work_for_slot(&self, slot_id: &str) -> Option<(String, TeamSlotWorkPayload)> {
        let guard = self.state.lock().await;
        let run = guard.as_ref().filter(|run| run.is_active())?;
        let payload = run.payload();
        let work = payload.slot_work.iter().find(|work| work.slot_id == slot_id).cloned()?;
        Some((payload.team_run_id, work))
    }

    pub async fn mark_slot_runtime_health(
        &self,
        slot_id: &str,
        health: TeamSlotRuntimeHealth,
    ) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut().filter(|run| run.is_active())?;
        run.slot_runtime_health.insert(slot_id.to_owned(), health);
        let payload = run.payload();
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload.clone());
        Some(payload)
    }

    pub async fn observe_slow_child_turns(&self, now: TimestampMs) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut().filter(|run| run.is_active())?;
        let mut observed = false;

        for child in run.active_child_turns.values_mut() {
            let elapsed_ms = now.saturating_sub(child.started_at_ms).max(0) as u64;
            if elapsed_ms < ACTIVE_CHILD_SLOW_THRESHOLD_MS {
                continue;
            }
            let due = child
                .last_slow_notified_at_ms
                .map(|last| now.saturating_sub(last).max(0) as u64 >= ACTIVE_CHILD_SLOW_REPEAT_MS)
                .unwrap_or(true);
            if !due {
                continue;
            }
            child.last_slow_notified_at_ms = Some(now);
            observed = true;
            info!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                slot_id = %child.slot_id,
                role = ?child.role,
                conversation_id = %child.conversation_id,
                turn_id = %child.turn_id,
                elapsed_ms,
                slow_threshold_ms = ACTIVE_CHILD_SLOW_THRESHOLD_MS,
                "team_child_turn slow"
            );
        }

        if !observed {
            return None;
        }

        let payload = run.payload();
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload.clone());
        Some(payload)
    }

    pub async fn pause_slot_work(&self, slot_id: &str, reason: Option<String>) -> Result<PauseSlotOutcome, TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            return Err(TeamError::InvalidRequest("no active team run to pause".into()));
        };

        let role = run
            .role_for_slot(slot_id)
            .ok_or_else(|| TeamError::AgentNotFound(slot_id.to_owned()))?;
        let reason = reason.unwrap_or_else(|| "user_stop".into());

        if let Some(child) = run.active_child_turns.get(slot_id).cloned() {
            info!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                slot_id,
                active_turn_id = %child.turn_id,
                reason = %reason,
                "team slot pause requested for active child"
            );
            return Ok(PauseSlotOutcome {
                team_run_id: run.team_run_id.clone(),
                cancel_target: Some(ChildCancelTarget::Active(child)),
                payload: run.payload(),
            });
        }

        let pending_count = run.pending_wakes.remove(slot_id).map(|wakes| wakes.len()).unwrap_or(0);
        run.slot_wake_gate.pause(slot_id, role, reason.clone());
        run.slot_wake_gate.add_suppressed(slot_id, pending_count);

        let cancel_target = take_starting_cancel_target_locked(run, slot_id);
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            slot_id,
            active_turn_id = Option::<&str>::None,
            pending_wake_count = pending_count,
            reason = %reason,
            "team slot paused"
        );
        let outcome = PauseSlotOutcome {
            team_run_id: run.team_run_id.clone(),
            cancel_target,
            payload: payload.clone(),
        };
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Ok(outcome)
    }

    #[cfg(test)]
    pub(crate) async fn record_pending_wake(
        &self,
        slot_id: &str,
        target_role: TeamRunTargetRole,
        wake_source: TeamWakeSource,
    ) -> Result<(), TeamError> {
        self.record_or_suppress_wake(slot_id, target_role, wake_source, None)
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) async fn record_or_suppress_wake(
        &self,
        slot_id: &str,
        target_role: TeamRunTargetRole,
        wake_source: TeamWakeSource,
        trigger_message_id: Option<String>,
    ) -> Result<WakeRecordDecision, TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            warn!(
                team_id = %self.team_id,
                slot_id,
                target_role = ?target_role,
                wake_source = %wake_source,
                "team_run pending wake rejected because no active run exists"
            );
            return Err(TeamError::InvalidRequest(
                "no active team run for run-scoped wake".into(),
            ));
        };

        match run
            .slot_wake_gate
            .before_wake(slot_id, wake_source, trigger_message_id.clone())
        {
            WakeGateDecision::Suppress => {
                let suppressed_wake_count = run.slot_wake_gate.snapshot_for_slot(slot_id).suppressed_wake_count;
                let payload = run.payload();
                info!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id,
                    wake_source = %wake_source,
                    suppressed_wake_count,
                    "team wake suppressed"
                );
                drop(guard);
                self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
                Ok(WakeRecordDecision::Suppressed)
            }
            WakeGateDecision::Record { resumed_from_pause } => {
                push_pending_wake_locked(
                    run,
                    slot_id.to_owned(),
                    target_role.clone(),
                    wake_source,
                    trigger_message_id.clone(),
                );
                let slot_pending_wake_count = run.pending_wake_count_for_slot(slot_id);
                let payload = run.payload();
                if resumed_from_pause {
                    info!(
                        team_id = %self.team_id,
                        team_run_id = %run.team_run_id,
                        slot_id,
                        resume_source = %wake_source,
                        foreground_message_id = ?trigger_message_id.as_deref(),
                        "team slot resumed"
                    );
                }
                info!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id,
                    target_role = ?target_role,
                    wake_source = %wake_source,
                    slot_pending_wake_count,
                    pending_wake_count = payload.pending_wake_count,
                    starting_child_count = payload.starting_child_count,
                    active_child_count = payload.active_child_count,
                    "team_run pending wake recorded"
                );
                drop(guard);
                self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
                Ok(WakeRecordDecision::Recorded)
            }
        }
    }

    pub async fn claim_wake_for_turn(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
        conversation_id: &str,
    ) -> Option<StartingChildReservation> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut().filter(|r| r.is_active())?;
        let pending = match run.pending_wakes.get_mut(slot_id).and_then(VecDeque::pop_front) {
            Some(pending) => pending,
            None => {
                warn!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id,
                    role = ?role,
                    pending_wake_count = run.pending_wake_count(),
                    "team_run reservation claim ignored because no pending wake exists for slot"
                );
                return None;
            }
        };
        if run.pending_wakes.get(slot_id).is_some_and(VecDeque::is_empty) {
            run.pending_wakes.remove(slot_id);
        }
        run.slot_runtime_health.remove(slot_id);
        if pending.slot_id != slot_id {
            warn!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                expected_slot_id = %pending.slot_id,
                actual_slot_id = %slot_id,
                wake_source = %pending.source,
                "team_run reservation claimed with slot mismatch"
            );
        }
        if pending.role != role {
            warn!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                slot_id,
                expected_role = ?pending.role,
                actual_role = ?role,
                wake_source = %pending.source,
                "team_run reservation claimed with role mismatch"
            );
        }
        let reservation = StartingChildReservation {
            reservation_id: generate_id(),
            team_run_id: run.team_run_id.clone(),
            slot_id: pending.slot_id.clone(),
            role,
            conversation_id: conversation_id.to_owned(),
            wake_source: pending.source,
            message_id: pending.message_id.clone(),
            state: StartingReservationState::Starting,
        };
        run.starting_reservations
            .insert(reservation.reservation_id.clone(), reservation.clone());
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %reservation.team_run_id,
            reservation_id = %reservation.reservation_id,
            slot_id = %reservation.slot_id,
            role = ?reservation.role,
            wake_source = %pending.source,
            slot_pending_wake_count = run.pending_wake_count_for_slot(slot_id),
            pending_wake_count = payload.pending_wake_count,
            starting_child_count = payload.starting_child_count,
            active_child_count = payload.active_child_count,
            "team_run reservation claimed"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Some(reservation)
    }

    pub async fn retry_child_start_later(&self, reservation_id: &str, reason: &str) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut().filter(|run| run.is_active())?;
        let reservation = match run.starting_reservations.remove(reservation_id) {
            Some(reservation) => reservation,
            None => {
                warn!(
                    team_id = %self.team_id,
                    reservation_id,
                    error = %reason,
                    "team_run reservation retry ignored because reservation is missing"
                );
                return None;
            }
        };

        let pending = PendingWake {
            slot_id: reservation.slot_id.clone(),
            role: reservation.role,
            source: reservation.wake_source,
            message_id: reservation.message_id.clone(),
        };
        run.pending_wakes
            .entry(reservation.slot_id.clone())
            .or_default()
            .push_front(pending);
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            reservation_id = %reservation.reservation_id,
            slot_id = %reservation.slot_id,
            error = %reason,
            slot_pending_wake_count = run.pending_wake_count_for_slot(&reservation.slot_id),
            pending_wake_count = payload.pending_wake_count,
            starting_child_count = payload.starting_child_count,
            active_child_count = payload.active_child_count,
            "team_run reservation deferred for retry"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload.clone());
        Some(payload)
    }

    pub(crate) async fn peek_next_pending_wake(&self, slot_id: &str) -> Option<PendingWakeView> {
        let guard = self.state.lock().await;
        guard
            .as_ref()
            .filter(|r| r.is_active())
            .and_then(|run| run.pending_wakes.get(slot_id))
            .and_then(|wakes| wakes.front())
            .map(|wake| PendingWakeView {
                source: wake.source,
                message_id: wake.message_id.clone(),
            })
    }

    pub async fn record_empty_wake_observed(&self, slot_id: &str) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let consumed = match run.pending_wakes.get_mut(slot_id).and_then(VecDeque::pop_front) {
            Some(pending) => {
                if run.pending_wakes.get(slot_id).is_some_and(VecDeque::is_empty) {
                    run.pending_wakes.remove(slot_id);
                }
                Some(pending)
            }
            None => None,
        };
        let payload_before_completion = run.payload();
        debug!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            slot_id,
            consumed_wake_source = ?consumed.as_ref().map(|wake| wake.source.as_str()),
            slot_pending_wake_count = run.pending_wake_count_for_slot(slot_id),
            pending_wake_count = payload_before_completion.pending_wake_count,
            starting_child_count = payload_before_completion.starting_child_count,
            active_child_count = payload_before_completion.active_child_count,
            "team_run empty mailbox wake observed"
        );
        let payload = maybe_complete_locked(run, &self.emitter);
        if payload.is_some() {
            *guard = None;
        }
        payload
    }

    pub async fn record_child_started(&self, reservation_id: &str, child: ActiveChildTurn) -> ChildStartDecision {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut() else {
            warn!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                reservation_id,
                slot_id = %child.slot_id,
                turn_id = %child.turn_id,
                "team_run child start ignored because no run is active"
            );
            return ChildStartDecision::Ignored;
        };
        let Some(reservation) = run.starting_reservations.remove(reservation_id) else {
            warn!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                reservation_id,
                slot_id = %child.slot_id,
                turn_id = %child.turn_id,
                "team_run child start ignored because reservation is missing"
            );
            return ChildStartDecision::Ignored;
        };
        if run.team_run_id != child.team_run_id {
            warn!(
                team_id = %self.team_id,
                expected_team_run_id = %run.team_run_id,
                actual_team_run_id = %child.team_run_id,
                reservation_id,
                slot_id = %child.slot_id,
                turn_id = %child.turn_id,
                "team_run child start ignored because run id mismatched"
            );
            return ChildStartDecision::Ignored;
        }

        let should_cancel = matches!(run.status, TeamRunStatus::Cancelling)
            || matches!(reservation.state, StartingReservationState::Cancelling);
        let first_child_for_run = run.started_at.is_none();
        if first_child_for_run {
            run.started_at = Some(now_ms());
        }
        if !should_cancel {
            run.status = TeamRunStatus::Running;
        }
        run.active_child_turns.insert(child.slot_id.clone(), child.clone());
        let run_payload = run.payload();
        let child_payload = child_payload(&run.team_id, &child, TeamRunStatus::Running);
        drop(guard);

        if first_child_for_run && !should_cancel {
            info!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                target_slot_id = %run_payload.target_slot_id,
                target_role = ?run_payload.target_role,
                active_child_count = run_payload.active_child_count,
                pending_wake_count = run_payload.pending_wake_count,
                starting_child_count = run_payload.starting_child_count,
                "team_run started"
            );
            self.emitter
                .broadcast_team_run(TEAM_RUN_STARTED_EVENT, run_payload.clone());
        } else {
            self.emitter
                .broadcast_team_run(TEAM_RUN_UPDATED_EVENT, run_payload.clone());
        }
        info!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            reservation_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            cancelling = should_cancel,
            active_child_count = run_payload.active_child_count,
            pending_wake_count = run_payload.pending_wake_count,
            starting_child_count = run_payload.starting_child_count,
            "team_child_turn started"
        );
        self.emitter
            .broadcast_child_turn(TEAM_CHILD_TURN_STARTED_EVENT, child_payload);
        if should_cancel {
            ChildStartDecision::CancelImmediately(child)
        } else {
            ChildStartDecision::Accepted
        }
    }

    pub async fn record_child_start_failed(&self, reservation_id: &str, reason: &str) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let Some(reservation) = run.starting_reservations.remove(reservation_id) else {
            warn!(
                team_id = %self.team_id,
                reservation_id,
                error = %reason,
                "team_run reservation start failure ignored because reservation is missing"
            );
            return None;
        };

        if matches!(run.status, TeamRunStatus::Cancelling)
            || matches!(reservation.state, StartingReservationState::Cancelling)
        {
            let payload = maybe_cancelled_locked(run, &self.emitter);
            if payload.is_some() {
                *guard = None;
            }
            return payload;
        }

        warn!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            reservation_id = %reservation.reservation_id,
            slot_id = %reservation.slot_id,
            error = %reason,
            active_child_count = run.active_child_turns.len(),
            pending_wake_count = run.pending_wake_count(),
            starting_child_count = run.starting_reservations.len(),
            "team_run reservation failed before start"
        );
        run.status = TeamRunStatus::Failed;
        run.completed_at = Some(now_ms());
        let payload = run.payload();
        *guard = None;
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_FAILED_EVENT, payload.clone());
        Some(payload)
    }

    pub async fn record_child_completed(
        &self,
        slot_id: &str,
        turn_id: &str,
        status: TeamRunStatus,
    ) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let child = run.active_child_turns.remove(slot_id)?;
        if child.turn_id != turn_id {
            run.active_child_turns.insert(slot_id.to_owned(), child);
            return None;
        }

        let child_payload = child_payload(&run.team_id, &child, status.clone());
        match status {
            TeamRunStatus::Failed => {
                warn!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id = %child.slot_id,
                    role = ?child.role,
                    conversation_id = %child.conversation_id,
                    turn_id = %child.turn_id,
                    active_child_count = run.active_child_turns.len(),
                    pending_wake_count = run.pending_wake_count(),
                    starting_child_count = run.starting_reservations.len(),
                    "team_child_turn failed"
                );
                run.status = TeamRunStatus::Failed;
                run.completed_at = Some(now_ms());
                let payload = run.payload();
                *guard = None;
                drop(guard);
                self.emitter
                    .broadcast_child_turn(TEAM_CHILD_TURN_COMPLETED_EVENT, child_payload);
                warn!(
                    team_id = %payload.team_id,
                    team_run_id = %payload.team_run_id,
                    target_slot_id = %payload.target_slot_id,
                    target_role = ?payload.target_role,
                    active_child_count = payload.active_child_count,
                    pending_wake_count = payload.pending_wake_count,
                    starting_child_count = payload.starting_child_count,
                    "team_run failed"
                );
                self.emitter.broadcast_team_run(TEAM_RUN_FAILED_EVENT, payload.clone());
                Some(payload)
            }
            _ => {
                info!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id = %child.slot_id,
                    role = ?child.role,
                    conversation_id = %child.conversation_id,
                    turn_id = %child.turn_id,
                    status = ?status,
                    active_child_count = run.active_child_turns.len(),
                    pending_wake_count = run.pending_wake_count(),
                    starting_child_count = run.starting_reservations.len(),
                    "team_child_turn completed"
                );
                self.emitter
                    .broadcast_child_turn(TEAM_CHILD_TURN_COMPLETED_EVENT, child_payload);
                let payload = maybe_complete_locked(run, &self.emitter);
                if payload.is_some() {
                    *guard = None;
                }
                payload
            }
        }
    }

    pub async fn maybe_complete(&self) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let payload = maybe_complete_locked(run, &self.emitter)?;
        *guard = None;
        Some(payload)
    }

    pub async fn begin_cancel(&self, target_slot_id: Option<String>, reason: Option<String>) -> Result<(), TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            return Err(TeamError::InvalidRequest("no active team run to cancel".into()));
        };
        if let Some(target) = target_slot_id.as_deref()
            && target != run.target_slot_id
            && !run.active_child_turns.contains_key(target)
            && !run
                .starting_reservations
                .values()
                .any(|reservation| reservation.slot_id == target)
        {
            return Err(TeamError::AgentNotFound(target.to_owned()));
        }
        run.status = TeamRunStatus::Cancelling;
        run.cancel_reason = reason;
        run.pending_wakes.clear();
        run.slot_wake_gate.clear();
        for reservation in run.starting_reservations.values_mut() {
            reservation.state = StartingReservationState::Cancelling;
        }
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            target_slot_id = ?target_slot_id.as_deref(),
            active_child_count = run.active_child_turns.len(),
            starting_child_count = run.starting_reservations.len(),
            pending_wake_count = payload.pending_wake_count,
            "team_run cancel requested"
        );
        drop(guard);

        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Ok(())
    }

    pub async fn try_complete_cancelled(&self) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let payload = maybe_cancelled_locked(run, &self.emitter)?;
        *guard = None;
        Some(payload)
    }

    pub async fn complete_failed(&self) -> Option<String> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        run.status = TeamRunStatus::Failed;
        run.completed_at = Some(now_ms());
        let team_run_id = run.team_run_id.clone();
        let payload = run.payload();
        *guard = None;
        drop(guard);

        warn!(
            team_id = %self.team_id,
            team_run_id = %team_run_id,
            active_child_count = payload.active_child_count,
            pending_wake_count = payload.pending_wake_count,
            starting_child_count = payload.starting_child_count,
            "team_run failed"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_FAILED_EVENT, payload);
        Some(team_run_id)
    }

    pub async fn begin_cancel_child(&self, slot_id: &str) -> Result<ChildCancelTarget, TeamError> {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut().filter(|r| r.is_active()) else {
            return Err(TeamError::InvalidRequest("no active team run".into()));
        };
        if let Some(child) = run.active_child_turns.get(slot_id).cloned() {
            info!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                slot_id,
                turn_id = %child.turn_id,
                state = "active",
                active_child_count = run.active_child_turns.len(),
                pending_wake_count = run.pending_wake_count(),
                starting_child_count = run.starting_reservations.len(),
                "team_run child cancel requested"
            );
            return Ok(ChildCancelTarget::Active(child));
        }
        if let Some(reservation) = run
            .starting_reservations
            .values_mut()
            .find(|reservation| reservation.slot_id == slot_id)
        {
            reservation.state = StartingReservationState::Cancelling;
            let reservation = reservation.clone();
            let team_run_id = run.team_run_id.clone();
            let active_child_count = run.active_child_turns.len();
            let pending_wake_count = run.pending_wake_count();
            let starting_child_count = run.starting_reservations.len();
            info!(
                team_id = %self.team_id,
                team_run_id = %team_run_id,
                slot_id,
                reservation_id = %reservation.reservation_id,
                state = "starting",
                active_child_count,
                pending_wake_count,
                starting_child_count,
                "team_run child cancel requested"
            );
            return Ok(ChildCancelTarget::Starting(reservation));
        }
        Err(TeamError::InvalidRequest(format!(
            "agent {slot_id} has no active or starting child turn"
        )))
    }

    pub async fn record_child_cancelled(&self, child: &ActiveChildTurn) {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut() else {
            return;
        };
        run.active_child_turns.remove(&child.slot_id);
        let payload = child_payload(&run.team_id, child, TeamRunStatus::Cancelled);
        let run_payload = if matches!(run.status, TeamRunStatus::Cancelling) {
            maybe_cancelled_locked(run, &self.emitter)
        } else {
            None
        };
        let counts_payload = run.payload();
        if run_payload.is_some() {
            *guard = None;
        }
        drop(guard);

        info!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            active_child_count = counts_payload.active_child_count,
            pending_wake_count = counts_payload.pending_wake_count,
            starting_child_count = counts_payload.starting_child_count,
            "team_child_turn cancelled"
        );
        self.emitter
            .broadcast_child_turn(TEAM_CHILD_TURN_CANCELLED_EVENT, payload);
    }

    pub async fn complete_pause_after_child_cancelled(
        &self,
        child: &ActiveChildTurn,
        reason: Option<String>,
    ) -> Option<TeamRunPayload> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        let reason = reason.unwrap_or_else(|| "user_stop".into());
        let pending_count = run
            .pending_wakes
            .remove(&child.slot_id)
            .map(|wakes| wakes.len())
            .unwrap_or(0);
        run.slot_wake_gate
            .pause(&child.slot_id, child.role.clone(), reason.clone());
        run.slot_wake_gate.add_suppressed(&child.slot_id, pending_count);
        run.active_child_turns.remove(&child.slot_id);

        let child_payload = child_payload(&run.team_id, child, TeamRunStatus::Cancelled);
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            pending_wake_count = pending_count,
            reason = %reason,
            "team slot paused"
        );
        info!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            active_child_count = payload.active_child_count,
            pending_wake_count = payload.pending_wake_count,
            starting_child_count = payload.starting_child_count,
            "team_child_turn cancelled"
        );
        drop(guard);

        self.emitter
            .broadcast_child_turn(TEAM_CHILD_TURN_CANCELLED_EVENT, child_payload);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload.clone());
        Some(payload)
    }

    pub(crate) async fn release_suppressed_wake_if_resumed(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
    ) -> Option<TeamWakeSource> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut().filter(|r| r.is_active())?;
        let source = run.slot_wake_gate.release_suppressed_if_resumed(slot_id)?;
        push_pending_wake_locked(run, slot_id.to_owned(), role.clone(), source, None);
        let payload = run.payload();
        let slot_work = payload.slot_work.iter().find(|work| work.slot_id == slot_id);
        info!(
            team_id = %self.team_id,
            team_run_id = %payload.team_run_id,
            slot_id,
            role = ?role,
            released_wake_source = %source,
            pending_wake_count = payload.pending_wake_count,
            slot_pending_wake_count = slot_work.map(|work| work.pending_wake_count).unwrap_or_default(),
            suppressed_wake_count = slot_work.map(|work| work.suppressed_wake_count).unwrap_or_default(),
            "team suppressed wake released"
        );
        drop(guard);
        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Some(source)
    }
}

pub fn target_role_for(role: TeammateRole) -> TeamRunTargetRole {
    match role {
        TeammateRole::Lead => TeamRunTargetRole::Lead,
        TeammateRole::Teammate => TeamRunTargetRole::Teammate,
    }
}

fn child_payload(team_id: &str, child: &ActiveChildTurn, status: TeamRunStatus) -> TeamChildTurnPayload {
    TeamChildTurnPayload {
        team_id: team_id.to_owned(),
        team_run_id: child.team_run_id.clone(),
        slot_id: child.slot_id.clone(),
        role: child.role.clone(),
        conversation_id: child.conversation_id.clone(),
        turn_id: child.turn_id.clone(),
        status,
    }
}

fn take_starting_cancel_target_locked(run: &mut TeamRunRecord, slot_id: &str) -> Option<ChildCancelTarget> {
    let reservation_id = run
        .starting_reservations
        .iter()
        .find_map(|(id, reservation)| (reservation.slot_id == slot_id).then(|| id.clone()));
    reservation_id.and_then(|id| run.starting_reservations.remove(&id).map(ChildCancelTarget::Starting))
}

fn maybe_complete_locked(run: &mut TeamRunRecord, emitter: &TeamEventEmitter) -> Option<TeamRunPayload> {
    if run.pending_wake_count() > 0
        || !run.starting_reservations.is_empty()
        || !run.active_child_turns.is_empty()
        || run.active_operation_lease_count() > 0
        || run.has_retained_wake_gate_work()
    {
        emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, run.payload());
        return None;
    }
    if !matches!(run.status, TeamRunStatus::Running | TeamRunStatus::Accepted) {
        return None;
    }

    run.status = TeamRunStatus::Completed;
    run.completed_at = Some(now_ms());
    let payload = run.payload();
    info!(
        team_id = %payload.team_id,
        team_run_id = %payload.team_run_id,
        target_slot_id = %payload.target_slot_id,
        target_role = ?payload.target_role,
        active_child_count = payload.active_child_count,
        pending_wake_count = payload.pending_wake_count,
        starting_child_count = payload.starting_child_count,
        "team_run completed"
    );
    emitter.broadcast_team_run(TEAM_RUN_COMPLETED_EVENT, payload.clone());
    Some(payload)
}

fn maybe_cancelled_locked(run: &mut TeamRunRecord, emitter: &TeamEventEmitter) -> Option<TeamRunPayload> {
    if !matches!(run.status, TeamRunStatus::Cancelling) {
        return None;
    }
    if run.pending_wake_count() > 0
        || !run.starting_reservations.is_empty()
        || !run.active_child_turns.is_empty()
        || run.active_operation_lease_count() > 0
        || run.has_retained_wake_gate_work()
    {
        emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, run.payload());
        return None;
    }

    run.status = TeamRunStatus::Cancelled;
    run.cancelled_at = Some(now_ms());
    let payload = run.payload();
    info!(
        team_id = %payload.team_id,
        team_run_id = %payload.team_run_id,
        target_slot_id = %payload.target_slot_id,
        target_role = ?payload.target_role,
        active_child_count = payload.active_child_count,
        pending_wake_count = payload.pending_wake_count,
        starting_child_count = payload.starting_child_count,
        "team_run cancelled"
    );
    emitter.broadcast_team_run(TEAM_RUN_CANCELLED_EVENT, payload.clone());
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::TeamWakeSource;
    use aionui_api_types::WebSocketMessage;
    use aionui_realtime::EventBroadcaster;

    #[derive(Default)]
    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }

        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.name.clone())
                .collect()
        }

        fn run_payloads(&self) -> Vec<TeamRunPayload> {
            self.events()
                .into_iter()
                .filter(|event| event.name.starts_with("team.run"))
                .map(|event| serde_json::from_value(event.data).unwrap())
                .collect()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn manager() -> (TeamRunManager, Arc<RecordingBroadcaster>) {
        let bc = Arc::new(RecordingBroadcaster::default());
        let emitter = Arc::new(TeamEventEmitter::new("team-1".into(), bc.clone()));
        (TeamRunManager::new("team-1".into(), emitter), bc)
    }

    fn slot_work<'a>(payload: &'a TeamRunPayload, slot_id: &str) -> &'a aionui_api_types::TeamSlotWorkPayload {
        payload
            .slot_work
            .iter()
            .find(|work| work.slot_id == slot_id)
            .expect("slot work must exist")
    }

    #[tokio::test]
    async fn user_message_run_payload_has_user_source() {
        let (manager, _bc) = manager();
        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user message should create run");

        assert_eq!(ack.source, TeamRunSource::UserMessage);
        assert!(ack.has_user_intervention);

        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-1".into()))
            .await
            .expect("commit user wake");

        let payload = manager.current_payload().await.expect("active payload");
        assert_eq!(payload.source, TeamRunSource::UserMessage);
        assert!(payload.has_user_intervention);
    }

    #[tokio::test]
    async fn active_recovery_run_records_user_intervention_without_changing_source() {
        let (manager, _bc) = manager();
        let result = manager
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: "lead".into(),
                role: TeamRunTargetRole::Lead,
                unread_count: 2,
            }])
            .await
            .expect("recovery scan should succeed");

        assert_eq!(result.source, TeamRunSource::RecoveryDrain);
        assert_eq!(
            manager.current_payload().await.unwrap().source,
            TeamRunSource::RecoveryDrain
        );

        let (ack, lease) = manager
            .acquire_user_message_wake("worker", TeamRunTargetRole::Teammate)
            .await
            .expect("user intervention should join recovery run");
        assert_eq!(ack.team_run_id, result.team_run_id);
        assert_eq!(ack.source, TeamRunSource::RecoveryDrain);
        assert!(ack.has_user_intervention);
        assert!(!lease.accepted_as_new_run);

        let payload = manager.current_payload().await.expect("active payload");
        assert_eq!(payload.source, TeamRunSource::RecoveryDrain);
        assert!(payload.has_user_intervention);
    }

    #[tokio::test]
    async fn recovery_drain_creates_run_with_pending_wakes() {
        let (manager, _bc) = manager();
        let result = manager
            .recover_mailbox_backlog(vec![
                RecoveryWakeCandidate {
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    unread_count: 2,
                },
                RecoveryWakeCandidate {
                    slot_id: "worker".into(),
                    role: TeamRunTargetRole::Teammate,
                    unread_count: 1,
                },
            ])
            .await
            .expect("recovery should create run");

        assert_eq!(result.source, TeamRunSource::RecoveryDrain);
        assert_eq!(result.recorded_wakes.len(), 2);
        assert_eq!(result.pending_wake_count, 2);

        let lead = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .expect("lead recovery wake should be claimable");
        assert_eq!(lead.team_run_id, result.team_run_id);
        assert_eq!(lead.wake_source, TeamWakeSource::RecoveryDrain);
        assert!(lead.message_id.is_none());
    }

    #[tokio::test]
    async fn recovery_backlog_attaches_to_active_user_run() {
        let (manager, _bc) = manager();
        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user run");
        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-user".into()))
            .await
            .expect("commit user wake");

        let result = manager
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: "worker".into(),
                role: TeamRunTargetRole::Teammate,
                unread_count: 3,
            }])
            .await
            .expect("recovery should attach to active run");

        assert_eq!(result.team_run_id, ack.team_run_id);
        assert_eq!(result.source, TeamRunSource::UserMessage);
        assert_eq!(result.recorded_wakes, vec!["worker".to_string()]);

        let worker = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .expect("attached recovery wake should be claimable");
        assert_eq!(worker.wake_source, TeamWakeSource::RecoveryDrain);
        assert!(worker.message_id.is_none());
    }

    #[tokio::test]
    async fn user_intervention_wake_prioritizes_over_recovery_backlog() {
        let (manager, _bc) = manager();
        let result = manager
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: "lead".into(),
                role: TeamRunTargetRole::Lead,
                unread_count: 2,
            }])
            .await
            .expect("recovery should create run");

        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user intervention should join recovery run");
        assert_eq!(ack.team_run_id, result.team_run_id);
        assert_eq!(ack.source, TeamRunSource::RecoveryDrain);
        assert!(ack.has_user_intervention);
        assert!(!lease.accepted_as_new_run);

        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-user".into()))
            .await
            .expect("commit user intervention wake");

        let foreground = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead-user")
            .await
            .expect("foreground user wake should be claimed first");
        assert_eq!(foreground.team_run_id, result.team_run_id);
        assert_eq!(foreground.wake_source, TeamWakeSource::UserIntervention);
        assert_eq!(foreground.message_id.as_deref(), Some("mailbox-user"));

        let recovery = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead-recovery")
            .await
            .expect("recovery backlog should remain after foreground wake");
        assert_eq!(recovery.team_run_id, result.team_run_id);
        assert_eq!(recovery.wake_source, TeamWakeSource::RecoveryDrain);
        assert!(recovery.message_id.is_none());
    }

    #[tokio::test]
    async fn recovery_backlog_does_not_duplicate_pending_slot_work() {
        let (manager, _bc) = manager();
        let (_ack, lease) = manager
            .acquire_user_message_wake("worker", TeamRunTargetRole::Teammate)
            .await
            .expect("user run");
        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-worker".into()))
            .await
            .expect("commit pending user wake");

        let result = manager
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: "worker".into(),
                role: TeamRunTargetRole::Teammate,
                unread_count: 3,
            }])
            .await;

        assert!(result.is_none(), "pending slot work already owns the unread backlog");

        let user_wake = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .expect("original pending user wake should remain");
        assert_eq!(user_wake.wake_source, TeamWakeSource::UserMessage);
        assert_eq!(user_wake.message_id.as_deref(), Some("mailbox-worker"));

        assert!(
            manager
                .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker-dup")
                .await
                .is_none(),
            "recovery scan must not append a duplicate wake for represented work"
        );
    }

    #[tokio::test]
    async fn recovery_backlog_does_not_duplicate_paused_gate_work() {
        let (manager, _bc) = manager();
        let (_ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user run");
        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-lead".into()))
            .await
            .expect("commit pending user wake");

        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .expect("claim lead wake");
        let child = ActiveChildTurn {
            team_run_id: reservation.team_run_id.clone(),
            slot_id: "lead".into(),
            role: TeamRunTargetRole::Lead,
            conversation_id: "conv-lead".into(),
            turn_id: "turn-lead".into(),
            started_at_ms: now_ms(),
            last_slow_notified_at_ms: None,
        };
        assert_eq!(
            manager
                .record_child_started(&reservation.reservation_id, child.clone())
                .await,
            ChildStartDecision::Accepted
        );
        manager
            .complete_pause_after_child_cancelled(&child, Some("test_pause".into()))
            .await
            .expect("pause slot");

        let result = manager
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: "lead".into(),
                role: TeamRunTargetRole::Lead,
                unread_count: 3,
            }])
            .await;

        assert!(result.is_none(), "paused wake gate already represents retained work");
        assert!(
            manager
                .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead-dup")
                .await
                .is_none(),
            "recovery scan must not append a duplicate wake for paused gate work"
        );
    }

    #[tokio::test]
    async fn empty_recovery_backlog_does_not_create_run() {
        let (manager, _bc) = manager();
        let result = manager.recover_mailbox_backlog(vec![]).await;
        assert!(result.is_none());
        assert!(manager.active_run_id().await.is_none());
    }

    #[tokio::test]
    async fn lease_keeps_run_active_until_commit() {
        let (manager, _bc) = manager();
        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user message should create run and lease");

        assert_eq!(lease.team_run_id, ack.team_run_id);
        assert_eq!(lease.slot_id, "lead");
        assert_eq!(lease.wake_source, TeamWakeSource::UserMessage);
        assert!(lease.accepted_as_new_run);

        let completed = manager.maybe_complete().await;
        assert!(completed.is_none(), "active lease must retain the run");
        assert_eq!(manager.active_run_id().await.as_deref(), Some(ack.team_run_id.as_str()));

        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-1".into()))
            .await
            .expect("commit should convert lease to pending wake");

        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .expect("committed wake should be claimable");
        assert_eq!(reservation.message_id.as_deref(), Some("mailbox-1"));
    }

    #[tokio::test]
    async fn abort_lease_releases_completion_hold() {
        let (manager, _bc) = manager();
        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("user message should create run and lease");

        manager
            .abort_operation_lease(&lease.lease_id, "mailbox_write_failed")
            .await
            .expect("abort should remove lease");

        let completed = manager
            .maybe_complete()
            .await
            .expect("run should complete after aborted only lease");
        assert_eq!(completed.team_run_id, ack.team_run_id);
        assert_eq!(completed.status, TeamRunStatus::Completed);
    }

    #[tokio::test]
    async fn commit_missing_lease_returns_internal_consistency_error() {
        let (manager, _bc) = manager();
        manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("create active run");

        let err = manager
            .commit_operation_lease("missing-lease", Some("mailbox-1".into()))
            .await
            .expect_err("missing lease is a contract violation");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "team run operation lease missing: missing-lease"
        ));
    }

    #[tokio::test]
    async fn run_scoped_wake_without_active_run_is_rejected() {
        let (manager, _bc) = manager();

        let err = manager
            .acquire_run_scoped_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .expect_err("run-scoped wake must need active run");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "no active team run for run-scoped wake"
        ));
    }

    #[tokio::test]
    async fn user_message_busy_active_slot_is_rejected_without_lease() {
        let (manager, _bc) = manager();
        let (ack, lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();
        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-1".into()))
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();
        manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv-lead".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        let err = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect_err("foreground user message must reject busy active slot");

        assert!(matches!(err, TeamError::SlotBusy(slot) if slot == "lead"));
        let payload = manager.current_payload().await.unwrap();
        assert_eq!(payload.pending_wake_count, 0);
    }

    #[tokio::test]
    async fn user_message_pending_slot_is_accepted_as_additional_foreground_wake() {
        let (manager, _bc) = manager();
        let (_ack, first) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();
        manager
            .commit_operation_lease(&first.lease_id, Some("mailbox-1".into()))
            .await
            .unwrap();

        let (_ack, second) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .expect("pending foreground message must be accepted");
        manager
            .commit_operation_lease(&second.lease_id, Some("mailbox-2".into()))
            .await
            .unwrap();

        let payload = manager.current_payload().await.unwrap();
        assert_eq!(payload.pending_wake_count, 2);
    }

    #[tokio::test]
    async fn mcp_send_message_busy_slot_is_accepted_and_queued() {
        let (manager, _bc) = manager();
        let (ack, first) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();
        manager
            .commit_operation_lease(&first.lease_id, Some("mailbox-1".into()))
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();
        manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv-lead".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        let outcome = manager
            .acquire_run_scoped_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .expect("MCP message to busy slot must be accepted");
        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            panic!("MCP message should not be suppressed");
        };
        manager
            .commit_operation_lease(&lease.lease_id, Some("mailbox-2".into()))
            .await
            .unwrap();

        let payload = manager.current_payload().await.unwrap();
        assert_eq!(payload.pending_wake_count, 1);
    }

    #[tokio::test]
    async fn paused_slot_suppresses_mcp_send_message_without_lease() {
        let (manager, _bc) = manager();
        let (ack, _lease) = manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();
        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        let outcome = manager
            .acquire_run_scoped_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .expect("paused background wake suppresses cleanly");
        assert_eq!(outcome, TeamRunWakeAcquireOutcome::Suppressed);
        assert_eq!(manager.active_run_id().await.as_deref(), Some(ack.team_run_id.as_str()));
    }

    #[tokio::test]
    async fn duplicate_spawn_welcome_is_suppressed() {
        let (manager, _bc) = manager();
        manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();

        let first = manager
            .acquire_run_scoped_wake("new-worker", TeamRunTargetRole::Teammate, TeamWakeSource::SpawnWelcome)
            .await
            .unwrap();
        let TeamRunWakeAcquireOutcome::Accepted(first) = first else {
            panic!("first spawn welcome should be accepted");
        };
        manager
            .commit_operation_lease(&first.lease_id, Some("welcome-1".into()))
            .await
            .unwrap();

        let second = manager
            .acquire_run_scoped_wake("new-worker", TeamRunTargetRole::Teammate, TeamWakeSource::SpawnWelcome)
            .await
            .unwrap();
        assert_eq!(second, TeamRunWakeAcquireOutcome::Suppressed);
    }

    #[tokio::test]
    async fn idle_notification_idle_slot_is_suppressed_without_scheduler_wake_target() {
        let (manager, _bc) = manager();
        manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();

        let outcome = manager
            .acquire_run_scoped_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::IdleNotification)
            .await
            .expect("generic idle notification should suppress cleanly");

        assert_eq!(outcome, TeamRunWakeAcquireOutcome::Suppressed);
    }

    #[tokio::test]
    async fn scheduler_idle_notification_idle_slot_is_accepted() {
        let (manager, _bc) = manager();
        manager
            .acquire_user_message_wake("lead", TeamRunTargetRole::Lead)
            .await
            .unwrap();

        let outcome = manager
            .acquire_scheduler_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::IdleNotification)
            .await
            .expect("scheduler-produced wake target should be accepted");
        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            panic!("scheduler wake target should produce a lease");
        };

        manager
            .commit_operation_lease(&lease.lease_id, None)
            .await
            .expect("scheduler wake should commit");
        let reservation = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .expect("scheduler wake should be claimable");
        assert_eq!(reservation.wake_source, TeamWakeSource::IdleNotification);
    }

    #[tokio::test]
    async fn active_slot_work_includes_backend_slow_fields_after_threshold() {
        let (manager, _rx) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, true, Some("msg-1".into()))
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();

        let child = ActiveChildTurn {
            team_run_id: ack.team_run_id.clone(),
            slot_id: "lead".into(),
            role: TeamRunTargetRole::Lead,
            conversation_id: "conv-lead".into(),
            turn_id: "turn-lead".into(),
            started_at_ms: 10_000,
            last_slow_notified_at_ms: None,
        };
        manager.record_child_started(&reservation.reservation_id, child).await;

        manager.observe_slow_child_turns(610_001).await;
        let payload = manager.current_payload().await.unwrap();
        let lead = slot_work(&payload, "lead");

        assert_eq!(lead.active_turn_started_at_ms, Some(10_000));
        assert_eq!(lead.active_turn_elapsed_ms, Some(600_001));
        assert_eq!(lead.active_turn_slow, Some(true));
        assert_eq!(lead.active_turn_slow_threshold_ms, Some(600_000));
        assert_eq!(lead.runtime_health, None);
    }

    #[tokio::test]
    async fn slow_observation_is_rate_limited_per_active_child() {
        let (manager, _rx) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, true, Some("msg-1".into()))
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();

        let child = ActiveChildTurn {
            team_run_id: ack.team_run_id.clone(),
            slot_id: "lead".into(),
            role: TeamRunTargetRole::Lead,
            conversation_id: "conv-lead".into(),
            turn_id: "turn-lead".into(),
            started_at_ms: 0,
            last_slow_notified_at_ms: None,
        };
        manager.record_child_started(&reservation.reservation_id, child).await;

        assert!(manager.observe_slow_child_turns(600_001).await.is_some());
        assert!(manager.observe_slow_child_turns(900_000).await.is_none());
        assert!(manager.observe_slow_child_turns(1_200_001).await.is_some());
    }

    #[tokio::test]
    async fn cancel_run_clears_paused_gate_and_reaches_cancelled_terminal() {
        let (manager, bc) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();
        manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::InterruptedNotification,
                None,
            )
            .await
            .unwrap();

        manager.begin_cancel(None, Some("stop all".into())).await.unwrap();
        let cancelled = manager
            .try_complete_cancelled()
            .await
            .expect("cancel should clear retained gate work");

        assert_eq!(cancelled.status, TeamRunStatus::Cancelled);
        assert_eq!(cancelled.pending_wake_count, 0);
        assert_eq!(cancelled.slot_work.len(), 0);
        assert_eq!(manager.active_run_id().await, None);
        assert!(bc.names().contains(&TEAM_RUN_CANCELLED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn run_payload_reports_pending_starting_and_active_work_by_slot() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();

        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        manager
            .record_pending_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();
        let worker_reservation = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .unwrap();
        let lead_reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();
        manager
            .record_child_started(
                &lead_reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv-lead".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        let latest = bc.run_payloads().last().cloned().expect("run payload");
        let lead = slot_work(&latest, "lead");
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.starting_child_count, 0);
        assert_eq!(lead.active_turn_id.as_deref(), Some("turn-lead"));

        let worker = slot_work(&latest, "worker");
        assert_eq!(worker.pending_wake_count, 0);
        assert_eq!(worker.starting_child_count, 1);
        assert_eq!(worker.active_turn_id, None);
        assert_eq!(worker_reservation.slot_id, "worker");
    }

    #[tokio::test]
    async fn pause_active_child_prepares_cancel_without_mutating_gate_or_active_child() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();
        manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv-lead".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();

        let outcome = manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        assert!(matches!(outcome.cancel_target, Some(ChildCancelTarget::Active(_))));
        let latest = bc.run_payloads().last().cloned().expect("run payload");
        let lead = slot_work(&latest, "lead");
        assert!(!lead.paused);
        assert_eq!(lead.pending_wake_count, 1);
        assert_eq!(lead.suppressed_wake_count, 0);
        assert_eq!(lead.active_turn_id.as_deref(), Some("turn-lead"));
    }

    #[tokio::test]
    async fn complete_pause_after_active_child_cancel_marks_paused_moves_pending_and_removes_child() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();
        let child = ActiveChildTurn {
            team_run_id: ack.team_run_id,
            slot_id: "lead".into(),
            role: TeamRunTargetRole::Lead,
            conversation_id: "conv-lead".into(),
            turn_id: "turn-lead".into(),
            started_at_ms: now_ms(),
            last_slow_notified_at_ms: None,
        };
        manager
            .record_child_started(&reservation.reservation_id, child.clone())
            .await;
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();
        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        let payload = manager
            .complete_pause_after_child_cancelled(&child, Some("user stopped".into()))
            .await
            .expect("pause completion payload");

        let lead = slot_work(&payload, "lead");
        assert!(lead.paused);
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.suppressed_wake_count, 1);
        assert_eq!(lead.active_turn_id, None);
        assert!(bc.names().contains(&TEAM_CHILD_TURN_CANCELLED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn pause_slot_moves_pending_wakes_to_suppressed_slot_work() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::UserMessage,
                Some("msg-1".into()),
            )
            .await
            .unwrap();

        let outcome = manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        assert_eq!(outcome.team_run_id, ack.team_run_id);
        let latest = bc.run_payloads().pop().expect("run update");
        let lead = slot_work(&latest, "lead");
        assert!(lead.paused);
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.suppressed_wake_count, 1);
    }

    #[tokio::test]
    async fn paused_slot_suppresses_background_wake_without_pending_count() {
        let (manager, bc) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        let decision = manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::InterruptedNotification,
                None,
            )
            .await
            .unwrap();

        assert_eq!(decision, WakeRecordDecision::Suppressed);
        let latest = bc.run_payloads().pop().expect("run update");
        let lead = slot_work(&latest, "lead");
        assert!(lead.paused);
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.suppressed_wake_count, 1);
    }

    #[tokio::test]
    async fn user_intervention_resumes_paused_slot_and_records_pending_wake() {
        let (manager, bc) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();

        let decision = manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::UserIntervention,
                Some("msg-user".into()),
            )
            .await
            .unwrap();

        assert_eq!(decision, WakeRecordDecision::Recorded);
        let latest = bc.run_payloads().pop().expect("run update");
        let lead = slot_work(&latest, "lead");
        assert!(!lead.paused);
        assert_eq!(lead.pending_wake_count, 1);
        assert_eq!(lead.suppressed_wake_count, 0);
    }

    #[tokio::test]
    async fn resumed_user_message_releases_suppressed_wake_and_allows_completion() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();

        manager
            .pause_slot_work("lead", Some("user stopped".into()))
            .await
            .unwrap();
        manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::IdleNotification,
                Some("idle-msg".into()),
            )
            .await
            .unwrap();

        manager
            .record_or_suppress_wake(
                "lead",
                TeamRunTargetRole::Lead,
                TeamWakeSource::UserMessage,
                Some("user-msg".into()),
            )
            .await
            .unwrap();

        let user_reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .expect("foreground user message wake should run first");
        assert_eq!(
            manager
                .record_child_started(
                    &user_reservation.reservation_id,
                    ActiveChildTurn {
                        team_run_id: ack.team_run_id.clone(),
                        slot_id: "lead".into(),
                        role: TeamRunTargetRole::Lead,
                        conversation_id: "conv-lead".into(),
                        turn_id: "turn-user".into(),
                        started_at_ms: now_ms(),
                        last_slow_notified_at_ms: None,
                    },
                )
                .await,
            ChildStartDecision::Accepted
        );
        assert!(
            manager
                .record_child_completed("lead", "turn-user", TeamRunStatus::Completed)
                .await
                .is_none(),
            "suppressed background wake should retain the run until released"
        );

        let source = manager
            .release_suppressed_wake_if_resumed("lead", TeamRunTargetRole::Lead)
            .await;
        assert_eq!(source, Some(TeamWakeSource::IdleNotification));

        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .expect("released suppressed wake should become pending wake");
        assert_eq!(
            manager
                .record_child_started(
                    &reservation.reservation_id,
                    ActiveChildTurn {
                        team_run_id: ack.team_run_id.clone(),
                        slot_id: "lead".into(),
                        role: TeamRunTargetRole::Lead,
                        conversation_id: "conv-lead".into(),
                        turn_id: "turn-background".into(),
                        started_at_ms: now_ms(),
                        last_slow_notified_at_ms: None,
                    },
                )
                .await,
            ChildStartDecision::Accepted
        );
        let completed = manager
            .record_child_completed("lead", "turn-background", TeamRunStatus::Completed)
            .await
            .expect("run should complete after released background wake is consumed");

        assert_eq!(completed.status, TeamRunStatus::Completed);
        assert_eq!(completed.pending_wake_count, 0);
        assert_eq!(completed.starting_child_count, 0);
        assert_eq!(completed.active_child_count, 0);
        assert_eq!(
            completed.slot_work.len(),
            0,
            "gate state should not leave empty slot work"
        );
        assert!(bc.names().contains(&TEAM_RUN_COMPLETED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn active_intervention_ack_uses_accepted_slot_without_changing_initial_target() {
        let (manager, _) = manager();
        let first = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();

        let second = manager
            .accept_user_message("worker", TeamRunTargetRole::Teammate, true, Some("msg-2".into()))
            .await
            .unwrap();

        assert_eq!(second.team_run_id, first.team_run_id);
        assert_eq!(second.target_slot_id, "lead");
        assert_eq!(second.target_role, TeamRunTargetRole::Lead);
        assert_eq!(second.accepted_slot_id, "worker");
        assert_eq!(second.accepted_role, TeamRunTargetRole::Teammate);
        assert_eq!(second.message_id.as_deref(), Some("msg-2"));
    }

    #[tokio::test]
    async fn active_intervention_rejects_when_same_slot_has_pending_work() {
        let (manager, _) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();

        let err = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, true, None)
            .await
            .expect_err("same slot pending work must be busy");

        assert!(matches!(err, TeamError::SlotBusy(slot_id) if slot_id == "lead"));
    }

    #[tokio::test]
    async fn active_intervention_rejects_when_same_slot_has_starting_or_active_work() {
        let (manager, _) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
            .await
            .unwrap();

        let starting_err = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, true, None)
            .await
            .expect_err("same slot starting work must be busy");
        assert!(matches!(starting_err, TeamError::SlotBusy(slot_id) if slot_id == "lead"));

        manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv-lead".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        let active_err = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, true, None)
            .await
            .expect_err("same slot active work must be busy");
        assert!(matches!(active_err, TeamError::SlotBusy(slot_id) if slot_id == "lead"));
    }

    #[tokio::test]
    async fn record_pending_wake_requires_active_run() {
        let (manager, _) = manager();

        let err = manager
            .record_pending_wake("worker-1", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .expect_err("run-scoped wake without active run must fail");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "no active team run for run-scoped wake"
        ));
    }

    #[tokio::test]
    async fn record_pending_wake_increments_active_run_count() {
        let (manager, _) = manager();
        manager
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        manager
            .record_pending_wake("worker-1", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .expect("record pending wake");

        let reservation = manager
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .expect("pending wake should be claimable");

        assert_eq!(reservation.slot_id, "worker-1");
        assert_eq!(reservation.role, TeamRunTargetRole::Teammate);
    }

    #[tokio::test]
    async fn empty_wake_for_one_slot_does_not_consume_another_slot_pending_wake() {
        let (manager, _) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        manager
            .record_pending_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .expect("record worker pending wake");

        let completed = manager.record_empty_wake_observed("lead").await;
        assert!(
            completed.is_none(),
            "leader empty wake must not complete while worker has pending wake"
        );
        assert_eq!(manager.active_run_id().await.as_deref(), Some(ack.team_run_id.as_str()));

        let reservation = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .expect("worker pending wake must remain claimable");

        assert_eq!(reservation.slot_id, "worker");
        assert_eq!(reservation.role, TeamRunTargetRole::Teammate);
    }

    #[tokio::test]
    async fn empty_wake_consumes_only_same_slot_pending_wake() {
        let (manager, _) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .expect("record lead pending wake");
        manager
            .record_pending_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .expect("record worker pending wake");

        assert!(
            manager.record_empty_wake_observed("lead").await.is_none(),
            "worker wake should keep run active"
        );

        assert!(
            manager
                .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv-lead")
                .await
                .is_none(),
            "lead pending wake was consumed by lead empty wake"
        );

        assert!(
            manager
                .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
                .await
                .is_some(),
            "worker pending wake must remain"
        );
    }

    #[tokio::test]
    async fn leader_message_rejects_when_run_is_active() {
        let (manager, _) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();

        let err = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap_err();

        assert!(matches!(err, TeamError::InvalidRequest(message) if message.contains("already active")));
    }

    #[tokio::test]
    async fn teammate_intervention_reuses_active_run() {
        let (manager, _) = manager();
        let first = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();

        let second = manager
            .accept_user_message("worker", TeamRunTargetRole::Teammate, true, Some("msg-1".into()))
            .await
            .unwrap();

        assert_eq!(second.team_run_id, first.team_run_id);
        assert_eq!(second.accepted_slot_id, "worker");
        assert_eq!(second.accepted_role, TeamRunTargetRole::Teammate);
        assert_eq!(second.message_id.as_deref(), Some("msg-1"));
    }

    #[tokio::test]
    async fn child_start_and_completion_emit_lifecycle_events() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .unwrap();

        manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv".into(),
                    turn_id: "turn".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;
        manager
            .record_child_completed("lead", "turn", TeamRunStatus::Completed)
            .await;

        assert_eq!(manager.active_run_id().await, None);
        let names = bc.names();
        assert!(names.contains(&TEAM_RUN_ACCEPTED_EVENT.to_owned()));
        assert!(names.contains(&TEAM_CHILD_TURN_STARTED_EVENT.to_owned()));
        assert!(names.contains(&TEAM_RUN_COMPLETED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn claimed_wake_prevents_completion_until_child_finishes() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();

        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .expect("reservation should be claimed");

        assert_eq!(reservation.team_run_id, ack.team_run_id);
        assert_eq!(manager.maybe_complete().await, None);
        assert_eq!(manager.active_run_id().await.as_deref(), Some(ack.team_run_id.as_str()));

        let payloads = bc.run_payloads();
        assert!(payloads.iter().any(|payload| payload.starting_child_count == 1));
    }

    #[tokio::test]
    async fn child_start_promotes_matching_reservation_to_active() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .unwrap();

        let decision = manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv".into(),
                    turn_id: "turn".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        assert_eq!(decision, ChildStartDecision::Accepted);
        let payloads = bc.run_payloads();
        assert!(payloads.iter().any(|payload| {
            payload.status == TeamRunStatus::Running
                && payload.starting_child_count == 0
                && payload.active_child_count == 1
        }));
    }

    #[tokio::test]
    async fn child_completion_completes_only_when_pending_starting_and_active_are_empty() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .unwrap();
        assert_eq!(
            manager
                .record_child_started(
                    &reservation.reservation_id,
                    ActiveChildTurn {
                        team_run_id: ack.team_run_id,
                        slot_id: "lead".into(),
                        role: TeamRunTargetRole::Lead,
                        conversation_id: "conv".into(),
                        turn_id: "turn".into(),
                        started_at_ms: now_ms(),
                        last_slow_notified_at_ms: None,
                    },
                )
                .await,
            ChildStartDecision::Accepted
        );

        let completed = manager
            .record_child_completed("lead", "turn", TeamRunStatus::Completed)
            .await
            .expect("run should complete after last child");

        assert_eq!(completed.status, TeamRunStatus::Completed);
        assert_eq!(completed.pending_wake_count, 0);
        assert_eq!(completed.starting_child_count, 0);
        assert_eq!(completed.active_child_count, 0);
        assert_eq!(manager.active_run_id().await, None);
        assert!(bc.names().contains(&TEAM_RUN_COMPLETED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn child_start_failed_releases_reservation_and_fails_run() {
        let (manager, bc) = manager();
        manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .unwrap();

        let failed = manager
            .record_child_start_failed(&reservation.reservation_id, "spawn failed")
            .await
            .expect("run should fail before child start");

        assert_eq!(failed.status, TeamRunStatus::Failed);
        assert_eq!(failed.starting_child_count, 0);
        assert_eq!(manager.active_run_id().await, None);
        assert!(bc.names().contains(&TEAM_RUN_FAILED_EVENT.to_owned()));
    }

    #[tokio::test]
    async fn cancel_marks_starting_reservation_and_late_start_requests_immediate_cancel() {
        let (manager, _) = manager();
        let ack = manager
            .accept_user_message("worker", TeamRunTargetRole::Teammate, true, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("worker", TeamRunTargetRole::Teammate, TeamWakeSource::UserIntervention)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("worker", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .unwrap();

        let target = manager.begin_cancel_child("worker").await.unwrap();
        assert!(matches!(target, ChildCancelTarget::Starting(_)));

        let decision = manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "worker".into(),
                    role: TeamRunTargetRole::Teammate,
                    conversation_id: "conv-worker".into(),
                    turn_id: "turn-worker".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        assert!(matches!(decision, ChildStartDecision::CancelImmediately(child) if child.turn_id == "turn-worker"));
    }

    #[tokio::test]
    async fn stale_child_start_does_not_revive_completed_run() {
        let (manager, _) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager
            .record_pending_wake("lead", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = manager
            .claim_wake_for_turn("lead", TeamRunTargetRole::Lead, "conv")
            .await
            .unwrap();
        manager
            .record_child_start_failed(&reservation.reservation_id, "failed")
            .await
            .unwrap();

        let decision = manager
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "conv".into(),
                    turn_id: "late-turn".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        assert_eq!(decision, ChildStartDecision::Ignored);
        assert_eq!(manager.active_run_id().await, None);
    }
}
