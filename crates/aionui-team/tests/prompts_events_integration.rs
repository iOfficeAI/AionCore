mod common;

use std::collections::HashSet;
use std::sync::Arc;

use aionui_api_types::{
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentSpawnedPayload, TeamAgentStatusPayload, WebSocketMessage,
};
use aionui_db::models::MessageRow;
use aionui_realtime::EventBroadcaster;
use aionui_team::events::TeamEventEmitter;
use aionui_team::message_projection::{
    ProjectedTeamMessage, TeamMessageProjection, TeamProjectionMessageStore, TeamProjectionRequest,
    TeamProjectionSource,
};
use aionui_team::prompts::{AvailableAssistant, build_lead_prompt, build_teammate_prompt, build_wake_payload};
use aionui_team::types::{
    MailboxMessage, MailboxMessageType, TaskStatus, TeamAgent, TeamTask, TeammateRole, TeammateStatus,
};
use aionui_team::visibility::TeamVisibilityPolicy;
use aionui_team::{Mailbox, TaskBoard, TeammateManager};
use async_trait::async_trait;
use common::MockTeamRepo;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

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

fn roster(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

#[derive(Default)]
struct RecordingProjectionStore {
    rows: std::sync::Mutex<Vec<MessageRow>>,
}

impl RecordingProjectionStore {
    fn rows(&self) -> Vec<MessageRow> {
        self.rows.lock().unwrap().clone()
    }
}

#[async_trait]
impl TeamProjectionMessageStore for RecordingProjectionStore {
    fn mint_message_id(&self) -> String {
        format!("msg-recorded-{}", self.rows.lock().unwrap().len())
    }

    async fn find_projected_message(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, aionui_team::TeamError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| {
                row.conversation_id == conversation_id
                    && row.msg_id.as_deref() == Some(msg_id)
                    && row.r#type == msg_type
            })
            .cloned())
    }

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), aionui_team::TeamError> {
        self.rows.lock().unwrap().push(row.clone());
        Ok(())
    }
}

fn make_agent(slot_id: &str, name: &str, role: TeammateRole) -> TeamAgent {
    TeamAgent {
        slot_id: slot_id.into(),
        name: name.into(),
        role,
        conversation_id: format!("conv-{slot_id}"),
        backend: "acp".into(),
        model: "claude".into(),
        assistant_id: None,
        status: None,
        conversation_type: None,
        cli_path: None,
    }
}

// ===========================================================================
// Round 2: Visibility policy and Team message projection
// ===========================================================================

#[test]
fn visibility_policy_user_message_has_explicit_decisions() {
    let policy = TeamVisibilityPolicy::user_message();

    assert!(policy.write_mailbox);
    assert!(policy.insert_user_visible_bubble);
    assert!(!policy.insert_teammate_visible_bubble);
    assert!(!policy.allow_hidden_conversation_message);
    assert!(policy.strip_system_notes);
}

#[test]
fn projection_request_for_teammate_mirror_uses_stable_mailbox_dedupe_key() {
    let req = TeamProjectionRequest::teammate_visible(
        "team-1",
        "lead-1",
        "conv-lead",
        "worker-1",
        "Worker",
        "Done",
        "mailbox-123",
    );

    assert_eq!(
        req.dedupe_key.as_deref(),
        Some("team:team-1:mailbox:mailbox-123:conversation:conv-lead")
    );
    assert!(req.visibility.insert_teammate_visible_bubble);
    assert!(!req.visibility.allow_hidden_conversation_message);
}

#[test]
fn projection_request_for_team_system_message_uses_stable_mailbox_dedupe_key() {
    let req = TeamProjectionRequest::team_system_visible(
        "team-1",
        "lead-1",
        "conv-lead",
        "Roster changed",
        "mailbox-system-123",
    );

    assert_eq!(
        req.dedupe_key.as_deref(),
        Some("team:team-1:mailbox:mailbox-system-123:conversation:conv-lead")
    );
    assert!(req.visibility.insert_teammate_visible_bubble);
    assert!(!req.visibility.allow_hidden_conversation_message);
}

