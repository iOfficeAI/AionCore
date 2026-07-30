use std::sync::Arc;

use aionui_api_types::{PairingRequestedPayload, UserAuthorizedPayload, WebSocketMessage};
use aionui_common::{TimestampMs, generate_id, now_ms};
use aionui_db::IChannelRepository;
use aionui_db::models::{AssistantUserRow, PairingCodeRow};
use aionui_realtime::EventBroadcaster;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::constants::{PAIRING_CLEANUP_INTERVAL, PAIRING_CODE_LENGTH, PAIRING_CODE_TTL};
use crate::error::ChannelError;
use crate::types::PairingStatus;

/// Generates a random numeric pairing code of the configured length.
///
/// Uses `getrandom` for cryptographically secure randomness.
/// Returns a zero-padded string (e.g., "003421").
pub fn generate_pairing_code() -> Result<String, ChannelError> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).map_err(|e| ChannelError::InvalidConfig(format!("RNG failure: {e}")))?;
    let num = u32::from_le_bytes(bytes) % 10u32.pow(PAIRING_CODE_LENGTH as u32);
    Ok(format!("{num:0>width$}", width = PAIRING_CODE_LENGTH))
}

/// Service for managing pairing authorization flow.
///
/// Handles:
/// - Pairing code generation and creation
/// - Approval / rejection of pairing requests
/// - Periodic cleanup of expired codes
/// - Event broadcasting to WebSocket clients
pub struct PairingService {
    repo: Arc<dyn IChannelRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl PairingService {
    pub fn new(repo: Arc<dyn IChannelRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { repo, broadcaster }
    }

    /// Creates a pairing request for an IM user.
    ///
    /// Generates a 6-digit code, stores it with a 10-minute TTL, and
    /// broadcasts a `channel.pairing-requested` event to all WebSocket
    /// clients.
    ///
    /// If the same platform user already has a pending code, that code is
    /// marked as expired before creating the new one.
    pub async fn request_pairing(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
        display_name: Option<&str>,
    ) -> Result<String, ChannelError> {
        // Expire any existing pending codes for this user
        self.expire_user_pending_codes(owner_user_id, platform_user_id, platform_type)
            .await?;

        let code = generate_pairing_code()?;
        let now = now_ms();
        let expires_at = now + PAIRING_CODE_TTL.as_millis() as TimestampMs;

        let row = PairingCodeRow {
            code: code.clone(),
            owner_user_id: owner_user_id.to_owned(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            display_name: display_name.map(String::from),
            requested_at: now,
            expires_at,
            status: PairingStatus::Pending.to_string(),
        };

        self.repo.create_pairing(owner_user_id, &row).await?;

        info!(
            owner_user_id = %owner_user_id,
            platform_user_id = %platform_user_id,
            platform_type = %platform_type,
            "pairing code created"
        );

        // Broadcast event
        let payload = PairingRequestedPayload {
            user_id: owner_user_id.to_owned(),
            code: code.clone(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            display_name: display_name.map(String::from),
            expires_at,
        };
        let value = serde_json::to_value(payload)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.pairing-requested", value));

        Ok(code)
    }

    /// Approves a pending pairing code.
    ///
    /// - Validates the code exists and is still pending + not expired
    /// - Creates an `assistant_users` record
    /// - Updates the pairing status to `approved`
    /// - Broadcasts a `channel.user-authorized` event
    pub async fn approve_pairing(&self, owner_user_id: &str, code: &str) -> Result<(), ChannelError> {
        let row = self.get_valid_pending_pairing(owner_user_id, code).await?;
        let now = now_ms();

        // Create user record
        let user_id = generate_id();
        let user_row = AssistantUserRow {
            id: user_id.clone(),
            owner_user_id: owner_user_id.to_owned(),
            platform_user_id: row.platform_user_id.clone(),
            platform_type: row.platform_type.clone(),
            display_name: row.display_name.clone(),
            authorized_at: now,
            last_active: None,
            session_id: None,
        };
        self.repo.create_user(owner_user_id, &user_row).await?;

        // Update pairing status
        self.repo
            .update_pairing_status(owner_user_id, code, &PairingStatus::Approved.to_string())
            .await?;

        info!(
            owner_user_id = %owner_user_id,
            user_id = %user_id,
            platform_user_id = %row.platform_user_id,
            "pairing approved, user created"
        );

        // Broadcast event
        let payload = UserAuthorizedPayload {
            user_id: owner_user_id.to_owned(),
            id: user_id,
            platform_user_id: row.platform_user_id,
            platform_type: row.platform_type,
            display_name: row.display_name,
        };
        let value = serde_json::to_value(payload)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.user-authorized", value));

        Ok(())
    }

    /// Rejects a pending pairing code.
    ///
    /// Validates the code exists and is still pending (not expired or
    /// already processed), then marks it as rejected.
    pub async fn reject_pairing(&self, owner_user_id: &str, code: &str) -> Result<(), ChannelError> {
        let _row = self.get_valid_pending_pairing(owner_user_id, code).await?;

        self.repo
            .update_pairing_status(owner_user_id, code, &PairingStatus::Rejected.to_string())
            .await?;

        info!(owner_user_id = %owner_user_id, "pairing rejected");
        Ok(())
    }

    /// Returns all pending (not expired) pairing requests.
    pub async fn get_pending_pairings(&self, owner_user_id: &str) -> Result<Vec<PairingCodeRow>, ChannelError> {
        let rows = self.repo.get_pending_pairings(owner_user_id).await?;
        let now = now_ms();
        // Filter out expired ones that haven't been cleaned up yet
        let active: Vec<PairingCodeRow> = rows.into_iter().filter(|r| r.expires_at > now).collect();
        Ok(active)
    }

    /// Checks whether a platform user is already authorized.
    pub async fn is_user_authorized(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
    ) -> Result<bool, ChannelError> {
        let user = self
            .repo
            .get_user_by_platform(owner_user_id, platform_user_id, platform_type)
            .await?;
        Ok(user.is_some())
    }

    /// Looks up the internal user ID for a platform user.
    ///
    /// Returns `None` if the user is not authorized.
    pub async fn get_internal_user_id(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
    ) -> Result<Option<String>, ChannelError> {
        let user = self
            .repo
            .get_user_by_platform(owner_user_id, platform_user_id, platform_type)
            .await?;
        Ok(user.map(|u| u.id))
    }

    /// Starts a background task that periodically cleans up expired
    /// pairing codes. Returns a `JoinHandle` that can be used to cancel
    /// the task on shutdown.
    pub fn start_cleanup_timer(owner_user_id: String, repo: Arc<dyn IChannelRepository>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PAIRING_CLEANUP_INTERVAL);
            loop {
                interval.tick().await;
                let now = now_ms();
                match repo.cleanup_expired_pairings(&owner_user_id, now).await {
                    Ok(count) if count > 0 => {
                        debug!(count, "cleaned up expired pairing codes");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(error = %e, "failed to clean up expired pairings");
                    }
                }
            }
        })
    }

