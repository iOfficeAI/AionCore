//! SQLite repositories for Agent Center side tables.

use sqlx::SqlitePool;

use super::assistant::{IAssistantAgentCenterRepository, IAssistantDefinitionRevisionRepository};
use crate::error::DbError;
use crate::models::{
    AssistantAgentCenterRow, AssistantDefinitionRevisionRow, CreateAssistantDefinitionRevisionParams,
    UpsertAssistantAgentCenterParams,
};

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct SqliteAssistantAgentCenterRepository {
    pool: SqlitePool,
}

impl SqliteAssistantAgentCenterRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAssistantAgentCenterRepository for SqliteAssistantAgentCenterRepository {
    async fn get(&self, assistant_definition_id: &str) -> Result<Option<AssistantAgentCenterRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantAgentCenterRow>(
            "SELECT * FROM assistant_agent_center WHERE assistant_definition_id = ?",
        )
        .bind(assistant_definition_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_visibility(&self, visibility: &str) -> Result<Vec<AssistantAgentCenterRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantAgentCenterRow>(
            "SELECT * FROM assistant_agent_center WHERE visibility = ? ORDER BY updated_at DESC",
        )
        .bind(visibility)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_team(&self, team_id: &str) -> Result<Vec<AssistantAgentCenterRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantAgentCenterRow>(
            "SELECT * FROM assistant_agent_center
             WHERE visibility = 'team' AND team_id = ?
             ORDER BY updated_at DESC",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertAssistantAgentCenterParams<'_>) -> Result<AssistantAgentCenterRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_agent_center (
                assistant_definition_id, visibility, team_id, enterprise_id, status, version,
                published_revision_id, knowledge_scopes, skill_refs, mcp_policy, role_bindings,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(assistant_definition_id) DO UPDATE SET
                visibility = excluded.visibility,
                team_id = excluded.team_id,
                enterprise_id = excluded.enterprise_id,
                status = excluded.status,
                version = excluded.version,
                published_revision_id = excluded.published_revision_id,
                knowledge_scopes = excluded.knowledge_scopes,
                skill_refs = excluded.skill_refs,
                mcp_policy = excluded.mcp_policy,
                role_bindings = excluded.role_bindings,
                updated_at = excluded.updated_at",
        )
        .bind(params.assistant_definition_id)
        .bind(params.visibility)
        .bind(params.team_id)
        .bind(params.enterprise_id)
        .bind(params.status)
        .bind(params.version)
        .bind(params.published_revision_id)
        .bind(params.knowledge_scopes)
        .bind(params.skill_refs)
        .bind(params.mcp_policy)
        .bind(params.role_bindings)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.assistant_definition_id)
            .await?
            .ok_or_else(|| DbError::Init("assistant_agent_center upsert missing row".into()))
    }

    async fn delete(&self, assistant_definition_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_agent_center WHERE assistant_definition_id = ?")
            .bind(assistant_definition_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

pub struct SqliteAssistantDefinitionRevisionRepository {
    pool: SqlitePool,
}

impl SqliteAssistantDefinitionRevisionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAssistantDefinitionRevisionRepository for SqliteAssistantDefinitionRevisionRepository {
    async fn list(&self, assistant_definition_id: &str) -> Result<Vec<AssistantDefinitionRevisionRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantDefinitionRevisionRow>(
            "SELECT * FROM assistant_definition_revisions
             WHERE assistant_definition_id = ?
             ORDER BY revision DESC",
        )
        .bind(assistant_definition_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<AssistantDefinitionRevisionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRevisionRow>(
            "SELECT * FROM assistant_definition_revisions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_revision(
        &self,
        assistant_definition_id: &str,
        revision: i64,
    ) -> Result<Option<AssistantDefinitionRevisionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRevisionRow>(
            "SELECT * FROM assistant_definition_revisions
             WHERE assistant_definition_id = ? AND revision = ?",
        )
        .bind(assistant_definition_id)
        .bind(revision)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn create(
        &self,
        params: &CreateAssistantDefinitionRevisionParams<'_>,
    ) -> Result<AssistantDefinitionRevisionRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_definition_revisions (
                id, assistant_definition_id, revision, snapshot_json, changelog, created_by, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.assistant_definition_id)
        .bind(params.revision)
        .bind(params.snapshot_json)
        .bind(params.changelog)
        .bind(params.created_by)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.id)
            .await?
            .ok_or_else(|| DbError::Init("assistant_definition_revisions insert missing row".into()))
    }
}
