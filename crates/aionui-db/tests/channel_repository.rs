//! Black-box integration tests for `IChannelRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.
//! Covers test-plan items: DC-1..DC-4, PC-1..PC-3, PG-2.

use std::sync::Arc;

use aionui_db::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
use aionui_db::{DbError, IChannelRepository, SqliteChannelRepository, UpdatePluginStatusParams, init_database_memory};

const OWNER_ID: &str = "system_default_user";

async fn repo() -> (Arc<dyn IChannelRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    (r as Arc<dyn IChannelRepository>, db)
}

fn make_plugin(id: &str, plugin_type: &str) -> ChannelPluginRow {
    let now = aionui_common::now_ms();
    ChannelPluginRow {
        id: id.into(),
        owner_user_id: OWNER_ID.into(),
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
        owner_user_id: OWNER_ID.into(),
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
        created_at: now,
        last_activity: now,
    }
}

fn make_pairing(code: &str, platform_uid: &str, expires_offset_ms: i64) -> PairingCodeRow {
    let now = aionui_common::now_ms();
    PairingCodeRow {
        code: code.into(),
        owner_user_id: OWNER_ID.into(),
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
    assert!(repo.get_all_plugins(OWNER_ID).await.unwrap().is_empty());

    // Create two plugins.
    repo.upsert_plugin(OWNER_ID, &make_plugin("tg-1", "telegram"))
        .await
        .unwrap();
    repo.upsert_plugin(OWNER_ID, &make_plugin("lark-1", "lark"))
        .await
        .unwrap();
    assert_eq!(repo.get_all_plugins(OWNER_ID).await.unwrap().len(), 2);

    // Update status.
    repo.update_plugin_status(
        OWNER_ID,
        "tg-1",
        &UpdatePluginStatusParams {
            status: Some("running".into()),
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let tg = repo.get_plugin(OWNER_ID, "tg-1").await.unwrap().unwrap();
    assert!(tg.enabled);
    assert_eq!(tg.status.as_deref(), Some("running"));

    // Delete one.
    repo.delete_plugin(OWNER_ID, "lark-1").await.unwrap();
    assert_eq!(repo.get_all_plugins(OWNER_ID).await.unwrap().len(), 1);
}

// ── DC-3: Same platform user uniqueness constraint ───────────────────

#[tokio::test]
async fn dc3_duplicate_platform_user_rejected() {
    let (repo, _db) = repo().await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_100", "telegram"))
        .await
        .unwrap();

    // Same platform_user_id + platform_type with different id.
    let dup = make_user("u2", "tg_100", "telegram");
    let err = repo.create_user(OWNER_ID, &dup).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));
}

// ── DC-1: Revoke user cascade deletes sessions ───────────────────────

#[tokio::test]
async fn dc1_delete_user_cascades_sessions() {
    let (repo, _db) = repo().await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_1", "telegram"))
        .await
        .unwrap();

    // Create two sessions for the user.
    repo.get_or_create_session(OWNER_ID, "u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();
    repo.get_or_create_session(OWNER_ID, "u1", "chat-b", &make_session("s2", "u1", "chat-b"))
        .await
        .unwrap();
    assert_eq!(repo.get_all_sessions(OWNER_ID).await.unwrap().len(), 2);

    // Delete user → sessions cascade.
    repo.delete_user(OWNER_ID, "u1").await.unwrap();
    assert!(repo.get_all_sessions(OWNER_ID).await.unwrap().is_empty());
}

// ── Cross-account guard: a channel session may not bind another Core user's
//    conversation. The INSERT's ownership predicate matches zero rows, so the
//    session is never created and no data leaks across accounts.
#[tokio::test]
async fn session_rejects_conversation_owned_by_another_core_user() {
    let (repo, db) = repo().await;
    let pool = db.pool();

    repo.create_user(OWNER_ID, &make_user("u1", "tg_1", "telegram"))
        .await
        .unwrap();

    // Core users referenced by the conversations below (conversations.user_id
    // FK → users). OWNER_ID may already be seeded; the other must be created.
    for uid in [OWNER_ID, "other_core_user"] {
        sqlx::query(
            "INSERT OR IGNORE INTO users \
                (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(uid)
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
    }

    for (id, owner) in [("conv-own", OWNER_ID), ("conv-other", "other_core_user")] {
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES (?, ?, 'c', 'gemini', '{}', 'pending', 1, 1)",
        )
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    }

    let with_conv = |sid: &str, conv: &str| {
        let mut session = make_session(sid, "u1", "chat-a");
        session.conversation_id = Some(conv.to_owned());
        session
    };

    // Binding the owner's OWN conversation succeeds.
    let ok = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-a", &with_conv("s-own", "conv-own"))
        .await
        .unwrap();
    assert_eq!(ok.conversation_id.as_deref(), Some("conv-own"));

    // Binding a DIFFERENT Core user's conversation is rejected.
    let err = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-b", &with_conv("s-other", "conv-other"))
        .await;
    assert!(
        matches!(err, Err(DbError::NotFound(_))),
        "cross-account conversation bind must be rejected, got {err:?}"
    );

    // Only the legitimate session exists — the rejected one never landed.
    let sessions = repo.get_all_sessions(OWNER_ID).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "s-own");
}

// ── PC-1: Same user, different chatId → different sessions ───────────

#[tokio::test]
async fn pc1_same_user_different_chat_ids() {
    let (repo, _db) = repo().await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_1", "telegram"))
        .await
        .unwrap();

    let s1 = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();
    let s2 = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-b", &make_session("s2", "u1", "chat-b"))
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
    assert_eq!(repo.get_all_sessions(OWNER_ID).await.unwrap().len(), 2);
}

// ── PC-2: Different users, same chatId → different sessions ──────────

#[tokio::test]
async fn pc2_different_users_same_chat_id() {
    let (repo, _db) = repo().await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_1", "telegram"))
        .await
        .unwrap();
    repo.create_user(OWNER_ID, &make_user("u2", "tg_2", "telegram"))
        .await
        .unwrap();

    let s1 = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-x", &make_session("s1", "u1", "chat-x"))
        .await
        .unwrap();
    let s2 = repo
        .get_or_create_session(OWNER_ID, "u2", "chat-x", &make_session("s2", "u2", "chat-x"))
        .await
        .unwrap();

    assert_ne!(s1.id, s2.id);
}

// ── PC-3: Same user, same chatId → reuse session ─────────────────────

#[tokio::test]
async fn pc3_same_user_same_chat_reuses_session() {
    let (repo, _db) = repo().await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_1", "telegram"))
        .await
        .unwrap();

    let s1 = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-a", &make_session("s1", "u1", "chat-a"))
        .await
        .unwrap();

    // Second call with a different new_row id but same user+chat.
    let s2 = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-a", &make_session("s999", "u1", "chat-a"))
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
    repo.create_pairing(OWNER_ID, &pairing).await.unwrap();

    let found = repo.get_pairing_by_code(OWNER_ID, "123456").await.unwrap().unwrap();
    assert_eq!(found.expires_at - found.requested_at, 600_000);
}

// ── EC-1 / EC-2: Expired pairings cleaned up, valid ones preserved ──

#[tokio::test]
async fn expired_pairings_cleaned_up() {
    let (repo, _db) = repo().await;
    let now = aionui_common::now_ms();

    // Already expired.
    repo.create_pairing(OWNER_ID, &make_pairing("111111", "tg_1", -1000))
        .await
        .unwrap();
    // Still valid.
    repo.create_pairing(OWNER_ID, &make_pairing("222222", "tg_2", 600_000))
        .await
        .unwrap();

    let cleaned = repo.cleanup_expired_pairings(OWNER_ID, now).await.unwrap();
    assert_eq!(cleaned, 1);

    let expired = repo.get_pairing_by_code(OWNER_ID, "111111").await.unwrap().unwrap();
    assert_eq!(expired.status, "expired");

    let valid = repo.get_pairing_by_code(OWNER_ID, "222222").await.unwrap().unwrap();
    assert_eq!(valid.status, "pending");
}

// ── Pairing status transitions ───────────────────────────────────────

#[tokio::test]
async fn pairing_approve_and_reject() {
    let (repo, _db) = repo().await;
    repo.create_pairing(OWNER_ID, &make_pairing("100001", "tg_a", 600_000))
        .await
        .unwrap();
    repo.create_pairing(OWNER_ID, &make_pairing("100002", "tg_b", 600_000))
        .await
        .unwrap();

    repo.update_pairing_status(OWNER_ID, "100001", "approved")
        .await
        .unwrap();
    repo.update_pairing_status(OWNER_ID, "100002", "rejected")
        .await
        .unwrap();

    // Neither should appear in pending list.
    let pending = repo.get_pending_pairings(OWNER_ID).await.unwrap();
    assert!(pending.is_empty());

    assert_eq!(
        repo.get_pairing_by_code(OWNER_ID, "100001")
            .await
            .unwrap()
            .unwrap()
            .status,
        "approved"
    );
    assert_eq!(
        repo.get_pairing_by_code(OWNER_ID, "100002")
            .await
            .unwrap()
            .unwrap()
            .status,
        "rejected"
    );
}

// ── User list ordered by authorized_at desc ──────────────────────────

#[tokio::test]
async fn users_ordered_by_authorized_at_desc() {
    let (repo, _db) = repo().await;

    let mut u1 = make_user("u1", "tg_1", "telegram");
    u1.authorized_at = 1000;
    repo.create_user(OWNER_ID, &u1).await.unwrap();

    let mut u2 = make_user("u2", "tg_2", "telegram");
    u2.authorized_at = 2000;
    repo.create_user(OWNER_ID, &u2).await.unwrap();

    let users = repo.get_all_users(OWNER_ID).await.unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, "u2"); // more recent first
    assert_eq!(users[1].id, "u1");
}
