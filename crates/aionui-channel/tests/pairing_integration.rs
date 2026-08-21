//! Black-box integration tests for `PairingService`.
//!
//! Uses real SQLite (in-memory) and mock EventBroadcaster.
//! Covers test-plan items: PG-1..PG-3, AP-1..AP-6, RP-1..RP-4,
//! PP-1..PP-3, EC-1..EC-2, DC-2..DC-3, WS-1, WS-3.

use std::sync::{Arc, Mutex};

use aionui_api_types::WebSocketMessage;
use aionui_common::{TimestampMs, now_ms};
use aionui_db::models::{ChannelConnectionRow, ChannelPairingRequestRow};
use aionui_db::{DbError, IChannelRepository, SqliteChannelRepository, init_database_memory};
use aionui_realtime::EventBroadcaster;

use aionui_channel::constants::{PAIRING_CODE_LENGTH, PAIRING_CODE_TTL};
use aionui_channel::error::ChannelError;
use aionui_channel::pairing::{PairingSelector, PairingService, pairing_code_hash};

// ── Test infrastructure ─────────────────────────────────────────────
const OWNER_ID: &str = "system_default_user";
/// Fixed key so tests can recompute the hash the service stored.
const TEST_KEY: [u8; 32] = [0x42u8; 32];

fn hash(code: &str) -> String {
    pairing_code_hash(code, &TEST_KEY)
}

/// Connection ids the seeded platforms resolve to.
fn connection_id_for(plugin_key: &str) -> String {
    format!("conn-{plugin_key}")
}

fn make_connection(plugin_key: &str) -> ChannelConnectionRow {
    ChannelConnectionRow {
        id: connection_id_for(plugin_key),
        owner_user_id: OWNER_ID.into(),
        plugin_key: plugin_key.into(),
        name: format!("{plugin_key} bot"),
        enabled: true,
        config: "{}".into(),
        status: None,
        last_connected: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    }
}

/// Builds a pending request row directly, bypassing the service.
fn make_pairing_row(id: &str, code: &str, platform_user_id: &str, plugin_key: &str) -> ChannelPairingRequestRow {
    ChannelPairingRequestRow {
        id: id.into(),
        owner_user_id: OWNER_ID.into(),
        connection_id: connection_id_for(plugin_key),
        platform_user_id: platform_user_id.into(),
        platform_type: plugin_key.into(),
        display_name: None,
        code_hash: hash(code),
        status: "pending".into(),
        requested_at: 1000,
        expires_at: 1001,
        approved_channel_user_id: None,
    }
}

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

async fn setup() -> (PairingService, Arc<dyn IChannelRepository>, Arc<MockBroadcaster>) {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IChannelRepository> = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    let bc = Arc::new(MockBroadcaster::new());
    let svc = PairingService::new(repo.clone(), bc.clone(), TEST_KEY);
    // Pairing resolves a connection by plugin key, so every platform the
    // tests pair on needs one.
    for plugin_key in ["telegram", "lark"] {
        repo.upsert_connection(OWNER_ID, &make_connection(plugin_key))
            .await
            .unwrap();
    }
    // Keep db alive by leaking — test process exits anyway
    std::mem::forget(db);
    (svc, repo, bc)
}

/// Resolves a still-pending request's surrogate id from its plaintext code.
async fn pending_id(repo: &Arc<dyn IChannelRepository>, code: &str) -> String {
    repo.get_pending_pairing_by_code_hash(OWNER_ID, &hash(code))
        .await
        .unwrap()
        .unwrap()
        .id
}

// ── PG-1: Generated code is 6 digits ───────────────────────────────

#[tokio::test]
async fn pg1_code_is_six_digits() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc
        .request_pairing(OWNER_ID, "u1", "telegram", Some("Alice"))
        .await
        .unwrap();
    assert_eq!(code.len(), PAIRING_CODE_LENGTH);
    assert!(code.chars().all(|c| c.is_ascii_digit()));
}

// ── PG-2: Code expires after 10 minutes ────────────────────────────

#[tokio::test]
async fn pg2_code_expires_after_ten_minutes() {
    let (svc, repo, _bc) = setup().await;
    let before = now_ms();
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();
    let after = now_ms();

    let row = repo
        .get_pending_pairing_by_code_hash(OWNER_ID, &hash(&code))
        .await
        .unwrap()
        .unwrap();
    let ttl = PAIRING_CODE_TTL.as_millis() as TimestampMs;
    assert!(row.expires_at >= before + ttl);
    assert!(row.expires_at <= after + ttl);
    // Only the keyed hash is persisted, never the plaintext code.
    assert_eq!(row.code_hash, hash(&code));
    assert_ne!(row.code_hash, code);
}

// ── PG-3: Same user re-request expires old code ────────────────────

