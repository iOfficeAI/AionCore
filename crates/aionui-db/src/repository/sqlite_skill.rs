use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{SkillImportRecordRow, SkillRow};
use crate::repository::skill::{CreateSkillImportRecordParams, ISkillRepository, UpsertSkillParams};

/// SQLite-backed implementation of [`ISkillRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSkillRepository {
    pool: SqlitePool,
}

impl SqliteSkillRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISkillRepository for SqliteSkillRepository {
    async fn list(&self) -> Result<Vec<SkillRow>, DbError> {
        let rows = sqlx::query_as::<_, SkillRow>(
            "SELECT * FROM skills WHERE deleted_at IS NULL AND enabled = 1 ORDER BY updated_at DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<SkillRow>, DbError> {
        let row =
            sqlx::query_as::<_, SkillRow>("SELECT * FROM skills WHERE name = ? AND deleted_at IS NULL AND enabled = 1")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn find_by_name_any(&self, name: &str) -> Result<Option<SkillRow>, DbError> {
        let row = sqlx::query_as::<_, SkillRow>("SELECT * FROM skills WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn upsert(&self, params: UpsertSkillParams<'_>) -> Result<SkillRow, DbError> {
        let now = aionui_common::now_ms();
        let existing = self.find_by_name_any(params.name).await?;
        let id = existing
            .as_ref()
            .map(|row| row.id.clone())
            .unwrap_or_else(|| aionui_common::generate_prefixed_id("skill"));
        let created_at = existing.as_ref().map(|row| row.created_at).unwrap_or(now);

        sqlx::query(
            "INSERT INTO skills \
                (id, name, description, path, source, enabled, deleted_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET \
                description = excluded.description, \
                path = excluded.path, \
                source = excluded.source, \
                enabled = excluded.enabled, \
                deleted_at = NULL, \
                updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.path)
        .bind(params.source)
        .bind(params.enabled)
        .bind(created_at)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.find_by_name_any(params.name)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("skill '{}' was not found after upsert", params.name)))
    }

    async fn delete_by_name(&self, name: &str) -> Result<SkillRow, DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE skills SET enabled = 0, deleted_at = ?, updated_at = ? WHERE name = ? AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(name)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("skill '{name}'")));
        }

        self.find_by_name_any(name)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("skill '{name}'")))
    }

    async fn create_import_record(
        &self,
        params: CreateSkillImportRecordParams<'_>,
    ) -> Result<SkillImportRecordRow, DbError> {
        let id = aionui_common::generate_prefixed_id("skill_import");
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO skill_import_records \
                (id, operation_id, source_label, source_path, source_name, skill_id, skill_name, \
                 status, error_code, error_path, actual_bytes, limit_bytes, line, column, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.operation_id)
        .bind(params.source_label)
        .bind(params.source_path)
        .bind(params.source_name)
        .bind(params.skill_id)
        .bind(params.skill_name)
        .bind(params.status)
        .bind(params.error_code)
        .bind(params.error_path)
        .bind(params.actual_bytes)
        .bind(params.limit_bytes)
        .bind(params.line)
        .bind(params.column)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, SkillImportRecordRow>("SELECT * FROM skill_import_records WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_import_records(&self, limit: i64) -> Result<Vec<SkillImportRecordRow>, DbError> {
        let rows = sqlx::query_as::<_, SkillImportRecordRow>(
            "SELECT * FROM skill_import_records ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteSkillRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteSkillRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn upsert_restores_soft_deleted_skill() {
        let (repo, _db) = setup().await;

        let created = repo
            .upsert(UpsertSkillParams {
                name: "sample",
                description: Some("Old"),
                path: "/tmp/old",
                source: "user",
                enabled: true,
            })
            .await
            .unwrap();
        repo.delete_by_name("sample").await.unwrap();

        let restored = repo
            .upsert(UpsertSkillParams {
                name: "sample",
                description: Some("New"),
                path: "/tmp/new",
                source: "user",
                enabled: true,
            })
            .await
            .unwrap();

        assert_eq!(restored.id, created.id);
        assert_eq!(restored.description.as_deref(), Some("New"));
        assert_eq!(restored.path, "/tmp/new");
        assert_eq!(restored.deleted_at, None);
        assert!(repo.find_by_name("sample").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_filters_soft_deleted_skills() {
        let (repo, _db) = setup().await;

        repo.upsert(UpsertSkillParams {
            name: "active",
            description: None,
            path: "/tmp/active",
            source: "user",
            enabled: true,
        })
        .await
        .unwrap();
        repo.upsert(UpsertSkillParams {
            name: "deleted",
            description: None,
            path: "/tmp/deleted",
            source: "user",
            enabled: true,
        })
        .await
        .unwrap();
        repo.delete_by_name("deleted").await.unwrap();

        let names: Vec<_> = repo.list().await.unwrap().into_iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["active"]);
        assert!(repo.find_by_name_any("deleted").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn import_records_keep_structured_error_details() {
        let (repo, _db) = setup().await;

        let row = repo
            .create_import_record(CreateSkillImportRecordParams {
                operation_id: "import_1",
                source_label: "parent-pack",
                source_path: Some("/tmp/parent-pack"),
                source_name: "beta-skill",
                skill_id: None,
                skill_name: None,
                status: "failed",
                error_code: Some("SKILL_IMPORT_FILE_TOO_LARGE"),
                error_path: Some("assets/movie.mp4"),
                actual_bytes: Some(73_400_320),
                limit_bytes: Some(10_485_760),
                line: None,
                column: None,
            })
            .await
            .unwrap();

        assert_eq!(row.operation_id, "import_1");
        assert_eq!(row.error_path.as_deref(), Some("assets/movie.mp4"));
        assert_eq!(row.actual_bytes, Some(73_400_320));
        assert_eq!(row.limit_bytes, Some(10_485_760));
        let records = repo.list_import_records(10).await.unwrap();
        assert_eq!(records.len(), 1);
    }
}
