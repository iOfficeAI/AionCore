use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use aionui_api_types::{
    TeamChildTurnPayload, TeamRunAckResponse, TeamRunPayload, TeamRunStatus, TeamRunTargetRole, TeamSlotWorkPayload,
};
use aionui_common::{TimestampMs, generate_id, now_ms};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::TeamError;
use crate::events::{
    TEAM_CHILD_TURN_CANCELLED_EVENT, TEAM_CHILD_TURN_COMPLETED_EVENT, TEAM_CHILD_TURN_STARTED_EVENT,
    TEAM_RUN_ACCEPTED_EVENT, TEAM_RUN_CANCELLED_EVENT, TEAM_RUN_COMPLETED_EVENT, TEAM_RUN_FAILED_EVENT,
    TEAM_RUN_STARTED_EVENT, TEAM_RUN_UPDATED_EVENT, TeamEventEmitter,
};
use crate::slot_wake_gate::{SlotWakeGate, WakeGateDecision};
use crate::types::TeammateRole;
use crate::wake::TeamWakeSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveChildTurn {
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeRecordDecision {
    Recorded,
    Suppressed,
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

#[derive(Debug, Clone)]
struct TeamRunRecord {
    team_run_id: String,
    team_id: String,
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
    slot_wake_gate: SlotWakeGate,
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
            .chain(self.slot_wake_gate.slot_ids().cloned())
            .collect::<Vec<_>>();
        slot_ids.sort();
        slot_ids.dedup();

        slot_ids
            .into_iter()
            .filter_map(|slot_id| {
                let role = self.role_for_slot(&slot_id)?;
                let gate = self.slot_wake_gate.snapshot_for_slot(&slot_id);
                Some(TeamSlotWorkPayload {
                    pending_wake_count: self.pending_wake_count_for_slot(&slot_id),
                    starting_child_count: self.starting_child_count_for_slot(&slot_id),
                    paused: gate.paused,
                    suppressed_wake_count: gate.suppressed_wake_count,
                    active_turn_id: self.active_child_turns.get(&slot_id).map(|child| child.turn_id.clone()),
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
        if let Some(active) = guard.as_ref().filter(|r| r.is_active()) {
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

        let record = TeamRunRecord {
            team_run_id: generate_id(),
            team_id: self.team_id.clone(),
            target_slot_id: target_slot_id.to_owned(),
            target_role: target_role.clone(),
            status: TeamRunStatus::Accepted,
            started_at: None,
            completed_at: None,
            cancelled_at: None,
            cancel_reason: None,
            active_child_turns: HashMap::new(),
            starting_reservations: HashMap::new(),
            pending_wakes: HashMap::new(),
            slot_wake_gate: SlotWakeGate::default(),
        };
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
        guard.as_ref().map(TeamRunRecord::payload)
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
                let pending = PendingWake {
                    slot_id: slot_id.to_owned(),
                    role: target_role.clone(),
                    source: wake_source,
                    message_id: trigger_message_id.clone(),
                };
                run.pending_wakes
                    .entry(slot_id.to_owned())
                    .or_default()
                    .push_back(pending);
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
        run.pending_wakes
            .entry(slot_id.to_owned())
            .or_default()
            .push_back(PendingWake {
                slot_id: slot_id.to_owned(),
                role: role.clone(),
                source,
                message_id: None,
            });
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
                },
            )
            .await;

        assert_eq!(decision, ChildStartDecision::Ignored);
        assert_eq!(manager.active_run_id().await, None);
    }
}
