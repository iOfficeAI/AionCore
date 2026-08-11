//! Sidebar read-model DTOs (`GET /api/sidebar`, `GET /api/sidebar/items`).
//!
//! One request renders the whole left panel: the backend classifies every
//! conversation/team into its group (pinned / project / pseudo-dir / chats),
//! windows each group, and hydrates items. The frontend only renders in the
//! given order — it runs no classification. See
//! `feat-project-design/temp/left-panel/api-contract-sidebar.md` §4.

use serde::{Deserialize, Serialize};

use crate::conversation::ConversationResponse;

/// Root of `GET /api/sidebar`.
///
/// `groups` order **is** render order: `pinned → project-area (project + dir
/// interleaved) × N → chats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarResponse {
    /// Rendered top to bottom in this exact order.
    pub groups: Vec<SidebarGroup>,
    /// True when the project area exceeded the 100-group hard cap and was
    /// truncated.
    pub has_more_groups: bool,
}

/// Response of `GET /api/sidebar/items` — one more window of a single group.
///
/// Same shape as [`SidebarGroup`] minus `scope` (the caller already knows which
/// group it paged). See `api-contract-sidebar.md` §3.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItemsResponse {
    pub items: Vec<SidebarItem>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One group (a section's window) in the sidebar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarGroup {
    pub scope: SidebarScope,
    pub items: Vec<SidebarItem>,
    /// True when this group has items beyond the returned window (paginate via
    /// `GET /api/sidebar/items?scope=<token>&cursor=<next_cursor>`).
    pub has_more: bool,
    /// Keyset cursor for the next page; `None` iff `has_more` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Which section a group belongs to; the tag doubles as the group-head shape.
///
/// The frontend has three sections (pinned / project / chats); both `Project`
/// and `Dir` render into the project area, distinguished only by group-head
/// form (a real project head carries the "+" / remove entry, a dir head does
/// not).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidebarScope {
    /// Pinned group. Pinned rows appear only here.
    Pinned,
    /// Real project group. `workspace` feeds the project-head "+" entry
    /// (resolved server-side from the project source, not scanned from items).
    Project {
        project_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Pseudo project group (directory aggregation). `key` is the dir token used
    /// for paging / `win`; `name` is the directory's last segment.
    Dir { key: String, path: String, name: String },
    /// The flat "chats" group.
    Chats,
}

/// One row in a group: either a full conversation or an aggregated team row.
// `Conversation` is by far the common variant and the hot path (most sidebar
// rows are conversations); boxing it just to shrink the rarer `Team` variant
// would add a heap allocation per row for no real memory win, so the size
// disparity is accepted deliberately.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidebarItem {
    /// A conversation row, reusing the shared conversation DTO. Its `pinned`
    /// flag is derived from `user_order` row existence, not any table column.
    Conversation { conversation: ConversationResponse },
    /// An aggregated team row (server-side aggregate; the frontend does not
    /// reconstruct it from member conversations).
    Team(SidebarTeamItem),
}

/// Result of `DELETE /api/sidebar/project/{id}` (and its `dry_run` preview).
///
/// Counts are of the units classified into the project's group (BR-19). A live
/// delete reports how many were actually removed; a `dry_run` reports how many
/// *would* be — the two agree when no concurrent deletion races in. Team-member
/// conversations are not counted separately: they are folded into their team and
/// removed by the team cascade, so only the visible rows (`teams_deleted` +
/// `conversations_deleted`) are reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveProjectResult {
    /// Teams classified into the project's group.
    pub teams_deleted: i64,
    /// Independent conversations classified into the project's group.
    pub conversations_deleted: i64,
    /// The named units in the delete set, so a `dry_run` preview can list *which*
    /// items go — not just how many. Pinned members live in the top pinned group
    /// (B1 double-render: a project's pinned rows are anti-joined out of its own
    /// group), so the frontend cannot reconstruct project membership itself; the
    /// names must come from here. Empty on a live delete (the preview already
    /// showed them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<RemoveProjectItem>,
}

/// One named unit in a [`RemoveProjectResult`] preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveProjectItem {
    pub name: String,
    /// Whether the unit is currently pinned (hoisted into the top pinned group).
    pub pinned: bool,
    pub kind: RemoveProjectItemKind,
}

/// Which sidebar unit a [`RemoveProjectItem`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveProjectItemKind {
    Conversation,
    Team,
}

/// Aggregated team row for the sidebar.
///
/// Membership grouping is already expressed by the group the row sits in, so
/// (unlike the old draft) there is no `project` back-reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarTeamItem {
    pub team_id: String,
    pub name: String,
    /// `MAX(updated_at)` across active member conversations.
    pub updated_at: i64,
    /// Derived from a `user_order` scene=`pinned` row existing for this team.
    pub pinned: bool,
    /// Active member conversation ids, `created_at` ascending.
    pub member_conversation_ids: Vec<String>,
}
