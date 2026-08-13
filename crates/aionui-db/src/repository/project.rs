use async_trait::async_trait;

use crate::error::DbError;
use crate::models::{FolderRow, ProjectExplorerRow, ProjectKind, ProjectRow};

/// Access boundary for the three project-bind tables (`projects`, `folders`,
/// `project_explorer`).
///
/// The store owns SQL and transactions; the `aionui-project` service holds an
/// `Arc<dyn IProjectStore>` and never opens transactions itself. Business
/// identities (`folder_id` / `project_id` / `pe_id`) and timestamps are
/// generated inside the store — callers pass only domain values.
///
/// Scope model: `folders` is a global canonical-path registry (no owner);
/// `projects` and `project_explorer` are user-owned — every method touching
/// them takes the acting `user_id` and filters/writes the owner column, so a
/// user can neither see nor mutate another user's projects.
#[async_trait]
pub trait IProjectStore: Send + Sync {
    /// `INSERT OR IGNORE` a folder keyed by `canonical`, then `SELECT` it.
    /// Idempotent: on an existing canonical the row is returned unchanged
    /// (`resource_uri` keeps its first-insert value, `updated_at` is not bumped).
    async fn upsert_folder(&self, canonical: &str, raw_uri: &str) -> Result<FolderRow, DbError>;

    async fn get_folder(&self, folder_id: &str) -> Result<Option<FolderRow>, DbError>;
    async fn get_project(&self, user_id: &str, project_id: &str) -> Result<Option<ProjectRow>, DbError>;

    /// The workspace entry (if any) of `user_id` whose folder is `folder_id`.
    /// At most one exists per owner (enforced by
    /// `UNIQUE(owner_user_id, folder_id) WHERE role = 'workspace'`).
    async fn select_workspace_entry_by_folder(
        &self,
        user_id: &str,
        folder_id: &str,
    ) -> Result<Option<ProjectExplorerRow>, DbError>;

    async fn get_entry(&self, user_id: &str, pe_id: &str) -> Result<Option<ProjectExplorerRow>, DbError>;

    /// All explorer entries of a project owned by `user_id`, each joined with
    /// its folder, ordered by `order_index`.
    async fn list_entries(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Vec<(ProjectExplorerRow, FolderRow)>, DbError>;

    /// Atomic unit: create a project and its single `workspace` entry in one
    /// transaction, both owned by `user_id`. On a
    /// `UNIQUE(owner_user_id, folder_id) WHERE role = 'workspace'` violation
    /// (concurrent create for the same folder and owner) it rolls back and
    /// returns the existing project + entry (idempotent race guard).
    async fn create_project_with_workspace_entry(
        &self,
        user_id: &str,
        folder_id: &str,
        name: &str,
        kind: ProjectKind,
    ) -> Result<(ProjectRow, ProjectExplorerRow), DbError>;

    async fn insert_attached_entry(
        &self,
        user_id: &str,
        project_id: &str,
        folder_id: &str,
        display_name: Option<&str>,
        order_index: i64,
    ) -> Result<ProjectExplorerRow, DbError>;

    async fn remove_entry(&self, user_id: &str, pe_id: &str) -> Result<(), DbError>;
    async fn reorder(&self, user_id: &str, project_id: &str, ordered_pe_ids: &[String]) -> Result<(), DbError>;
    async fn rename_entry(
        &self,
        user_id: &str,
        pe_id: &str,
        display_name: Option<&str>,
    ) -> Result<ProjectExplorerRow, DbError>;
}
