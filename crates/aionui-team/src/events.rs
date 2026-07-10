use std::sync::Arc;

use aionui_api_types::{
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentRuntimeStatus, TeamAgentRuntimeStatusPayload,
    TeamAgentSpawnedPayload, TeamAgentStatusPayload, TeamChildTurnPayload, TeamRunPayload, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use tracing::debug;

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

pub struct TeamEventEmitter {
    team_id: String,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl TeamEventEmitter {
    pub fn new(team_id: String, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { team_id, broadcaster }
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn broadcast_agent_status(&self, slot_id: &str, status: TeammateStatus) {
        let payload = TeamAgentStatusPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
            status: status.to_string(),
        };
        let event = WebSocketMessage::new(
            TEAM_AGENT_STATUS_CHANGED_EVENT,
            serde_json::to_value(payload).expect("serialize status payload"),
        );
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_spawned(&self, agent: &TeamAgent) {
        let payload = TeamAgentSpawnedPayload {
            team_id: self.team_id.clone(),
            assistant: agent.to_response(),
        };
        let event = WebSocketMessage::new(
            TEAM_AGENT_SPAWNED_EVENT,
            serde_json::to_value(payload).expect("serialize spawned payload"),
        );
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_removed(&self, slot_id: &str) {
        let payload = TeamAgentRemovedPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
        };
        let event = WebSocketMessage::new(
            TEAM_AGENT_REMOVED_EVENT,
            serde_json::to_value(payload).expect("serialize removed payload"),
        );
        self.broadcaster.broadcast(event);
    }

    pub fn broadcast_agent_renamed(&self, slot_id: &str, name: &str) {
        let payload = TeamAgentRenamedPayload {
            team_id: self.team_id.clone(),
            slot_id: slot_id.to_owned(),
            name: name.to_owned(),
        };
        let event = WebSocketMessage::new(
            TEAM_AGENT_RENAMED_EVENT,
            serde_json::to_value(payload).expect("serialize renamed payload"),
        );
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
        let event = WebSocketMessage::new(
            TEAM_AGENT_RUNTIME_STATUS_CHANGED_EVENT,
            serde_json::to_value(payload).expect("serialize agent runtime status payload"),
        );
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
        let event = WebSocketMessage::new(
            event_name,
            serde_json::to_value(payload).expect("serialize team run payload"),
        );
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
        let event = WebSocketMessage::new(
            event_name,
            serde_json::to_value(payload).expect("serialize team child turn payload"),
        );
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
        let emitter = TeamEventEmitter::new("team-1".into(), bc.clone());
        (emitter, bc)
    }

    #[test]
    fn status_event_has_correct_shape() {
        let (emitter, bc) = make_emitter();
        emitter.broadcast_agent_status("slot-1", TeammateStatus::Working);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "team.agentStatusChanged");

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
