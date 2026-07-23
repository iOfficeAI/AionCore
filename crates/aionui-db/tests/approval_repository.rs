use std::sync::Arc;

use aionui_db::models::ApprovalRequestRow;
use aionui_db::{IApprovalRepository, SqliteApprovalRepository, init_database_memory};

async fn setup() -> (Arc<SqliteApprovalRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('user-1', 'developer', '', 1, 1), ('user-2', 'reviewer', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (id, user_id, name, type, extra, pinned, created_at, updated_at) \
         VALUES ('conversation-1', 'user-1', 'Approval test', 'acp', '{}', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    (Arc::new(SqliteApprovalRepository::new(db.pool().clone())), db)
}

fn approval(id: &str, expires_at: i64) -> ApprovalRequestRow {
    ApprovalRequestRow {
        id: id.into(),
        requester_user_id: "user-1".into(),
        project_id: None,
        run_id: None,
        task_id: None,
        conversation_id: "conversation-1".into(),
        agent_id: Some("claude-code".into()),
        call_id: format!("call-{id}"),
        action_type: "tool_call".into(),
        command: Some("cargo test".into()),
        working_directory: Some("/tmp/project".into()),
        risk_level: "medium".into(),
        options: r#"[{"label":"Allow","value":"allow"},{"label":"Reject","value":"reject"}]"#.into(),
        status: "pending".into(),
        approver_user_id: None,
        source_channel: Some("telegram".into()),
        source_user_id: Some("telegram-user-1".into()),
        source_chat_id: Some("-1003977604085".into()),
        source_thread_id: Some(5),
        expires_at,
        consumed_at: None,
        created_at: 10,
        updated_at: 10,
    }
}

#[tokio::test]
async fn approval_roundtrips_and_is_scoped_to_requester() {
    let (repo, _db) = setup().await;
    let row = approval("approval-1", 1_000);
    repo.create(&row).await.unwrap();

    assert_eq!(repo.get("approval-1").await.unwrap().unwrap().call_id, row.call_id);
    assert!(repo.list_for_user("user-2", None).await.unwrap().is_empty());
    assert_eq!(repo.list_for_user("user-1", None).await.unwrap(), vec![row]);
}

#[tokio::test]
async fn approval_consumption_is_atomic_and_single_use() {
    let (repo, _db) = setup().await;
    repo.create(&approval("approval-2", 1_000)).await.unwrap();

    let left = Arc::clone(&repo);
    let right = Arc::clone(&repo);
    let (left, right) = tokio::join!(
        left.consume("approval-2", "user-1", "approved", 100),
        right.consume("approval-2", "user-1", "rejected", 100),
    );
    let successes = [left.unwrap(), right.unwrap()]
        .into_iter()
        .filter(|updated| *updated)
        .count();
    assert_eq!(successes, 1);
    assert!(!repo.consume("approval-2", "user-1", "approved", 101).await.unwrap());

    let stored = repo.get("approval-2").await.unwrap().unwrap();
    assert!(matches!(stored.status.as_str(), "approved" | "rejected"));
    assert_eq!(stored.approver_user_id.as_deref(), Some("user-1"));
    assert_eq!(stored.consumed_at, Some(100));
}

#[tokio::test]
async fn expired_approvals_cannot_be_consumed() {
    let (repo, _db) = setup().await;
    repo.create(&approval("approval-3", 99)).await.unwrap();

    assert!(!repo.consume("approval-3", "user-1", "approved", 100).await.unwrap());
    assert_eq!(repo.mark_expired(100).await.unwrap(), 1);
    assert_eq!(repo.get("approval-3").await.unwrap().unwrap().status, "expired");
}

#[tokio::test]
async fn conversation_and_call_are_idempotency_key() {
    let (repo, _db) = setup().await;
    let first = approval("approval-4", 1_000);
    repo.create(&first).await.unwrap();
    let mut duplicate = approval("approval-5", 1_000);
    duplicate.call_id = first.call_id.clone();

    assert!(repo.create(&duplicate).await.is_err());
    assert_eq!(
        repo.get_by_conversation_call("conversation-1", &first.call_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        "approval-4"
    );
}