#[tokio::test]
async fn pg3_same_user_re_request_expires_old_code() {
    let (svc, repo, _bc) = setup().await;
    let code1 = svc
        .request_pairing(OWNER_ID, "u1", "telegram", Some("Alice"))
        .await
        .unwrap();
    let old_id = pending_id(&repo, &code1).await;
    let code2 = svc
        .request_pairing(OWNER_ID, "u1", "telegram", Some("Alice"))
        .await
        .unwrap();

    assert_ne!(code1, code2);

    let old = repo.get_pairing(OWNER_ID, &old_id).await.unwrap().unwrap();
    let new = repo
        .get_pending_pairing_by_code_hash(OWNER_ID, &hash(&code2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.status, "expired");
    assert_eq!(new.status, "pending");
    // The superseded code is no longer resolvable for approval.
    assert!(
        repo.get_pending_pairing_by_code_hash(OWNER_ID, &hash(&code1))
            .await
            .unwrap()
            .is_none()
    );
}

// ── PP-1: No pending pairings returns empty ────────────────────────

#[tokio::test]
async fn pp1_no_pending_returns_empty() {
    let (svc, _repo, _bc) = setup().await;
    let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
    assert!(pending.is_empty());
}

// ── PP-2: Multiple pending pairings returned ───────────────────────

#[tokio::test]
async fn pp2_multiple_pending_returned() {
    let (svc, _repo, _bc) = setup().await;
    svc.request_pairing(OWNER_ID, "u1", "telegram", Some("Alice"))
        .await
        .unwrap();
    svc.request_pairing(OWNER_ID, "u2", "lark", Some("Bob")).await.unwrap();

    let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
    assert_eq!(pending.len(), 2);
}

// ── PP-3: Expired pairings not in pending list ─────────────────────

#[tokio::test]
async fn pp3_expired_not_in_pending() {
    let (svc, repo, _bc) = setup().await;
    svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();

    // Insert already-expired code directly
    let expired_row = make_pairing_row("pair-stale", "000001", "u2", "lark");
    repo.create_pairing(OWNER_ID, &expired_row).await.unwrap();

    let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_user_id, "u1");
}

// ── AP-1: Approve valid pairing ────────────────────────────────────

#[tokio::test]
async fn ap1_approve_valid_pairing() {
    let (svc, repo, _bc) = setup().await;
    let code = svc
        .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();

    let id = pending_id(&repo, &code).await;
    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    // Status updated, and the request records the user it authorized.
    let row = repo.get_pairing(OWNER_ID, &id).await.unwrap().unwrap();
    assert_eq!(row.status, "approved");
    let users = repo.get_all_users(OWNER_ID).await.unwrap();
    assert_eq!(row.approved_channel_user_id.as_deref(), Some(users[0].id.as_str()));
}

// ── AP-2: Approved user appears in authorized list (DC-2) ──────────

#[tokio::test]
async fn ap2_dc2_approved_user_in_authorized_list() {
    let (svc, repo, _bc) = setup().await;
    let code = svc
        .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();
    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    let users = repo.get_all_users(OWNER_ID).await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].platform_user_id, "tg_42");
    assert_eq!(users[0].platform_type, "telegram");
    assert_eq!(users[0].connection_id, connection_id_for("telegram"));
    assert_eq!(users[0].status, "active");
    assert_eq!(users[0].display_name.as_deref(), Some("Alice"));
}

// ── AP-3: Approve nonexistent code ─────────────────────────────────

#[tokio::test]
async fn ap3_approve_nonexistent_code() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Code("000000"))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));

    // Unknown surrogate id is equally rejected.
    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Id("pair-nope"))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(id) if id == "pair-nope"));
}

// ── AP-4: Approve expired code ─────────────────────────────────────

#[tokio::test]
async fn ap4_approve_expired_code() {
    let (_svc, repo, bc) = setup().await;
    let svc = PairingService::new(repo.clone(), bc.clone(), TEST_KEY);

    let expired_row = make_pairing_row("pair-expired", "999999", "u1", "telegram");
    repo.create_pairing(OWNER_ID, &expired_row).await.unwrap();

    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Code("999999"))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingExpired(_)));
    // The stale request is marked expired, and nobody was authorized.
    assert_eq!(
        repo.get_pairing(OWNER_ID, "pair-expired")
            .await
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );
    assert!(repo.get_all_users(OWNER_ID).await.unwrap().is_empty());
}

// ── AP-5: Double approve returns already processed ─────────────────

#[tokio::test]
async fn ap5_double_approve_returns_already_processed() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();
    let id = pending_id(&_repo, &code).await;
    svc.approve_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();

    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Id(&id))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
}

// ── AP-6: Missing code field (validated by DTO layer, but test via service)

#[tokio::test]
async fn ap6_empty_code_returns_not_found() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Code(""))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));
}

// ── RP-1: Reject valid pairing ─────────────────────────────────────

