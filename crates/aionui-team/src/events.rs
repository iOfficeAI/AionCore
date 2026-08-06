use std::sync::Arc;

use aionui_api_types::{
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentRuntimeStatus, TeamAgentRuntimeStatusPayload,
    TeamAgentSpawnedPayload, TeamAgentStatusPayload, TeamChildTurnPayload, TeamMailboxChange,
    TeamMailboxChangedPayload, TeamMailboxMessageResponse, TeamRunPayload, TeamSlotWorkChangedPayload, TeamTaskChange,
    TeamTaskChangedPayload, TeamTaskResponse, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, info};

use crate::types::{TeamAgent, TeammateStatus};

pub const TEAMMATE_MESSAGE_EVENT: &str = "team.teammateMessage";
pub const TEAM_AGENT_STATUS_CHANGED_EVENT: &str = "team.agentStatusChanged";
pub const TEAM_AGENT_SPAWNED_EVENT: &str = "team.agentSpawned";
pub const TEAM_AGENT_REMOVED_EVENT: &str = "team.agentRemoved";
pub const TEAM_AGENT_RENAMED_EVENT: &str = "team.agentRenamed";
pub const TEAM_AGENT_RUNTIME_STATUS_CHANGED_EVENT: &str = "team.agentRuntimeStatusChanged";
pub const TEAM_LIST_CHANGED_EVENT: &str = "team.listChanged";
pub const TEAM_CREATED_EVENT: &str = "team.created";
pub const TEAM_REMOVED_EVENT: &str = "team.removed";
pub const TEAM_RENAMED_EVENT: &str = "team.renamed";
pub const TEAM_SESSION_STATUS_CHANGED_EVENT: &str = "team.sessionStatusChanged";
pub const TEAM_TASK_CHANGED_EVENT: &str = "team.taskChanged";
pub const TEAM_MAILBOX_CHANGED_EVENT: &str = "team.mailboxChanged";
pub const TEAM_SESSION_CHANGED_EVENT: &str = "team.sessionChanged";
pub const TEAM_RUN_ACCEPTED_EVENT: &str = "team.runAccepted";
pub const TEAM_RUN_STARTED_EVENT: &str = "team.runStarted";
pub const TEAM_RUN_UPDATED_EVENT: &str = "team.runUpdated";
pub const TEAM_RUN_COMPLETED_EVENT: &str = "team.runCompleted";
pub const TEAM_RUN_CANCELLED_EVENT: &str = "team.runCancelled";
pub const TEAM_RUN_FAILED_EVENT: &str = "team.runFailed";
pub const TEAM_CHILD_TURN_STARTED_EVENT: &str = "team.childTurnStarted";
pub const TEAM_CHILD_TURN_COMPLETED_EVENT: &str = "team.childTurnCompleted";
pub const TEAM_CHILD_TURN_CANCELLED_EVENT: &str = "team.childTurnCancelled";
pub const TEAM_SLOT_WORK_CHANGED_EVENT: &str = "team.slotWorkChanged";

