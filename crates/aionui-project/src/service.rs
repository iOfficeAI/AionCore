use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    DirtyWorktreeChoice, ProjectKnowledgeFact, ProjectKnowledgeStatus, ProjectRepositoryFacts,
    ProjectRepositoryOnboardingInput, ProjectTaskContext,
};
use aionui_common::now_ms;
use aionui_db::models::{
    ProjectCommandProfileRow, ProjectKnowledgeContextRow, ProjectKnowledgeFactRow, ProjectKnowledgeIndexRow,
    ProjectRepositoryFactsRow, ProjectResourceLinkRow, ProjectRow, ProjectRuntimeProfileRow,
};
use aionui_db::{IConversationRepository, IProjectRepository, ITeamRepository, UpdateProjectParams};

use crate::error::ProjectError;
use crate::knowledge::{
    CodebaseMemoryCliProvider, KnowledgeProviderError, ProjectKnowledgeProvider, ProjectKnowledgeProviderRequest,
    ProjectKnowledgeProviderResult, inspect_repository,
};
use crate::repository_source::RepositoryOnboarder;
use crate::types::{
    AgentCapabilitySnapshot, AgentPreflightResult, CreateProjectInput, OnboardProjectResult, PreflightCheck,
    ProjectCommandProfileInput, ProjectPreflightResult, ProjectRuntimeProfileInput, UpdateProjectInput,
};

const AGENT_SNAPSHOT_STALE_MS: i64 = 24 * 60 * 60 * 1000;

#[async_trait::async_trait]
pub trait ProjectAgentCapabilityPort: Send + Sync {
    async fn snapshot(&self, id: &str, refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError>;
}

#[derive(Clone)]
pub struct ProjectService {
    project_repo: Arc<dyn IProjectRepository>,
    conversation_repo: Arc<dyn IConversationRepository>,
    team_repo: Arc<dyn ITeamRepository>,
    agent_port: Arc<dyn ProjectAgentCapabilityPort>,
    repository_onboarder: RepositoryOnboarder,
    knowledge_provider: Arc<dyn ProjectKnowledgeProvider>,
}

impl ProjectService {
    pub fn new(
        project_repo: Arc<dyn IProjectRepository>,
        conversation_repo: Arc<dyn IConversationRepository>,
        team_repo: Arc<dyn ITeamRepository>,
        agent_port: Arc<dyn ProjectAgentCapabilityPort>,
    ) -> Self {
        Self {
            project_repo,
            conversation_repo,
            team_repo,
            agent_port,
            repository_onboarder: RepositoryOnboarder::new(std::env::temp_dir().join("aionui-managed-projects")),
            knowledge_provider: Arc::new(CodebaseMemoryCliProvider::default()),
        }
    }

    pub fn with_managed_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repository_onboarder = RepositoryOnboarder::new(root);
        self
    }

    pub fn with_knowledge_provider(mut self, provider: Arc<dyn ProjectKnowledgeProvider>) -> Self {
        self.knowledge_provider = provider;
        self
    }

