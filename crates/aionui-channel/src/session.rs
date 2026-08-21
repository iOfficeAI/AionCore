use std::sync::Arc;

use aionui_common::{generate_id, now_ms};
use aionui_db::IChannelRepository;
use aionui_db::models::ChannelConversationBindingRow;
use tracing::{debug, info};

use crate::error::ChannelError;

/// Manages per-chat session isolation for channel users.
///
/// Each (user_id, chat_id) pair maps to exactly one session. This ensures
/// that the same user chatting in different groups/DMs gets independent
/// conversation contexts, while repeated messages in the same chat reuse
/// the existing session.
pub struct SessionManager {
    repo: Arc<dyn IChannelRepository>,
}

impl SessionManager {
    pub fn new(repo: Arc<dyn IChannelRepository>) -> Self {
        Self { repo }
    }

    /// Finds an existing session for the user+chat pair, or creates one.
    ///
    /// - If found: updates `last_activity` and returns the existing session.
    /// - If not found: creates a fresh binding for the pair.
    ///
    /// Agent configuration is not part of the binding: it is resolved per
    /// turn from channel settings and the conversation snapshot.
    pub async fn get_or_create_session(
        &self,
        owner_user_id: &str,
        user_id: &str,
        chat_id: &str,
    ) -> Result<ChannelConversationBindingRow, ChannelError> {
        let now = now_ms();
        let new_row = ChannelConversationBindingRow {
            id: generate_id(),
            owner_user_id: owner_user_id.to_owned(),
            // The repository INSERT derives the real connection id from the
            // active `channel_users` row, so the caller never supplies one.
            connection_id: String::new(),
            user_id: user_id.to_owned(),
            chat_id: Some(chat_id.to_owned()),
            conversation_id: None,
            created_at: now,
            last_activity: now,
        };

        let session = self
            .repo
            .get_or_create_session(owner_user_id, user_id, chat_id, &new_row)
            .await?;

        debug!(
            session_id = %session.id,
            user_id = %user_id,
            chat_id = %chat_id,
            "session resolved"
        );

        Ok(session)
    }