#[tokio::test]
async fn projection_inserts_user_visible_bubble_with_stripped_system_notes() {
    let store = Arc::new(RecordingProjectionStore::default());
    let bc = Arc::new(RecordingBroadcaster::new());
    let projection = TeamMessageProjection::new(store.clone(), bc.clone());
    let req = TeamProjectionRequest {
        team_id: "team-1".into(),
        slot_id: "lead-1".into(),
        conversation_id: "conv-lead".into(),
        source: TeamProjectionSource::User,
        content: "Visible\n[SYSTEM NOTE: internal]\ntext".into(),
        files: vec![],
        visibility: TeamVisibilityPolicy::user_message(),
        dedupe_key: None,
    };

    let projected = projection.project(req).await.unwrap();

    assert!(matches!(projected, ProjectedTeamMessage::Inserted { .. }));
    let rows = store.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].position.as_deref(), Some("right"));
    assert!(!rows[0].hidden);
    let content: serde_json::Value = serde_json::from_str(&rows[0].content).unwrap();
    assert_eq!(content["content"], "Visible\ntext");
    assert!(!rows[0].content.contains("SYSTEM NOTE"));
    assert!(bc.events().is_empty(), "user projection should rely on message.stream");
}

#[tokio::test]
async fn projection_dedupes_teammate_mirror_and_broadcasts_persisted_msg_id() {
    let store = Arc::new(RecordingProjectionStore::default());
    let bc = Arc::new(RecordingBroadcaster::new());
    let projection = TeamMessageProjection::new(store.clone(), bc.clone());
    let req = TeamProjectionRequest::teammate_visible(
        "team-1",
        "lead-1",
        "conv-lead",
        "worker-1",
        "Worker",
        "Done",
        "mailbox-123",
    );

    let first = projection.project(req.clone()).await.unwrap();
    let second = projection.project(req).await.unwrap();

    let first_msg_id = match first {
        ProjectedTeamMessage::Inserted { msg_id } => msg_id,
        other => panic!("expected insert, got {other:?}"),
    };
    assert!(matches!(
        second,
        ProjectedTeamMessage::AlreadyProjected { ref msg_id } if msg_id == &first_msg_id
    ));
    assert_eq!(
        store.rows().len(),
        1,
        "duplicate projection must not insert a second row"
    );

    let events = bc.events();
    assert_eq!(events.len(), 1, "duplicate projection must not re-broadcast");
    assert_eq!(events[0].name, "team.teammateMessage");
    assert_eq!(events[0].data["conversation_id"], "conv-lead");
    assert_eq!(events[0].data["msg_id"], first_msg_id);
    assert_eq!(events[0].data["from_slot_id"], "worker-1");
    assert_eq!(events[0].data["from_name"], "Worker");
}

#[tokio::test]
async fn projection_inserts_team_system_bubble_and_broadcasts_it_like_existing_mirror() {
    let store = Arc::new(RecordingProjectionStore::default());
    let bc = Arc::new(RecordingBroadcaster::new());
    let projection = TeamMessageProjection::new(store.clone(), bc.clone());
    let req = TeamProjectionRequest::team_system_visible(
        "team-1",
        "lead-1",
        "conv-lead",
        "Roster changed",
        "mailbox-system-123",
    );

    let projected = projection.project(req).await.unwrap();

    let msg_id = match projected {
        ProjectedTeamMessage::Inserted { msg_id } => msg_id,
        other => panic!("expected insert, got {other:?}"),
    };
    let rows = store.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].position.as_deref(), Some("left"));
    assert_eq!(rows[0].msg_id.as_deref(), Some(msg_id.as_str()));
    let content: serde_json::Value = serde_json::from_str(&rows[0].content).unwrap();
    assert_eq!(content["content"], "Roster changed");
    assert_eq!(content["teammate_message"], true);
    assert_eq!(content["sender_name"], "team_system");

    let events = bc.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "team.teammateMessage");
    assert_eq!(events[0].data["conversation_id"], "conv-lead");
    assert_eq!(events[0].data["msg_id"], msg_id);
    assert_eq!(events[0].data["from_slot_id"], "team_system");
    assert_eq!(events[0].data["from_name"], "team_system");
}

// ===========================================================================
// Test-plan §9: Prompt Templates
// ===========================================================================

fn default_assistants() -> Vec<AvailableAssistant> {
    vec![
        AvailableAssistant {
            assistant_id: "research-assistant".into(),
            name: "Research Assistant".into(),
            backend: "claude".into(),
            description: "General-purpose research assistant".into(),
            skills: vec!["web-search".into(), "synthesis".into()],
        },
        AvailableAssistant {
            assistant_id: "writer-assistant".into(),
            name: "Writer Assistant".into(),
            backend: "codex".into(),
            description: "Writing-focused assistant".into(),
            skills: vec!["drafting".into()],
        },
        AvailableAssistant {
            assistant_id: "slides-assistant".into(),
            name: "Slides Assistant".into(),
            backend: "gemini".into(),
            description: "Presentation builder".into(),
            skills: vec!["slides".into()],
        },
    ]
}