    pub async fn onboard(
        &self,
        user_id: &str,
        input: ProjectRepositoryOnboardingInput,
    ) -> Result<OnboardProjectResult, ProjectError> {
        let name = non_empty(input.name, "project name")?;
        validate_project_type(&input.project_type)?;
        let repository = self
            .repository_onboarder
            .onboard(input.source, input.dirty_worktree_choice)
            .await?;
        let now = now_ms();
        let project = ProjectRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: user_id.to_owned(),
            name,
            local_path: repository.local_path.clone(),
            repository_url: repository.repository_url.clone(),
            default_branch: repository.default_branch.clone(),
            project_type: input.project_type,
            created_at: now,
            updated_at: now,
        };
        self.project_repo.create(&project).await?;
        let facts = repository_facts_row(&project.id, &repository)?;
        if let Err(error) = self.project_repo.upsert_repository_facts(&facts).await {
            let _ = self.project_repo.delete_for_user(&project.id, user_id).await;
            return Err(error.into());
        }
        Ok(OnboardProjectResult { project, repository })
    }

    pub async fn get_repository_facts(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectRepositoryFacts, ProjectError> {
        let project = self.get(user_id, project_id).await?;
        let row = self
            .project_repo
            .get_repository_facts(project_id, user_id)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("repository facts for project {project_id}")))?;
        project_repository_facts(row, project.local_path)
    }

    pub async fn get_knowledge_status(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectKnowledgeStatus, ProjectError> {
        let project = self.get(user_id, project_id).await?;
        let repository = inspect_repository(&project.local_path)
            .await
            .map_err(provider_project_error)?;
        let Some(row) = self.project_repo.get_knowledge_index(project_id, user_id).await? else {
            return Ok(ProjectKnowledgeStatus {
                project_id: project_id.into(),
                provider: "codebase-memory".into(),
                provider_project_name: provider_project_name(project_id),
                provider_version: None,
                status: "stale".into(),
                generation: 0,
                source_commit: repository.source_commit,
                indexed_at: None,
                changed_paths: repository.changed_paths,
                error_category: Some("not_indexed".into()),
                updated_at: now_ms(),
            });
        };
        let mut status = knowledge_status_from_row(&row)?;
        if matches!(status.status.as_str(), "healthy" | "stale")
            && (status.source_commit != repository.source_commit || status.changed_paths != repository.changed_paths)
        {
            status.status = "stale".into();
            status.source_commit = repository.source_commit;
            status.changed_paths = repository.changed_paths;
            status.error_category = Some("repository_changed".into());
        }
        Ok(status)
    }

    pub async fn refresh_knowledge(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectKnowledgeStatus, ProjectError> {
        let project = self.get(user_id, project_id).await?;
        let repository = inspect_repository(&project.local_path)
            .await
            .map_err(provider_project_error)?;
        let existing = self.project_repo.get_knowledge_index(project_id, user_id).await?;
        if let Some(row) = existing.as_ref()
            && row.status == "healthy"
            && row.source_commit == repository.source_commit
            && parse_string_list(&row.changed_paths_json)? == repository.changed_paths
        {
            return knowledge_status_from_row(row);
        }

        let project_name = existing
            .as_ref()
            .map(|row| row.provider_project_name.clone())
            .unwrap_or_else(|| provider_project_name(project_id));
        let health = match self.knowledge_provider.health().await {
            Ok(health) => health,
            Err(error) => {
                self.persist_provider_failure(project_id, existing.as_ref(), &project_name, &repository, &error)
                    .await?;
                return Err(provider_project_error(error));
            }
        };
        let generation = existing.as_ref().map_or(1, |row| row.generation + 1);
        let started_at = now_ms();
        let indexing = ProjectKnowledgeIndexRow {
            project_id: project_id.into(),
            provider: health.provider.clone(),
            provider_project_name: project_name.clone(),
            provider_version: health.version.clone(),
            status: "indexing".into(),
            generation: existing.as_ref().map_or(0, |row| row.generation),
            source_commit: repository.source_commit.clone(),
            indexed_at: existing.as_ref().and_then(|row| row.indexed_at),
            changed_paths_json: serialize_string_list(&repository.changed_paths)?,
            error_category: None,
            updated_at: started_at,
        };
        self.project_repo.upsert_knowledge_index(&indexing).await?;

        let request = ProjectKnowledgeProviderRequest {
            project_path: project.local_path,
            provider_project_name: project_name.clone(),
            source_commit: repository.source_commit.clone(),
            changed_paths: repository.changed_paths.clone(),
        };
        let provider_result = if existing.as_ref().is_some_and(|row| row.generation > 0) {
            self.knowledge_provider.update(&request).await
        } else {
            self.knowledge_provider.index(&request).await
        };
        let provider_result = match provider_result {
            Ok(result) => result,
            Err(error) => {
                self.persist_provider_failure(project_id, existing.as_ref(), &project_name, &repository, &error)
                    .await?;
                return Err(provider_project_error(error));
            }
        };
        validate_provider_result(&provider_result, &request)?;
        let architecture = match self.knowledge_provider.architecture(&project_name).await {
            Ok(facts) => facts,
            Err(error) => {
                self.persist_provider_failure(project_id, existing.as_ref(), &project_name, &repository, &error)
                    .await?;
                return Err(provider_project_error(error));
            }
        };
        let indexed_at = now_ms();
        let mut facts = provider_result.facts;
        facts.extend(architecture);
        normalize_and_validate_facts(&mut facts, indexed_at)?;
        let fact_rows = facts
            .into_iter()
            .map(|fact| ProjectKnowledgeFactRow {
                id: uuid::Uuid::now_v7().to_string(),
                project_id: project_id.into(),
                generation,
                kind: fact.kind,
                name: fact.name,
                qualified_name: fact.qualified_name,
                source_path: fact.source_path,
                source_line: fact.source_line,
                indexed_at: fact.indexed_at,
            })
            .collect::<Vec<_>>();
        let completed = ProjectKnowledgeIndexRow {
            project_id: project_id.into(),
            provider: health.provider,
            provider_project_name: project_name,
            provider_version: health.version,
            status: "healthy".into(),
            generation,
            source_commit: provider_result.source_commit,
            indexed_at: Some(indexed_at),
            changed_paths_json: serialize_string_list(&provider_result.changed_paths)?,
            error_category: None,
            updated_at: indexed_at,
        };
        self.project_repo
            .commit_knowledge_generation(&completed, &fact_rows)
            .await?;
        knowledge_status_from_row(&completed)
    }

    async fn persist_provider_failure(
        &self,
        project_id: &str,
        existing: Option<&ProjectKnowledgeIndexRow>,
        project_name: &str,
        repository: &crate::knowledge::RepositoryKnowledgeState,
        error: &KnowledgeProviderError,
    ) -> Result<(), ProjectError> {
        let now = now_ms();
        let status = if matches!(error, KnowledgeProviderError::Unavailable) {
            "unavailable"
        } else {
            "failed"
        };
        self.project_repo
            .upsert_knowledge_index(&ProjectKnowledgeIndexRow {
                project_id: project_id.into(),
                provider: "codebase-memory".into(),
                provider_project_name: project_name.into(),
                provider_version: existing.and_then(|row| row.provider_version.clone()),
                status: status.into(),
                generation: existing.map_or(0, |row| row.generation),
                source_commit: repository.source_commit.clone(),
                indexed_at: existing.and_then(|row| row.indexed_at),
                changed_paths_json: serialize_string_list(&repository.changed_paths)?,
                error_category: Some(error.category().into()),
                updated_at: now,
            })
            .await?;
        Ok(())
    }

    pub async fn list_knowledge_facts(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, ProjectError> {
        self.get(user_id, project_id).await?;
        Ok(self
            .project_repo
            .list_knowledge_facts(project_id, user_id)
            .await?
            .into_iter()
            .map(|row| ProjectKnowledgeFact {
                kind: row.kind,
                name: row.name,
                qualified_name: row.qualified_name,
                source_path: row.source_path,
                source_line: row.source_line,
                indexed_at: row.indexed_at,
            })
            .collect())
    }

    pub async fn task_context(
        &self,
        user_id: &str,
        project_id: &str,
        query: &str,
    ) -> Result<ProjectTaskContext, ProjectError> {
        self.get(user_id, project_id).await?;
        let query = query.trim();
        if query.is_empty() || query.len() > 500 {
            return Err(ProjectError::BadRequest(
                "task context query must contain 1 to 500 bytes".into(),
            ));
        }
        let index = self
            .project_repo
            .get_knowledge_index(project_id, user_id)
            .await?
            .filter(|row| row.generation > 0)
            .ok_or_else(|| ProjectError::Conflict("project knowledge has not been indexed".into()))?;
        let mut context = self
            .knowledge_provider
            .task_context(&index.provider_project_name, query, index.generation)
            .await
            .map_err(provider_project_error)?;
        if context.provider_project_name != index.provider_project_name || context.generation != index.generation {
            return Err(provider_project_error(KnowledgeProviderError::MalformedOutput));
        }
        validate_context(&context)?;
        context.id = uuid::Uuid::now_v7().to_string();
        context.project_id = project_id.into();
        context.query = query.into();
        context.created_at = now_ms();
        self.project_repo
            .insert_knowledge_context(&ProjectKnowledgeContextRow {
                id: context.id.clone(),
                project_id: context.project_id.clone(),
                provider_project_name: context.provider_project_name.clone(),
                generation: context.generation,
                query: context.query.clone(),
                symbols_json: serialize_string_list(&context.symbols)?,
                callers_json: serialize_string_list(&context.callers)?,
                tests_json: serialize_string_list(&context.tests)?,
                routes_json: serialize_string_list(&context.routes)?,
                data_entities_json: serialize_string_list(&context.data_entities)?,
                created_at: context.created_at,
            })
            .await?;
        Ok(context)
    }

    pub async fn create(&self, user_id: &str, input: CreateProjectInput) -> Result<ProjectRow, ProjectError> {
        let name = non_empty(input.name, "project name")?;
        let local_path = canonical_directory(&input.local_path)?;
        validate_project_type(&input.project_type)?;
        let now = now_ms();
        let row = ProjectRow {
            id: uuid::Uuid::now_v7().to_string(),
            user_id: user_id.to_owned(),
            name,
            local_path,
            repository_url: trim_optional(input.repository_url),
            default_branch: trim_optional(input.default_branch),
            project_type: input.project_type,
            created_at: now,
            updated_at: now,
        };
        self.project_repo.create(&row).await?;
        Ok(row)
    }

    pub async fn list(&self, user_id: &str) -> Result<Vec<ProjectRow>, ProjectError> {
        Ok(self.project_repo.list_for_user(user_id).await?)
    }

    pub async fn get(&self, user_id: &str, project_id: &str) -> Result<ProjectRow, ProjectError> {
        self.project_repo
            .get_for_user(project_id, user_id)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project {project_id}")))
    }

    pub async fn update(
        &self,
        user_id: &str,
        project_id: &str,
        input: UpdateProjectInput,
    ) -> Result<ProjectRow, ProjectError> {
        if let Some(ref value) = input.project_type {
            validate_project_type(value)?;
        }
        let params = UpdateProjectParams {
            name: input.name.map(|value| non_empty(value, "project name")).transpose()?,
            local_path: input.local_path.map(|value| canonical_directory(&value)).transpose()?,
            repository_url: input.repository_url.map(trim_optional),
            default_branch: input.default_branch.map(trim_optional),
            project_type: input.project_type,
        };
        Ok(self.project_repo.update_for_user(project_id, user_id, &params).await?)
    }

    pub async fn delete(&self, user_id: &str, project_id: &str) -> Result<(), ProjectError> {
        if !self.project_repo.delete_for_user(project_id, user_id).await? {
            return Err(ProjectError::NotFound(format!("project {project_id}")));
        }
        Ok(())
    }

    pub async fn upsert_command_profile(
        &self,
        user_id: &str,
        project_id: &str,
        input: ProjectCommandProfileInput,
    ) -> Result<ProjectCommandProfileRow, ProjectError> {
        self.get(user_id, project_id).await?;
        if input.command_timeout_seconds <= 0 {
            return Err(ProjectError::BadRequest("command timeout must be positive".into()));
        }
        let row = ProjectCommandProfileRow {
            project_id: project_id.to_owned(),
            install_command: trim_optional(input.install_command),
            format_command: trim_optional(input.format_command),
            lint_command: trim_optional(input.lint_command),
            typecheck_command: trim_optional(input.typecheck_command),
            unit_test_command: trim_optional(input.unit_test_command),
            integration_test_command: trim_optional(input.integration_test_command),
            e2e_command: trim_optional(input.e2e_command),
            build_command: trim_optional(input.build_command),
            security_scan_command: trim_optional(input.security_scan_command),
            command_timeout_seconds: input.command_timeout_seconds,
            updated_at: now_ms(),
        };
        self.project_repo.upsert_command_profile(&row).await?;
        Ok(row)
    }

    pub async fn get_command_profile(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectCommandProfileRow, ProjectError> {
        self.project_repo
            .get_command_profile(project_id, user_id)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("command profile for project {project_id}")))
    }

    pub async fn upsert_runtime_profile(
        &self,
        user_id: &str,
        project_id: &str,
        input: ProjectRuntimeProfileInput,
    ) -> Result<ProjectRuntimeProfileRow, ProjectError> {
        self.get(user_id, project_id).await?;
        if !matches!(input.environment_kind.as_str(), "local" | "container") {
            return Err(ProjectError::BadRequest(format!(
                "unsupported environment kind: {}",
                input.environment_kind
            )));
        }
        let row = ProjectRuntimeProfileRow {
            project_id: project_id.to_owned(),
            environment_kind: input.environment_kind,
            language: trim_optional(input.language),
            package_manager: trim_optional(input.package_manager),
            runtime_version: trim_optional(input.runtime_version),
            env_keys: serde_json::to_string(&input.env_keys)
                .map_err(|error| ProjectError::BadRequest(error.to_string()))?,
            metadata: serde_json::to_string(&input.metadata)
                .map_err(|error| ProjectError::BadRequest(error.to_string()))?,
            updated_at: now_ms(),
        };
        self.project_repo.upsert_runtime_profile(&row).await?;
        Ok(row)
    }

    pub async fn get_runtime_profile(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ProjectRuntimeProfileRow, ProjectError> {
        self.project_repo
            .get_runtime_profile(project_id, user_id)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("runtime profile for project {project_id}")))
    }

    pub async fn bind_resource(
        &self,
        user_id: &str,
        project_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<(), ProjectError> {
        self.get(user_id, project_id).await?;
        let owned = match resource_type {
            "conversation" => self
                .conversation_repo
                .get(resource_id)
                .await?
                .is_some_and(|row| row.user_id == user_id),
            "team" => self
                .team_repo
                .get_team(resource_id)
                .await?
                .is_some_and(|row| row.user_id == user_id),
            "cron" | "channel" => {
                self.project_repo
                    .resource_is_owned(user_id, resource_type, resource_id)
                    .await?
            }
            other => return Err(ProjectError::BadRequest(format!("unsupported resource type: {other}"))),
        };
        if !owned {
            return Err(ProjectError::NotFound(format!("{resource_type} {resource_id}")));
        }
        self.project_repo
            .bind_resource(project_id, user_id, resource_type, resource_id)
            .await?;
        Ok(())
    }

    pub async fn list_resource_links(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<Vec<ProjectResourceLinkRow>, ProjectError> {
        self.get(user_id, project_id).await?;
        Ok(self.project_repo.list_resource_links(project_id, user_id).await?)
    }

    pub async fn get_for_resource(
        &self,
        user_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ProjectRow, ProjectError> {
        if !matches!(resource_type, "conversation" | "team" | "cron" | "channel") {
            return Err(ProjectError::BadRequest(format!(
                "unsupported resource type: {resource_type}"
            )));
        }
        self.project_repo
            .get_for_resource(user_id, resource_type, resource_id)
            .await?
            .ok_or_else(|| ProjectError::NotFound(format!("project for {resource_type} {resource_id}")))
    }

    pub async fn preflight(
        &self,
        user_id: &str,
        project_id: &str,
        agent_ids: &[String],
        refresh_agents: bool,
    ) -> Result<ProjectPreflightResult, ProjectError> {
        let project = self.get(user_id, project_id).await?;
        let command_profile = self.project_repo.get_command_profile(project_id, user_id).await?;
        let runtime_profile = self.project_repo.get_runtime_profile(project_id, user_id).await?;
        let mut checks = inspect_project(&project, command_profile.as_ref(), runtime_profile.as_ref());
        let mut agents = Vec::with_capacity(agent_ids.len());
        for agent_id in agent_ids {
            let snapshot = self.agent_port.snapshot(agent_id, refresh_agents).await?;
            agents.push(inspect_agent(agent_id, snapshot));
        }
        let overall_status = overall_level(
            checks
                .iter()
                .map(|item| item.level.as_str())
                .chain(agents.iter().map(|item| item.level.as_str())),
        );
        checks.sort_by(|left, right| left.code.cmp(&right.code));
        Ok(ProjectPreflightResult {
            project_id: project.id,
            overall_status,
            checks,
            agents,
            checked_at: now_ms(),
        })
    }
}

fn non_empty(value: String, field: &str) -> Result<String, ProjectError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(ProjectError::BadRequest(format!("{field} must not be empty")))
    } else {
        Ok(value)
    }
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value.map(|item| item.trim().to_owned()).filter(|item| !item.is_empty())
}

fn repository_facts_row(
    project_id: &str,
    facts: &ProjectRepositoryFacts,
) -> Result<ProjectRepositoryFactsRow, ProjectError> {
    let json_error = |error: serde_json::Error| ProjectError::Internal(error.to_string());
    Ok(ProjectRepositoryFactsRow {
        project_id: project_id.to_owned(),
        repository_url: facts.repository_url.clone(),
        default_branch: facts.default_branch.clone(),
        baseline_commit: facts.baseline_commit.clone(),
        repository_dirty: facts.dirty,
        dirty_worktree_choice: dirty_choice_name(facts.dirty_worktree_choice).into(),
        dirty_snapshot_ref: facts.dirty_snapshot_ref.clone(),
        credential_reference: facts.credential_reference.clone(),
        detected_languages_json: serde_json::to_string(&facts.languages).map_err(json_error)?,
        detected_package_managers_json: serde_json::to_string(&facts.package_managers).map_err(json_error)?,
        detected_rules_files_json: serde_json::to_string(&facts.rules_files).map_err(json_error)?,
        monorepo_packages_json: serde_json::to_string(&facts.monorepo_packages).map_err(json_error)?,
        submodules_json: serde_json::to_string(&facts.submodules).map_err(json_error)?,
        lfs_detected: facts.lfs_detected,
        detected_at: facts.detected_at,
    })
}

fn project_repository_facts(
    row: ProjectRepositoryFactsRow,
    local_path: String,
) -> Result<ProjectRepositoryFacts, ProjectError> {
    let json_error = |error: serde_json::Error| ProjectError::Internal(error.to_string());
    Ok(ProjectRepositoryFacts {
        local_path,
        repository_url: row.repository_url,
        default_branch: row.default_branch,
        baseline_commit: row.baseline_commit,
        dirty: row.repository_dirty,
        dirty_worktree_choice: match row.dirty_worktree_choice.as_str() {
            "preserve" => DirtyWorktreeChoice::Preserve,
            "snapshot" => DirtyWorktreeChoice::Snapshot,
            "reject" => DirtyWorktreeChoice::Reject,
            value => {
                return Err(ProjectError::Internal(format!(
                    "invalid persisted dirty choice: {value}"
                )));
            }
        },
        dirty_snapshot_ref: row.dirty_snapshot_ref,
        credential_reference: row.credential_reference,
        languages: serde_json::from_str(&row.detected_languages_json).map_err(json_error)?,
        package_managers: serde_json::from_str(&row.detected_package_managers_json).map_err(json_error)?,
        rules_files: serde_json::from_str(&row.detected_rules_files_json).map_err(json_error)?,
        monorepo_packages: serde_json::from_str(&row.monorepo_packages_json).map_err(json_error)?,
        submodules: serde_json::from_str(&row.submodules_json).map_err(json_error)?,
        lfs_detected: row.lfs_detected,
        detected_at: row.detected_at,
    })
}

fn dirty_choice_name(value: DirtyWorktreeChoice) -> &'static str {
    match value {
        DirtyWorktreeChoice::Preserve => "preserve",
        DirtyWorktreeChoice::Snapshot => "snapshot",
        DirtyWorktreeChoice::Reject => "reject",
    }
}