    /// Returns all active sessions.
    pub async fn get_active_sessions(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<ChannelConversationBindingRow>, ChannelError> {
        let sessions = self.repo.get_all_sessions(owner_user_id).await?;
        Ok(sessions)
    }

    /// Deletes the existing session for a user+chat pair and creates a
    /// fresh one. Returns the newly created session.
    ///
    /// Used by `session.new` to give the user a clean slate in a chat.
    pub async fn reset_session(
        &self,
        owner_user_id: &str,
        user_id: &str,
        chat_id: &str,
    ) -> Result<ChannelConversationBindingRow, ChannelError> {
        // Delete old session if it exists
        self.repo
            .delete_session_by_user_chat(owner_user_id, user_id, chat_id)
            .await?;

        // Create a fresh session
        let now = now_ms();
        let new_row = ChannelConversationBindingRow {
            id: generate_id(),
            owner_user_id: owner_user_id.to_owned(),
            // Derived by the repository INSERT from the active
            // `channel_users` row — see `get_or_create_session`.
            connection_id: String::new(),
            user_id: user_id.to_owned(),
            chat_id: Some(chat_id.to_owned()),
            conversation_id: None,
            created_at: now,
            last_activity: now,
        };

        let session = self
            .repo
            .get_or_create_session(owner_user_id, user_id, chat_id, &new_row)
            .await?;

        info!(
            session_id = %session.id,
            user_id = %user_id,
            chat_id = %chat_id,
            "session reset"
        );

        Ok(session)
    }

    /// Removes all sessions belonging to a user.
    ///
    /// Called when a user is revoked to clean up their session state.
    pub async fn cleanup_user_sessions(&self, owner_user_id: &str, user_id: &str) -> Result<(), ChannelError> {
        self.repo.delete_sessions_by_user(owner_user_id, user_id).await?;
        info!(user_id = %user_id, "cleaned up user sessions");
        Ok(())
    }

    /// Removes all sessions across all users.
    ///
    /// Called after settings sync to force sessions to be recreated
    /// with updated agent/model configuration.
    pub async fn clear_all_sessions(&self, owner_user_id: &str) -> Result<(), ChannelError> {
        let sessions = self.repo.get_all_sessions(owner_user_id).await?;
        let mut cleared_users = std::collections::HashSet::new();
        for session in &sessions {
            if cleared_users.insert(session.user_id.clone()) {
                self.repo
                    .delete_sessions_by_user(owner_user_id, &session.user_id)
                    .await?;
            }
        }
        info!(count = sessions.len(), "cleared all channel sessions");
        Ok(())
    }

    /// Looks up a session by its unique ID.
    pub async fn get_session_by_id(
        &self,
        owner_user_id: &str,
        session_id: &str,
    ) -> Result<Option<ChannelConversationBindingRow>, ChannelError> {
        Ok(self.repo.get_session(owner_user_id, session_id).await?)
    }

    /// Persists the conversation binding for a session.
    ///
    /// Called after a new conversation is created for this session,
    /// linking the session to its backing conversation in the database.
    pub async fn bind_conversation(
        &self,
        owner_user_id: &str,
        session_id: &str,
        conversation_id: &str,
    ) -> Result<(), ChannelError> {
        self.repo
            .update_session_conversation(owner_user_id, session_id, conversation_id)
            .await?;

        debug!(
            session_id = %session_id,
            conversation_id = %conversation_id,
            "session bound to conversation"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_common::TimestampMs;
    use aionui_db::models::{
        ChannelConnectionRow, ChannelConversationBindingRow, ChannelPairingRequestRow, ChannelUserRow,
    };
    use aionui_db::{DbError, IChannelRepository, UpdateConnectionStatusParams};
    use std::sync::Mutex;

    // ── Mock IChannelRepository ────────────────────────────────────────
    const OWNER_ID: &str = "owner-test";
    /// The connection the mock's single authorized channel user hangs off.
    /// Stands in for the `channel_users` row the real INSERT derives from.
    const CONNECTION_ID: &str = "conn-test";

    struct MockRepo {
        sessions: Mutex<Vec<ChannelConversationBindingRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                sessions: Mutex::new(Vec::new()),
            }
        }

        fn get_sessions(&self) -> Vec<ChannelConversationBindingRow> {
            self.sessions.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        // -- Plugin CRUD (unused stubs) --
        async fn get_all_connections(&self, _owner_user_id: &str) -> Result<Vec<ChannelConnectionRow>, DbError> {
            Ok(vec![])
        }
        async fn get_connection(
            &self,
            _owner_user_id: &str,
            _id: &str,
        ) -> Result<Option<ChannelConnectionRow>, DbError> {
            Ok(None)
        }

        async fn get_connection_by_plugin_key(
            &self,
            _owner_user_id: &str,
            _plugin_key: &str,
        ) -> Result<Option<ChannelConnectionRow>, DbError> {
            Ok(None)
        }
        async fn upsert_connection(&self, _owner_user_id: &str, _row: &ChannelConnectionRow) -> Result<(), DbError> {
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
        async fn delete_connection(&self, _owner_user_id: &str, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- User CRUD (unused stubs) --
        async fn get_all_users(&self, _owner_user_id: &str) -> Result<Vec<ChannelUserRow>, DbError> {
            Ok(vec![])
        }
        async fn get_user_by_platform(
            &self,
            _owner_user_id: &str,
            _platform_user_id: &str,
            _platform_type: &str,
        ) -> Result<Option<ChannelUserRow>, DbError> {
            Ok(None)
        }
        async fn create_user(&self, _owner_user_id: &str, _row: &ChannelUserRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_user_last_active(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _last_active: TimestampMs,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn revoke_user(&self, _owner_user_id: &str, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Conversation binding CRUD --
        async fn get_all_sessions(&self, _owner_user_id: &str) -> Result<Vec<ChannelConversationBindingRow>, DbError> {
            Ok(self.sessions.lock().unwrap().clone())
        }

        async fn get_session(
            &self,
            _owner_user_id: &str,
            id: &str,
        ) -> Result<Option<ChannelConversationBindingRow>, DbError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions.iter().find(|s| s.id == id).cloned())
        }

        async fn get_or_create_session(
            &self,
            owner_user_id: &str,
            user_id: &str,
            chat_id: &str,
            new_row: &ChannelConversationBindingRow,
        ) -> Result<ChannelConversationBindingRow, DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            // Look for existing session by user_id + chat_id
            if let Some(existing) = sessions
                .iter_mut()
                .find(|s| s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id))
            {
                existing.last_activity = new_row.last_activity;
                return Ok(existing.clone());
            }
            // Mirror the real INSERT: owner/connection come from the channel
            // user row, never from the caller-supplied binding.
            let created = ChannelConversationBindingRow {
                owner_user_id: owner_user_id.to_owned(),
                connection_id: CONNECTION_ID.to_owned(),
                ..new_row.clone()
            };
            sessions.push(created.clone());
            Ok(created)
        }

        async fn update_session_activity(
            &self,
            _owner_user_id: &str,
            id: &str,
            last_activity: TimestampMs,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.last_activity = last_activity;
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn update_session_conversation(
            &self,
            _owner_user_id: &str,
            id: &str,
            conversation_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.conversation_id = Some(conversation_id.to_owned());
                s.last_activity = aionui_common::now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn delete_sessions_by_user(&self, _owner_user_id: &str, user_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| s.user_id != user_id);
            Ok(())
        }

        async fn delete_session_by_user_chat(
            &self,
            _owner_user_id: &str,
            user_id: &str,
            chat_id: &str,
        ) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| !(s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id)));
            Ok(())
        }

        // -- Pairing requests (unused stubs) --
        async fn create_pairing(&self, _owner_user_id: &str, _row: &ChannelPairingRequestRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_pending_pairings(&self, _owner_user_id: &str) -> Result<Vec<ChannelPairingRequestRow>, DbError> {
            Ok(vec![])
        }
        async fn get_pairing(
            &self,
            _owner_user_id: &str,
            _id: &str,
        ) -> Result<Option<ChannelPairingRequestRow>, DbError> {
            Ok(None)
        }
        async fn get_pending_pairing_by_code_hash(
            &self,
            _owner_user_id: &str,
            _code_hash: &str,
        ) -> Result<Option<ChannelPairingRequestRow>, DbError> {
            Ok(None)
        }
        async fn update_pairing_status(
            &self,
            _owner_user_id: &str,
            _id: &str,
            _status: &str,
            _approved_channel_user_id: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn expire_pending_pairings_for_user(
            &self,
            _owner_user_id: &str,
            _connection_id: &str,
            _external_user_id: &str,
        ) -> Result<u64, DbError> {
            Ok(0)
        }
        async fn cleanup_expired_pairings(&self, _owner_user_id: &str, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    fn make_manager() -> (SessionManager, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let mgr = SessionManager::new(repo.clone());
        (mgr, repo)
    }

    // ── get_or_create_session ──────────────────────────────────────────

    #[tokio::test]
    async fn creates_new_session() {
        let (mgr, repo) = make_manager();
        let session = mgr.get_or_create_session(OWNER_ID, "user1", "chat1").await.unwrap();

        assert_eq!(session.user_id, "user1");
        assert_eq!(session.chat_id.as_deref(), Some("chat1"));
        assert!(session.conversation_id.is_none());
        // Identity comes back resolved from the channel user row.
        assert_eq!(session.owner_user_id, OWNER_ID);
        assert_eq!(session.connection_id, CONNECTION_ID);
        assert!(!session.connection_id.is_empty());

        let all = repo.get_sessions();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].owner_user_id, OWNER_ID);
        assert_eq!(all[0].connection_id, CONNECTION_ID);
    }

    #[tokio::test]
    async fn reuses_existing_session_for_same_user_chat() {
        let (mgr, repo) = make_manager();

        let s1 = mgr.get_or_create_session(OWNER_ID, "user1", "chat1").await.unwrap();
        let s2 = mgr.get_or_create_session(OWNER_ID, "user1", "chat1").await.unwrap();

        assert_eq!(s1.id, s2.id);
        assert_eq!(repo.get_sessions().len(), 1);
    }

    #[tokio::test]
    async fn different_chats_get_different_sessions() {
        let (mgr, repo) = make_manager();

        let s1 = mgr.get_or_create_session(OWNER_ID, "user1", "chatA").await.unwrap();
        let s2 = mgr.get_or_create_session(OWNER_ID, "user1", "chatB").await.unwrap();

        assert_ne!(s1.id, s2.id);
        assert_eq!(repo.get_sessions().len(), 2);
    }

    #[tokio::test]
    async fn different_users_same_chat_get_different_sessions() {
        let (mgr, repo) = make_manager();

        let s1 = mgr.get_or_create_session(OWNER_ID, "user1", "chat1").await.unwrap();
        let s2 = mgr.get_or_create_session(OWNER_ID, "user2", "chat1").await.unwrap();

        assert_ne!(s1.id, s2.id);
        assert_eq!(repo.get_sessions().len(), 2);
    }

    // ── get_active_sessions ────────────────────────────────────────────

    #[tokio::test]
    async fn get_active_sessions_empty() {
        let (mgr, _repo) = make_manager();
        let sessions = mgr.get_active_sessions(OWNER_ID).await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn get_active_sessions_returns_all() {
        let (mgr, _repo) = make_manager();
        mgr.get_or_create_session(OWNER_ID, "u1", "c1").await.unwrap();
        mgr.get_or_create_session(OWNER_ID, "u2", "c2").await.unwrap();

        let sessions = mgr.get_active_sessions(OWNER_ID).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    // ── cleanup_user_sessions ──────────────────────────────────────────

    #[tokio::test]
    async fn cleanup_removes_user_sessions() {
        let (mgr, repo) = make_manager();
        mgr.get_or_create_session(OWNER_ID, "u1", "c1").await.unwrap();
        mgr.get_or_create_session(OWNER_ID, "u1", "c2").await.unwrap();
        mgr.get_or_create_session(OWNER_ID, "u2", "c1").await.unwrap();

        mgr.cleanup_user_sessions(OWNER_ID, "u1").await.unwrap();

        let sessions = repo.get_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_id, "u2");
    }

    #[tokio::test]
    async fn cleanup_noop_for_unknown_user() {
        let (mgr, repo) = make_manager();
        mgr.get_or_create_session(OWNER_ID, "u1", "c1").await.unwrap();

        mgr.cleanup_user_sessions(OWNER_ID, "u999").await.unwrap();

        assert_eq!(repo.get_sessions().len(), 1);
    }

    // ── bind_conversation ──────────────────────────────────────────────

    #[tokio::test]
    async fn bind_conversation_persists_conversation_id() {
        let (mgr, repo) = make_manager();
        let session = mgr.get_or_create_session(OWNER_ID, "u1", "c1").await.unwrap();
        assert!(session.conversation_id.is_none());

        mgr.bind_conversation(OWNER_ID, &session.id, "conv_123").await.unwrap();

        let updated = repo.get_sessions().into_iter().find(|s| s.id == session.id).unwrap();
        assert_eq!(updated.conversation_id.as_deref(), Some("conv_123"));
    }

    #[tokio::test]
    async fn bind_conversation_not_found() {
        let (mgr, _repo) = make_manager();
        let err = mgr.bind_conversation(OWNER_ID, "nonexistent", "conv_123").await;
        assert!(err.is_err());
    }

    // ── reset_session ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reset_session_creates_fresh_session() {
        let (mgr, repo) = make_manager();
        let s1 = mgr.get_or_create_session(OWNER_ID, "u1", "c1").await.unwrap();

        let s2 = mgr.reset_session(OWNER_ID, "u1", "c1").await.unwrap();

        // New session should have a different ID
        assert_ne!(s1.id, s2.id);
        assert_eq!(s2.user_id, "u1");
        assert_eq!(s2.chat_id.as_deref(), Some("c1"));
        assert!(s2.conversation_id.is_none());
        // The replacement binding is re-derived, not carried over.
        assert_eq!(s2.owner_user_id, OWNER_ID);
        assert_eq!(s2.connection_id, CONNECTION_ID);

        // Only 1 session should exist (old one deleted)
        assert_eq!(repo.get_sessions().len(), 1);
    }

    #[tokio::test]
    async fn reset_session_noop_when_no_existing() {
        let (mgr, repo) = make_manager();
        let session = mgr.reset_session(OWNER_ID, "u1", "c1").await.unwrap();

        assert_eq!(session.user_id, "u1");
        assert_eq!(session.owner_user_id, OWNER_ID);
        assert_eq!(session.connection_id, CONNECTION_ID);
        assert_eq!(repo.get_sessions().len(), 1);
    }
}