#[tokio::test]
async fn rp1_reject_valid_pairing() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();

    let id = pending_id(&repo, &code).await;
    svc.reject_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    let row = repo.get_pairing(OWNER_ID, &id).await.unwrap().unwrap();
    assert_eq!(row.status, "rejected");
    // A rejection authorizes nobody.
    assert_eq!(row.approved_channel_user_id, None);
    assert!(repo.get_all_users(OWNER_ID).await.unwrap().is_empty());
}

// ── RP-2: Rejected code not in pending list ────────────────────────

#[tokio::test]
async fn rp2_rejected_not_in_pending() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();
    svc.reject_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
    assert!(pending.is_empty());
}

// ── RP-3: Reject nonexistent code ──────────────────────────────────

#[tokio::test]
async fn rp3_reject_nonexistent_code() {
    let (svc, _repo, _bc) = setup().await;
    let err = svc
        .reject_pairing(OWNER_ID, PairingSelector::Code("000000"))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingNotFound(_)));
}

// ── RP-4: Reject already approved code ─────────────────────────────

#[tokio::test]
async fn rp4_reject_already_approved() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();
    let id = pending_id(&_repo, &code).await;
    svc.approve_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();

    let err = svc
        .reject_pairing(OWNER_ID, PairingSelector::Id(&id))
        .await
        .unwrap_err();
    assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
}

// ── EC-1: Expired codes cleaned up ─────────────────────────────────

#[tokio::test]
async fn ec1_expired_codes_cleaned_up() {
    let (_svc, repo, bc) = setup().await;
    let _svc = PairingService::new(repo.clone(), bc.clone(), TEST_KEY);

    let expired_row = make_pairing_row("pair-stale", "111111", "u1", "telegram");
    repo.create_pairing(OWNER_ID, &expired_row).await.unwrap();

    let count = repo.cleanup_expired_pairings(OWNER_ID, now_ms()).await.unwrap();
    assert_eq!(count, 1);

    let row = repo.get_pairing(OWNER_ID, "pair-stale").await.unwrap().unwrap();
    assert_eq!(row.status, "expired");
}

// ── EC-2: Non-expired codes unaffected by cleanup ──────────────────

#[tokio::test]
async fn ec2_non_expired_unaffected() {
    let (svc, repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();

    let count = repo.cleanup_expired_pairings(OWNER_ID, now_ms()).await.unwrap();
    assert_eq!(count, 0);

    let row = repo
        .get_pending_pairing_by_code_hash(OWNER_ID, &hash(&code))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "pending");
}

// ── DC-3: Same platform user unique constraint ─────────────────────

#[tokio::test]
async fn dc3_same_platform_user_unique() {
    let (svc, _repo, _bc) = setup().await;

    // Approve first pairing
    let code1 = svc
        .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();
    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code1))
        .await
        .unwrap();

    // Second pairing for same user should fail on user creation (unique constraint)
    let code2 = svc
        .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();
    let err = svc
        .approve_pairing(OWNER_ID, PairingSelector::Code(&code2))
        .await
        .unwrap_err();
    // The DB rejects a second ACTIVE authorization for the same
    // (owner, connection, external user).
    assert!(
        matches!(err, ChannelError::Database(DbError::Conflict(_))),
        "expected a conflict, got {err:?}"
    );
}

// ── WS-1: Pairing request broadcasts event ─────────────────────────

#[tokio::test]
async fn ws1_pairing_request_broadcasts_event() {
    let (svc, _repo, bc) = setup().await;
    svc.request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "channel.pairing-requested");
    assert_eq!(events[0].data["platform_user_id"], "tg_42");
    assert_eq!(events[0].data["platform_type"], "telegram");
    assert_eq!(events[0].data["display_name"], "Alice");
    assert!(events[0].data["code"].is_string());
    assert!(events[0].data["expires_at"].is_number());
    // The event also carries the addressable request id.
    assert!(events[0].data["id"].is_string());
}

// ── WS-3: Approve broadcasts user-authorized event ─────────────────

#[tokio::test]
async fn ws3_approve_broadcasts_user_authorized() {
    let (svc, _repo, bc) = setup().await;
    let code = svc
        .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
        .await
        .unwrap();
    bc.take_events(); // clear request event

    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "channel.user-authorized");
    assert_eq!(events[0].data["platform_user_id"], "tg_42");
    assert_eq!(events[0].data["platform_type"], "telegram");
    assert_eq!(events[0].data["display_name"], "Alice");
    assert!(events[0].data["id"].is_string());
}

// ── is_user_authorized ─────────────────────────────────────────────

#[tokio::test]
async fn is_user_authorized_false_before_approval() {
    let (svc, _repo, _bc) = setup().await;
    assert!(!svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap());
}

#[tokio::test]
async fn is_user_authorized_true_after_approval() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    assert!(svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap());
}

#[tokio::test]
async fn is_user_authorized_different_platform_false() {
    let (svc, _repo, _bc) = setup().await;
    let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
    svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
        .await
        .unwrap();

    // Same user ID but different platform
    assert!(!svc.is_user_authorized(OWNER_ID, "tg_42", "lark").await.unwrap());
}
