use aionui_common::now_ms;
use aionui_db::{IUsageEventRepository, InsertUsageEventParams, SqliteUsageEventRepository, init_database_memory};

fn spend_params<'a>(
    user_id: &'a str,
    conversation_id: &'a str,
    fingerprint: &'a str,
    input_tokens: i64,
) -> InsertUsageEventParams<'a> {
    InsertUsageEventParams {
        user_id,
        conversation_id,
        recorded_at: now_ms(),
        fingerprint,
        backend: "claude",
        conversation_source: "lark",
        conversation_name: Some("Channel chat"),
        assistant_id: Some("asst-1"),
        assistant_name: Some("Claude"),
        model_id: Some("sonnet"),
        turn_id: Some("turn-1"),
        total_tokens: input_tokens + 4,
        input_tokens,
        output_tokens: 4,
        thought_tokens: 0,
        cached_read_tokens: 0,
        cached_write_tokens: 0,
        cost_delta: 0.1,
        session_cost_amount: Some(0.1),
        cost_currency: Some("USD"),
        event_source: "acp",
    }
}

#[tokio::test]
async fn insert_is_idempotent_on_fingerprint() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    let first = repo
        .insert_if_new(&spend_params("user-1", "conv-1", "turn:turn-1", 10))
        .await
        .unwrap();
    let second = repo
        .insert_if_new(&spend_params("user-1", "conv-1", "turn:turn-1", 99))
        .await
        .unwrap();

    assert!(first.is_some());
    assert!(second.is_none());
    let listed = repo.list_for_user("user-1", None, 50).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].input_tokens, 10);
    assert_eq!(listed[0].total_tokens, 14);
    assert_eq!(listed[0].fingerprint, "turn:turn-1");
    assert_eq!(listed[0].turn_id.as_deref(), Some("turn-1"));
    assert_eq!(listed[0].conversation_source, "lark");
}

#[tokio::test]
async fn clear_and_list_are_user_scoped() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    repo.insert_if_new(&spend_params("user-1", "conv-1", "turn:a", 8))
        .await
        .unwrap();
    repo.insert_if_new(&spend_params("user-2", "conv-2", "turn:b", 12))
        .await
        .unwrap();

    assert_eq!(repo.clear_for_user("user-1").await.unwrap(), 1);
    assert!(repo.list_for_user("user-1", None, 50).await.unwrap().is_empty());
    assert_eq!(repo.list_for_user("user-2", None, 50).await.unwrap().len(), 1);
}
