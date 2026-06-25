use std::sync::Arc;

use aionui_ai_agent::protocol::events::{
    AgentStreamEvent, FinishEventData,
    tool_call::{AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData},
};
use aionui_common::now_ms;
use aionui_conversation::stream_relay::StreamRelay;
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, IUserRepository, MessagePageDirection, MessagePageParams, SqliteConversationRepository,
    SqliteUserRepository, init_database_memory,
};
use serde_json::json;
use tokio::sync::broadcast;

#[tokio::test]
async fn run_acp_tool_call_update_without_insert_creates_placeholder() {
    let db = init_database_memory().await.unwrap();
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let user = user_repo.create_user("user-1", "hash").await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    repo.create(&ConversationRow {
        id: "conv-1".into(),
        user_id: user.id,
        name: "test".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("running".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    })
    .await
    .unwrap();

    let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
    let (tx, _) = broadcast::channel(64);
    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "user-1".into(),
        repo.clone(),
        bus,
        None,
    );
    let rx = tx.subscribe();

    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "sess-1".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
            tool_call_id: "atc-late".into(),
            status: Some(AcpToolCallStatus::Completed),
            title: Some("Read".into()),
            kind: None,
            raw_input: None,
            raw_output: Some(json!("done")),
            content: None,
            locations: None,
        },
        meta: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    let messages = repo
        .list_messages_page(
            "conv-1",
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(
        messages
            .iter()
            .any(|m| m.id == "atc-late" && m.r#type == "acp_tool_call")
    );
}

#[tokio::test]
async fn run_acp_tool_call_late_initial_event_merges_with_update_placeholder() {
    let db = init_database_memory().await.unwrap();
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let user = user_repo.create_user("user-1", "hash").await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    repo.create(&ConversationRow {
        id: "conv-1".into(),
        user_id: user.id,
        name: "test".into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("running".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    })
    .await
    .unwrap();

    let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
    let (tx, _) = broadcast::channel(64);
    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "user-1".into(),
        repo.clone(),
        bus,
        None,
    );
    let rx = tx.subscribe();

    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "sess-1".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
            tool_call_id: "atc-out-of-order".into(),
            status: Some(AcpToolCallStatus::Completed),
            title: None,
            kind: None,
            raw_input: None,
            raw_output: Some(json!("exit 0")),
            content: None,
            locations: None,
        },
        meta: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "sess-1".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCall,
            tool_call_id: "atc-out-of-order".into(),
            status: Some(AcpToolCallStatus::InProgress),
            title: Some("Bash".into()),
            kind: None,
            raw_input: Some(json!({"command": "echo hi"})),
            raw_output: None,
            content: None,
            locations: None,
        },
        meta: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    let messages = repo
        .list_messages_page(
            "conv-1",
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    let msg = messages
        .iter()
        .find(|m| m.id == "atc-out-of-order" && m.r#type == "acp_tool_call")
        .expect("acp tool call row should be persisted");
    assert_eq!(msg.status.as_deref(), Some("finish"));

    let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
    let update = content.get("update").expect("content should include update object");
    assert_eq!(update["status"], "completed");
    assert_eq!(update["title"], "Bash");
    assert_eq!(update["raw_input"]["command"], "echo hi");
    assert_eq!(update["raw_output"], "exit 0");
}
