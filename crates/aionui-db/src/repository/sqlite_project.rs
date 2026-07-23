use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    ProjectCommandProfileRow, ProjectKnowledgeContextRow, ProjectKnowledgeFactRow, ProjectKnowledgeIndexRow,
    ProjectRepositoryFactsRow, ProjectResourceLinkRow, ProjectRow, ProjectRuntimeProfileRow,
};
use crate::repository::project::{IProjectRepository, UpdateProjectParams};

#[derive(Clone, Debug)]
pub struct SqliteProjectRepository {
    pool: SqlitePool,
}

impl SqliteProjectRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn map_project_write_error(error: sqlx::Error, local_path: &str) -> DbError {
    if error
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "2067")
    {
        DbError::Conflict(format!("project path already registered: {local_path}"))
    } else {
        DbError::Query(error)
    }
}

#[async_trait::async_trait]
impl IProjectRepository for SqliteProjectRepository {
    async fn create(&self, row: &ProjectRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO projects \
             (id, user_id, name, local_path, repository_url, default_branch, project_type, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.local_path)
        .bind(&row.repository_url)
        .bind(&row.default_branch)
        .bind(&row.project_type)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|error| map_project_write_error(error, &row.local_path))?;
        Ok(())
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<ProjectRow>, DbError> {
        Ok(
            sqlx::query_as::<_, ProjectRow>(
                "SELECT * FROM projects WHERE user_id = ? ORDER BY updated_at DESC, id ASC",
            )
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?,
        )
    }

