use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{TeamChildTurnPayload, TeamRunAckResponse, TeamRunPayload, TeamRunStatus, TeamRunTargetRole};
use aionui_common::{TimestampMs, generate_id, now_ms};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::TeamError;
use crate::events::{
    TEAM_CHILD_TURN_CANCELLED_EVENT, TEAM_CHILD_TURN_COMPLETED_EVENT, TEAM_CHILD_TURN_STARTED_EVENT,
    TEAM_RUN_ACCEPTED_EVENT, TEAM_RUN_CANCELLED_EVENT, TEAM_RUN_COMPLETED_EVENT, TEAM_RUN_FAILED_EVENT,
    TEAM_RUN_STARTED_EVENT, TEAM_RUN_UPDATED_EVENT, TeamEventEmitter,
};
use crate::types::TeammateRole;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveChildTurn {
    pub team_run_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
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
    pending_wake_count: usize,
}

impl TeamRunRecord {
    fn payload(&self) -> TeamRunPayload {
        TeamRunPayload {
            team_id: self.team_id.clone(),
            team_run_id: self.team_run_id.clone(),
            target_slot_id: self.target_slot_id.clone(),
            target_role: self.target_role.clone(),
            status: self.status.clone(),
            active_child_count: self.active_child_turns.len(),
            pending_wake_count: self.pending_wake_count,
        }
    }

