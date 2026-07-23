//! Black-box integration tests for `IChannelRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.
//! Covers test-plan items: DC-1..DC-4, PC-1..PC-3, PG-2.

use std::sync::Arc;

use aionui_db::models::{
    AssistantSessionRow, AssistantUserRow, ChannelPluginRow, ChannelTopicModelOverrideRow, PairingCodeRow,
    TelegramTopicBindingRow,
};
use aionui_db::{DbError, IChannelRepository, SqliteChannelRepository, UpdatePluginStatusParams, init_database_memory};

async fn repo() -> (Arc<dyn IChannelRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    (r as Arc<dyn IChannelRepository>, db)
}

fn make_plugin(id: &str, plugin_type: &str) -> ChannelPluginRow {
    let now = aionui_common::now_ms();
    ChannelPluginRow {
        id: id.into(),
        r#type: plugin_type.into(),
        name: format!("{plugin_type} bot"),
        enabled: false,
        config: r#"{"credentials":{}}"#.into(),
        status: None,
        last_connected: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_user(id: &str, platform_uid: &str, platform: &str) -> AssistantUserRow {
    let now = aionui_common::now_ms();
    AssistantUserRow {
        id: id.into(),
        platform_user_id: platform_uid.into(),
        platform_type: platform.into(),
        display_name: Some(format!("User {id}")),
        authorized_at: now,
        last_active: None,
        session_id: None,
    }
}

fn make_session(id: &str, user_id: &str, chat_id: &str) -> AssistantSessionRow {
    let now = aionui_common::now_ms();
    AssistantSessionRow {
        id: id.into(),
        user_id: user_id.into(),
        agent_type: "gemini".into(),
        conversation_id: None,
        workspace: None,
        chat_id: Some(chat_id.into()),
        message_thread_id: None,
        bound_agent_id: None,
        bound_backend: None,
        bound_provider_id: None,
        bound_model: None,
        created_at: now,
        last_activity: now,
    }
}

fn make_topic_session(id: &str, user_id: &str, chat_id: &str, thread_id: i64) -> AssistantSessionRow {
    AssistantSessionRow {
        message_thread_id: Some(thread_id),
        bound_agent_id: Some("agent-a".into()),
        bound_backend: Some("codex".into()),
        ..make_session(id, user_id, chat_id)
    }
}

fn make_pairing(code: &str, platform_uid: &str, expires_offset_ms: i64) -> PairingCodeRow {
    let now = aionui_common::now_ms();
    PairingCodeRow {
        code: code.into(),
        platform_user_id: platform_uid.into(),
        platform_type: "telegram".into(),
        display_name: Some("Tester".into()),
        requested_at: now,
        expires_at: now + expires_offset_ms,
        status: "pending".into(),
    }
}

// ── Plugin integration tests ─────────────────────────────────────────

#[tokio::test]
async fn plugin_full_lifecycle() {
    let (repo, _db) = repo().await;

    // Empty initially.
    assert!(repo.get_all_plugins().await.unwrap().is_empty());

    // Create two plugins.
    repo.upsert_plugin(&make_plugin("tg-1", "telegram")).await.unwrap();
    repo.upsert_plugin(&make_plugin("lark-1", "lark")).await.unwrap();
    assert_eq!(repo.get_all_plugins().await.unwrap().len(), 2);

    // Update status.
    repo.update_plugin_status(
        "tg-1",
        &UpdatePluginStatusParams {
            status: Some("running".into()),
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let tg = repo.get_plugin("tg-1").await.unwrap().unwrap();
    assert!(tg.enabled);
    assert_eq!(tg.status.as_deref(), Some("running"));

    // Delete one.
    repo.delete_plugin("lark-1").await.unwrap();
    assert_eq!(repo.get_all_plugins().await.unwrap().len(), 1);
}

// ── DC-3: Same platform user uniqueness constraint ───────────────────

#[tokio::test]
async fn dc3_duplicate_platform_user_rejected() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_100", "telegram")).await.unwrap();

    // Same platform_user_id + platform_type with different id.
    let dup = make_user("u2", "tg_100", "telegram");
    let err = repo.create_user(&dup).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));
}

// ── DC-1: Revoke user cascade deletes sessions ───────────────────────

#[tokio::test]
async fn dc1_delete_user_cascades_sessions() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_1", "telegram")).await.unwrap();

    // Create two sessions for the user.
    repo.get_or_create_session("u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();
    repo.get_or_create_session("u1", "chat-b", &make_session("s2", "u1", "chat-b"))
        .await
        .unwrap();
    assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);

    // Delete user → sessions cascade.
    repo.delete_user("u1").await.unwrap();
    assert!(repo.get_all_sessions().await.unwrap().is_empty());
}