pub struct TeamEventEmitter {
    team_id: String,
    user_id: String,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl TeamEventEmitter {
    pub fn new(team_id: String, user_id: String, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            team_id,
            user_id,
            broadcaster,
        }
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    fn scoped_payload<T: Serialize>(&self, payload: T) -> Value {
        let mut value = serde_json::to_value(payload).expect("serialize team event payload");
        value["user_id"] = Value::String(self.user_id.clone());
        value
    }

    pub fn broadcast_agent_status(&self, slot_id: &str, status: TeammateStatus) {
        let payload = TeamAgentStatusPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
            status: status.to_string(),
        };
        let event = WebSocketMessage::new(TEAM_AGENT_STATUS_CHANGED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_spawned(&self, agent: &TeamAgent) {
        let payload = TeamAgentSpawnedPayload {
            team_id: self.team_id.clone(),
            assistant: agent.to_response(),
        };
        let event = WebSocketMessage::new(TEAM_AGENT_SPAWNED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_removed(&self, slot_id: &str) {
        let payload = TeamAgentRemovedPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
        };
        let event = WebSocketMessage::new(TEAM_AGENT_REMOVED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_renamed(&self, slot_id: &str, name: &str) {
        let payload = TeamAgentRenamedPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
            name: name.to_owned(),
        };
        let event = WebSocketMessage::new(TEAM_AGENT_RENAMED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_runtime_status(
        &self,
        agent: &TeamAgent,
        status: TeamAgentRuntimeStatus,
        error: Option<String>,
    ) {
        let payload = TeamAgentRuntimeStatusPayload {
            team_id: self.team_id.clone(),
            slot_id: agent.slot_id.clone(),
            conversation_id: agent.conversation_id.clone(),
            status,
            error,
        };
        // Per-member runtime status (dormant/pending/ready/failed) drives the
        // inline column badge and send-box gate. It is a low-volume, important
        // per-member lifecycle change, so log at info for production
        // diagnosability (production runs at info). The reason is the sanitized
        // public failure text, never a raw payload.
        info!(
            team_id = %payload.team_id,
            slot_id = %payload.slot_id,
            status = ?payload.status,
            error = payload.error.as_deref().unwrap_or(""),
            "team member runtime status broadcast"
        );
        // Keep per-user scoping so the event is delivered only to the owning
        // user's WebSocket subscribers.
        let event = WebSocketMessage::new(TEAM_AGENT_RUNTIME_STATUS_CHANGED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_team_run(&self, event_name: &'static str, payload: TeamRunPayload) {
        debug!(
            event_name = event_name,
            team_id = %payload.team_id,
            team_run_id = %payload.team_run_id,
            target_slot_id = %payload.target_slot_id,
            target_role = ?payload.target_role,
            status = ?payload.status,
            queued_intent_count = payload.queued_intent_count,
            starting_batch_count = payload.starting_batch_count,
            running_batch_count = payload.running_batch_count,
            active_enqueue_lease_count = payload.active_enqueue_lease_count,
            slot_work_count = payload.slot_work.len(),
            "team websocket event emitted"
        );
        let event = WebSocketMessage::new(event_name, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_slot_work(&self, payload: TeamSlotWorkChangedPayload) {
        debug!(
            event_name = TEAM_SLOT_WORK_CHANGED_EVENT,
            team_id = %payload.team_id,
            slot_id = %payload.slot_work.slot_id,
            state = ?payload.slot_work.state,
            active_turn_id = ?payload.slot_work.active_turn_id,
            "team websocket event emitted"
        );
        let event = WebSocketMessage::new(TEAM_SLOT_WORK_CHANGED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_child_turn(&self, event_name: &'static str, payload: TeamChildTurnPayload) {
        debug!(
            event_name = event_name,
            team_id = %payload.team_id,
            team_run_id = %payload.team_run_id,
            slot_id = %payload.slot_id,
            role = ?payload.role,
            conversation_id = %payload.conversation_id,
            turn_id = %payload.turn_id,
            status = ?payload.status,
            "team websocket event emitted"
        );
        let event = WebSocketMessage::new(event_name, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    /// Broadcasts `team.taskChanged` after a task is created or updated.
    ///
    /// Only non-sensitive correlation fields (id / change / team_id) are
    /// logged; subject and description never appear in logs.
    pub fn broadcast_task_changed(&self, task: TeamTaskResponse, change: TeamTaskChange) {
        debug!(
            event_name = TEAM_TASK_CHANGED_EVENT,
            team_id = %self.team_id,
            task_id = %task.id,
            change = ?change,
            "team websocket event emitted"
        );
        let payload = TeamTaskChangedPayload {
            team_id: self.team_id.clone(),
            task,
            change,
        };
        let event = WebSocketMessage::new(TEAM_TASK_CHANGED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }

    /// Broadcasts `team.mailboxChanged` after a message is written or read.
    ///
    /// Only non-sensitive correlation fields (id / change / team_id) are
    /// logged; content and summary never appear in logs.
    pub fn broadcast_mailbox_changed(&self, message: TeamMailboxMessageResponse, change: TeamMailboxChange) {
        debug!(
            event_name = TEAM_MAILBOX_CHANGED_EVENT,
            team_id = %self.team_id,
            message_id = %message.id,
            change = ?change,
            "team websocket event emitted"
        );
        let payload = TeamMailboxChangedPayload {
            team_id: self.team_id.clone(),
            message,
            change,
        };
        let event = WebSocketMessage::new(TEAM_MAILBOX_CHANGED_EVENT, self.scoped_payload(payload));
        self.broadcaster.broadcast(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TeammateRole;
    use aionui_api_types::{
        TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentRuntimeStatusPayload, TeamAgentSpawnedPayload,
        TeamAgentStatusPayload,
    };

    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(vec![]),
            }
        }

        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_emitter() -> (TeamEventEmitter, Arc<RecordingBroadcaster>) {
        let bc = Arc::new(RecordingBroadcaster::new());
        let emitter = TeamEventEmitter::new("team-1".into(), "user-1".into(), bc.clone());
        (emitter, bc)
    }

    #[test]
    fn status_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_agent_status("slot-1", TeammateStatus::Working);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentStatusChanged");
        assert_eq!(events[0].data["user_id"], "user-1");

        let payload: TeamAgentStatusPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.slot_id, "slot-1");
        assert_eq!(payload.status, "working");
    }

    #[test]
    fn spawned_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        let agent = TeamAgent {
            slot_id: "slot-2".into(),
            name: "Worker".into(),
            role: TeammateRole::Teammate,
            conversation_id: "conv-2".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: Some(TeammateStatus::Idle),
            conversation_type: None,
            cli_path: None,
        };
        emitter.broadcast_agent_spawned(&agent);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentSpawned");

        let payload: TeamAgentSpawnedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.assistant.slot_id, "slot-2");
        assert_eq!(payload.assistant.name, "Worker");
        assert_eq!(payload.assistant.role, "teammate");
    }

    #[test]
    fn agent_runtime_status_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        let agent = TeamAgent {
            slot_id: "slot-runtime".into(),
            name: "Worker".into(),
            role: TeammateRole::Teammate,
            conversation_id: "conv-runtime".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: Some(TeammateStatus::Idle),
            conversation_type: None,
            cli_path: None,
        };

        emitter.broadcast_agent_runtime_status(&agent, TeamAgentRuntimeStatus::Ready, None);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentRuntimeStatusChanged");

        let payload: TeamAgentRuntimeStatusPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.slot_id, "slot-runtime");
        assert_eq!(payload.conversation_id, "conv-runtime");
        assert_eq!(payload.status, TeamAgentRuntimeStatus::Ready);
        assert_eq!(payload.error, None);
    }

    #[test]
    fn removed_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_agent_removed("slot-3");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentRemoved");

        let payload: TeamAgentRemovedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.slot_id, "slot-3");
    }

    #[test]
    fn renamed_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_agent_renamed("slot-1", "New Name");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentRenamed");

        let payload: TeamAgentRenamedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.slot_id, "slot-1");
        assert_eq!(payload.name, "New Name");
    }

    #[test]
    fn team_id_accessor() {
        let (emitter, _) = make_emitter();
        assert_eq!(emitter.team_id(), "team-1");
    }

    #[test]
    fn multiple_events_accumulate() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_agent_status("s1", TeammateStatus::Working);
        emitter.broadcast_agent_status("s1", TeammateStatus::Idle);
        emitter.broadcast_agent_removed("s2");

        let events = bc.events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].name, "team.agentStatusChanged");
        assert_eq!(events[1].name, "team.agentStatusChanged");
        assert_eq!(events[2].name, "team.agentRemoved");
    }

    #[test]
    fn team_run_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_team_run(
            TEAM_RUN_ACCEPTED_EVENT,
            aionui_api_types::TeamRunPayload {
                team_id: "team-1".into(),
                team_run_id: "run-1".into(),
                source: aionui_api_types::TeamRunSource::UserMessage,
                has_user_intervention: true,
                target_slot_id: "lead-1".into(),
                target_role: aionui_api_types::TeamRunTargetRole::Lead,
                status: aionui_api_types::TeamRunStatus::Accepted,
                queued_intent_count: 1,
                starting_batch_count: 0,
                running_batch_count: 0,
                active_enqueue_lease_count: 0,
                slot_work: Vec::new(),
            },
        );

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.runAccepted");

        let payload: aionui_api_types::TeamRunPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.team_run_id, "run-1");
        assert_eq!(payload.target_role, aionui_api_types::TeamRunTargetRole::Lead);
        assert_eq!(payload.status, aionui_api_types::TeamRunStatus::Accepted);
        assert_eq!(payload.starting_batch_count, 0);
    }