// -- LP-1: Lead prompt relies on team_members for roster ----------------------

#[test]
fn lp1_lead_prompt_does_not_contain_member_snapshot() {
    let members = vec![
        make_agent("lead-1", "Lead", TeammateRole::Lead),
        make_agent("w1", "Alice", TeammateRole::Teammate),
        make_agent("w2", "Bob", TeammateRole::Teammate),
    ];
    let assistants = default_assistants();
    let lead = make_agent("lead-1", "Lead", TeammateRole::Lead);
    let prompt = build_lead_prompt(&lead, "Alpha", &members, &assistants);

    assert!(!prompt.contains("## Your Teammates"));
    assert!(!prompt.contains("- Lead ("), "lead snapshot leaked");
    assert!(!prompt.contains("- Alice ("), "teammate Alice snapshot leaked");
    assert!(!prompt.contains("- Bob ("), "teammate Bob snapshot leaked");
    assert!(prompt.to_lowercase().contains("first team turn"));
    assert!(prompt.contains("team_members"));
}

// -- LP-2: Lead prompt contains tool descriptions ----------------------------

#[test]
fn lp2_lead_prompt_contains_tool_descriptions() {
    let lead = make_agent("lead-1", "Lead", TeammateRole::Lead);
    let prompt = build_lead_prompt(&lead, "Beta", &[], &default_assistants());

    // AionUi lead prompt references the `team_*` coordination tools that the
    // leader must use; the MCP layer enumerates them with arguments, so the
    // prompt mentions each tool at least once.
    let expected_tools = [
        "team_send_message",
        "team_spawn_agent",
        "team_task_create",
        "team_task_list",
        "team_members",
        "team_rename_agent",
        "team_shutdown_agent",
    ];
    for tool in expected_tools {
        assert!(prompt.contains(tool), "missing tool: {tool}");
    }
    assert!(prompt.contains("team_list_assistants"));
    assert!(!prompt.contains("## Available Assistants for Spawning"));
}

// -- LP-3: Lead prompt contains task management guidance ---------------------

#[test]
fn lp3_lead_prompt_contains_task_management_guidance() {
    let lead = make_agent("lead-1", "Lead", TeammateRole::Lead);
    let prompt = build_lead_prompt(&lead, "Gamma", &[], &default_assistants());

    assert!(
        prompt.contains("Break the work into tasks"),
        "missing decompose guidance"
    );
    assert!(prompt.contains("Assign tasks"), "missing assign guidance");
    assert!(prompt.contains("dependency"), "missing dependency guidance");
    assert!(
        prompt.contains("When teammates report back"),
        "missing teammate result-review guidance"
    );
}

// -- TP-1: Teammate prompt contains execution guidance -----------------------

#[test]
fn tp1_teammate_prompt_contains_execution_guidance() {
    let agent = make_agent("w1", "Worker1", TeammateRole::Teammate);
    let members = vec![make_agent("lead-1", "Lead", TeammateRole::Lead), agent.clone()];
    let prompt = build_teammate_prompt(&agent, "Alpha", &members);

    assert!(prompt.contains("## Team Governance"), "missing governance");
    assert!(
        prompt.contains("You MUST use the `team_*` MCP tools for ALL team coordination."),
        "missing canonical coordination rule"
    );
    assert!(prompt.contains("## How to Work"), "missing execution guidance");
    assert!(prompt.contains("team_send_message"), "missing communication tool");
    assert!(prompt.contains("team_task_update"), "missing task update tool");
    assert!(prompt.contains("shutdown_request"), "missing shutdown protocol");
    assert!(prompt.contains("shutdown_approved"), "missing shutdown_approved");
    assert!(prompt.contains("STOP GENERATING"), "missing stop protocol");
    assert!(prompt.contains("Slot ID: w1"), "missing teammate slot id");
    assert!(
        !prompt.contains("Teammates:"),
        "static teammate list must not be injected"
    );
}

// -- TP-2: Teammate prompt contains team name --------------------------------

#[test]
fn tp2_teammate_prompt_contains_team_name() {
    let agent = make_agent("w1", "Worker1", TeammateRole::Teammate);
    let members = vec![make_agent("lead-1", "Lead", TeammateRole::Lead), agent.clone()];
    let prompt = build_teammate_prompt(&agent, "Project Falcon", &members);

    assert!(prompt.contains("Team: Project Falcon"));
}

// -- WP-1: Wake payload includes unread messages -----------------------------