// ── PC-1: Same user, different chatId → different sessions ───────────

#[tokio::test]
async fn pc1_same_user_different_chat_ids() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_1", "telegram")).await.unwrap();

    let s1 = repo
        .get_or_create_session("u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();
    let s2 = repo
        .get_or_create_session("u1", "chat-b", &make_session("s2", "u1", "chat-b"))
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
    assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);
}

// ── PC-2: Different users, same chatId → different sessions ──────────

#[tokio::test]
async fn pc2_different_users_same_chat_id() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_1", "telegram")).await.unwrap();
    repo.create_user(&make_user("u2", "tg_2", "telegram")).await.unwrap();

    let s1 = repo
        .get_or_create_session("u1", "chat-x", &make_session("s1", "u1", "chat-x"))
        .await
        .unwrap();
    let s2 = repo
        .get_or_create_session("u2", "chat-x", &make_session("s2", "u2", "chat-x"))
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
}

// ── PC-3: Same user, same chatId → reuse session ─────────────────────

#[tokio::test]
async fn pc3_same_user_same_chat_reuses_session() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_1", "telegram")).await.unwrap();

    let s1 = repo
        .get_or_create_session("u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();

    // Second call with a different new_row id but same user+chat.
    let s2 = repo
        .get_or_create_session("u1", "chat-a", &make_session("s999", "u1", "chat-a"))
        .await
        .unwrap();

    assert_eq!(s1.id, s2.id);
    // last_activity should be >= original.
    assert!(s2.last_activity >= s1.last_activity);
}

// ── PG-2: Pairing code expires_at = requested_at + 600s ─────────────

#[tokio::test]
async fn pg2_pairing_code_expiry_is_10_minutes() {
    let (repo, _db) = repo().await;
    let pairing = make_pairing("123456", "tg_99", 600_000);
    repo.create_pairing(&pairing).await.unwrap();

    let found = repo.get_pairing_by_code("123456").await.unwrap().unwrap();
    assert_eq!(found.expires_at - found.requested_at, 600_000);
}

// ── EC-1 / EC-2: Expired pairings cleaned up, valid ones preserved ──

#[tokio::test]
async fn expired_pairings_cleaned_up() {
    let (repo, _db) = repo().await;
    let now = aionui_common::now_ms();

    // Already expired.
    repo.create_pairing(&make_pairing("111111", "tg_1", -1000))
        .await
        .unwrap();
    // Still valid.
    repo.create_pairing(&make_pairing("222222", "tg_2", 600_000))
        .await
        .unwrap();

    let cleaned = repo.cleanup_expired_pairings(now).await.unwrap();
    assert_eq!(cleaned, 1);

    let expired = repo.get_pairing_by_code("111111").await.unwrap().unwrap();
    assert_eq!(expired.status, "expired");

    let valid = repo.get_pairing_by_code("222222").await.unwrap().unwrap();
    assert_eq!(valid.status, "pending");
}

// ── Pairing status transitions ───────────────────────────────────────

