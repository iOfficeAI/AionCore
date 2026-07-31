use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aionui_api_types::{ConversationRuntimeSummary, TeamRunTargetRole};
use async_trait::async_trait;

use crate::error::TeamError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConversationBindingLookup {
    pub conversation_id: String,
    pub user_id: String,
    pub team_id: Option<String>,
    pub slot_id: Option<String>,
    pub role: Option<String>,
}

#[async_trait]
pub trait TeamConversationLookupPort: Send + Sync {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError>;
}

/// Which tier of the recognition degradation chain produced a slash catalog.
/// All three point at the same `capabilities().slash_commands` source; they
/// differ only in freshness/liveness (see the fix spec §8-1 degradation chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCatalogSource {
    /// Read from a live backend session's current capabilities.
    Live,
    /// Read from the persisted `agent_metadata.available_commands` snapshot.
    Cached,
    /// Read from a static backend catalog (e.g. codex builtin commands).
    Static,
}

impl SlashCatalogSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Cached => "cached",
            Self::Static => "static",
        }
    }
}

/// Result of asking whether a user message is a native backend slash command.
///
/// Carries the source tag (not a bare `bool`) so the team layer can emit the
/// structured recognition log required by the fix spec (§14/AC11) while staying
/// backend-agnostic — it only consumes this enum, never a concrete backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandRecognition {
    /// `content` is a single command this member's backend advertises. `command`
    /// is the name without the leading `/`.
    Recognized {
        command: String,
        source: SlashCatalogSource,
    },
    /// Leading `/` with a non-empty name, but the name is not in the advertised
    /// catalog → ordinary body text (§14 debug: `not_in_catalog`).
    NotInCatalog { name: String },
    /// Leading `/` with a non-empty name, but the catalog RESOLVED to an EMPTY
    /// list (e.g. a live backend returned no commands). Distinct from
    /// `NotInCatalog` (a populated catalog that simply lacks this name): an empty
    /// catalog is an anomaly that silently disables every team command, so it is
    /// surfaced separately and logged at `warn` (§14 warn: `catalog_empty`).
    /// Still falls back to the wrapped wake turn (zero regression).
    CatalogEmpty { name: String },
    /// Leading `/` with a non-empty name, but the catalog could not be resolved
    /// (degradation chain exhausted) → fall back to the wrapped wake turn
    /// (§14 warn: `catalog_unavailable`).
    CatalogUnavailable { name: String },
    /// First character is not `/` (leading whitespace included) or the name is
    /// empty → not a command at all.
    NotCommand,
}

/// Recognizes whether a raw user message is a native backend slash command for a
/// given conversation. Backend-agnostic contract owned by the team layer; the
/// composition layer supplies an implementation that consults the backend's
/// self-advertised command catalog (live → cached → static degradation chain).
#[async_trait]
pub trait NativeSlashCommandPort: Send + Sync {
    async fn recognize(&self, conversation_id: &str, content: &str) -> SlashCommandRecognition;
}

/// No-op recognizer: every message is `NotCommand`, so team behaviour is
/// byte-identical to the pre-fix wrapped-wake path. Used as the default when no
/// composition-layer recognizer is wired (e.g. unit tests that don't exercise
/// command recognition).
pub struct NoopNativeSlashCommandPort;

#[async_trait]
impl NativeSlashCommandPort for NoopNativeSlashCommandPort {
    async fn recognize(&self, _conversation_id: &str, _content: &str) -> SlashCommandRecognition {
        SlashCommandRecognition::NotCommand
    }
}

#[async_trait]
pub trait TeamAssistantCatalogPort: Send + Sync {
    async fn list_team_selectable_assistants(&self, user_id: &str)
    -> Result<Vec<TeamAssistantCatalogEntry>, TeamError>;

    async fn resolve_team_selectable_assistant(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<TeamAssistantCatalogEntry>, TeamError> {
        Ok(self
            .list_team_selectable_assistants(user_id)
            .await?
            .into_iter()
            .find(|assistant| assistant.assistant_id == assistant_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamAssistantCatalogEntry {
    pub assistant_id: String,
    pub name: String,
    pub backend: String,
    pub description: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurnSource {
    Mailbox {
        unread_message_ids: Vec<String>,
        unread_count: usize,
    },
}

#[derive(Clone)]
pub struct AgentTurnRequest {
    pub team_run_id: Option<String>,
    pub team_id: String,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub user_id: String,
    pub content: String,
    pub files: Vec<String>,
    pub source: AgentTurnSource,
    pub on_started: Option<AgentTurnStartedCallback>,
}

impl fmt::Debug for AgentTurnRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTurnRequest")
            .field("team_run_id", &self.team_run_id)
            .field("team_id", &self.team_id)
            .field("slot_id", &self.slot_id)
            .field("role", &self.role)
            .field("conversation_id", &self.conversation_id)
            .field("user_id", &self.user_id)
            .field("files", &self.files)
            .field("source", &self.source)
            .field("has_on_started", &self.on_started.is_some())
            .finish_non_exhaustive()
    }
}

pub type AgentTurnStartedCallback =
    Arc<dyn Fn(AgentTurnStarted) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurnStarted {
    pub team_run_id: Option<String>,
    pub slot_id: String,
    pub role: TeamRunTargetRole,
    pub conversation_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTurnStatus {
    Completed,
    Failed,
    Skipped,
}

impl AgentTurnStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

#[derive(Debug, Clone)]
pub struct AgentTurnOutcome {
    pub conversation_id: String,
    pub turn_id: String,
    pub status: AgentTurnStatus,
    pub runtime: Option<ConversationRuntimeSummary>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentTurnExecutionError {
    #[error("agent turn skipped: {reason}")]
    Skipped { reason: String },
    #[error("agent turn failed: {reason}")]
    Failed { reason: String },
}

#[async_trait]
pub trait AgentTurnExecutionPort: Send + Sync {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError>;
}

#[async_trait]
pub trait AgentTurnCancellationPort: Send + Sync {
    async fn cancel_agent_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<(), AgentTurnExecutionError>;
}
