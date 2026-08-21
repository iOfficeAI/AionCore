use std::sync::Arc;

use aionui_api_types::{PairingRequestedPayload, UserAuthorizedPayload, WebSocketMessage};
use aionui_common::{TimestampMs, generate_id, generate_prefixed_id, now_ms};
use aionui_db::IChannelRepository;
use aionui_db::models::{ChannelPairingRequestRow, ChannelUserRow};
use aionui_realtime::EventBroadcaster;
use hmac::{Hmac, Mac};
use sha2::Sha256;
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

/// Server-side keyed hash of a pairing code (hex-encoded HMAC-SHA256).
///
/// Only this hash is persisted; the plaintext code lives exclusively in the
/// transient flow (IM reply + WebSocket event). The short numeric code is
/// protected against offline brute force by the server-side key.
pub fn pairing_code_hash(code: &str, key: &[u8; 32]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(code.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// How an approval/rejection identifies the pairing request:
/// by the surrogate id (cold-loaded pending list) or by the plaintext code
/// (transient WS event / manual entry), which is hashed for lookup.
#[derive(Debug, Clone)]
pub enum PairingSelector<'a> {
    Id(&'a str),
    Code(&'a str),
}

/// Service for managing pairing authorization flow.
///
/// Handles:
/// - Pairing code generation and creation (hashed at rest)
/// - Approval / rejection of pairing requests
/// - Periodic cleanup of expired codes
/// - Event broadcasting to WebSocket clients
pub struct PairingService {
    repo: Arc<dyn IChannelRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Key for `pairing_code_hash`; shares the channel credential key.
    code_hash_key: [u8; 32],
}

impl PairingService {
    pub fn new(
        repo: Arc<dyn IChannelRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        code_hash_key: [u8; 32],
    ) -> Self {
        Self {
            repo,
            broadcaster,
            code_hash_key,
        }
    }

    /// Creates a pairing request for an IM user.
    ///
    /// Generates a 6-digit code, stores its HMAC with a 10-minute TTL, and
    /// broadcasts a `channel.pairing-requested` event to all WebSocket
    /// clients (the event carries the transient plaintext code).
    ///
    /// The request attaches to the owner's connection for the platform; a
    /// platform without a configured connection cannot pair. If the same
    /// platform user already has a pending code, it is expired first.
    pub async fn request_pairing(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
        display_name: Option<&str>,
    ) -> Result<String, ChannelError> {
        let connection = self
            .repo
            .get_connection_by_plugin_key(owner_user_id, platform_type)
            .await?
            .ok_or_else(|| ChannelError::PluginNotFound(platform_type.to_owned()))?;

        // Expire any existing pending codes for this user
        self.repo
            .expire_pending_pairings_for_user(owner_user_id, &connection.id, platform_user_id)
            .await?;

        let code = generate_pairing_code()?;
        let now = now_ms();
        let expires_at = now + PAIRING_CODE_TTL.as_millis() as TimestampMs;

        let request_id = generate_prefixed_id("pair");
        let row = ChannelPairingRequestRow {
            id: request_id.clone(),
            owner_user_id: owner_user_id.to_owned(),
            connection_id: connection.id.clone(),
            platform_user_id: platform_user_id.to_owned(),
            platform_type: platform_type.to_owned(),
            display_name: display_name.map(String::from),
            code_hash: pairing_code_hash(&code, &self.code_hash_key),
            status: PairingStatus::Pending.to_string(),
            requested_at: now,
            expires_at,
            approved_channel_user_id: None,
        };

        self.repo.create_pairing(owner_user_id, &row).await?;

        info!(
            owner_user_id = %owner_user_id,
            platform_user_id = %platform_user_id,
            platform_type = %platform_type,
            connection_id = %connection.id,
            pairing_id = %request_id,
            "pairing code created"
        );

        // Broadcast event (transient plaintext code + addressable id)
        let payload = PairingRequestedPayload {
            user_id: owner_user_id.to_owned(),
            id: request_id,
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

    /// Approves a pending pairing request (by id or code).
    ///
    /// - Validates the request exists and is still pending + not expired
    /// - Creates (or reactivates) the `channel_users` record
    /// - Updates the pairing status to `approved`, recording the user
    /// - Broadcasts a `channel.user-authorized` event
    pub async fn approve_pairing(
        &self,
        owner_user_id: &str,
        selector: PairingSelector<'_>,
    ) -> Result<(), ChannelError> {
        let row = self.get_valid_pending_pairing(owner_user_id, selector).await?;
        let now = now_ms();

        // Create user record bound to the pairing request's connection
        let user_id = generate_id();
        let user_row = ChannelUserRow {
            id: user_id.clone(),
            owner_user_id: owner_user_id.to_owned(),
            connection_id: row.connection_id.clone(),
            platform_user_id: row.platform_user_id.clone(),
            platform_type: row.platform_type.clone(),
            display_name: row.display_name.clone(),
            status: "active".into(),
            revoked_at: None,
            authorized_at: now,
            last_active: None,
        };
        self.repo.create_user(owner_user_id, &user_row).await?;

        // The created id may differ when a revoked row was reactivated —
        // resolve the effective row for the event + audit linkage.
        let effective = self
            .repo
            .get_user_by_platform(owner_user_id, &row.platform_user_id, &row.platform_type)
            .await?
            .ok_or_else(|| ChannelError::PairingNotFound(row.id.clone()))?;

        self.repo
            .update_pairing_status(
                owner_user_id,
                &row.id,
                &PairingStatus::Approved.to_string(),
                Some(&effective.id),
            )
            .await?;

        info!(
            owner_user_id = %owner_user_id,
            user_id = %effective.id,
            platform_user_id = %row.platform_user_id,
            pairing_id = %row.id,
            "pairing approved, user created"
        );

        // Broadcast event
        let payload = UserAuthorizedPayload {
            user_id: owner_user_id.to_owned(),
            id: effective.id,
            platform_user_id: row.platform_user_id,
            platform_type: row.platform_type,
            display_name: row.display_name,
        };
        let value = serde_json::to_value(payload)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.user-authorized", value));

        Ok(())
    }

    /// Rejects a pending pairing request (by id or code).
    ///
    /// Validates the request exists and is still pending (not expired or
    /// already processed), then marks it as rejected.
    pub async fn reject_pairing(&self, owner_user_id: &str, selector: PairingSelector<'_>) -> Result<(), ChannelError> {
        let row = self.get_valid_pending_pairing(owner_user_id, selector).await?;

        self.repo
            .update_pairing_status(owner_user_id, &row.id, &PairingStatus::Rejected.to_string(), None)
            .await?;

        info!(owner_user_id = %owner_user_id, pairing_id = %row.id, "pairing rejected");
        Ok(())
    }

    /// Returns all pending (not expired) pairing requests.
    pub async fn get_pending_pairings(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<ChannelPairingRequestRow>, ChannelError> {
        let rows = self.repo.get_pending_pairings(owner_user_id).await?;
        let now = now_ms();
        // Filter out expired ones that haven't been cleaned up yet
        let active: Vec<ChannelPairingRequestRow> = rows.into_iter().filter(|r| r.expires_at > now).collect();
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

    /// Resolves a pending pairing request from a selector and validates it
    /// is still pending and not expired.
    async fn get_valid_pending_pairing(
        &self,
        owner_user_id: &str,
        selector: PairingSelector<'_>,
    ) -> Result<ChannelPairingRequestRow, ChannelError> {
        let row = match selector {
            PairingSelector::Id(id) => self
                .repo
                .get_pairing(owner_user_id, id)
                .await?
                .ok_or_else(|| ChannelError::PairingNotFound(id.to_owned()))?,
            PairingSelector::Code(code) => {
                let hash = pairing_code_hash(code, &self.code_hash_key);
                self.repo
                    .get_pending_pairing_by_code_hash(owner_user_id, &hash)
                    .await?
                    // The plaintext code is not persisted; report the lookup
                    // failure without echoing the code itself.
                    .ok_or_else(|| ChannelError::PairingNotFound("<code>".to_owned()))?
            }
        };

        if row.status != PairingStatus::Pending.to_string() {
            return Err(ChannelError::PairingAlreadyProcessed(row.id.clone()));
        }

        let now = now_ms();
        if row.expires_at <= now {
            // Mark as expired for consistency
            let _ = self
                .repo
                .update_pairing_status(owner_user_id, &row.id, &PairingStatus::Expired.to_string(), None)
                .await;
            return Err(ChannelError::PairingExpired(row.id.clone()));
        }

        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::{ChannelConnectionRow, ChannelConversationBindingRow};
    use aionui_db::{DbError, IChannelRepository, UpdateConnectionStatusParams};
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
        connections: Mutex<Vec<ChannelConnectionRow>>,
        pairings: Mutex<Vec<ChannelPairingRequestRow>>,
        users: Mutex<Vec<ChannelUserRow>>,
    }

    impl MockRepo {
        /// Starts with one connection per platform the tests pair on —
        /// pairing now resolves a connection by plugin key and refuses
        /// platforms that have none.
        fn new() -> Self {
            let connections = ["telegram", "lark", "dingtalk"]
                .into_iter()
                .map(|plugin_key| ChannelConnectionRow {
                    id: format!("conn-{plugin_key}"),
                    owner_user_id: OWNER_ID.into(),
                    plugin_key: plugin_key.into(),
                    name: format!("{plugin_key} bot"),
                    enabled: true,
                    config: "{}".into(),
                    status: None,
                    last_connected: None,
                    created_at: 0,
                    updated_at: 0,
                })
                .collect();

            Self {
                connections: Mutex::new(connections),
                pairings: Mutex::new(Vec::new()),
                users: Mutex::new(Vec::new()),
            }
        }

        fn get_pairings(&self) -> Vec<ChannelPairingRequestRow> {
            self.pairings.lock().unwrap().clone()
        }

        fn get_users(&self) -> Vec<ChannelUserRow> {
            self.users.lock().unwrap().clone()
        }

        /// Finds the request whose stored hash matches `code`'s hash.
        fn find_by_code(&self, code: &str) -> Option<ChannelPairingRequestRow> {
            let wanted = hash(code);
            self.get_pairings().into_iter().find(|p| p.code_hash == wanted)
        }

        /// Resolves the platform of a request through its connection, the
        /// way the SQL implementation's JOIN does.
        fn platform_of(&self, connection_id: &str) -> String {
            self.connections
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.id == connection_id)
                .map(|c| c.plugin_key.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        // -- Connection CRUD --

        async fn get_all_connections(&self, _owner_user_id: &str) -> Result<Vec<ChannelConnectionRow>, DbError> {
            Ok(self.connections.lock().unwrap().clone())
        }
        async fn get_connection(
            &self,
            _owner_user_id: &str,
            id: &str,
        ) -> Result<Option<ChannelConnectionRow>, DbError> {
            Ok(self.connections.lock().unwrap().iter().find(|c| c.id == id).cloned())
        }

        async fn get_connection_by_plugin_key(
            &self,
            _owner_user_id: &str,
            plugin_key: &str,
        ) -> Result<Option<ChannelConnectionRow>, DbError> {
            Ok(self
                .connections
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.plugin_key == plugin_key)
                .cloned())
        }
        async fn upsert_connection(&self, _owner_user_id: &str, row: &ChannelConnectionRow) -> Result<(), DbError> {
            let mut connections = self.connections.lock().unwrap();
            connections.retain(|c| c.id != row.id);
            connections.push(row.clone());
            Ok(())
        }
        async fn update_connection_status(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _params: &UpdateConnectionStatusParams,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_connection(&self, _owner_user_id: &str, id: &str) -> Result<(), DbError> {
            self.connections.lock().unwrap().retain(|c| c.id != id);
            Ok(())
        }

        // -- User CRUD --

        async fn get_all_users(&self, _owner_user_id: &str) -> Result<Vec<ChannelUserRow>, DbError> {
            Ok(self
                .users
                .lock()
                .unwrap()
                .iter()
                .filter(|u| u.status == "active")
                .cloned()
                .collect())
        }

        async fn get_user_by_platform(
            &self,
            _owner_user_id: &str,
            platform_user_id: &str,
            platform_type: &str,
        ) -> Result<Option<ChannelUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| {
                    u.status == "active"
                        && u.platform_user_id == platform_user_id
                        && self.platform_of(&u.connection_id) == platform_type
                })
                .cloned())
        }

        async fn create_user(&self, _owner_user_id: &str, row: &ChannelUserRow) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            let existing = users
                .iter_mut()
                .find(|u| u.connection_id == row.connection_id && u.platform_user_id == row.platform_user_id);
            match existing {
                Some(u) if u.status == "active" => Err(DbError::Conflict("user already exists".into())),
                Some(u) => {
                    // Reactivate the revoked authorization in place.
                    u.status = "active".into();
                    u.revoked_at = None;
                    u.display_name = row.display_name.clone();
                    u.authorized_at = row.authorized_at;
                    u.last_active = row.last_active;
                    Ok(())
                }
                None => {
                    users.push(row.clone());
                    Ok(())
                }
            }
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

        async fn revoke_user(&self, _owner_user_id: &str, id: &str) -> Result<(), DbError> {
            let mut users = self.users.lock().unwrap();
            match users.iter_mut().find(|u| u.id == id && u.status == "active") {
                Some(u) => {
                    // Soft delete: the audit row stays, marked revoked.
                    u.status = "revoked".into();
                    u.revoked_at = Some(now_ms());
                    Ok(())
                }
                None => Err(DbError::NotFound(id.into())),
            }
        }

        // -- Session CRUD (unused stubs) --

        async fn get_all_sessions(&self, _owner_user_id: &str) -> Result<Vec<ChannelConversationBindingRow>, DbError> {
            Ok(vec![])
        }
        async fn get_session(
            &self,
            _owner_user_id: &str,
            _id: &str,
        ) -> Result<Option<ChannelConversationBindingRow>, DbError> {
            Ok(None)
        }
        async fn get_or_create_session(
            &self,
            owner_user_id: &str,
            _user_id: &str,
            _chat_id: &str,
            new_row: &ChannelConversationBindingRow,
        ) -> Result<ChannelConversationBindingRow, DbError> {
            // Mirror the real INSERT: identity comes from the channel user.
            Ok(ChannelConversationBindingRow {
                owner_user_id: owner_user_id.to_owned(),
                connection_id: STUB_CONNECTION_ID.to_owned(),
                ..new_row.clone()
            })
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

        // -- Pairing requests --

        async fn create_pairing(&self, _owner_user_id: &str, row: &ChannelPairingRequestRow) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            // Mirrors the partial unique indexes: one pending request per
            // (connection, external user) and per code hash.
            if pairings.iter().any(|p| {
                p.status == "pending"
                    && (p.code_hash == row.code_hash
                        || (p.connection_id == row.connection_id && p.platform_user_id == row.platform_user_id))
            }) {
                return Err(DbError::Conflict("duplicate pending pairing request".into()));
            }
            pairings.push(row.clone());
            Ok(())
        }

        async fn get_pending_pairings(&self, _owner_user_id: &str) -> Result<Vec<ChannelPairingRequestRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }

        async fn get_pairing(
            &self,
            _owner_user_id: &str,
            id: &str,
        ) -> Result<Option<ChannelPairingRequestRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().find(|p| p.id == id).cloned())
        }

        async fn get_pending_pairing_by_code_hash(
            &self,
            _owner_user_id: &str,
            code_hash: &str,
        ) -> Result<Option<ChannelPairingRequestRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings
                .iter()
                .find(|p| p.code_hash == code_hash && p.status == "pending")
                .cloned())
        }

        async fn update_pairing_status(
            &self,
            _owner_user_id: &str,
            id: &str,
            status: &str,
            approved_channel_user_id: Option<&str>,
        ) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if let Some(p) = pairings.iter_mut().find(|p| p.id == id) {
                p.status = status.to_owned();
                if let Some(user_id) = approved_channel_user_id {
                    p.approved_channel_user_id = Some(user_id.to_owned());
                }
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn expire_pending_pairings_for_user(
            &self,
            _owner_user_id: &str,
            connection_id: &str,
            external_user_id: &str,
        ) -> Result<u64, DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            let mut count = 0u64;
            for p in pairings.iter_mut() {
                if p.status == "pending" && p.connection_id == connection_id && p.platform_user_id == external_user_id {
                    p.status = "expired".into();
                    count += 1;
                }
            }
            Ok(count)
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
    /// Connection the stub binding CRUD derives its `connection_id` from.
    const STUB_CONNECTION_ID: &str = "conn-test";
    /// Fixed key so tests can recompute the hash the service stored.
    const TEST_KEY: [u8; 32] = [0x42u8; 32];

    fn hash(code: &str) -> String {
        pairing_code_hash(code, &TEST_KEY)
    }

    fn make_service() -> (PairingService, Arc<MockRepo>, Arc<MockBroadcaster>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster::new());
        let svc = PairingService::new(repo.clone(), broadcaster.clone(), TEST_KEY);
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

    // ── pairing_code_hash ──────────────────────────────────────────────

    #[test]
    fn code_hash_is_deterministic_and_key_dependent() {
        let other_key = [0x11u8; 32];
        assert_eq!(hash("123456"), hash("123456"));
        assert_ne!(hash("123456"), hash("123457"));
        assert_ne!(hash("123456"), pairing_code_hash("123456", &other_key));
        // Hex-encoded SHA-256 output, and never the plaintext itself.
        assert_eq!(hash("123456").len(), 64);
        assert_ne!(hash("123456"), "123456");
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
        // Only the hash is persisted; the plaintext never reaches the row.
        assert_eq!(pairings[0].code_hash, hash(&code));
        assert_ne!(pairings[0].code_hash, code);
        assert_eq!(pairings[0].connection_id, "conn-telegram");
        assert_eq!(pairings[0].platform_user_id, "tg_42");
        assert_eq!(pairings[0].platform_type, "telegram");
        assert_eq!(pairings[0].display_name.as_deref(), Some("Alice"));
        assert_eq!(pairings[0].status, "pending");
        assert!(!pairings[0].id.is_empty());
        assert_eq!(pairings[0].approved_channel_user_id, None);
    }

    #[tokio::test]
    async fn request_pairing_without_connection_is_rejected() {
        let (svc, repo, _bc) = make_service();
        repo.delete_connection(OWNER_ID, "conn-telegram").await.unwrap();

        let err = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(platform) if platform == "telegram"));
        assert!(repo.get_pairings().is_empty());
    }

    #[tokio::test]
    async fn request_pairing_broadcasts_event() {
        let (svc, repo, bc) = make_service();
        let code = svc
            .request_pairing(OWNER_ID, "tg_42", "telegram", Some("Alice"))
            .await
            .unwrap();

        let events = bc.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "channel.pairing-requested");
        assert_eq!(events[0].data["platform_user_id"], "tg_42");
        assert_eq!(events[0].data["platform_type"], "telegram");
        assert_eq!(events[0].data["display_name"], "Alice");
        // The event carries the transient plaintext code plus the id the
        // cold-loaded pending list uses.
        assert_eq!(events[0].data["code"], code);
        assert_eq!(events[0].data["id"], repo.get_pairings()[0].id);
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

        let old = repo.find_by_code(&code1).unwrap();
        let new = repo.find_by_code(&code2).unwrap();
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

        svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        // Check user created
        let users = repo.get_users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].platform_user_id, "tg_42");
        assert_eq!(users[0].connection_id, "conn-telegram");
        assert_eq!(users[0].status, "active");
        assert_eq!(users[0].display_name.as_deref(), Some("Alice"));

        // Check pairing status, and that it links the user it authorized.
        let p = repo.find_by_code(&code).unwrap();
        assert_eq!(p.status, "approved");
        assert_eq!(p.approved_channel_user_id.as_deref(), Some(users[0].id.as_str()));
    }

    #[tokio::test]
    async fn approve_by_id_selector_works() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        let id = repo.get_pairings()[0].id.clone();

        svc.approve_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();

        assert_eq!(repo.get_pairings()[0].status, "approved");
        assert_eq!(repo.get_users().len(), 1);
    }

    #[tokio::test]
    async fn approve_reactivates_a_revoked_user() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        let first_id = repo.get_users()[0].id.clone();
        repo.revoke_user(OWNER_ID, &first_id).await.unwrap();

        // Pairing again re-authorizes the same identity rather than
        // creating a second authorization row.
        let code2 = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code2))
            .await
            .unwrap();

        let users = repo.get_users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].id, first_id);
        assert_eq!(users[0].status, "active");
        assert_eq!(users[0].revoked_at, None);

        // The second request records the reactivated user, not a new id.
        let p = repo.find_by_code(&code2).unwrap();
        assert_eq!(p.approved_channel_user_id.as_deref(), Some(first_id.as_str()));
    }

    #[tokio::test]
    async fn approve_broadcasts_user_authorized() {
        let (svc, repo, bc) = make_service();
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
        assert_eq!(events[0].data["id"], repo.get_users()[0].id);
    }

    #[tokio::test]
    async fn approve_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc
            .approve_pairing(OWNER_ID, PairingSelector::Code("000000"))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn approve_nonexistent_id_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc
            .approve_pairing(OWNER_ID, PairingSelector::Id("pair-nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(id) if id == "pair-nope"));
    }

    /// A rejected code must not be replayable: the code-hash lookup only
    /// resolves pending requests.
    #[tokio::test]
    async fn approve_rejected_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.reject_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        let err = svc
            .approve_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn approve_already_approved_returns_already_processed() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        let id = repo.get_pairings()[0].id.clone();
        svc.approve_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();

        let err = svc
            .approve_pairing(OWNER_ID, PairingSelector::Id(&id))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    #[tokio::test]
    async fn approve_expired_code_returns_expired() {
        let (svc, repo, _bc) = make_service();
        // Manually insert an already-expired code
        let row = ChannelPairingRequestRow {
            id: "pair-expired".into(),
            owner_user_id: OWNER_ID.into(),
            connection_id: "conn-telegram".into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            display_name: None,
            code_hash: hash("999999"),
            status: "pending".into(),
            requested_at: 1000,
            expires_at: 1001, // long expired
            approved_channel_user_id: None,
        };
        repo.pairings.lock().unwrap().push(row);

        let err = svc
            .approve_pairing(OWNER_ID, PairingSelector::Code("999999"))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingExpired(_)));
        // The stale request is marked expired, and no user was authorized.
        assert_eq!(repo.get_pairings()[0].status, "expired");
        assert!(repo.get_users().is_empty());
    }

    // ── reject_pairing ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reject_updates_status() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();

        svc.reject_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        let p = repo.find_by_code(&code).unwrap();
        assert_eq!(p.status, "rejected");
        // A rejection authorizes nobody.
        assert_eq!(p.approved_channel_user_id, None);
        assert!(repo.get_users().is_empty());
    }

    #[tokio::test]
    async fn reject_by_id_selector_works() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        let id = repo.get_pairings()[0].id.clone();

        svc.reject_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();
        assert_eq!(repo.get_pairings()[0].status, "rejected");
    }

    #[tokio::test]
    async fn reject_nonexistent_code_returns_not_found() {
        let (svc, _repo, _bc) = make_service();
        let err = svc
            .reject_pairing(OWNER_ID, PairingSelector::Code("000000"))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingNotFound(_)));
    }

    #[tokio::test]
    async fn reject_already_approved_returns_already_processed() {
        let (svc, repo, _bc) = make_service();
        svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        let id = repo.get_pairings()[0].id.clone();
        svc.approve_pairing(OWNER_ID, PairingSelector::Id(&id)).await.unwrap();

        let err = svc
            .reject_pairing(OWNER_ID, PairingSelector::Id(&id))
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PairingAlreadyProcessed(_)));
    }

    // ── get_pending_pairings ───────────────────────────────────────────

    #[tokio::test]
    async fn get_pending_filters_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert valid pending code
        svc.request_pairing(OWNER_ID, "u1", "telegram", None).await.unwrap();

        // Insert manually expired code
        let expired_row = ChannelPairingRequestRow {
            id: "pair-stale".into(),
            owner_user_id: OWNER_ID.into(),
            connection_id: "conn-lark".into(),
            platform_user_id: "u2".into(),
            platform_type: "lark".into(),
            display_name: None,
            code_hash: hash("000001"),
            status: "pending".into(),
            requested_at: 1000,
            expires_at: 1001,
            approved_channel_user_id: None,
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
        svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        let authorized = svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap();
        assert!(authorized);
    }

    #[tokio::test]
    async fn revoked_user_is_no_longer_authorized() {
        let (svc, repo, _bc) = make_service();
        let code = svc.request_pairing(OWNER_ID, "tg_42", "telegram", None).await.unwrap();
        svc.approve_pairing(OWNER_ID, PairingSelector::Code(&code))
            .await
            .unwrap();

        let user_id = repo.get_users()[0].id.clone();
        repo.revoke_user(OWNER_ID, &user_id).await.unwrap();

        assert!(!svc.is_user_authorized(OWNER_ID, "tg_42", "telegram").await.unwrap());
        assert!(
            svc.get_internal_user_id(OWNER_ID, "tg_42", "telegram")
                .await
                .unwrap()
                .is_none()
        );
    }

    // ── cleanup_expired_pairings (via repo directly) ───────────────────

    #[tokio::test]
    async fn cleanup_marks_expired_as_expired() {
        let (svc, repo, _bc) = make_service();

        // Insert manually expired pending code
        let expired_row = ChannelPairingRequestRow {
            id: "pair-stale".into(),
            owner_user_id: OWNER_ID.into(),
            connection_id: "conn-telegram".into(),
            platform_user_id: "u1".into(),
            platform_type: "telegram".into(),
            display_name: None,
            code_hash: hash("111111"),
            status: "pending".into(),
            requested_at: 1000,
            expires_at: 2000,
            approved_channel_user_id: None,
        };
        repo.pairings.lock().unwrap().push(expired_row);

        // Insert valid pending code
        svc.request_pairing(OWNER_ID, "u2", "lark", None).await.unwrap();

        let count = repo.cleanup_expired_pairings(OWNER_ID, now_ms()).await.unwrap();
        assert_eq!(count, 1);

        let expired = repo.find_by_code("111111").unwrap();
        assert_eq!(expired.status, "expired");
    }
}
