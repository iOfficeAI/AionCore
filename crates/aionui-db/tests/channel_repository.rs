//! Black-box integration tests for `IChannelRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.
//! Covers test-plan items: DC-1..DC-4, PC-1..PC-3, PG-2.

use std::sync::Arc;

use aionui_db::models::{
    ChannelConnectionRow, ChannelConversationBindingRow, ChannelPairingRequestRow, ChannelUserRow,
};
use aionui_db::{
    DbError, IChannelRepository, SqliteChannelRepository, UpdateConnectionStatusParams, init_database_memory,
};

const OWNER_ID: &str = "system_default_user";
/// Connection every channel user / pairing request in this file attaches to.
const TG_CONN: &str = "conn-telegram";

async fn repo() -> (Arc<dyn IChannelRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    (r as Arc<dyn IChannelRepository>, db)
}

/// Seeds the FK-parent connection channel users and pairings hang off.
async fn seed_connection(repo: &Arc<dyn IChannelRepository>) {
    repo.upsert_connection(OWNER_ID, &make_plugin(TG_CONN, "telegram"))
        .await
        .unwrap();
}

fn make_plugin(id: &str, plugin_type: &str) -> ChannelConnectionRow {
    let now = aionui_common::now_ms();
    ChannelConnectionRow {
        id: id.into(),
        owner_user_id: OWNER_ID.into(),
        plugin_key: plugin_type.into(),
        name: format!("{plugin_type} bot"),
        enabled: false,
        config: r#"{"credentials":{}}"#.into(),
        status: None,
        last_connected: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_user(id: &str, platform_uid: &str, platform: &str) -> ChannelUserRow {
    let now = aionui_common::now_ms();
    ChannelUserRow {
        id: id.into(),
        owner_user_id: OWNER_ID.into(),
        connection_id: TG_CONN.into(),
        platform_user_id: platform_uid.into(),
        // Derived from the connection on read; ignored on write.
        platform_type: platform.into(),
        display_name: Some(format!("User {id}")),
        status: "active".into(),
        revoked_at: None,
        authorized_at: now,
        last_active: None,
    }
}

/// `owner_user_id`/`connection_id` are left empty: the repository derives
/// both from the active `channel_users` row rather than trusting the caller.
fn make_session(id: &str, user_id: &str, chat_id: &str) -> ChannelConversationBindingRow {
    let now = aionui_common::now_ms();
    ChannelConversationBindingRow {
        id: id.into(),
        owner_user_id: String::new(),
        connection_id: String::new(),
        user_id: user_id.into(),
        chat_id: Some(chat_id.into()),
        conversation_id: None,
        created_at: now,
        last_activity: now,
    }
}

/// Builds a pairing request. `code` is only ever hashed — the plaintext is
/// never persisted, so the row carries `code_hash` derived from it here.
fn make_pairing(id: &str, code: &str, platform_uid: &str, expires_offset_ms: i64) -> ChannelPairingRequestRow {
    let now = aionui_common::now_ms();
    ChannelPairingRequestRow {
        id: id.into(),
        owner_user_id: OWNER_ID.into(),
        connection_id: TG_CONN.into(),
        platform_user_id: platform_uid.into(),
        // Derived from the connection on read; ignored on write.
        platform_type: "telegram".into(),
        display_name: Some("Tester".into()),
        code_hash: format!("hash-{code}"),
        status: "pending".into(),
        requested_at: now,
        expires_at: now + expires_offset_ms,
        approved_channel_user_id: None,
    }
}

// ── Plugin integration tests ─────────────────────────────────────────

#[tokio::test]
async fn plugin_full_lifecycle() {
    let (repo, _db) = repo().await;

    // Empty initially.
    assert!(repo.get_all_connections(OWNER_ID).await.unwrap().is_empty());

    // Create two plugins.
    repo.upsert_connection(OWNER_ID, &make_plugin("tg-1", "telegram"))
        .await
        .unwrap();
    repo.upsert_connection(OWNER_ID, &make_plugin("lark-1", "lark"))
        .await
        .unwrap();
    assert_eq!(repo.get_all_connections(OWNER_ID).await.unwrap().len(), 2);

    // Update status.
    repo.update_connection_status(
        OWNER_ID,
        "tg-1",
        &UpdateConnectionStatusParams {
            status: Some("running".into()),
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let tg = repo.get_connection(OWNER_ID, "tg-1").await.unwrap().unwrap();
    assert!(tg.enabled);
    assert_eq!(tg.status.as_deref(), Some("running"));

    // Delete one.
    repo.delete_connection(OWNER_ID, "lark-1").await.unwrap();
    assert_eq!(repo.get_all_connections(OWNER_ID).await.unwrap().len(), 1);
}

#[tokio::test]
async fn phase1_single_connection_per_plugin_key_enforced() {
    let (repo, _db) = repo().await;
    repo.upsert_connection(OWNER_ID, &make_plugin("conn-a", "telegram"))
        .await
        .unwrap();

    // A second connection for the same (owner, plugin_key) violates the
    // phase-1 single-instance unique index.
    let err = repo
        .upsert_connection(OWNER_ID, &make_plugin("conn-b", "telegram"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "unexpected error: {err}");

    // Plugin-key lookup resolves the single instance.
    let found = repo
        .get_connection_by_plugin_key(OWNER_ID, "telegram")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, "conn-a");
    assert!(
        repo.get_connection_by_plugin_key(OWNER_ID, "lark")
            .await
            .unwrap()
            .is_none()
    );
}

// ── DC-3: Same platform user uniqueness constraint ───────────────────

#[tokio::test]
async fn dc3_duplicate_platform_user_rejected() {
    let (repo, _db) = repo().await;
    seed_connection(&repo).await;
    repo.create_user(OWNER_ID, &make_user("u1", "tg_100", "telegram"))
        .await
        .unwrap();

    // Same platform_user_id + platform_type with different id.
    let dup = make_user("u2", "tg_100", "telegram");
    let err = repo.create_user(OWNER_ID, &dup).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));
}

// ── DC-1: Revoking a user removes their sessions ─────────────────────
//
// Revocation is a soft delete, so the sessions no longer disappear via FK
// cascade — `revoke_user` deletes them explicitly. The authorization row
// itself is retained for audit.

#[tokio::test]
async fn dc1_revoke_user_removes_sessions() {
    let (repo, db) = repo().await;
    seed_connection(&repo).await;
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

    repo.revoke_user(OWNER_ID, "u1").await.unwrap();

    assert!(repo.get_all_sessions(OWNER_ID).await.unwrap().is_empty());
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_conversation_bindings WHERE channel_user_id = 'u1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(remaining, 0);

    // Revocation is also forward-looking: a revoked user cannot open a new
    // binding, so message routing cannot resurrect itself.
    let err = repo
        .get_or_create_session(OWNER_ID, "u1", "chat-c", &make_session("s3", "u1", "chat-c"))
        .await;
    assert!(
        matches!(err, Err(DbError::NotFound(_))),
        "revoked user must not create a binding, got {err:?}"
    );
    assert!(repo.get_all_sessions(OWNER_ID).await.unwrap().is_empty());

    // The user is gone from the active surface …
    assert!(repo.get_all_users(OWNER_ID).await.unwrap().is_empty());
    assert!(
        repo.get_user_by_platform(OWNER_ID, "tg_1", "telegram")
            .await
            .unwrap()
            .is_none()
    );
    // … but the audit row survives.
    let (status, revoked_at): (String, Option<i64>) =
        sqlx::query_as("SELECT status, revoked_at FROM channel_users WHERE id = 'u1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(status, "revoked");
    assert!(revoked_at.is_some());
}

// ── Cross-account guard: a channel session may not bind another Core user's
//    conversation. The INSERT's ownership predicate matches zero rows, so the
//    session is never created and no data leaks across accounts.
#[tokio::test]
async fn session_rejects_conversation_owned_by_another_core_user() {
    let (repo, db) = repo().await;
    let pool = db.pool();

    seed_connection(&repo).await;
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
    seed_connection(&repo).await;
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

    // Both bindings carry the identity derived from the channel user, not the
    // empty owner/connection the caller passed in.
    for s in [&s1, &s2] {
        assert_eq!(s.owner_user_id, OWNER_ID);
        assert_eq!(s.connection_id, TG_CONN);
    }
}

// ── PC-2: Different users, same chatId → different sessions ──────────

#[tokio::test]
async fn pc2_different_users_same_chat_id() {
    let (repo, _db) = repo().await;
    seed_connection(&repo).await;
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
    seed_connection(&repo).await;
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
    seed_connection(&repo).await;
    let pairing = make_pairing("p1", "123456", "tg_99", 600_000);
    repo.create_pairing(OWNER_ID, &pairing).await.unwrap();

    let found = repo.get_pairing(OWNER_ID, "p1").await.unwrap().unwrap();
    assert_eq!(found.expires_at - found.requested_at, 600_000);

    // The code is addressable only through its hash while pending.
    let by_hash = repo
        .get_pending_pairing_by_code_hash(OWNER_ID, "hash-123456")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_hash.id, "p1");
}

// ── EC-1 / EC-2: Expired pairings cleaned up, valid ones preserved ──

#[tokio::test]
async fn expired_pairings_cleaned_up() {
    let (repo, _db) = repo().await;
    seed_connection(&repo).await;
    let now = aionui_common::now_ms();

    // Already expired.
    repo.create_pairing(OWNER_ID, &make_pairing("p-old", "111111", "tg_1", -1000))
        .await
        .unwrap();
    // Still valid.
    repo.create_pairing(OWNER_ID, &make_pairing("p-new", "222222", "tg_2", 600_000))
        .await
        .unwrap();

    let cleaned = repo.cleanup_expired_pairings(OWNER_ID, now).await.unwrap();
    assert_eq!(cleaned, 1);

    let expired = repo.get_pairing(OWNER_ID, "p-old").await.unwrap().unwrap();
    assert_eq!(expired.status, "expired");

    let valid = repo.get_pairing(OWNER_ID, "p-new").await.unwrap().unwrap();
    assert_eq!(valid.status, "pending");
}

// ── Pairing status transitions ───────────────────────────────────────

#[tokio::test]
async fn pairing_approve_and_reject() {
    let (repo, _db) = repo().await;
    seed_connection(&repo).await;
    repo.create_user(OWNER_ID, &make_user("u-approved", "tg_a", "telegram"))
        .await
        .unwrap();
    repo.create_pairing(OWNER_ID, &make_pairing("p-a", "100001", "tg_a", 600_000))
        .await
        .unwrap();
    repo.create_pairing(OWNER_ID, &make_pairing("p-b", "100002", "tg_b", 600_000))
        .await
        .unwrap();

    repo.update_pairing_status(OWNER_ID, "p-a", "approved", Some("u-approved"))
        .await
        .unwrap();
    repo.update_pairing_status(OWNER_ID, "p-b", "rejected", None)
        .await
        .unwrap();

    // Neither should appear in pending list.
    let pending = repo.get_pending_pairings(OWNER_ID).await.unwrap();
    assert!(pending.is_empty());

    let approved = repo.get_pairing(OWNER_ID, "p-a").await.unwrap().unwrap();
    assert_eq!(approved.status, "approved");
    // An approval records which channel user it created.
    assert_eq!(approved.approved_channel_user_id.as_deref(), Some("u-approved"));

    let rejected = repo.get_pairing(OWNER_ID, "p-b").await.unwrap().unwrap();
    assert_eq!(rejected.status, "rejected");
    assert_eq!(rejected.approved_channel_user_id, None);

    // Processed requests are no longer resolvable by code hash.
    assert!(
        repo.get_pending_pairing_by_code_hash(OWNER_ID, "hash-100001")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.get_pending_pairing_by_code_hash(OWNER_ID, "hash-100002")
            .await
            .unwrap()
            .is_none()
    );
}

// ── User list ordered by authorized_at desc ──────────────────────────

#[tokio::test]
async fn users_ordered_by_authorized_at_desc() {
    let (repo, _db) = repo().await;
    seed_connection(&repo).await;

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
