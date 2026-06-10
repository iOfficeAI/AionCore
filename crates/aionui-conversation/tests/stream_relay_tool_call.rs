use std::sync::Arc;

use aionui_ai_agent::{
    AgentStreamEvent,
    protocol::events::{FinishEventData, ToolCallEventData, ToolCallStatus},
};
use aionui_common::now_ms;
use aionui_conversation::stream_relay::StreamRelay;
use aionui_db::{
    IConversationRepository, SortOrder, SqliteConversationRepository, init_database_memory, models::ConversationRow,
};
use aionui_realtime::BroadcastEventBus;
use serde_json::json;
use tokio::sync::broadcast;

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
        None,
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

    let messages = repo.get_messages("conv-1", 1, 100, SortOrder::Asc).await.unwrap();

    assert!(
        messages.items.iter().all(|row| row.r#type != "tool_call"),
        "empty call_id tool_call must not be persisted"
    );
}