    /// Validates that a pairing code exists, is pending, and not expired.
    async fn get_valid_pending_pairing(&self, owner_user_id: &str, code: &str) -> Result<PairingCodeRow, ChannelError> {
        let row = self
            .repo
            .get_pairing_by_code(owner_user_id, code)
            .await?
            .ok_or_else(|| ChannelError::PairingNotFound(code.to_owned()))?;

        if row.status != PairingStatus::Pending.to_string() {
            return Err(ChannelError::PairingAlreadyProcessed(code.to_owned()));
        }

        let now = now_ms();
        if row.expires_at <= now {
            // Mark as expired for consistency
            let _ = self
                .repo
                .update_pairing_status(owner_user_id, code, &PairingStatus::Expired.to_string())
                .await;
            return Err(ChannelError::PairingExpired(code.to_owned()));
        }

        Ok(row)
    }

    /// Expires any pending codes for the given platform user.
    ///
    /// Called before creating a new code to ensure only one active code
    /// per user at a time.
    async fn expire_user_pending_codes(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
    ) -> Result<(), ChannelError> {
        let pending = self.repo.get_pending_pairings(owner_user_id).await?;
        for row in pending {
            if row.platform_user_id == platform_user_id && row.platform_type == platform_type {
                self.repo
                    .update_pairing_status(owner_user_id, &row.code, &PairingStatus::Expired.to_string())
                    .await?;
                debug!(
                    owner_user_id = %owner_user_id,
                    "expired old pending code for user"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
    use aionui_db::{DbError, IChannelRepository, UpdatePluginStatusParams};
    use std::sync::Mutex;

    // ── Mock EventBroadcaster ──────────────────────────────────────────

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

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        pairings: Mutex<Vec<PairingCodeRow>>,
        users: Mutex<Vec<AssistantUserRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                pairings: Mutex::new(Vec::new()),
                users: Mutex::new(Vec::new()),
            }
        }

        fn get_pairings(&self) -> Vec<PairingCodeRow> {
            self.pairings.lock().unwrap().clone()
        }

        fn get_users(&self) -> Vec<AssistantUserRow> {
            self.users.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        // -- Plugin CRUD (unused stubs) --

        async fn get_all_plugins(&self, _owner_user_id: &str) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(vec![])
        }
        async fn get_plugin(&self, _owner_user_id: &str, _id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(None)
        }
        async fn upsert_plugin(&self, _owner_user_id: &str, _row: &ChannelPluginRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_status(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _params: &UpdatePluginStatusParams,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_plugin(&self, _owner_user_id: &str, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- User CRUD --

        async fn get_all_users(&self, _owner_user_id: &str) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(self.users.lock().unwrap().clone())
        }

        async fn get_user_by_platform(
            &self,
            _owner_user_id: &str,
            platform_user_id: &str,
            platform_type: &str,
        ) -> Result<Option<AssistantUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| u.platform_user_id == platform_user_id && u.platform_type == platform_type)
                .cloned())
        }

        async fn create_user(&self, _owner_user_id: &str, row: &AssistantUserRow) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            if users
                .iter()
                .any(|u| u.platform_user_id == row.platform_user_id && u.platform_type == row.platform_type)
            {
                return Err(DbError::Conflict("user already exists".into()));
            }
            users.push(row.clone());
            Ok(())
        }

