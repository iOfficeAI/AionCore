//! Deletion ports for `remove_project` (BR-19 / D13 "所见即所删").
//!
//! Removing a project deletes everything classified into its group: independent
//! conversations, whole teams, and the project record itself. Each of those is a
//! heavy, cross-crate orchestration that already lives in a domain service —
//! killing agent processes and cascading member conversations (team delete),
//! running the conversation delete hook (conversation delete), dropping the
//! bind-chain rows (project delete). The sidebar crate must not re-implement or
//! depend on those services directly, so this trait is the seam: `aionui-app`
//! injects an adapter over the concrete conversation / team / project services.
//!
//! Errors are opaque strings on purpose. The orchestration is best-effort per
//! entity (see `SidebarService::remove_project`): the sidebar only needs to know
//! "this one failed" for a warn log, not the error taxonomy of three foreign
//! crates.

use async_trait::async_trait;

/// The three deletion primitives `remove_project` drives, one per unit kind.
#[async_trait]
pub trait RemoveProjectPorts: Send + Sync {
    /// Delete one independent (non-team-member) conversation. Its `user_order`
    /// row is cascaded by the conversation delete hook.
    async fn delete_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;

    /// Remove a whole team: kill its agents, cascade its member conversations,
    /// drop the team row and its own `user_order` row (the standalone
    /// team-delete path, reused verbatim).
    async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), String>;

    /// Delete the project record and its explorer entries (owner-scoped).
    async fn delete_project_record(&self, user_id: &str, project_id: &str) -> Result<(), String>;
}