    fn ack(&self, message_id: Option<String>) -> TeamRunAckResponse {
        TeamRunAckResponse {
            team_run_id: self.team_run_id.clone(),
            team_id: self.team_id.clone(),
            target_slot_id: self.target_slot_id.clone(),
            target_role: self.target_role.clone(),
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
                debug!(
                    team_id = %self.team_id,
                    team_run_id = %active.team_run_id,
                    target_slot_id = %target_slot_id,
                    target_role = ?target_role,
                    active_target_slot_id = %active.target_slot_id,
                    active_target_role = ?active.target_role,
                    "team_run active intervention accepted"
                );
                return Ok(active.ack(message_id));
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
            target_role,
            status: TeamRunStatus::Accepted,
            started_at: None,
            completed_at: None,
            cancelled_at: None,
            cancel_reason: None,
            active_child_turns: HashMap::new(),
            pending_wake_count: 0,
        };
        let ack = record.ack(message_id);
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

    pub async fn record_pending_wake(&self) {
        let mut guard = self.state.lock().await;
        if let Some(run) = guard.as_mut().filter(|r| r.is_active()) {
            run.pending_wake_count = run.pending_wake_count.saturating_add(1);
            debug!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                pending_wake_count = run.pending_wake_count,
                active_child_count = run.active_child_turns.len(),
                "team_run pending wake recorded"
            );
            self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, run.payload());
        }
    }

    pub async fn record_wake_consumed(&self) {
        let mut guard = self.state.lock().await;
        if let Some(run) = guard.as_mut() {
            run.pending_wake_count = run.pending_wake_count.saturating_sub(1);
            debug!(
                team_id = %self.team_id,
                team_run_id = %run.team_run_id,
                pending_wake_count = run.pending_wake_count,
                active_child_count = run.active_child_turns.len(),
                "team_run wake consumed"
            );
            self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, run.payload());
        }
    }

    pub async fn record_child_started(&self, child: ActiveChildTurn) {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut() else {
            warn!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                slot_id = %child.slot_id,
                turn_id = %child.turn_id,
                "team_run child start ignored because no run is active"
            );
            return;
        };
        if run.team_run_id != child.team_run_id || matches!(run.status, TeamRunStatus::Cancelling) {
            warn!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                slot_id = %child.slot_id,
                turn_id = %child.turn_id,
                "team_run child start ignored for stale or cancelling run"
            );
            return;
        }

        let first_child_for_run = run.started_at.is_none();
        run.status = TeamRunStatus::Running;
        if first_child_for_run {
            run.started_at = Some(now_ms());
        }
        run.pending_wake_count = run.pending_wake_count.saturating_sub(1);
        run.active_child_turns.insert(child.slot_id.clone(), child.clone());
        let run_payload = run.payload();
        let child_payload = child_payload(&run.team_id, &child, TeamRunStatus::Running);
        drop(guard);

        if first_child_for_run {
            info!(
                team_id = %self.team_id,
                team_run_id = %child.team_run_id,
                target_slot_id = %run_payload.target_slot_id,
                target_role = ?run_payload.target_role,
                active_child_count = run_payload.active_child_count,
                pending_wake_count = run_payload.pending_wake_count,
                "team_run started"
            );
        }
        debug!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            "team_child_turn started"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_STARTED_EVENT, run_payload);
        self.emitter
            .broadcast_child_turn(TEAM_CHILD_TURN_STARTED_EVENT, child_payload);
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
                    "team_run failed"
                );
                self.emitter.broadcast_team_run(TEAM_RUN_FAILED_EVENT, payload.clone());
                Some(payload)
            }
            _ => {
                debug!(
                    team_id = %self.team_id,
                    team_run_id = %run.team_run_id,
                    slot_id = %child.slot_id,
                    role = ?child.role,
                    conversation_id = %child.conversation_id,
                    turn_id = %child.turn_id,
                    status = ?status,
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
        {
            return Err(TeamError::AgentNotFound(target.to_owned()));
        }
        run.status = TeamRunStatus::Cancelling;
        run.cancel_reason = reason;
        let payload = run.payload();
        info!(
            team_id = %self.team_id,
            team_run_id = %run.team_run_id,
            target_slot_id = ?target_slot_id.as_deref(),
            active_child_count = run.active_child_turns.len(),
            pending_wake_count = run.pending_wake_count,
            "team_run cancel requested"
        );
        drop(guard);

        self.emitter.broadcast_team_run(TEAM_RUN_UPDATED_EVENT, payload);
        Ok(())
    }

    pub async fn complete_cancelled(&self) -> Option<String> {
        let mut guard = self.state.lock().await;
        let run = guard.as_mut()?;
        run.status = TeamRunStatus::Cancelled;
        run.cancelled_at = Some(now_ms());
        let cancelled_child_count = run.active_child_turns.len();
        run.active_child_turns.clear();
        let team_run_id = run.team_run_id.clone();
        let payload = run.payload();
        *guard = None;
        drop(guard);

        info!(
            team_id = %self.team_id,
            team_run_id = %team_run_id,
            cancelled_child_count,
            pending_wake_count = payload.pending_wake_count,
            "team_run cancelled"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_CANCELLED_EVENT, payload);
        Some(team_run_id)
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
            "team_run failed"
        );
        self.emitter.broadcast_team_run(TEAM_RUN_FAILED_EVENT, payload);
        Some(team_run_id)
    }

    pub async fn begin_cancel_child(&self, slot_id: &str) -> Result<ActiveChildTurn, TeamError> {
        let guard = self.state.lock().await;
        let Some(run) = guard.as_ref().filter(|r| r.is_active()) else {
            return Err(TeamError::InvalidRequest("no active team run".into()));
        };
        run.active_child_turns
            .get(slot_id)
            .cloned()
            .ok_or_else(|| TeamError::InvalidRequest(format!("agent {slot_id} has no active child turn")))
    }

    pub async fn record_child_cancelled(&self, child: &ActiveChildTurn) {
        let mut guard = self.state.lock().await;
        let Some(run) = guard.as_mut() else {
            return;
        };
        run.active_child_turns.remove(&child.slot_id);
        let payload = child_payload(&run.team_id, child, TeamRunStatus::Cancelled);
        drop(guard);

        debug!(
            team_id = %self.team_id,
            team_run_id = %child.team_run_id,
            slot_id = %child.slot_id,
            role = ?child.role,
            conversation_id = %child.conversation_id,
            turn_id = %child.turn_id,
            "team_child_turn cancelled"
        );
        self.emitter
            .broadcast_child_turn(TEAM_CHILD_TURN_CANCELLED_EVENT, payload);
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

fn maybe_complete_locked(run: &mut TeamRunRecord, emitter: &TeamEventEmitter) -> Option<TeamRunPayload> {
    if !run.active_child_turns.is_empty() || run.pending_wake_count > 0 {
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
        "team_run completed"
    );
    emitter.broadcast_team_run(TEAM_RUN_COMPLETED_EVENT, payload.clone());
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::WebSocketMessage;
    use aionui_realtime::EventBroadcaster;

    #[derive(Default)]
    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.name.clone())
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
        assert_eq!(second.message_id.as_deref(), Some("msg-1"));
    }

    #[tokio::test]
    async fn child_start_and_completion_emit_lifecycle_events() {
        let (manager, bc) = manager();
        let ack = manager
            .accept_user_message("lead", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        manager.record_pending_wake().await;

        manager
            .record_child_started(ActiveChildTurn {
                team_run_id: ack.team_run_id.clone(),
                slot_id: "lead".into(),
                role: TeamRunTargetRole::Lead,
                conversation_id: "conv".into(),
                turn_id: "turn".into(),
            })
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
}