#[test]
fn wp1_wake_payload_includes_unread_messages() {
    let agent = make_agent("lead-1", "Lead", TeammateRole::Lead);
    let messages = vec![
        MailboxMessage {
            id: "msg-1".into(),
            team_id: "t1".into(),
            to_agent_id: "lead-1".into(),
            from_agent_id: "w1".into(),
            msg_type: MailboxMessageType::Message,
            content: "Feature X is done".into(),
            summary: None,
            files: None,
            read: false,
            created_at: 0,
        },
        MailboxMessage {
            id: "msg-2".into(),
            team_id: "t1".into(),
            to_agent_id: "lead-1".into(),
            from_agent_id: "w2".into(),
            msg_type: MailboxMessageType::IdleNotification,
            content: "idle".into(),
            summary: Some("Finished task Y".into()),
            files: None,
            read: false,
            created_at: 0,
        },
    ];
    let payload = build_wake_payload(&agent, &[], &messages, &roster(&["lead-1", "w1", "w2"]));

    assert!(payload.contains("Feature X is done"));
    assert!(payload.contains("`w1`"));
    assert!(payload.contains("[message]"));
    assert!(payload.contains("`w2`"));
    assert!(payload.contains("[idle_notification]"));
    assert!(payload.contains("Summary: Finished task Y"));
}

// -- WP-2: Wake payload includes current task list ---------------------------

#[test]
fn wp2_wake_payload_includes_task_list() {
    let agent = make_agent("lead-1", "Lead", TeammateRole::Lead);
    let tasks = vec![
        TeamTask {
            id: "aaaaaaaa-1111-2222-3333-444444444444".into(),
            team_id: "t1".into(),
            subject: "Implement auth".into(),
            description: None,
            status: TaskStatus::InProgress,
            owner: Some("w1".into()),
            blocked_by: vec![],
            blocks: vec![],
            metadata: None,
            created_at: 0,
            updated_at: 0,
        },
        TeamTask {
            id: "bbbbbbbb-1111-2222-3333-444444444444".into(),
            team_id: "t1".into(),
            subject: "Write tests".into(),
            description: None,
            status: TaskStatus::Pending,
            owner: Some("w2".into()),
            blocked_by: vec!["aaaaaaaa-1111-2222-3333-444444444444".into()],
            blocks: vec![],
            metadata: None,
            created_at: 0,
            updated_at: 0,
        },
    ];
    let payload = build_wake_payload(&agent, &tasks, &[], &roster(&["lead-1", "w1", "w2"]));

    assert!(payload.contains("Current Task Board Summary"));
    assert!(payload.contains("Showing 2 of 2 tasks."));
    assert!(payload.contains("Implement auth"));
    assert!(payload.contains("in_progress"));
    assert!(payload.contains("Write tests"));
    assert!(payload.contains("pending"));
    assert!(payload.contains("w1"));
    assert!(payload.contains("w2"));
    assert!(payload.contains("aaaaaaaa…"));
    assert!(
        !payload.contains("aaaaaaaa-1111-2222-3333-444444444444"),
        "summary blocked_by column should use short task IDs"
    );
}

// -- WP-3: Wake payload with no messages and no tasks builds normally --------

#[test]
fn wp3_wake_payload_empty_builds_normally() {
    let agent = make_agent("w1", "Worker1", TeammateRole::Teammate);
    let payload = build_wake_payload(&agent, &[], &[], &roster(&["w1"]));

    assert!(payload.contains("No new messages"));
    assert!(payload.contains("No tasks on the board"));
    assert!(payload.contains("**Worker1**"));
    assert!(payload.contains("teammate"));
}

// ===========================================================================
// Test-plan §8: WebSocket Event Broadcasting
// ===========================================================================

// -- WE-1: Agent status change event -----------------------------------------

#[tokio::test]
async fn we1_agent_status_change_event() {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo));
    let bc = Arc::new(RecordingBroadcaster::new());
    let agents = vec![
        make_agent("lead-1", "Lead", TeammateRole::Lead),
        make_agent("w1", "Worker", TeammateRole::Teammate),
    ];
    let mgr = TeammateManager::new("t1".into(), &agents, mailbox, task_board, bc.clone());

    mgr.set_status("w1", TeammateStatus::Working).await.unwrap();

    let events = bc.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "team.agentStatusChanged");

    let payload: TeamAgentStatusPayload = serde_json::from_value(events[0].data.clone()).unwrap();
    assert_eq!(payload.team_id, "t1");
    assert_eq!(payload.slot_id, "w1");
    assert_eq!(payload.status, "working");
}

