use std::sync::Arc;

use aionui_ai_agent::{
    AgentError, AgentInstance, AgentStreamEvent, IWorkerTaskManager,
    agent_task::{IAgentTask, IMockAgent},
    protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus},
    types::{BuildTaskOptions, SendMessageData},
};
use aionui_api_types::AgentModeResponse;
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, TimestampMs, now_ms};
use aionui_conversation::{
    ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationService,
    skill_resolver::{ResolvedAgentSkill, SkillResolver},
    stream_relay::StreamRelay,
};
use aionui_db::{
    IConversationRepository, MessagePageDirection, MessagePageParams, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteConversationRepository, init_database_memory, models::ConversationRow,
};
use aionui_realtime::BroadcastEventBus;
use serde_json::json;
use tokio::sync::broadcast;

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &std::path::Path,
        _rel_dirs: &[&str],
        _skills: &[ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

async fn setup_repo() -> (Arc<SqliteConversationRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let now = now_ms();
    repo.create(&ConversationRow {
        id: "conv-1".into(),
        user_id: "system_default_user".into(),
        name: "Tool call test".into(),
        r#type: "aionrs".into(),
        extra: "{}".into(),
        model: None,
        status: Some("running".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();

    (repo, db)
}

#[tokio::test]
async fn run_tool_call_with_empty_call_id_is_not_persisted() {
    let (repo, _db) = setup_repo().await;
    let bus = Arc::new(BroadcastEventBus::new(64));
    let (tx, _) = broadcast::channel(64);

    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "system_default_user".into(),
        repo.clone(),
        bus,
    );

    let rx = tx.subscribe();
    tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: "".into(),
        name: "Glob".into(),
        args: json!({"pattern": "*.rs"}),
        status: ToolCallStatus::Running,
        input: Some(json!({"pattern": "*.rs"})),
        output: None,
        description: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    let messages = repo
        .list_messages_page(
            "conv-1",
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();

    assert!(
        messages.items.iter().all(|row| row.r#type != "tool_call"),
        "empty call_id tool_call must not be persisted"
    );
}

#[tokio::test]
async fn run_tool_call_late_running_event_does_not_regress_completed_message() {
    let (repo, _db) = setup_repo().await;
    let bus = Arc::new(BroadcastEventBus::new(64));
    let (tx, _) = broadcast::channel(64);

    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "system_default_user".into(),
        repo.clone(),
        bus,
    );

    let rx = tx.subscribe();
    tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: "glob-1".into(),
        name: "Glob".into(),
        args: json!({"pattern": "*.rs"}),
        status: ToolCallStatus::Completed,
        input: None,
        output: Some("src/main.rs".into()),
        description: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: "glob-1".into(),
        name: "Glob".into(),
        args: json!({"pattern": "*.rs"}),
        status: ToolCallStatus::Running,
        input: Some(json!({"pattern": "*.rs"})),
        output: None,
        description: Some("search files".into()),
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    let messages = repo
        .list_messages_page(
            "conv-1",
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let msg = messages
        .items
        .iter()
        .find(|row| row.id == "glob-1" && row.r#type == "tool_call")
        .expect("tool call row should be persisted");
    assert_eq!(msg.status.as_deref(), Some("finish"));

    let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
    assert_eq!(content["status"], "completed");
    assert_eq!(content["output"], "src/main.rs");
    assert_eq!(content["input"]["pattern"], "*.rs");
    assert_eq!(content["description"], "search files");
}

struct ToolCallAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
}

impl ToolCallAgent {
    fn new(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
        }
    }
}

#[async_trait::async_trait]
impl IAgentTask for ToolCallAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Aionrs
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        "/tmp"
    }

    fn status(&self) -> Option<ConversationStatus> {
        None
    }

    fn last_activity_at(&self) -> TimestampMs {
        now_ms()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "".into(),
            name: "Glob".into(),
            args: json!({"pattern": "*.rs"}),
            status: ToolCallStatus::Running,
            input: Some(json!({"pattern": "*.rs"})),
            output: None,
            description: None,
        }));
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for ToolCallAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        Vec::new()
    }

    async fn mode(&self) -> Result<AgentModeResponse, AgentError> {
        Ok(AgentModeResponse {
            mode: "default".into(),
            initialized: false,
        })
    }
}

struct ToolCallTaskManager {
    agent: AgentInstance,
}

#[async_trait::async_trait]
impl IWorkerTaskManager for ToolCallTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        Some(self.agent.clone())
    }

    async fn get_or_build_task(
        &self,
        _conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        Ok(self.agent.clone())
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        1
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn run_agent_turn_with_empty_call_id_tool_call_is_not_persisted() {
    let (repo, db) = setup_repo().await;
    let bus = Arc::new(BroadcastEventBus::new(64));
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(ToolCallTaskManager {
        agent: AgentInstance::Mock(Arc::new(ToolCallAgent::new("conv-1"))),
    });
    let service = ConversationService::new(
        std::env::temp_dir(),
        bus,
        Arc::new(EmptySkillResolver),
        task_manager,
        repo.clone(),
        Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())),
        Arc::new(SqliteAcpSessionRepository::new(db.pool().clone())),
    );

    let outcome = service
        .run_agent_turn(ConversationAgentTurnRequest {
            user_id: "system_default_user".into(),
            conversation_id: "conv-1".into(),
            content: "run glob".into(),
            files: Vec::new(),
            inject_skills: Vec::new(),
            required_runtime_mode: None,
            persist_user_message: false,
            user_message_hidden: false,
            on_started: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome.status, ConversationAgentTurnStatus::Completed);

    let messages = repo
        .list_messages_page(
            "conv-1",
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert!(
        messages.items.iter().all(|row| row.r#type != "tool_call"),
        "empty call_id tool_call must not be persisted through run_agent_turn"
    );
}
