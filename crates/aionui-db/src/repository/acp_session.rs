//! Repository trait for the `acp_session` table.
//!
//! Each ACP-type conversation owns exactly one `acp_session` row. The
//! row is created alongside the conversation (not on first message) so
//! the runtime-state write path can assume the row exists.
//!
//! `session_config` is a JSON blob that carries everything that is not
//! session identity. Under the `"runtime"` key it holds the user's last
//! per-session choices: current mode, current model, config selections,
//! context usage. `AcpAgentService` updates those fields through
//! [`IAcpSessionRepository::save_runtime_state_for_user`] and
//! `AcpAgentManager` preloads them on resume through
//! [`IAcpSessionRepository::load_runtime_state_for_user`].

use crate::error::DbError;
use crate::models::AcpSessionRow;

/// Parameters for [`IAcpSessionRepository::create`].
///
/// `session_id` stays `None` until the CLI returns one (first
/// `session/new` or `session/load`), at which point the caller flips
/// it through [`IAcpSessionRepository::update_session_id_for_user`].
#[derive(Debug, Clone)]
pub struct CreateAcpSessionParams<'a> {
    pub user_id: &'a str,
    pub conversation_id: &'a str,
    pub agent_source: &'a str,
    pub agent_id: &'a str,
}

/// The decoded `session_config.runtime` payload. See module docs.
///
/// All fields are optional because we persist partials — the service
/// may write just the mode or just the usage without touching siblings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedSessionState {
    pub current_mode_id: Option<String>,
    pub current_model_id: Option<String>,
    /// JSON-encoded map of `config_id -> value`. Stored as a raw string
    /// so the repository layer does not have to know the shape.
    pub config_selections_json: Option<String>,
    /// JSON-encoded `UsageUpdate`. Same rationale as
    /// `config_selections_json`.
    pub context_usage_json: Option<String>,
}

/// Partial update for [`IAcpSessionRepository::save_runtime_state_for_user`].
///
/// `Option<Option<_>>` lets callers distinguish "leave untouched"
/// (outer `None`) from "clear to null" (inner `None`).
#[derive(Debug, Clone, Default)]
pub struct SaveRuntimeStateParams<'a> {
    pub current_mode_id: Option<Option<&'a str>>,
    pub current_model_id: Option<Option<&'a str>>,
    pub config_selections_json: Option<Option<&'a str>>,
    pub context_usage_json: Option<Option<&'a str>>,
}

impl SaveRuntimeStateParams<'_> {
    pub fn is_empty(&self) -> bool {
        self.current_mode_id.is_none()
            && self.current_model_id.is_none()
            && self.config_selections_json.is_none()
            && self.context_usage_json.is_none()
    }
}

#[async_trait::async_trait]
pub trait IAcpSessionRepository: Send + Sync {
    /// Fetch the full row only when the owning conversation belongs to `user_id`.
    async fn get_for_user(&self, user_id: &str, conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError>;

    /// Insert a fresh `acp_session` row. Called by `ConversationService`
    /// when an ACP-type conversation is created; primary-key conflict
    /// surfaces as `DbError::Conflict`.
    async fn create(&self, params: &CreateAcpSessionParams<'_>) -> Result<AcpSessionRow, DbError>;

    /// Record the CLI-assigned `session_id` after `session/new` or
    /// `session/load` succeeds. Returns `true` when the row existed and
    /// belongs to `user_id`.
    async fn update_session_id_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
        session_id: &str,
    ) -> Result<bool, DbError>;

    /// Null the stored `session_id`, dropping the resume anchor while keeping the
    /// row (config/runtime state) intact. Called on an unrecoverable resume error
    /// ("No conversation found" / `error_during_execution`) so the NEXT turn opens
    /// Fresh instead of re-resuming a dead backend session forever. Distinct from
    /// [`delete_for_user`](Self::delete_for_user), which drops the whole row.
    /// Returns `true` when the row existed under `user_id`'s conversation. This
    /// is the direct-CLI equivalent of the clean-slate `Orchestrator` emitting
    /// `BackendBound{None}` and the legacy ACP
    /// `rebuild_after_session_not_found` → `clear_session_id` self-heal.
    async fn clear_session_id_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError>;

    /// Delete the row when the owning conversation belongs to `user_id`.
    /// Called by the conversation delete hook — no DB foreign key, so this
    /// must be invoked explicitly.
    async fn delete_for_user(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError>;

    /// Decode and return the `session_config.runtime` sub-object.
    /// Returns `None` when the row does not exist or the JSON lacks a
    /// `runtime` key; returns `Some(Default::default())` when the key
    /// is present but empty. Only returns a row when the owning conversation
    /// belongs to `user_id`.
    async fn load_runtime_state_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<PersistedSessionState>, DbError>;

    /// Merge a partial runtime update into `session_config.runtime` when the
    /// owning conversation belongs to `user_id`. Assumes the row exists
    /// (created alongside the conversation); returns `Ok(false)` when it does
    /// not.
    async fn save_runtime_state_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, DbError>;
}
