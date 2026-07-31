//! Empty-thinking handling, end to end against a real SQLite DB: a thinking
//! chunk that carries no text must be dropped by the relay, so it reaches
//! neither the DB nor the real service read path (`list_messages`, i.e. GET
//! /messages). Persisting such a segment is what filled a reloaded conversation
//! with blank「思考完成 · 0秒」cards. Live and reload agree because NEITHER
//! renders a card — the surrounding tool rows must survive untouched.

use std::sync::Arc;
use std::sync::Mutex;

use aionui_ai_agent::protocol::events::{
    AgentStreamEvent, FinishEventData, ThinkingEventData,
    tool_call::{ToolCallEventData, ToolCallStatus},
};
use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use aionui_api_types::{ListMessagesQuery, WebSocketMessage};
use aionui_common::{AgentKillReason, MessageType, TimestampMs, now_ms};
use aionui_conversation::ConversationService;
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_conversation::stream_relay::StreamRelay;
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, IUserRepository, SqliteConversationRepository, SqliteUserRepository, init_database_memory,
};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::broadcast;

struct TestBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<aionui_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        Err(AgentError::internal("noop"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &std::path::Path,
        _rel_dirs: &[&str],
        _skills: &[aionui_extension::ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

fn tool_call(call_id: &str) -> AgentStreamEvent {
    AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: call_id.into(),
        name: "read_file".into(),
        args: json!({"path": "a.ts"}),
        status: ToolCallStatus::Running,
        description: None,
        input: None,
        output: None,
    })
}

#[tokio::test]
async fn empty_thinking_segment_is_dropped_before_persistence() {
    let db = init_database_memory().await.unwrap();
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let user = user_repo.create_user("user-1", "hash").await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    repo.create(&ConversationRow {
        id: "conv-1".into(),
        user_id: user.id.clone(),
        name: "test".into(),
        r#type: "claude".into(),
        extra: "{}".into(),
        model: None,
        status: Some("running".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now_ms(),
        updated_at: now_ms(),
        project_id: None,
        folder_id: None,
    })
    .await
    .unwrap();

    let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
    let (tx, _) = broadcast::channel(64);
    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        user.id.clone(),
        repo.clone(),
        bus,
    );
    let rx = tx.subscribe();

    // tool → contentless thinking → tool. The blank thought is dropped, so the
    // two tool rows land in one group — the same grouping the live view shows,
    // which is the parity that matters.
    tx.send(tool_call("tc-1")).unwrap();
    tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
        content: String::new(),
        subject: None,
        duration: None,
        status: Some("thinking".into()),
    }))
    .unwrap();
    tx.send(tool_call("tc-2")).unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    // Read back through the real service path (what GET /messages executes).
    let svc = ConversationService::new(
        std::env::temp_dir(),
        Arc::new(TestBroadcaster {
            events: Mutex::new(vec![]),
        }),
        Arc::new(EmptySkillResolver),
        Arc::new(NoopTaskManager),
        repo.clone(),
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone())),
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone())),
    );
    let listed = svc
        .list_messages(&user.id, "conv-1", ListMessagesQuery::default())
        .await
        .unwrap();

    let thinking: Vec<_> = listed
        .items
        .iter()
        .filter(|m| m.r#type == MessageType::Thinking)
        .collect();
    assert!(
        thinking.is_empty(),
        "an empty thinking chunk must not reach the reload path at all — persisting \
         it is what produced the column of blank「思考完成 · 0秒」cards; got: {thinking:?}"
    );

    let tool_rows = listed
        .items
        .iter()
        .filter(|m| m.r#type == MessageType::ToolCall)
        .count();
    assert_eq!(tool_rows, 2, "both tool rows must still be present");
}