fn validate_project_type(value: &str) -> Result<(), ProjectError> {
    if matches!(value, "single" | "monorepo" | "unknown") {
        Ok(())
    } else {
        Err(ProjectError::BadRequest(format!("unsupported project type: {value}")))
    }
}

fn canonical_directory(value: &str) -> Result<String, ProjectError> {
    let path = Path::new(value);
    let canonical = path
        .canonicalize()
        .map_err(|error| ProjectError::BadRequest(format!("project path cannot be resolved: {error}")))?;
    if !canonical.is_dir() {
        return Err(ProjectError::BadRequest("project path is not a directory".into()));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn check(code: &str, level: &str, summary: impl Into<String>) -> PreflightCheck {
    PreflightCheck {
        code: code.into(),
        level: level.into(),
        summary: summary.into(),
        details: None,
    }
}

fn inspect_project(
    project: &ProjectRow,
    commands: Option<&ProjectCommandProfileRow>,
    runtime: Option<&ProjectRuntimeProfileRow>,
) -> Vec<PreflightCheck> {
    let path = PathBuf::from(&project.local_path);
    if !path.exists() {
        return vec![check("path.exists", "fail", "Project directory no longer exists")];
    }
    if !path.is_dir() {
        return vec![check("path.directory", "fail", "Project path is not a directory")];
    }
    let mut checks = vec![check("path.exists", "pass", "Project directory is available")];
    inspect_git(&path, project, &mut checks);
    if let Some(commands) = commands {
        inspect_commands(&path, commands, &mut checks);
    }
    if let Some(runtime) = runtime {
        inspect_runtime(&path, runtime, &mut checks);
    }
    checks
}

fn inspect_git(path: &Path, project: &ProjectRow, checks: &mut Vec<PreflightCheck>) {
    let Ok(repository) = git2::Repository::discover(path) else {
        checks.push(check(
            "git.repository",
            "warning",
            "Project directory is not inside a Git repository",
        ));
        return;
    };
    checks.push(check("git.repository", "pass", "Git repository detected"));
    let dirty = repository.statuses(None).map(|items| !items.is_empty()).unwrap_or(true);
    checks.push(check(
        "git.dirty",
        if dirty { "warning" } else { "pass" },
        if dirty {
            "Git working tree has local changes"
        } else {
            "Git working tree is clean"
        },
    ));
    if let Some(expected) = project.default_branch.as_deref() {
        let actual = repository
            .head()
            .ok()
            .and_then(|head| head.shorthand().map(str::to_owned));
        let matches = actual.as_deref() == Some(expected);
        checks.push(PreflightCheck {
            code: "git.branch".into(),
            level: if matches { "pass" } else { "warning" }.into(),
            summary: if matches {
                format!("Current branch matches {expected}")
            } else {
                format!(
                    "Current branch {:?} does not match configured branch {expected}",
                    actual
                )
            },
            details: Some(serde_json::json!({ "expected": expected, "actual": actual })),
        });
    }
}

fn inspect_commands(path: &Path, profile: &ProjectCommandProfileRow, checks: &mut Vec<PreflightCheck>) {
    for (name, command) in [
        ("install", profile.install_command.as_deref()),
        ("format", profile.format_command.as_deref()),
        ("lint", profile.lint_command.as_deref()),
        ("typecheck", profile.typecheck_command.as_deref()),
        ("unit_test", profile.unit_test_command.as_deref()),
        ("integration_test", profile.integration_test_command.as_deref()),
        ("e2e", profile.e2e_command.as_deref()),
        ("build", profile.build_command.as_deref()),
        ("security_scan", profile.security_scan_command.as_deref()),
    ] {
        let Some(command) = command else { continue };
        let program = command_program(command);
        let available = program
            .as_deref()
            .is_some_and(|program| program_available(path, program));
        checks.push(check(
            &format!("command.{name}"),
            if available { "pass" } else { "fail" },
            if available {
                format!("Command executable is available: {command}")
            } else {
                format!("Command executable is unavailable: {command}")
            },
        ));
    }
}

fn command_program(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .find(|part| !part.contains('=') || part.starts_with('/') || part.starts_with("./"))
        .map(|part| part.trim_matches(['\'', '"']).to_owned())
}

fn program_available(project_path: &Path, program: &str) -> bool {
    let program_path = Path::new(program);
    if program_path.is_absolute() {
        program_path.is_file()
    } else if program.contains('/') {
        project_path.join(program_path).is_file()
    } else {
        which::which(program).is_ok()
    }
}

fn inspect_runtime(path: &Path, runtime: &ProjectRuntimeProfileRow, checks: &mut Vec<PreflightCheck>) {
    let detected = [
        ("rust", "Cargo.toml"),
        ("typescript", "package.json"),
        ("python", "pyproject.toml"),
    ]
    .into_iter()
    .find_map(|(language, marker)| path.join(marker).is_file().then_some(language));
    if let (Some(configured), Some(detected)) = (runtime.language.as_deref(), detected) {
        checks.push(check(
            "runtime.language",
            if configured.eq_ignore_ascii_case(detected) {
                "pass"
            } else {
                "warning"
            },
            format!("Configured language is {configured}; detected marker suggests {detected}"),
        ));
    }
}

fn inspect_agent(agent_id: &str, snapshot: Option<AgentCapabilitySnapshot>) -> AgentPreflightResult {
    let Some(snapshot) = snapshot else {
        return AgentPreflightResult {
            agent_id: agent_id.into(),
            level: "fail".into(),
            summary: "Agent does not exist".into(),
            snapshot: None,
        };
    };
    let healthy = snapshot.enabled
        && snapshot.installed
        && matches!(snapshot.status.as_str(), "online" | "available")
        && snapshot
            .last_check_status
            .as_deref()
            .is_some_and(|status| matches!(status, "online" | "available"));
    let stale = snapshot
        .last_check_at
        .is_none_or(|checked_at| now_ms().saturating_sub(checked_at) > AGENT_SNAPSHOT_STALE_MS);
    let dynamic_stale = snapshot
        .dynamic_probe
        .as_ref()
        .is_some_and(|probe| now_ms().saturating_sub(probe.checked_at) > AGENT_SNAPSHOT_STALE_MS);
    let dynamic_missing = snapshot.agent_type == "acp" && snapshot.dynamic_probe.is_none();
    let dynamic_failed = snapshot.dynamic_probe.as_ref().is_some_and(|probe| !probe.is_usable());
    let (level, summary) = if !healthy {
        ("fail", "Agent is not currently healthy")
    } else if dynamic_missing {
        ("fail", "ACP Agent has no successful dynamic probe")
    } else if dynamic_stale {
        ("fail", "Agent dynamic probe is stale")
    } else if dynamic_failed {
        ("fail", "Agent failed the dynamic capability probe")
    } else if stale {
        ("warning", "Agent health snapshot is stale")
    } else {
        ("pass", "Agent is available")
    };
    AgentPreflightResult {
        agent_id: agent_id.into(),
        level: level.into(),
        summary: summary.into(),
        snapshot: Some(snapshot),
    }
}

fn overall_level<'a>(levels: impl Iterator<Item = &'a str>) -> String {
    let mut warning = false;
    for level in levels {
        if level == "fail" {
            return "fail".into();
        }
        warning |= level == "warning";
    }
    if warning { "warning" } else { "pass" }.into()
}

fn provider_project_name(project_id: &str) -> String {
    format!("aionui-project-{project_id}")
}

fn provider_project_error(error: KnowledgeProviderError) -> ProjectError {
    ProjectError::Internal(format!("knowledge provider {}", error.category()))
}

fn serialize_string_list(values: &[String]) -> Result<String, ProjectError> {
    serde_json::to_string(values).map_err(|_| ProjectError::Internal("invalid project knowledge data".into()))
}

fn parse_string_list(value: &str) -> Result<Vec<String>, ProjectError> {
    serde_json::from_str(value).map_err(|_| ProjectError::Internal("invalid project knowledge data".into()))
}

fn knowledge_status_from_row(row: &ProjectKnowledgeIndexRow) -> Result<ProjectKnowledgeStatus, ProjectError> {
    Ok(ProjectKnowledgeStatus {
        project_id: row.project_id.clone(),
        provider: row.provider.clone(),
        provider_project_name: row.provider_project_name.clone(),
        provider_version: row.provider_version.clone(),
        status: row.status.clone(),
        generation: row.generation,
        source_commit: row.source_commit.clone(),
        indexed_at: row.indexed_at,
        changed_paths: parse_string_list(&row.changed_paths_json)?,
        error_category: row.error_category.clone(),
        updated_at: row.updated_at,
    })
}

fn validate_provider_result(
    result: &ProjectKnowledgeProviderResult,
    request: &ProjectKnowledgeProviderRequest,
) -> Result<(), ProjectError> {
    if result.provider_project_name != request.provider_project_name
        || result.source_commit != request.source_commit
        || result.changed_paths != request.changed_paths
        || result.facts.len() > 500
    {
        return Err(provider_project_error(KnowledgeProviderError::MalformedOutput));
    }
    Ok(())
}

fn normalize_and_validate_facts(facts: &mut Vec<ProjectKnowledgeFact>, indexed_at: i64) -> Result<(), ProjectError> {
    if facts.len() > 500 {
        return Err(provider_project_error(KnowledgeProviderError::MalformedOutput));
    }
    for fact in facts.iter_mut() {
        if !matches!(
            fact.kind.as_str(),
            "symbol" | "caller" | "test" | "route" | "data_entity" | "architecture"
        ) || fact.name.trim().is_empty()
            || !safe_relative_source_path(&fact.source_path)
            || fact.source_line.is_some_and(|line| line <= 0)
        {
            return Err(provider_project_error(KnowledgeProviderError::MalformedOutput));
        }
        fact.name = fact.name.trim().to_owned();
        fact.indexed_at = indexed_at;
    }
    facts.sort_by(|left, right| {
        (&left.kind, &left.name, &left.source_path, left.source_line).cmp(&(
            &right.kind,
            &right.name,
            &right.source_path,
            right.source_line,
        ))
    });
    facts.dedup_by(|left, right| {
        left.kind == right.kind
            && left.name == right.name
            && left.qualified_name == right.qualified_name
            && left.source_path == right.source_path
            && left.source_line == right.source_line
    });
    Ok(())
}

fn safe_relative_source_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
}

fn validate_context(context: &ProjectTaskContext) -> Result<(), ProjectError> {
    for values in [
        &context.symbols,
        &context.callers,
        &context.tests,
        &context.routes,
        &context.data_entities,
    ] {
        if values.len() > 20 || values.iter().any(|value| value.trim().is_empty() || value.len() > 1000) {
            return Err(provider_project_error(KnowledgeProviderError::MalformedOutput));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::inspect_agent;
    use crate::types::AgentCapabilitySnapshot;

    #[test]
    fn healthy_acp_agent_without_dynamic_probe_fails_formal_preflight() {
        let now = aionui_common::now_ms();
        let result = inspect_agent(
            "codex",
            Some(AgentCapabilitySnapshot {
                id: "codex".into(),
                agent_type: "acp".into(),
                enabled: true,
                installed: true,
                status: "online".into(),
                last_check_status: Some("online".into()),
                last_check_at: Some(now),
                last_success_at: Some(now),
                agent_capabilities: None,
                available_models: None,
                available_modes: None,
                available_commands: None,
                dynamic_probe: None,
            }),
        );

        assert_eq!(result.level, "fail");
        assert!(result.summary.contains("no successful dynamic probe"));
    }
}
