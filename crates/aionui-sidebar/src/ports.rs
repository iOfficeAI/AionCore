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

/// Process-teardown ports for archiving (parallel to `RemoveProjectPorts`, but
/// stop-only — no data is deleted).
///
/// Archiving used to flip `archived_at` and stop there, leaving the agent
/// process streaming for a unit the user just moved out of the active
/// workspace. These primitives let the archive path release the runtime the
/// same way delete does — kill the agent process / tear down the team runtime —
/// while the conversation and history rows are preserved (unarchiving
/// cold-starts a fresh agent). Same cross-crate seam rationale as
/// `RemoveProjectPorts`: the sidebar crate must not depend on the conversation /
/// team services directly, so `aionui-app` injects an adapter.
///
/// Errors are opaque strings: teardown is best-effort (the archive flip already
/// succeeded), so the sidebar only needs "this failed" for a warn log.
#[async_trait]
pub trait ArchiveTeardownPorts: Send + Sync {
    /// Stop one conversation's agent process (kill + wait), keeping its rows.
    async fn stop_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;

    /// Stop a whole team's runtime and every member agent process, keeping all
    /// rows.
    async fn stop_team(&self, user_id: &str, team_id: &str) -> Result<(), String>;
}
