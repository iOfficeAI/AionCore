use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::models::ConversationRow;
use crate::repository::sidebar::{ISidebarStore, SidebarConversationThin, SidebarProjectMeta, SidebarTeamThin};

/// SQLite-backed implementation of [`ISidebarStore`].
#[derive(Clone, Debug)]
pub struct SqliteSidebarStore {
    pool: SqlitePool,
}

impl SqliteSidebarStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Shared "keep this conversation" fragment excluding probe / health-check rows
/// (BR-23). The marker is a legacy read-only key in `extra`; the whole workspace
/// currently has no writer for it, so this exclusion is defensive (a no-op until
/// something writes `$.is_health_check`). Both the thin listing and hydration use
/// this single constant so their口径 cannot drift.
const HEALTH_CHECK_KEEP: &str =
    "(json_extract(extra, '$.is_health_check') IS NULL OR json_extract(extra, '$.is_health_check') NOT IN (1, 'true'))";

/// Fold a stored string into `None` when it is empty or whitespace-only.
///
/// Applies uniformly to workspace paths (empty `extra.workspace` / `teams.workspace`
/// = "no workspace") and to the `teamId` marker (matches the service-layer
/// `team_id_marker_from_extra_str` trim rule).
fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

/// Build a `?,?,…` placeholder list for a dynamic `IN (...)` clause.
fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count).collect::<Vec<_>>().join(",")
}

#[async_trait::async_trait]
impl ISidebarStore for SqliteSidebarStore {
    async fn list_active_conversations_thin(&self, user_id: &str) -> Result<Vec<SidebarConversationThin>, DbError> {
        // Team-membership marker: canonical `team_id` first, camelCase `teamId`
        // fallback (BR-22 — production still writes camelCase).
        let sql = format!(
            "SELECT id, \
                    NULLIF(project_id, '') AS project_id, \
                    json_extract(extra, '$.workspace') AS workspace, \
                    COALESCE(NULLIF(json_extract(extra, '$.team_id'), ''), NULLIF(json_extract(extra, '$.teamId'), '')) AS team_id, \
                    updated_at, created_at \
             FROM conversations \
             WHERE user_id = ? AND archived_at IS NULL AND {HEALTH_CHECK_KEEP}"
        );
        let rows = sqlx::query(&sql).bind(user_id).fetch_all(&self.pool).await?;

        Ok(rows
            .into_iter()
            .map(|row| SidebarConversationThin {
                id: row.get("id"),
                project_id: non_blank(row.get("project_id")),
                workspace: non_blank(row.get("workspace")),
                team_id: non_blank(row.get("team_id")),
                updated_at: row.get("updated_at"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    async fn list_active_teams_thin(&self, user_id: &str) -> Result<Vec<SidebarTeamThin>, DbError> {
        let rows = sqlx::query(
            "SELECT id, name, \
                    NULLIF(project_id, '') AS project_id, \
                    workspace, updated_at, created_at \
             FROM teams \
             WHERE user_id = ? AND archived_at IS NULL",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| SidebarTeamThin {
                id: row.get("id"),
                name: row.get("name"),
                project_id: non_blank(row.get("project_id")),
                workspace: non_blank(row.get("workspace")),
                updated_at: row.get("updated_at"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    async fn list_user_projects(&self, user_id: &str) -> Result<Vec<SidebarProjectMeta>, DbError> {
        // One user-scoped enumeration of every project (standard + temp), each
        // LEFT JOINed to its workspace-root folder. `projects.user_id` (migration
        // 030) confines the result to the caller, so a foreign `project_id` carried
        // by one of the caller's conversations simply finds no match here and the
        // service treats it as dangling (BR-24). At most one workspace folder per
        // project (idx_project_explorer_one_workspace_folder), so no row fan-out.
        let rows = sqlx::query(
            "SELECT p.project_id AS project_id, p.name AS name, p.kind AS kind, p.created_at AS created_at, \
                    f.resource_canonical AS workspace_canonical, f.resource_uri AS workspace_uri \
             FROM projects p \
             LEFT JOIN project_explorer pe ON pe.project_id = p.project_id AND pe.role = 'workspace' \
             LEFT JOIN folders f ON f.folder_id = pe.folder_id \
             WHERE p.user_id = ?",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_project_meta).collect())
    }

    async fn hydrate_conversations(&self, user_id: &str, ids: &[String]) -> Result<Vec<ConversationRow>, DbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT * FROM conversations \
             WHERE user_id = ? AND archived_at IS NULL AND {HEALTH_CHECK_KEEP} AND id IN ({})",
            placeholders(ids.len())
        );
        let mut query = sqlx::query_as::<_, ConversationRow>(&sql);
        query = query.bind(user_id);
        for id in ids {
            query = query.bind(id);
        }
        Ok(query.fetch_all(&self.pool).await?)
    }
}

/// Map a project spine join row into [`SidebarProjectMeta`].
fn map_project_meta(row: sqlx::sqlite::SqliteRow) -> SidebarProjectMeta {
    SidebarProjectMeta {
        project_id: row.get("project_id"),
        name: row.get("name"),
        kind: row.get("kind"),
        workspace_canonical: row.get("workspace_canonical"),
        workspace_uri: row.get("workspace_uri"),
        created_at: row.get("created_at"),
    }
}

#[cfg(test)]
#[path = "sqlite_sidebar_test.rs"]
mod sqlite_sidebar_test;