// -- WE-2: Agent spawned event -----------------------------------------------

#[tokio::test]
async fn we2_agent_spawned_event() {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo));
    let bc = Arc::new(RecordingBroadcaster::new());
    let agents = vec![make_agent("lead-1", "Lead", TeammateRole::Lead)];
    let mgr = TeammateManager::new("t1".into(), &agents, mailbox, task_board, bc.clone());

    let new_agent = make_agent("w2", "NewWorker", TeammateRole::Teammate);
    mgr.add_agent(&new_agent).await;

    let spawned: Vec<_> = bc
        .events()
        .into_iter()
        .filter(|e| e.name == "team.agentSpawned")
        .collect();
    assert_eq!(spawned.len(), 1);

    let payload: TeamAgentSpawnedPayload = serde_json::from_value(spawned[0].data.clone()).unwrap();
    assert_eq!(payload.team_id, "t1");
    assert_eq!(payload.assistant.slot_id, "w2");
    assert_eq!(payload.assistant.name, "NewWorker");
}

// -- WE-3: Agent removed event -----------------------------------------------

#[tokio::test]
async fn we3_agent_removed_event() {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo));
    let bc = Arc::new(RecordingBroadcaster::new());
    let agents = vec![
        make_agent("lead-1", "Lead", TeammateRole::Lead),
        make_agent("w1", "Worker", TeammateRole::Teammate),
    ];
    let mgr = TeammateManager::new("t1".into(), &agents, mailbox, task_board, bc.clone());

    mgr.remove_agent("w1").await.unwrap();

    let removed: Vec<_> = bc
        .events()
        .into_iter()
        .filter(|e| e.name == "team.agentRemoved")
        .collect();
    assert_eq!(removed.len(), 1);

    let payload: TeamAgentRemovedPayload = serde_json::from_value(removed[0].data.clone()).unwrap();
    assert_eq!(payload.team_id, "t1");
    assert_eq!(payload.slot_id, "w1");
}

// -- WE-4: Agent renamed event -----------------------------------------------

#[tokio::test]
async fn we4_agent_renamed_event() {
    let repo = Arc::new(MockTeamRepo::new());
    let mailbox = Arc::new(Mailbox::new(repo.clone()));
    let task_board = Arc::new(TaskBoard::new(repo));
    let bc = Arc::new(RecordingBroadcaster::new());
    let agents = vec![
        make_agent("lead-1", "Lead", TeammateRole::Lead),
        make_agent("w1", "Worker", TeammateRole::Teammate),
    ];
    let mgr = TeammateManager::new("t1".into(), &agents, mailbox, task_board, bc.clone());

    mgr.rename_agent("w1", "SuperWorker").await.unwrap();

    let renamed: Vec<_> = bc
        .events()
        .into_iter()
        .filter(|e| e.name == "team.agentRenamed")
        .collect();
    assert_eq!(renamed.len(), 1);

    let payload: TeamAgentRenamedPayload = serde_json::from_value(renamed[0].data.clone()).unwrap();
    assert_eq!(payload.team_id, "t1");
    assert_eq!(payload.slot_id, "w1");
    assert_eq!(payload.name, "SuperWorker");
}

// -- Direct TeamEventEmitter test (event payloads use typed structs) ----------

#[test]
fn event_emitter_uses_typed_payloads() {
    let bc = Arc::new(RecordingBroadcaster::new());
    let emitter = TeamEventEmitter::new("team-x".into(), bc.clone());

    let agent = make_agent("s1", "A", TeammateRole::Teammate);
    emitter.broadcast_agent_status("s1", TeammateStatus::Thinking);
    emitter.broadcast_agent_spawned(&agent);
    emitter.broadcast_agent_removed("s1");
    emitter.broadcast_agent_renamed("s1", "B");

    let events = bc.events();
    assert_eq!(events.len(), 4);

    let p1: TeamAgentStatusPayload = serde_json::from_value(events[0].data.clone()).unwrap();
    assert_eq!(p1.status, "thinking");

    let p2: TeamAgentSpawnedPayload = serde_json::from_value(events[1].data.clone()).unwrap();
    assert_eq!(p2.assistant.slot_id, "s1");

    let p3: TeamAgentRemovedPayload = serde_json::from_value(events[2].data.clone()).unwrap();
    assert_eq!(p3.slot_id, "s1");

    let p4: TeamAgentRenamedPayload = serde_json::from_value(events[3].data.clone()).unwrap();
    assert_eq!(p4.name, "B");
}