#[tokio::test]
async fn pairing_approve_and_reject() {
    let (repo, _db) = repo().await;
    repo.create_pairing(&make_pairing("100001", "tg_a", 600_000))
        .await
        .unwrap();
    repo.create_pairing(&make_pairing("100002", "tg_b", 600_000))
        .await
        .unwrap();

    repo.update_pairing_status("100001", "approved").await.unwrap();
    repo.update_pairing_status("100002", "rejected").await.unwrap();

    // Neither should appear in pending list.
    let pending = repo.get_pending_pairings().await.unwrap();
    assert!(pending.is_empty());

    assert_eq!(
        repo.get_pairing_by_code("100001").await.unwrap().unwrap().status,
        "approved"
    );
    assert_eq!(
        repo.get_pairing_by_code("100002").await.unwrap().unwrap().status,
        "rejected"
    );
}

// ── User list ordered by authorized_at desc ──────────────────────────

#[tokio::test]
async fn users_ordered_by_authorized_at_desc() {
    let (repo, _db) = repo().await;

    let mut u1 = make_user("u1", "tg_1", "telegram");
    u1.authorized_at = 1000;
    repo.create_user(&u1).await.unwrap();

    let mut u2 = make_user("u2", "tg_2", "telegram");
    u2.authorized_at = 2000;
    repo.create_user(&u2).await.unwrap();

    let users = repo.get_all_users().await.unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, "u2"); // more recent first
    assert_eq!(users[1].id, "u1");
}

#[tokio::test]
async fn telegram_topic_sessions_are_isolated_and_can_be_invalidated_together() {
    let (repo, _db) = repo().await;
    repo.create_user(&make_user("u1", "tg_1", "telegram")).await.unwrap();
    repo.create_user(&make_user("u2", "tg_2", "telegram")).await.unwrap();

    let topic_3 = repo
        .get_or_create_topic_session("u1", "group", 3, &make_topic_session("s1", "u1", "group", 3))
        .await
        .unwrap();
    let topic_5 = repo
        .get_or_create_topic_session("u1", "group", 5, &make_topic_session("s2", "u1", "group", 5))
        .await
        .unwrap();
    let other_user_topic_3 = repo
        .get_or_create_topic_session("u2", "group", 3, &make_topic_session("s3", "u2", "group", 3))
        .await
        .unwrap();

    assert_ne!(topic_3.id, topic_5.id);
    assert_ne!(topic_3.id, other_user_topic_3.id);

    repo.delete_sessions_by_topic("group", 3).await.unwrap();
    assert!(repo.get_session(&topic_3.id).await.unwrap().is_none());
    assert!(repo.get_session(&other_user_topic_3.id).await.unwrap().is_none());
    assert!(repo.get_session(&topic_5.id).await.unwrap().is_some());
}

#[tokio::test]
async fn telegram_topic_binding_and_model_override_roundtrip() {
    let (repo, _db) = repo().await;
    let now = aionui_common::now_ms();
    repo.upsert_telegram_topic_binding(&TelegramTopicBindingRow {
        chat_id: "group".into(),
        message_thread_id: 3,
        agent_id: "8e1acf31".into(),
        bound_by_user_id: "admin".into(),
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    assert_eq!(
        repo.get_telegram_topic_binding("group", 3)
            .await
            .unwrap()
            .unwrap()
            .agent_id,
        "8e1acf31"
    );

    repo.upsert_topic_model_override(&ChannelTopicModelOverrideRow {
        platform: "telegram".into(),
        internal_user_id: "u1".into(),
        chat_id: "group".into(),
        message_thread_id: 3,
        agent_id: "8e1acf31".into(),
        provider_id: "openai-codex".into(),
        model: "gpt-5.3-codex".into(),
        updated_at: now,
    })
    .await
    .unwrap();

    let selected = repo
        .get_topic_model_override("telegram", "u1", "group", 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.model, "gpt-5.3-codex");
    assert!(
        repo.get_topic_model_override("telegram", "u2", "group", 3)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.get_topic_model_override("telegram", "u1", "group", 5)
            .await
            .unwrap()
            .is_none()
    );
}