    #[test]
    fn child_turn_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_child_turn(
            TEAM_CHILD_TURN_STARTED_EVENT,
            aionui_api_types::TeamChildTurnPayload {
                team_id: "team-1".into(),
                team_run_id: "run-1".into(),
                slot_id: "worker-1".into(),
                role: aionui_api_types::TeamRunTargetRole::Teammate,
                conversation_id: "conv-1".into(),
                turn_id: "turn-1".into(),
                status: aionui_api_types::TeamRunStatus::Running,
                reason: None,
                replacement_message_id: None,
            },
        );

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.childTurnStarted");

        let payload: aionui_api_types::TeamChildTurnPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.team_run_id, "run-1");
        assert_eq!(payload.slot_id, "worker-1");
        assert_eq!(payload.status, aionui_api_types::TeamRunStatus::Running);
    }

    #[test]
    fn slot_work_changed_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_slot_work(aionui_api_types::TeamSlotWorkChangedPayload {
            team_id: "team-1".into(),
            slot_work: aionui_api_types::TeamSlotWorkPayload {
                slot_id: "lead-1".into(),
                role: aionui_api_types::TeamRunTargetRole::Lead,
                state: aionui_api_types::TeamSlotWorkState::Idle,
                queued_foreground_count: 0,
                queued_background_count: 0,
                active_turn_id: None,
                active_turn_started_at_ms: None,
                active_turn_elapsed_ms: None,
                active_turn_slow: None,
                active_turn_slow_threshold_ms: None,
                blocked_reason: None,
                team_run_id: None,
            },
        });

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.slotWorkChanged");
        // Must carry user_id so the event-bus → WebSocket bridge delivers it to
        // the owning user instead of dropping it (multi-account isolation #669).
        assert_eq!(events[0].data["user_id"], "user-1");

        let payload: aionui_api_types::TeamSlotWorkChangedPayload =
            serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.slot_work.slot_id, "lead-1");
        assert_eq!(payload.slot_work.state, aionui_api_types::TeamSlotWorkState::Idle);
        assert_eq!(payload.slot_work.active_turn_id, None);
    }

    #[test]
    fn slot_work_changed_event_is_routable_to_owning_user() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_slot_work(aionui_api_types::TeamSlotWorkChangedPayload {
            team_id: "team-1".into(),
            slot_work: aionui_api_types::TeamSlotWorkPayload {
                slot_id: "lead-1".into(),
                role: aionui_api_types::TeamRunTargetRole::Lead,
                state: aionui_api_types::TeamSlotWorkState::Running,
                queued_foreground_count: 0,
                queued_background_count: 0,
                active_turn_id: Some("turn-1".into()),
                active_turn_started_at_ms: Some(1),
                active_turn_elapsed_ms: Some(2),
                active_turn_slow: Some(false),
                active_turn_slow_threshold_ms: Some(3),
                blocked_reason: None,
                team_run_id: Some("run-1".into()),
            },
        });

        let events = bc.events();
        let routed_user_id = events[0].data.get("user_id").and_then(Value::as_str);
        assert_eq!(routed_user_id, Some("user-1"));
    }

    #[test]
    fn task_changed_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_task_changed(
            TeamTaskResponse {
                id: "tk1".into(),
                team_id: "team-1".into(),
                subject: "Build".into(),
                description: None,
                status: "pending".into(),
                owner: None,
                blocked_by: vec![],
                blocks: vec![],
                created_at: 1,
                updated_at: 1,
            },
            TeamTaskChange::Created,
        );

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.taskChanged");
        // Must carry user_id so the event-bus → WebSocket bridge delivers it to
        // the owning user instead of dropping it (multi-account isolation #669).
        assert_eq!(events[0].data["user_id"], "user-1");

        let payload: TeamTaskChangedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.task.id, "tk1");
        assert_eq!(payload.change, TeamTaskChange::Created);
    }

    #[test]
    fn mailbox_changed_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_mailbox_changed(
            TeamMailboxMessageResponse {
                id: "m1".into(),
                team_id: "team-1".into(),
                from_agent_id: "a2".into(),
                to_agent_id: "a1".into(),
                msg_type: "message".into(),
                content: "hi".into(),
                summary: None,
                files: vec![],
                read: true,
                created_at: 1,
            },
            TeamMailboxChange::Read,
        );

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.mailboxChanged");
        // Must carry user_id so the event-bus → WebSocket bridge delivers it to
        // the owning user instead of dropping it (multi-account isolation #669).
        assert_eq!(events[0].data["user_id"], "user-1");

        let payload: TeamMailboxChangedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(payload.team_id, "team-1");
        assert_eq!(payload.message.id, "m1");
        assert_eq!(payload.change, TeamMailboxChange::Read);
    }

    #[test]
    fn all_status_variants_serialize() {
        let (emitter, bc) = make_emitter();
        let statuses = [
            TeammateStatus::Idle,
            TeammateStatus::Working,
            TeammateStatus::Thinking,
            TeammateStatus::ToolUse,
            TeammateStatus::Completed,
            TeammateStatus::Error,
        ];
        for s in statuses {
            emitter.broadcast_agent_status("s1", s);
        }

        let events = bc.events();
        assert_eq!(events.len(), 6);
        let expected = ["idle", "working", "thinking", "tool_use", "completed", "error"];
        for (event, exp) in events.iter().zip(expected.iter()) {
            let payload: TeamAgentStatusPayload = serde_json::from_value(event.data.clone()).unwrap();
            assert_eq!(payload.status, *exp);
        }
    }
}
