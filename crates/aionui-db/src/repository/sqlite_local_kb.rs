//! SQLite local-kb repository (M9 E15). Never touches experience_* tables.

use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    CreateLocalKbDocumentParams, CreateLocalKbSourceParams, LocalKbDocumentRow, LocalKbSettingsRow, LocalKbSourceRow,
    UpsertLocalKbSettingsParams,
};
use crate::repository::ILocalKbRepository;

#[derive(Clone, Debug)]
pub struct SqliteLocalKbRepository {
    pool: SqlitePool,
}

impl SqliteLocalKbRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ILocalKbRepository for SqliteLocalKbRepository {
    async fn get_settings(&self, user_id: &str) -> Result<Option<LocalKbSettingsRow>, DbError> {
        let row = sqlx::query_as::<_, LocalKbSettingsRow>(
            "SELECT user_id, enabled, updated_at FROM local_kb_settings WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert_settings(&self, params: UpsertLocalKbSettingsParams<'_>) -> Result<LocalKbSettingsRow, DbError> {
        let now = aionui_common::now_ms();
        let enabled = if params.enabled { 1 } else { 0 };
        sqlx::query(
            "INSERT INTO local_kb_settings (user_id, enabled, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
        )
        .bind(params.user_id)
        .bind(enabled)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(LocalKbSettingsRow {
            user_id: params.user_id.to_string(),
            enabled,
            updated_at: now,
        })
    }

    async fn list_sources(&self, user_id: &str) -> Result<Vec<LocalKbSourceRow>, DbError> {
        let rows = sqlx::query_as::<_, LocalKbSourceRow>(
            "SELECT id, user_id, kind, label, origin_path, created_at, updated_at \
             FROM local_kb_sources WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create_source(&self, params: CreateLocalKbSourceParams<'_>) -> Result<LocalKbSourceRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO local_kb_sources (id, user_id, kind, label, origin_path, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.user_id)
        .bind(params.kind)
        .bind(params.label)
        .bind(params.origin_path)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(LocalKbSourceRow {
            id: params.id.to_string(),
            user_id: params.user_id.to_string(),
            kind: params.kind.to_string(),
            label: params.label.to_string(),
            origin_path: params.origin_path.map(str::to_string),
            created_at: now,
            updated_at: now,
        })
    }

    async fn delete_source(&self, user_id: &str, source_id: &str) -> Result<bool, DbError> {
        // Documents cascade via FK when supported; also delete explicitly for safety.
        sqlx::query("DELETE FROM local_kb_documents WHERE user_id = ? AND source_id = ?")
            .bind(user_id)
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        let result = sqlx::query("DELETE FROM local_kb_sources WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn create_document(&self, params: CreateLocalKbDocumentParams<'_>) -> Result<LocalKbDocumentRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO local_kb_documents \
             (id, source_id, user_id, title, body_text, content_hash, relative_path, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.source_id)
        .bind(params.user_id)
        .bind(params.title)
        .bind(params.body_text)
        .bind(params.content_hash)
        .bind(params.relative_path)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(LocalKbDocumentRow {
            id: params.id.to_string(),
            source_id: params.source_id.to_string(),
            user_id: params.user_id.to_string(),
            title: params.title.to_string(),
            body_text: params.body_text.to_string(),
            content_hash: params.content_hash.to_string(),
            relative_path: params.relative_path.map(str::to_string),
            created_at: now,
            updated_at: now,
        })
    }

    async fn list_documents_for_source(
        &self,
        user_id: &str,
        source_id: &str,
    ) -> Result<Vec<LocalKbDocumentRow>, DbError> {
        let rows = sqlx::query_as::<_, LocalKbDocumentRow>(
            "SELECT id, source_id, user_id, title, body_text, content_hash, relative_path, created_at, updated_at \
             FROM local_kb_documents WHERE user_id = ? AND source_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn search_documents(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<LocalKbDocumentRow>, DbError> {
        let needle = format!("%{}%", query.trim());
        let rows = sqlx::query_as::<_, LocalKbDocumentRow>(
            "SELECT id, source_id, user_id, title, body_text, content_hash, relative_path, created_at, updated_at \
             FROM local_kb_documents \
             WHERE user_id = ? AND (title LIKE ? OR body_text LIKE ?) \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(&needle)
        .bind(&needle)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_document(&self, user_id: &str, document_id: &str) -> Result<Option<LocalKbDocumentRow>, DbError> {
        let row = sqlx::query_as::<_, LocalKbDocumentRow>(
            "SELECT id, source_id, user_id, title, body_text, content_hash, relative_path, created_at, updated_at \
             FROM local_kb_documents WHERE user_id = ? AND id = ?",
        )
        .bind(user_id)
        .bind(document_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;
    use crate::models::{CreateLocalKbDocumentParams, CreateLocalKbSourceParams, UpsertLocalKbSettingsParams};

    #[tokio::test]
    async fn defaults_disabled_and_search_isolated_from_experience() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteLocalKbRepository::new(db.pool().clone());
        let user = "system_default_user";

        assert!(repo.get_settings(user).await.unwrap().is_none());

        let settings = repo
            .upsert_settings(UpsertLocalKbSettingsParams {
                user_id: user,
                enabled: true,
            })
            .await
            .unwrap();
        assert_eq!(settings.enabled, 1);

        let source = repo
            .create_source(CreateLocalKbSourceParams {
                id: "src1",
                user_id: user,
                kind: "text",
                label: "notes",
                origin_path: None,
            })
            .await
            .unwrap();
        repo.create_document(CreateLocalKbDocumentParams {
            id: "doc1",
            source_id: &source.id,
            user_id: user,
            title: "Alpha runbook",
            body_text: "Restart the WorkMate host carefully.",
            content_hash: "abc",
            relative_path: None,
        })
        .await
        .unwrap();

        let hits = repo.search_documents(user, "WorkMate", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Alpha runbook");

        // Experience library tables must stay untouched by local-kb writes.
        let exp_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM experience_articles")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(exp_count.0, 0);
    }
}
