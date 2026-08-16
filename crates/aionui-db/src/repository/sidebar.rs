//! Sidebar read-model store: thin classification rows + batch hydration.
//!
//! The sidebar renders the whole left panel from one read snapshot. To stay
//! cheap at 10k–50k conversations this store returns *thin* rows (just the
//! fields the service needs to classify and order — no message/model blobs),
//! and hydrates only the windowed items via a single `IN (...)` batch. See
//! `api-contract-sidebar.md` §5.1 (single read transaction, no N+1).
//!
//! Base exclusion (conversations): `archived_at IS NULL` + the shared
//! health-check keep-fragment (BR-23, `$.is_health_check` in `extra`). The
//! marker is legacy and has no writer anywhere in the workspace today, so the
//! predicate is a defensive no-op — but it is applied identically by the thin
//! listing and by hydration so the口径 cannot drift if a writer ever appears.
//! Teams have no probe rows, so their thin listing filters `archived_at` only.

use crate::error::DbError;
use crate::models::ConversationRow;

/// A conversation reduced to what sidebar classification needs.
///
/// `workspace` / `team_id` are `json_extract`ed from `extra`; `project_id` is
/// `NULLIF`'d to fold the empty string into `None`.
#[derive(Debug, Clone)]
pub struct SidebarConversationThin {
    pub id: String,
    /// `conversations.project_id`, empty-string folded to `None`.
    pub project_id: Option<String>,
    /// `extra.$.workspace` — a plain, un-canonicalized fs path (the write path
    /// only fs-validates it; see contract §6①). Empty/absent → `None`.
    pub workspace: Option<String>,
    /// Team-membership marker, `COALESCE(extra.$.team_id, extra.$.teamId)` with
    /// canonical `team_id` winning (BR-22 — production still writes camelCase).
    /// Present → this conversation is a team member, not an independent row
    /// (contract §3.1). The referenced team may not be live/owned (orphan) —
    /// the service downgrades those back to independent rows (BR-8).
    pub team_id: Option<String>,
    pub updated_at: i64,
    pub created_at: i64,
}

/// A team reduced to what sidebar classification needs. The path source is the
/// `teams.workspace` column (contract §6③), never reconstructed from members.
#[derive(Debug, Clone)]
pub struct SidebarTeamThin {
    pub id: String,
    pub name: String,
    pub project_id: Option<String>,
    /// `teams.workspace` column; empty string → `None` (no workspace set).
    pub workspace: Option<String>,
    pub updated_at: i64,
    pub created_at: i64,
}

/// Project spine metadata: a project plus its workspace folder identity.
///
/// The service uses one enumeration of the user's projects for everything:
/// `kind` distinguishes standard vs temp for bound-item classification
/// (§2 case 1/2), `workspace_canonical` drives path merge (§2 case 3), and the
/// standard subset is the group spine that surfaces even empty projects (BR-5).
#[derive(Debug, Clone)]
pub struct SidebarProjectMeta {
    pub project_id: String,
    pub name: String,
    /// `projects.kind` — service-layer enum `standard` | `temp`.
    pub kind: String,
    /// Canonical URI of the workspace folder; `None` if the project has no
    /// workspace entry (a bound project may legitimately lack one).
    pub workspace_canonical: Option<String>,
    /// Raw workspace folder URI, for display in the project-head "+" entry.
    pub workspace_uri: Option<String>,
    /// `projects.created_at` — group-order tie-break for empty standard
    /// projects (BR-6: empty project groups order by created_at DESC).
    pub created_at: i64,
}

/// Read-only sidebar queries. Every method is user-scoped: conversations and
/// teams filter `user_id`, and the project enumeration filters `projects.user_id`
/// (added by migration 030). A conversation may carry a foreign `project_id`, but
/// because the project map is built only from the caller's own projects such a
/// binding resolves to nothing and degrades to a dangling id (BR-24).
#[async_trait::async_trait]
pub trait ISidebarStore: Send + Sync {
    /// All active (non-archived) conversations for a user, thin.
    async fn list_active_conversations_thin(&self, user_id: &str) -> Result<Vec<SidebarConversationThin>, DbError>;

    /// All active (non-archived) teams for a user, thin.
    async fn list_active_teams_thin(&self, user_id: &str) -> Result<Vec<SidebarTeamThin>, DbError>;

    /// Every project owned by the user (standard *and* temp), each joined to its
    /// workspace folder identity. This single enumeration is the service's whole
    /// project picture: the group spine (standard subset, incl. zero-conversation
    /// projects — BR-5), the id→kind map for bound-item classification, and the
    /// canonical→project map for path merge. Projects are few per user, so no
    /// windowing here — the LIMIT 100 on the project area is applied after
    /// activity ordering in the service (BR-5/6).
    async fn list_user_projects(&self, user_id: &str) -> Result<Vec<SidebarProjectMeta>, DbError>;

    /// Full rows for the windowed conversation ids, for response hydration.
    /// Filtered to the user and to non-archived rows (a race that archives a row
    /// mid-request simply drops it from the window).
    async fn hydrate_conversations(&self, user_id: &str, ids: &[String]) -> Result<Vec<ConversationRow>, DbError>;
}