    async fn get_for_user(&self, project_id: &str, user_id: &str) -> Result<Option<ProjectRow>, DbError> {
        Ok(
            sqlx::query_as::<_, ProjectRow>("SELECT * FROM projects WHERE id = ? AND user_id = ?")
                .bind(project_id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn update_for_user(
        &self,
        project_id: &str,
        user_id: &str,
        params: &UpdateProjectParams,
    ) -> Result<ProjectRow, DbError> {
        let existing = self
            .get_for_user(project_id, user_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("project {project_id}")))?;
        let updated = ProjectRow {
            id: existing.id,
            user_id: existing.user_id,
            name: params.name.clone().unwrap_or(existing.name),
            local_path: params.local_path.clone().unwrap_or(existing.local_path),
            repository_url: params.repository_url.clone().unwrap_or(existing.repository_url),
            default_branch: params.default_branch.clone().unwrap_or(existing.default_branch),
            project_type: params.project_type.clone().unwrap_or(existing.project_type),
            created_at: existing.created_at,
            updated_at: now_ms(),
        };

        sqlx::query(
            "UPDATE projects SET name = ?, local_path = ?, repository_url = ?, default_branch = ?, \
             project_type = ?, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(&updated.name)
        .bind(&updated.local_path)
        .bind(&updated.repository_url)
        .bind(&updated.default_branch)
        .bind(&updated.project_type)
        .bind(updated.updated_at)
        .bind(project_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|error| map_project_write_error(error, &updated.local_path))?;
        Ok(updated)
    }

    async fn delete_for_user(&self, project_id: &str, user_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM projects WHERE id = ? AND user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert_command_profile(&self, row: &ProjectCommandProfileRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO project_command_profiles \
             (project_id, install_command, format_command, lint_command, typecheck_command, unit_test_command, \
              integration_test_command, e2e_command, build_command, security_scan_command, command_timeout_seconds, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              install_command = excluded.install_command, format_command = excluded.format_command, \
              lint_command = excluded.lint_command, typecheck_command = excluded.typecheck_command, \
              unit_test_command = excluded.unit_test_command, integration_test_command = excluded.integration_test_command, \
              e2e_command = excluded.e2e_command, build_command = excluded.build_command, \
              security_scan_command = excluded.security_scan_command, \
              command_timeout_seconds = excluded.command_timeout_seconds, updated_at = excluded.updated_at",
        )
        .bind(&row.project_id)
        .bind(&row.install_command)
        .bind(&row.format_command)
        .bind(&row.lint_command)
        .bind(&row.typecheck_command)
        .bind(&row.unit_test_command)
        .bind(&row.integration_test_command)
        .bind(&row.e2e_command)
        .bind(&row.build_command)
        .bind(&row.security_scan_command)
        .bind(row.command_timeout_seconds)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_command_profile(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectCommandProfileRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectCommandProfileRow>(
            "SELECT profile.* FROM project_command_profiles profile \
             JOIN projects project ON project.id = profile.project_id \
             WHERE profile.project_id = ? AND project.user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert_runtime_profile(&self, row: &ProjectRuntimeProfileRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO project_runtime_profiles \
             (project_id, environment_kind, language, package_manager, runtime_version, env_keys, metadata, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              environment_kind = excluded.environment_kind, language = excluded.language, \
              package_manager = excluded.package_manager, runtime_version = excluded.runtime_version, \
              env_keys = excluded.env_keys, metadata = excluded.metadata, updated_at = excluded.updated_at",
        )
        .bind(&row.project_id)
        .bind(&row.environment_kind)
        .bind(&row.language)
        .bind(&row.package_manager)
        .bind(&row.runtime_version)
        .bind(&row.env_keys)
        .bind(&row.metadata)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_runtime_profile(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectRuntimeProfileRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectRuntimeProfileRow>(
            "SELECT profile.* FROM project_runtime_profiles profile \
             JOIN projects project ON project.id = profile.project_id \
             WHERE profile.project_id = ? AND project.user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert_repository_facts(&self, row: &ProjectRepositoryFactsRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO project_repository_facts \
             (project_id, repository_url, default_branch, baseline_commit, repository_dirty, \
              dirty_worktree_choice, dirty_snapshot_ref, credential_reference, detected_languages_json, \
              detected_package_managers_json, detected_rules_files_json, monorepo_packages_json, \
              submodules_json, lfs_detected, detected_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              repository_url = excluded.repository_url, default_branch = excluded.default_branch, \
              baseline_commit = excluded.baseline_commit, repository_dirty = excluded.repository_dirty, \
              dirty_worktree_choice = excluded.dirty_worktree_choice, dirty_snapshot_ref = excluded.dirty_snapshot_ref, \
              credential_reference = excluded.credential_reference, detected_languages_json = excluded.detected_languages_json, \
              detected_package_managers_json = excluded.detected_package_managers_json, \
              detected_rules_files_json = excluded.detected_rules_files_json, \
              monorepo_packages_json = excluded.monorepo_packages_json, submodules_json = excluded.submodules_json, \
              lfs_detected = excluded.lfs_detected, detected_at = excluded.detected_at",
        )
        .bind(&row.project_id)
        .bind(&row.repository_url)
        .bind(&row.default_branch)
        .bind(&row.baseline_commit)
        .bind(row.repository_dirty)
        .bind(&row.dirty_worktree_choice)
        .bind(&row.dirty_snapshot_ref)
        .bind(&row.credential_reference)
        .bind(&row.detected_languages_json)
        .bind(&row.detected_package_managers_json)
        .bind(&row.detected_rules_files_json)
        .bind(&row.monorepo_packages_json)
        .bind(&row.submodules_json)
        .bind(row.lfs_detected)
        .bind(row.detected_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_repository_facts(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectRepositoryFactsRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectRepositoryFactsRow>(
            "SELECT facts.* FROM project_repository_facts facts \
             JOIN projects project ON project.id = facts.project_id \
             WHERE facts.project_id = ? AND project.user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert_knowledge_index(&self, row: &ProjectKnowledgeIndexRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO project_knowledge_indexes \
             (project_id, provider, provider_project_name, provider_version, status, generation, source_commit, \
              indexed_at, changed_paths_json, error_category, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              provider = excluded.provider, provider_project_name = excluded.provider_project_name, \
              provider_version = excluded.provider_version, status = excluded.status, generation = excluded.generation, \
              source_commit = excluded.source_commit, indexed_at = excluded.indexed_at, \
              changed_paths_json = excluded.changed_paths_json, error_category = excluded.error_category, \
              updated_at = excluded.updated_at",
        )
        .bind(&row.project_id)
        .bind(&row.provider)
        .bind(&row.provider_project_name)
        .bind(&row.provider_version)
        .bind(&row.status)
        .bind(row.generation)
        .bind(&row.source_commit)
        .bind(row.indexed_at)
        .bind(&row.changed_paths_json)
        .bind(&row.error_category)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn commit_knowledge_generation(
        &self,
        index: &ProjectKnowledgeIndexRow,
        facts: &[ProjectKnowledgeFactRow],
    ) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO project_knowledge_indexes \
             (project_id, provider, provider_project_name, provider_version, status, generation, source_commit, \
              indexed_at, changed_paths_json, error_category, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              provider = excluded.provider, provider_project_name = excluded.provider_project_name, \
              provider_version = excluded.provider_version, status = excluded.status, generation = excluded.generation, \
              source_commit = excluded.source_commit, indexed_at = excluded.indexed_at, \
              changed_paths_json = excluded.changed_paths_json, error_category = excluded.error_category, \
              updated_at = excluded.updated_at",
        )
        .bind(&index.project_id)
        .bind(&index.provider)
        .bind(&index.provider_project_name)
        .bind(&index.provider_version)
        .bind(&index.status)
        .bind(index.generation)
        .bind(&index.source_commit)
        .bind(index.indexed_at)
        .bind(&index.changed_paths_json)
        .bind(&index.error_category)
        .bind(index.updated_at)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DELETE FROM project_knowledge_facts WHERE project_id = ?")
            .bind(&index.project_id)
            .execute(&mut *transaction)
            .await?;
        for fact in facts {
            sqlx::query(
                "INSERT INTO project_knowledge_facts \
                 (id, project_id, generation, kind, name, qualified_name, source_path, source_line, indexed_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&fact.id)
            .bind(&fact.project_id)
            .bind(fact.generation)
            .bind(&fact.kind)
            .bind(&fact.name)
            .bind(&fact.qualified_name)
            .bind(&fact.source_path)
            .bind(fact.source_line)
            .bind(fact.indexed_at)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn get_knowledge_index(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectKnowledgeIndexRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectKnowledgeIndexRow>(
            "SELECT knowledge.* FROM project_knowledge_indexes knowledge \
             JOIN projects project ON project.id = knowledge.project_id \
             WHERE knowledge.project_id = ? AND project.user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_knowledge_facts(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Vec<ProjectKnowledgeFactRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectKnowledgeFactRow>(
            "SELECT fact.* FROM project_knowledge_facts fact \
             JOIN projects project ON project.id = fact.project_id \
             JOIN project_knowledge_indexes knowledge ON knowledge.project_id = fact.project_id \
              AND knowledge.generation = fact.generation \
             WHERE fact.project_id = ? AND project.user_id = ? \
             ORDER BY fact.kind ASC, fact.name ASC, fact.source_path ASC, fact.source_line ASC",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn insert_knowledge_context(&self, row: &ProjectKnowledgeContextRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO project_knowledge_contexts \
             (id, project_id, provider_project_name, generation, query, symbols_json, callers_json, tests_json, \
              routes_json, data_entities_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.project_id)
        .bind(&row.provider_project_name)
        .bind(row.generation)
        .bind(&row.query)
        .bind(&row.symbols_json)
        .bind(&row.callers_json)
        .bind(&row.tests_json)
        .bind(&row.routes_json)
        .bind(&row.data_entities_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_knowledge_context(
        &self,
        context_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectKnowledgeContextRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectKnowledgeContextRow>(
            "SELECT context.* FROM project_knowledge_contexts context \
             JOIN projects project ON project.id = context.project_id \
             WHERE context.id = ? AND project.user_id = ?",
        )
        .bind(context_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn bind_resource(
        &self,
        project_id: &str,
        user_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ProjectResourceLinkRow, DbError> {
        if self.get_for_user(project_id, user_id).await?.is_none() {
            return Err(DbError::NotFound(format!("project {project_id}")));
        }
        let row = ProjectResourceLinkRow {
            project_id: project_id.to_owned(),
            user_id: user_id.to_owned(),
            resource_type: resource_type.to_owned(),
            resource_id: resource_id.to_owned(),
            created_at: now_ms(),
        };
        sqlx::query(
            "INSERT INTO project_resource_links (project_id, user_id, resource_type, resource_id, created_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, resource_type, resource_id) DO UPDATE SET \
              project_id = excluded.project_id, created_at = excluded.created_at",
        )
        .bind(&row.project_id)
        .bind(&row.user_id)
        .bind(&row.resource_type)
        .bind(&row.resource_id)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(row)
    }

    async fn resource_is_owned(&self, user_id: &str, resource_type: &str, resource_id: &str) -> Result<bool, DbError> {
        let query = match resource_type {
            "conversation" => "SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)",
            "team" => "SELECT EXISTS(SELECT 1 FROM teams WHERE id = ? AND user_id = ?)",
            "cron" => {
                "SELECT EXISTS(\
                    SELECT 1 FROM cron_jobs cron \
                    JOIN conversations conversation ON conversation.id = cron.conversation_id \
                    WHERE cron.id = ? AND conversation.user_id = ?\
                )"
            }
            "channel" => {
                "SELECT EXISTS(\
                    SELECT 1 FROM assistant_sessions channel \
                    JOIN conversations conversation ON conversation.id = channel.conversation_id \
                    WHERE channel.id = ? AND conversation.user_id = ?\
                )"
            }
            _ => return Ok(false),
        };
        let owned: i64 = sqlx::query_scalar(query)
            .bind(resource_id)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(owned != 0)
    }

    async fn get_for_resource(
        &self,
        user_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<Option<ProjectRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectRow>(
            "SELECT project.* FROM projects project \
             JOIN project_resource_links link ON link.project_id = project.id AND link.user_id = project.user_id \
             WHERE link.user_id = ? AND link.resource_type = ? AND link.resource_id = ?",
        )
        .bind(user_id)
        .bind(resource_type)
        .bind(resource_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_resource_links(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Vec<ProjectResourceLinkRow>, DbError> {
        Ok(sqlx::query_as::<_, ProjectResourceLinkRow>(
            "SELECT link.* FROM project_resource_links link \
             JOIN projects project ON project.id = link.project_id AND project.user_id = link.user_id \
             WHERE link.project_id = ? AND link.user_id = ? ORDER BY link.created_at DESC, link.resource_id ASC",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }
}