        async fn update_user_last_active(
            &self,
            _owner_user_id: &str,
            id: &str,
            last_active: TimestampMs,
        ) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            if let Some(u) = users.iter_mut().find(|u| u.id == id) {
                u.last_active = Some(last_active);
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn delete_user(&self, _owner_user_id: &str, id: &str) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            let len_before = users.len();
            users.retain(|u| u.id != id);
            if users.len() == len_before {
                Err(DbError::NotFound(id.into()))
            } else {
                Ok(())
            }
        }

        // -- Session CRUD (unused stubs) --

        async fn get_all_sessions(&self, _owner_user_id: &str) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(vec![])
        }
        async fn get_session(&self, _owner_user_id: &str, _id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            Ok(None)
        }
        async fn get_or_create_session(
            &self,
            _owner_user_id: &str,
            _user_id: &str,
            _chat_id: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            Ok(new_row.clone())
        }
        async fn update_session_activity(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _last_activity: TimestampMs,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _conversation_id: &str,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_agent_type(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _agent_type: &str,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_user(&self, _owner_user_id: &str, _user_id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_session_by_user_chat(
            &self,
            _owner_user_id: &str,
            _user_id: &str,
            _chat_id: &str,
        ) -> Result<(), DbError> {
            Ok(())
        }

        // -- Pairing codes --

        async fn create_pairing(&self, _owner_user_id: &str, row: &PairingCodeRow) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if pairings.iter().any(|p| p.code == row.code) {
                return Err(DbError::Conflict("duplicate code".into()));
            }
            pairings.push(row.clone());
            Ok(())
        }

        async fn get_pending_pairings(&self, _owner_user_id: &str) -> Result<Vec<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }

        async fn get_pairing_by_code(
            &self,
            _owner_user_id: &str,
            code: &str,
        ) -> Result<Option<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().find(|p| p.code == code).cloned())
        }

        async fn update_pairing_status(&self, _owner_user_id: &str, code: &str, status: &str) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if let Some(p) = pairings.iter_mut().find(|p| p.code == code) {
                p.status = status.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(code.into()))
            }
        }

        async fn cleanup_expired_pairings(&self, _owner_user_id: &str, now: TimestampMs) -> Result<u64, DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            let mut count = 0u64;
            for p in pairings.iter_mut() {
                if p.status == "pending" && p.expires_at <= now {
                    p.status = "expired".into();
                    count += 1;
                }
            }
            Ok(count)
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────
    const OWNER_ID: &str = "owner-test";

    fn make_service() -> (PairingService, Arc<MockRepo>, Arc<MockBroadcaster>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster::new());
        let svc = PairingService::new(repo.clone(), broadcaster.clone());
        (svc, repo, broadcaster)
    }

    // ── generate_pairing_code ──────────────────────────────────────────

    #[test]
    fn code_has_correct_length() {
        let code = generate_pairing_code().unwrap();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
    }

    #[test]
    fn code_is_all_digits() {
        let code = generate_pairing_code().unwrap();
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn code_is_zero_padded() {
        // Generate many codes; at least some should start with '0' statistically,
        // but more importantly verify format consistency.
        for _ in 0..100 {
            let code = generate_pairing_code().unwrap();
            assert_eq!(code.len(), PAIRING_CODE_LENGTH);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn codes_are_not_all_identical() {
        let codes: std::collections::HashSet<String> = (0..50).map(|_| generate_pairing_code().unwrap()).collect();
        // With 6-digit codes, 50 random samples should produce > 1 unique
        assert!(codes.len() > 1);
    }

    // ── request_pairing ────────────────────────────────────────────────

    #[tokio::test]
    async fn request_pairing_creates_code() {
        let (svc, repo, _bc) = make_service();
        let code = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);

        let pairings = repo.get_pairings();
        assert_eq!(pairings.len(), 1);
        assert_eq!(pairings[0].code, code);
        assert_eq!(pairings[0].platform_user_id, "tg_42");
        assert_eq!(pairings[0].platform_type, "telegram");
        assert_eq!(pairings[0].display_name.as_deref(), Some("Alice"));
        assert_eq!(pairings[0].status, "pending");
    }

    #[tokio::test]
    async fn request_pairing_broadcasts_event() {
        let (svc, _repo, bc) = make_service();
        svc.request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();

        let events = bc.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.pairing-requested");
        assert_eq!(events[0].data["platform_user_id"], "tg_42");
        assert_eq!(events[0].data["platform_type"], "telegram");
        assert_eq!(events[0].data["display_name"], "Alice");
    }

    #[tokio::test]
    async fn request_pairing_sets_correct_expiry() {
        let (svc, repo, _bc) = make_service();
        let before = now_ms();
        svc.request_pairing(OWNER_ID, "u1", "lark", None).await.unwrap();
        let after = now_ms();

        let p = &repo.get_pairings()[0];
        let expected_ttl = PAIRING_CODE_TTL.as_millis() as TimestampMs;
        assert!(p.expires_at >= before + expected_ttl);
        assert!(p.expires_at <= after + expected_ttl);
    }

    #[tokio::test]
    async fn request_pairing_expires_old_code() {
        let (svc, repo, _bc) = make_service();

        let code1 = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();
        let code2 = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();

        assert_ne!(code1, code2);

        let pairings = repo.get_pairings();
        let old = pairings.iter().find(|p| p.code == code1).unwrap();
        let new = pairings.iter().find(|p| p.code == code2).unwrap();
        assert_eq!(old.status, "expired");
        assert_eq!(new.status, "pending");
    }

    #[tokio::test]
    async fn request_pairing_no_display_name() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing(OWNER_ID, "u1", "dingtalk", None).await.unwrap();

        let pairings = repo.get_pairings();
        assert!(pairings[0].display_name.is_none());
    }

    // ── approve_pairing ────────────────────────────────────────────────

    #[tokio::test]
    async fn approve_creates_user_and_updates_status() {
        let (svc, repo, _bc) = make_service();
        let code = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();

        svc.approve_pairing(OWNER_ID, &code).await.unwrap();

        // Check pairing status
        let pairings = repo.get_pairings();
        let p = pairings.iter().find(|p| p.code == code).unwrap();
        assert_eq!(p.status, "approved");

        // Check user created
        let users = repo.get_users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].platform_user_id, "tg_42");
        assert_eq!(users[0].platform_type, "telegram");
        assert_eq!(users[0].display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn approve_broadcasts_user_authorized() {
        let (svc, _repo, bc) = make_service();
        let code = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();
        bc.take_events(); // clear request event

        svc.approve_pairing(OWNER_ID, &code).await.unwrap();

        let events = bc.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.user-authorized");
        assert_eq!(events[0].data["platform_user_id"], "tg_42");
        assert_eq!(events[0].data["platform_type"], "telegram");
        assert_eq!(events[0].data["display_name"], "Alice");
        assert!(events[0].data["id"].is_string());
    }

    #[tokio::test]
    async fn approve_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc.approve_pairing(OWNER_ID, "000000").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn approve_already_approved_returns_already_processed() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, &code).await.unwrap();

        let err = svc.approve_pairing(OWNER_ID, &code).await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    #[tokio::test]
    async fn approve_expired_code_returns_expired() {
        let (svc, repo, _bc) = make_service();
        // Manually insert an already-expired code
        let row = PairingCodeRow {
            code: "999999".into(),
            owner_user_id: OWNER_ID.into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            display_name: None,
            requested_at: 1000,
            expires_at: 1001, // long expired
            status: "pending".into(),
        };
        repo.pairings.lock().unwrap().push(row);

        let err = svc.approve_pairing(OWNER_ID, "999999").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingExpired(_)));
    }

    // ── reject_pairing ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reject_updates_status() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();

        svc.reject_pairing(OWNER_ID, &code).await.unwrap();

        let pairings = repo.get_pairings();
        let p = pairings.iter().find(|p| p.code == code).unwrap();
        assert_eq!(p.status, "rejected");
    }

    #[tokio::test]
    async fn reject_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc.reject_pairing(OWNER_ID, "000000").await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn reject_already_approved_returns_already_processed() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, &code).await.unwrap();

        let err = svc.reject_pairing(OWNER_ID, &code).await.unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    // ── get_pending_pairings ───────────────────────────────────────────

    #[tokio::test]
    async fn get_pending_filters_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert valid pending code
        svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();

        // Insert manually expired code
        let expired_row = PairingCodeRow {
            code: "000001".into(),
            owner_user_id: OWNER_ID.into(),
            platform_user_id: "u2".into(),
            platform_type: "lark".into(),
            display_name: None,
            requested_at: 1000,
            expires_at: 1001,
            status: "pending".into(),
        };
        repo.pairings.lock().unwrap().push(expired_row);

        let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].platform_user_id, "u1");
    }

    #[tokio::test]
    async fn get_pending_empty_when_none() {
        let (svc, _repo, _bc) = make_service();
        let pending = svc.get_pending_pairings(OWNER_ID).await.unwrap();
        assert!(pending.is_empty());
    }

    // ── is_user_authorized ─────────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_user_returns_false() {
        let (svc, _repo, _bc) = make_service();
        let authorized = svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap();
        assert!(!authorized);
    }

    #[tokio::test]
    async fn authorized_user_returns_true_after_approval() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, &code).await.unwrap();

        let authorized = svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap();
        assert!(authorized);
    }

    // ── cleanup_expired_pairings (via repo directly) ───────────────────

    #[tokio::test]
    async fn cleanup_marks_expired_as_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert manually expired pending code
        let expired_row = PairingCodeRow {
            code: "111111".into(),
            owner_user_id: OWNER_ID.into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            display_name: None,
            requested_at: 1000,
            expires_at: 2000,
            status: "pending".into(),
        };
        repo.pairings.lock().unwrap().push(expired_row);

        // Insert valid pending code
        svc.request_pairing(OWNER_ID, "u2", "lark", None).await.unwrap();

        let count = repo.cleanup_expired_pairings(OWNER_ID, now_ms()).await.unwrap();
        assert_eq!(count, 1);

        let pairings = repo.get_pairings();
        let expired = pairings.iter().find(|p| p.code == "111111").unwrap();
        assert_eq!(expired.status, "expired");
    }
}
