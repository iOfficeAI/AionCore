use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::{Row, SqlitePool};

use crate::error::DbError;
use crate::models::{FolderRow, ProjectExplorerRow, ProjectKind, ProjectRow};
use crate::repository::project::IProjectStore;

/// SQLite-backed implementation of [`IProjectStore`].
#[derive(Clone, Debug)]
pub struct SqliteProjectStore {
    pool: SqlitePool,
}

impl SqliteProjectStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const FOLDER_COLS: &str = "folder_id, resource_uri, resource_canonical, created_at, updated_at";
const PROJECT_COLS: &str = "project_id, name, kind, created_at, updated_at";
const ENTRY_COLS: &str = "pe_id, project_id, folder_id, role, display_name, order_index, created_at, updated_at";

#[async_trait::async_trait]
impl IProjectStore for SqliteProjectStore {
    async fn upsert_folder(&self, canonical: &str, raw_uri: &str) -> Result<FolderRow, DbError> {
        let now = now_ms();
        let folder_id = generate_prefixed_id("folder");
        // INSERT OR IGNORE: a canonical collision leaves the existing row
        // untouched (immutable folder rows, no updated_at bump).
        sqlx::query(
            "INSERT OR IGNORE INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&folder_id)
        .bind(raw_uri)
        .bind(canonical)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, FolderRow>(&format!(
            "SELECT {FOLDER_COLS} FROM folders WHERE resource_canonical = ?"
        ))
        .bind(canonical)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_folder(&self, folder_id: &str) -> Result<Option<FolderRow>, DbError> {
        let row = sqlx::query_as::<_, FolderRow>(&format!("SELECT {FOLDER_COLS} FROM folders WHERE folder_id = ?"))
            .bind(folder_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_project(&self, project_id: &str) -> Result<Option<ProjectRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectRow>(&format!("SELECT {PROJECT_COLS} FROM projects WHERE project_id = ?"))
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn select_workspace_entry_by_folder(&self, folder_id: &str) -> Result<Option<ProjectExplorerRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectExplorerRow>(&format!(
            "SELECT {ENTRY_COLS} FROM project_explorer WHERE folder_id = ? AND role = 'workspace'"
        ))
        .bind(folder_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_entry(&self, pe_id: &str) -> Result<Option<ProjectExplorerRow>, DbError> {
        let row = sqlx::query_as::<_, ProjectExplorerRow>(&format!(
            "SELECT {ENTRY_COLS} FROM project_explorer WHERE pe_id = ?"
        ))
        .bind(pe_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_entries(&self, project_id: &str) -> Result<Vec<(ProjectExplorerRow, FolderRow)>, DbError> {
        // Manual join projection: project_explorer and folders share column
        // names (folder_id / created_at / updated_at), so folder columns are
        // aliased and rows are mapped by name rather than via a tuple FromRow.
        let rows = sqlx::query(
            "SELECT pe.pe_id, pe.project_id, pe.folder_id, pe.role, pe.display_name, pe.order_index, \
                    pe.created_at, pe.updated_at, \
                    f.resource_uri AS f_resource_uri, f.resource_canonical AS f_resource_canonical, \
                    f.created_at AS f_created_at, f.updated_at AS f_updated_at \
             FROM project_explorer pe \
             JOIN folders f ON f.folder_id = pe.folder_id \
             WHERE pe.project_id = ? \
             ORDER BY pe.order_index ASC, pe.created_at ASC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let folder_id: String = r.get("folder_id");
            let entry = ProjectExplorerRow {
                pe_id: r.get("pe_id"),
                project_id: r.get("project_id"),
                folder_id: folder_id.clone(),
                role: r.get("role"),
                display_name: r.get("display_name"),
                order_index: r.get("order_index"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            };
            let folder = FolderRow {
                folder_id,
                resource_uri: r.get("f_resource_uri"),
                resource_canonical: r.get("f_resource_canonical"),
                created_at: r.get("f_created_at"),
                updated_at: r.get("f_updated_at"),
            };
            out.push((entry, folder));
        }
        Ok(out)
    }

    async fn create_project_with_workspace_entry(
        &self,
        folder_id: &str,
        name: &str,
        kind: ProjectKind,
    ) -> Result<(ProjectRow, ProjectExplorerRow), DbError> {
        let now = now_ms();
        let project_id = generate_prefixed_id("project");
        let pe_id = generate_prefixed_id("pe");

        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO projects (project_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&project_id)
            .bind(name)
            .bind(kind.as_str())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;

        let entry_insert = sqlx::query(
            "INSERT INTO project_explorer \
                 (pe_id, project_id, folder_id, role, display_name, order_index, created_at, updated_at) \
             VALUES (?, ?, ?, 'workspace', NULL, 0, ?, ?)",
        )
        .bind(&pe_id)
        .bind(&project_id)
        .bind(folder_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await;

        match entry_insert {
            Ok(_) => {
                tx.commit().await?;
                let project = self
                    .get_project(&project_id)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("project {project_id} after insert")))?;
                let entry = self
                    .get_entry(&pe_id)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id} after insert")))?;
                Ok((project, entry))
            }
            Err(err) => {
                tx.rollback().await?;
                let db_err = DbError::from(err);
                if db_err.is_unique_violation() {
                    // A workspace project already exists for this folder — return it.
                    let entry = self
                        .select_workspace_entry_by_folder(folder_id)
                        .await?
                        .ok_or_else(|| DbError::Conflict(format!("workspace entry for folder {folder_id}")))?;
                    let project = self
                        .get_project(&entry.project_id)
                        .await?
                        .ok_or_else(|| DbError::NotFound(format!("project {}", entry.project_id)))?;
                    Ok((project, entry))
                } else {
                    Err(db_err)
                }
            }
        }
    }

    async fn insert_attached_entry(
        &self,
        project_id: &str,
        folder_id: &str,
        display_name: Option<&str>,
        order_index: i64,
    ) -> Result<ProjectExplorerRow, DbError> {
        let now = now_ms();
        let pe_id = generate_prefixed_id("pe");
        sqlx::query(
            "INSERT INTO project_explorer \
                 (pe_id, project_id, folder_id, role, display_name, order_index, created_at, updated_at) \
             VALUES (?, ?, ?, 'attached', ?, ?, ?, ?)",
        )
        .bind(&pe_id)
        .bind(project_id)
        .bind(folder_id)
        .bind(display_name)
        .bind(order_index)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_entry(&pe_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id} after insert")))
    }

    async fn remove_entry(&self, pe_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM project_explorer WHERE pe_id = ?")
            .bind(pe_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn reorder(&self, project_id: &str, ordered_pe_ids: &[String]) -> Result<(), DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        for (position, pe_id) in ordered_pe_ids.iter().enumerate() {
            sqlx::query(
                "UPDATE project_explorer SET order_index = ?, updated_at = ? WHERE pe_id = ? AND project_id = ?",
            )
            .bind(position as i64)
            .bind(now)
            .bind(pe_id)
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn rename_entry(&self, pe_id: &str, display_name: Option<&str>) -> Result<ProjectExplorerRow, DbError> {
        let now = now_ms();
        let result = sqlx::query("UPDATE project_explorer SET display_name = ?, updated_at = ? WHERE pe_id = ?")
            .bind(display_name)
            .bind(now)
            .bind(pe_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("project_explorer {pe_id}")));
        }
        self.get_entry(pe_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project_explorer {pe_id}")))
    }
}
