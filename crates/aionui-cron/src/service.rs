use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use aionui_api_types::{
    CreateConversationCronRequest, CreateConversationCronResponse, CreateCronJobRequest, CronJobResponse,
    CronScheduleDto, HasSkillResponse, ListCronJobsQuery, RunNowResponse, SaveCronSkillRequest,
    UpdateConversationCronRequest, UpdateCronJobRequest,
};
use aionui_common::{
    AgentType, ProviderWithModel, WorkspacePathValidationError, generate_prefixed_id, now_ms,
    validate_workspace_path_availability,
};
use aionui_db::{
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository, ICronRepository,
    UpdateCronJobParams, resolve_agent_binding_from_rows,
};
use tracing::{error, info, warn};

use crate::events::CronEventEmitter;

use crate::error::CronError;
use crate::executor::{ExecutionResult, JobExecutor, RETRY_INTERVAL_MS};
use crate::scheduler::{CronScheduler, compute_next_run, validate_schedule};
use crate::skill_file::{delete_skill_file, has_skill_file, write_raw_skill_file, write_skill_file};
use crate::types::{
    CreatedBy, CronAgentConfig, CronJob, CronSchedule, ExecutionMode, cron_job_from_row, cron_job_to_response,
    cron_job_to_row, schedule_from_dto,
};

const PLACEHOLDER_PATTERNS: &[&str] = &[
    "todo:",
    "todo ",
    "fill in",
    "placeholder",
    "replace this",
    "your ",
    "insert ",
    "add your",
    "write your",
    "put your",
];
#[derive(Clone)]
pub struct CronService {
    repo: Arc<dyn ICronRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
    scheduler: Arc<CronScheduler>,
    executor: Arc<JobExecutor>,
    emitter: CronEventEmitter,
    data_dir: PathBuf,
}

pub struct CronServiceDeps {
    pub repo: Arc<dyn ICronRepository>,
    pub agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    pub assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    pub assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
    pub scheduler: Arc<CronScheduler>,
    pub executor: Arc<JobExecutor>,
    pub emitter: CronEventEmitter,
    pub data_dir: PathBuf,
}

impl CronService {
    pub fn new(deps: CronServiceDeps) -> Self {
        Self {
            repo: deps.repo,
            agent_metadata_repo: deps.agent_metadata_repo,
            assistant_definition_repo: deps.assistant_definition_repo,
            assistant_overlay_repo: deps.assistant_overlay_repo,
            scheduler: deps.scheduler,
            executor: deps.executor,
            emitter: deps.emitter,
            data_dir: deps.data_dir,
        }
    }

    // -----------------------------------------------------------------------
    // CRUD
    // -----------------------------------------------------------------------

    pub async fn add_job(&self, req: CreateCronJobRequest) -> Result<CronJob, CronError> {
        self.add_job_internal(req, None, None).await
    }

    pub async fn create_for_conversation_helper(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: CreateConversationCronRequest,
    ) -> Result<CreateConversationCronResponse, CronError> {
        let row = self
            .verify_conversation_helper_context(user_id, conversation_id)
            .await?;

        let schedule_dto = CronScheduleDto::Cron {
            expr: req.schedule,
            tz: None,
            description: Some(req.schedule_description),
        };

        let conversation_title = Some(row.name.clone());
        let (agent_type, agent_config, assistant_backend_override) =
            self.build_agent_config_from_conversation(&row).await;
        let create_req = CreateCronJobRequest {
            name: req.name,
            description: None,
            schedule: schedule_dto,
            prompt: None,
            message: Some(req.message),
            conversation_id: conversation_id.to_owned(),
            conversation_title,
            created_by: "agent".to_owned(),
            execution_mode: Some("existing".to_owned()),
            agent_config,
        };

        let job = self
            .add_job_internal(create_req, Some(agent_type), assistant_backend_override)
            .await?;
        if let Err(err) = self
            .executor
            .bind_cron_job_to_conversation(conversation_id, &job.id)
            .await
        {
            if let Err(cleanup_err) = self.remove_job(&job.id).await {
                warn!(
                    conversation_id,
                    job_id = %job.id,
                    error = %cleanup_err,
                    "Failed to remove cron job after helper conversation binding failed"
                );
            }
            warn!(
                conversation_id,
                job_id = %job.id,
                error = %err,
                "Cron helper failed to bind conversation to job"
            );
            return Err(err);
        }

        Ok(CreateConversationCronResponse {
            job_id: job.id.clone(),
            message: format!("Created cron job '{}' ({})", job.name, job.id),
        })
    }

    pub async fn list_for_conversation_helper(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<CronJob>, CronError> {
        self.verify_conversation_helper_context(user_id, conversation_id)
            .await?;
        self.list_jobs(&ListCronJobsQuery {
            conversation_id: Some(conversation_id.to_owned()),
        })
        .await
    }

    pub async fn update_for_conversation_helper(
        &self,
        user_id: &str,
        conversation_id: &str,
        job_id: &str,
        req: UpdateConversationCronRequest,
    ) -> Result<CronJob, CronError> {
        self.verify_conversation_helper_context(user_id, conversation_id)
            .await?;

        let existing = self.get_job(job_id).await?;
        if existing.conversation_id != conversation_id {
            return Err(CronError::JobNotFound(job_id.to_owned()));
        }

        let job = self
            .update_job(
                job_id,
                UpdateCronJobRequest {
                    name: Some(req.name),
                    description: None,
                    enabled: None,
                    schedule: Some(CronScheduleDto::Cron {
                        expr: req.schedule,
                        tz: None,
                        description: Some(req.schedule_description),
                    }),
                    message: Some(req.message),
                    execution_mode: None,
                    agent_config: None,
                    conversation_title: None,
                    max_retries: None,
                },
            )
            .await?;

        self.executor
            .bind_cron_job_to_conversation(conversation_id, &job.id)
            .await?;

        Ok(job)
    }

    async fn verify_conversation_helper_context(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<aionui_db::models::ConversationRow, CronError> {
        if !self.executor.is_conversation_claimed(conversation_id) {
            return Err(CronError::InvalidAgentConfig(
                "cron helper can only manage jobs during an active conversation turn".into(),
            ));
        }

        self.executor
            .get_conversation_row(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                CronError::Conversation(aionui_conversation::ConversationError::NotFound {
                    id: conversation_id.to_owned(),
                })
            })
    }

    async fn add_job_internal(
        &self,
        req: CreateCronJobRequest,
        runtime_agent_type: Option<String>,
        assistant_backend_override: Option<String>,
    ) -> Result<CronJob, CronError> {
        let schedule = schedule_from_dto(&req.schedule);
        validate_schedule(&schedule)?;
        let resolved_agent_type = match runtime_agent_type {
            Some(agent_type) => agent_type,
            None => self.resolve_new_job_agent_type(req.agent_config.as_ref()).await?,
        };
        validate_aionrs_agent_config(&resolved_agent_type, req.agent_config.as_ref())?;

        let execution_mode = parse_execution_mode(req.execution_mode.as_deref())?;
        let created_by = CreatedBy::from_str(&req.created_by)?;
        let message = req.message.or(req.prompt).unwrap_or_default();

        let agent_config = match req.agent_config {
            Some(config) => Some(
                self.build_cron_agent_config(
                    &resolved_agent_type,
                    sanitize_agent_config_dto(config),
                    assistant_backend_override.as_deref(),
                )
                .await?,
            ),
            None => None,
        };

        let now = now_ms();
        let next_run_at = compute_next_run(&schedule, now);

        let job = CronJob {
            id: generate_prefixed_id("cron"),
            name: req.name,
            enabled: true,
            schedule,
            message,
            execution_mode,
            agent_config,
            conversation_id: req.conversation_id,
            conversation_title: req.conversation_title,
            agent_type: resolved_agent_type,
            created_by,
            skill_content: None,
            description: req.description,
            created_at: now,
            updated_at: now,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        };

        self.validate_job_workspace(&job).await?;

        let row = cron_job_to_row(&job)?;
        self.repo.insert(&row).await?;
        self.bind_existing_conversation_if_needed(&job).await;
        self.scheduler.schedule_job(&job);
        self.emitter.emit_job_created(&cron_job_to_response(&job));

        info!(job_id = %job.id, name = %job.name, "Cron job created");
        Ok(job)
    }

    pub async fn update_job(&self, job_id: &str, req: UpdateCronJobRequest) -> Result<CronJob, CronError> {
        let existing_row = self
            .repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;
        let mut job = cron_job_from_row(existing_row)?;
        job.agent_type = self.resolve_job_agent_type(&job).await?;
        let original_execution_mode = job.execution_mode;

        if let Some(name) = &req.name {
            job.name = name.clone();
        }
        if let Some(description) = &req.description {
            job.description = Some(description.clone());
        }
        if let Some(enabled) = req.enabled {
            job.enabled = enabled;
        }
        if let Some(schedule_dto) = &req.schedule {
            let schedule = schedule_from_dto_with_existing_timezone(schedule_dto, &job.schedule);
            validate_schedule(&schedule)?;
            job.schedule = schedule;
        }
        if let Some(message) = &req.message {
            job.message = message.clone();
        }
        if let Some(mode_str) = &req.execution_mode {
            let requested_mode = parse_execution_mode(Some(mode_str))?;
            if requested_mode != original_execution_mode && self.is_team_conversation_job(&job).await? {
                return Err(CronError::InvalidExecutionMode(
                    "Team cron jobs must keep running in their owning Team conversation".into(),
                ));
            }
            job.execution_mode = requested_mode;
        }
        if req.agent_config.is_some()
            && (matches!(original_execution_mode, ExecutionMode::Existing)
                || matches!(job.execution_mode, ExecutionMode::Existing))
        {
            return Err(CronError::InvalidAgentConfig(
                "ongoing conversation jobs must keep their original assistant".into(),
            ));
        }
        if let Some(config_dto) = &req.agent_config {
            let config_dto = sanitize_agent_config_dto(config_dto.clone());
            job.agent_type = self.resolve_new_job_agent_type(Some(&config_dto)).await?;
            validate_aionrs_agent_config(&job.agent_type, Some(&config_dto))?;
            job.agent_config = Some(self.build_cron_agent_config(&job.agent_type, config_dto, None).await?);
        }
        if let Some(title) = &req.conversation_title {
            job.conversation_title = Some(title.clone());
        }
        if let Some(max_retries) = req.max_retries {
            job.max_retries = max_retries;
        }

        if req.schedule.is_some() || req.enabled.is_some() {
            job.next_run_at = compute_next_run(&job.schedule, now_ms());
        }

        job.updated_at = now_ms();
        self.validate_job_workspace(&job).await?;

        let params = build_update_params(&job, &req);
        self.repo.update(job_id, &params).await?;

        self.bind_existing_conversation_if_needed(&job).await;
        self.scheduler.reschedule_job(&job);
        self.emitter.emit_job_updated(&cron_job_to_response(&job));

        info!(job_id = %job.id, "Cron job updated");
        Ok(job)
    }

    pub async fn remove_job(&self, job_id: &str) -> Result<(), CronError> {
        self.scheduler.cancel_job(job_id);
        if let Err(err) = delete_skill_file(&self.data_dir, job_id).await {
            warn!(job_id, error = %err, "Failed to delete cron skill file during job removal");
        }
        self.repo.delete(job_id).await?;
        self.emitter.emit_job_removed(job_id);
        info!(job_id, "Cron job removed");
        Ok(())
    }

    pub async fn get_job(&self, job_id: &str) -> Result<CronJob, CronError> {
        let row = self
            .repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;
        let mut job = cron_job_from_row(row)?;
        job.agent_type = self.resolve_job_agent_type(&job).await?;
        Ok(job)
    }

    pub async fn list_jobs(&self, query: &ListCronJobsQuery) -> Result<Vec<CronJob>, CronError> {
        let rows = if let Some(conv_id) = &query.conversation_id {
            self.repo.list_by_conversation(conv_id).await?
        } else {
            self.repo.list_all().await?
        };

        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            let mut job = cron_job_from_row(row)?;
            job.agent_type = self.resolve_job_agent_type(&job).await?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    // -----------------------------------------------------------------------
    // Init / Tick / Resume / RunNow
    // -----------------------------------------------------------------------

    pub async fn init(&self) {
        let rows = match self.repo.list_enabled().await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to load enabled cron jobs");
                return;
            }
        };

        let mut scheduled = 0u32;
        let mut orphans = 0u32;

        for row in rows {
            let job = match cron_job_from_row(row) {
                Ok(j) => j,
                Err(e) => {
                    error!(error = %e, "Failed to parse cron job row");
                    continue;
                }
            };

            if self.is_orphan(&job).await {
                warn!(
                    job_id = %job.id,
                    job_name = %job.name,
                    conversation_id = %job.conversation_id,
                    execution_mode = job.execution_mode.as_str(),
                    "Deleting orphan cron job whose bound conversation no longer exists"
                );
                if let Err(e) = self.repo.delete(&job.id).await {
                    error!(job_id = %job.id, error = %e, "Failed to delete orphan cron job");
                }
                orphans += 1;
                continue;
            }

            self.scheduler.schedule_job(&job);
            scheduled += 1;
        }

        info!(scheduled, orphans, "Cron service initialized");
    }

    pub async fn tick(&self, job_id: &str) {
        let row = match self.repo.get_by_id(job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                warn!(job_id, "Tick: job not found, cancelling timer");
                self.scheduler.cancel_job(job_id);
                return;
            }
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to load job");
                return;
            }
        };

        let mut job = match cron_job_from_row(row) {
            Ok(j) => j,
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to parse job");
                return;
            }
        };
        match self.resolve_job_agent_type(&job).await {
            Ok(agent_type) => job.agent_type = agent_type,
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to resolve cron assistant runtime");
                return;
            }
        }

        if !job.enabled {
            info!(job_id, "Tick: job disabled, skipping");
            return;
        }

        let result = self.executor.execute(&job).await;
        self.handle_execution_result(job, result).await;
    }

    pub async fn handle_system_resume(&self) {
        let rows = match self.repo.list_enabled().await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Resume: failed to load enabled jobs");
                return;
            }
        };

        let now = now_ms();

        for row in rows {
            let job = match cron_job_from_row(row) {
                Ok(j) => j,
                Err(e) => {
                    error!(error = %e, "Resume: failed to parse job");
                    continue;
                }
            };

            if let Some(next_run) = job.next_run_at
                && next_run < now
            {
                info!(
                    job_id = %job.id,
                    conversation_id = %job.conversation_id,
                    "Resume: missed job detected, marking missed without auto-execution"
                );
                self.record_missed_execution(&job).await;
                self.insert_missed_job_tips(&job).await;
                self.reschedule_after_missed(&job).await;
                self.emitter.emit_job_executed(&job.id, "missed", None);
                continue;
            }

            self.scheduler.reschedule_job(&job);
        }

        info!("System resume: all cron timers rescheduled");
    }

    pub async fn run_now(&self, job_id: &str) -> Result<RunNowResponse, CronError> {
        let row = self
            .repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;
        let mut job = cron_job_from_row(row)?;
        job.agent_type = self.resolve_job_agent_type(&job).await?;
        let prepared = self.executor.prepare_run_now(&job).await?;
        let conversation_id = prepared.conversation_id.clone();
        let service = self.clone();
        let job_id = job.id.clone();

        tokio::spawn(async move {
            let result = service.executor.execute_prepared(&job, prepared).await;
            service.handle_run_now_result(&job_id, result).await;
        });

        Ok(RunNowResponse { conversation_id })
    }

    // -----------------------------------------------------------------------
    // Skill management
    // -----------------------------------------------------------------------

    pub async fn save_skill(&self, job_id: &str, req: SaveCronSkillRequest) -> Result<(), CronError> {
        let row = self
            .repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;

        validate_skill_body_content(&req.content)?;
        let job = cron_job_from_row(row)?;
        persist_skill_file(&self.data_dir, &job, &req.content).await?;

        let params = UpdateCronJobParams {
            skill_content: Some(Some(req.content)),
            ..Default::default()
        };
        self.repo.update(job_id, &params).await?;
        self.executor.mark_skill_suggest_artifacts_saved(job_id).await?;

        info!(job_id, "Skill content saved");
        Ok(())
    }

    pub async fn has_skill(&self, job_id: &str) -> Result<HasSkillResponse, CronError> {
        let row = self
            .repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;

        let has_skill = has_skill_file(&self.data_dir, job_id).await?
            || row.skill_content.as_ref().is_some_and(|s| !s.trim().is_empty());

        Ok(HasSkillResponse { has_skill })
    }

    pub async fn delete_skill(&self, job_id: &str) -> Result<(), CronError> {
        self.repo
            .get_by_id(job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_owned()))?;

        delete_skill_file(&self.data_dir, job_id).await?;

        let params = UpdateCronJobParams {
            skill_content: Some(None),
            ..Default::default()
        };
        self.repo.update(job_id, &params).await?;

        info!(job_id, "Skill content deleted");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    pub fn to_response(job: &CronJob) -> CronJobResponse {
        cron_job_to_response(job)
    }

    async fn resolve_new_job_agent_type(
        &self,
        agent_config: Option<&aionui_api_types::CronAgentConfigWriteDto>,
    ) -> Result<String, CronError> {
        let Some(assistant_id) = agent_config.and_then(|config| config.assistant_id.as_deref()) else {
            return Err(CronError::InvalidAgentConfig(
                "assistant_id is required for new cron jobs".into(),
            ));
        };

        self.resolve_agent_type_for_assistant_id(assistant_id).await
    }

    async fn resolve_job_agent_type(&self, job: &CronJob) -> Result<String, CronError> {
        if !job.agent_type.trim().is_empty() {
            return Ok(job.agent_type.clone());
        }

        let Some(assistant_id) = job
            .agent_config
            .as_ref()
            .and_then(|config| config.assistant_id.as_deref().or(config.custom_agent_id.as_deref()))
            .filter(|value| !value.trim().is_empty())
        else {
            return Err(CronError::InvalidAgentConfig(
                "assistant_id is required for cron jobs".into(),
            ));
        };

        self.resolve_agent_type_for_assistant_id(assistant_id).await
    }

    async fn is_team_conversation_job(&self, job: &CronJob) -> Result<bool, CronError> {
        let conversation_id = job.conversation_id.trim();
        if conversation_id.is_empty() {
            return Ok(false);
        }

        let Some(row) = self.executor.get_conversation_row(conversation_id).await? else {
            return Ok(false);
        };
        let extra = serde_json::from_str::<serde_json::Value>(&row.extra)?;
        Ok(extra
            .get("team_id")
            .or_else(|| extra.get("teamId"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty()))
    }

    async fn resolve_agent_type_for_assistant_id(&self, assistant_id: &str) -> Result<String, CronError> {
        let definition = self
            .assistant_definition_repo
            .get_by_assistant_id(assistant_id)
            .await?
            .ok_or_else(|| CronError::InvalidAgentConfig(format!("assistant '{assistant_id}' not found")))?;
        let overlay = self.assistant_overlay_repo.get(&definition.id).await?;
        let effective_agent_id = overlay
            .as_ref()
            .and_then(|item| item.agent_id_override.as_deref())
            .unwrap_or(definition.agent_id.as_str());
        let effective_backend = self.runtime_backend_for_agent_id(effective_agent_id).await?;

        Ok(runtime_agent_type_for_backend(&effective_backend).to_owned())
    }

    async fn bind_existing_conversation_if_needed(&self, job: &CronJob) {
        if !matches!(job.execution_mode, ExecutionMode::Existing) || job.conversation_id.trim().is_empty() {
            return;
        }

        if let Err(err) = self
            .executor
            .bind_cron_job_to_conversation(&job.conversation_id, &job.id)
            .await
        {
            warn!(
                conversation_id = %job.conversation_id,
                job_id = %job.id,
                error = %err,
                "Failed to bind existing-conversation cron job to conversation"
            );
        }
    }

    async fn is_orphan(&self, job: &CronJob) -> bool {
        // NewConversation jobs never depend on an existing conversation —
        // every run materializes a fresh one. They must not be cleaned up
        // based on conversation state.
        if matches!(job.execution_mode, ExecutionMode::NewConversation) {
            return false;
        }

        // Existing-mode jobs can legitimately carry an empty conversation_id
        // until the first run performs lazy binding. Leave them alone.
        if job.conversation_id.trim().is_empty() {
            return false;
        }

        if self.executor.is_conversation_claimed(&job.conversation_id) {
            return false;
        }

        // Only true orphan case: Existing + bound conversation_id, but that
        // conversation has been deleted.
        match self.executor.conversation_exists(&job.conversation_id).await {
            Ok(exists) => !exists,
            Err(err) => {
                warn!(
                    job_id = %job.id,
                    conversation_id = %job.conversation_id,
                    error = %err,
                    "Failed to verify cron conversation during orphan cleanup"
                );
                false
            }
        }
    }

    async fn validate_job_workspace(&self, job: &CronJob) -> Result<(), CronError> {
        let workspace = self.executor.resolve_job_workspace_raw(job).await?;
        match validate_workspace_path_availability(&workspace) {
            Ok(_) => Ok(()),
            Err(WorkspacePathValidationError::Empty) => Ok(()),
            Err(WorkspacePathValidationError::DoesNotExist(path))
            | Err(WorkspacePathValidationError::NotDirectory(path))
            | Err(WorkspacePathValidationError::NotAccessible { path, .. }) => {
                Err(CronError::WorkspacePathUnavailable(path))
            }
        }
    }

    async fn handle_execution_result(&self, job: CronJob, result: ExecutionResult) {
        let job_id = &job.id;

        match result {
            ExecutionResult::Success { conversation_id } => {
                self.update_job_after_success(job_id, &conversation_id).await;
                self.reschedule_after_execution(&job).await;
                self.emitter.emit_job_executed(job_id, "ok", None);
            }
            ExecutionResult::Retrying { attempt } => {
                let params = UpdateCronJobParams {
                    retry_count: Some(attempt),
                    ..Default::default()
                };
                if let Err(e) = self.repo.update(job_id, &params).await {
                    error!(job_id, error = %e, "Failed to update retry count");
                }
                self.schedule_retry(job_id, attempt);
            }
            ExecutionResult::Skipped => {
                let params = UpdateCronJobParams {
                    last_status: Some(Some("skipped".into())),
                    retry_count: Some(0),
                    ..Default::default()
                };
                if let Err(e) = self.repo.update(job_id, &params).await {
                    error!(job_id, error = %e, "Failed to update skipped status");
                }
                self.reschedule_after_execution(&job).await;
                self.emitter.emit_job_executed(job_id, "skipped", None);
            }
            ExecutionResult::Error { message } => {
                self.update_job_after_error(job_id, &message).await;
                self.reschedule_after_execution(&job).await;
                self.emitter.emit_job_executed(job_id, "error", Some(&message));
            }
        }
    }

    async fn handle_run_now_result(&self, job_id: &str, result: ExecutionResult) {
        match result {
            ExecutionResult::Success { conversation_id } => {
                self.update_job_after_success(job_id, &conversation_id).await;
                self.emitter.emit_job_executed(job_id, "ok", None);
            }
            ExecutionResult::Error { message } => {
                self.update_job_after_error(job_id, &message).await;
                self.emitter.emit_job_executed(job_id, "error", Some(&message));
            }
            ExecutionResult::Retrying { attempt } => {
                let params = UpdateCronJobParams {
                    retry_count: Some(attempt),
                    ..Default::default()
                };
                if let Err(err) = self.repo.update(job_id, &params).await {
                    error!(
                        job_id,
                        error = %err,
                        "Failed to update run-now retry count"
                    );
                }
            }
            ExecutionResult::Skipped => {
                let params = UpdateCronJobParams {
                    last_status: Some(Some("skipped".into())),
                    retry_count: Some(0),
                    ..Default::default()
                };
                if let Err(err) = self.repo.update(job_id, &params).await {
                    error!(
                        job_id,
                        error = %err,
                        "Failed to update run-now skipped status"
                    );
                }
                self.emitter.emit_job_executed(job_id, "skipped", None);
            }
        }
    }

    async fn update_job_after_success(&self, job_id: &str, conversation_id: &str) {
        let existing_row = match self.repo.get_by_id(job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                error!(job_id, error = %e, "Failed to read job for run_count");
                return;
            }
        };
        let now = now_ms();
        // Persist the conversation_id back onto the job the first time an
        // "existing" job is materialized (lazy binding). Subsequent runs then
        // reuse the same conversation, matching the UX where the job is the
        // continuation anchor.
        let needs_conversation_bind =
            existing_row.conversation_id.trim().is_empty() && !conversation_id.trim().is_empty();
        let params = UpdateCronJobParams {
            last_run_at: Some(Some(now)),
            last_status: Some(Some("ok".into())),
            last_error: Some(None),
            retry_count: Some(0),
            run_count: Some(existing_row.run_count + 1),
            conversation_id: needs_conversation_bind.then(|| conversation_id.to_owned()),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(job_id, &params).await {
            error!(job_id, error = %e, "Failed to update job after success");
            return;
        }

        if needs_conversation_bind
            && let Err(e) = self
                .executor
                .bind_cron_job_to_conversation(conversation_id, job_id)
                .await
        {
            warn!(
                job_id,
                conversation_id,
                error = %e,
                "Failed to bind lazily-created conversation to cron job"
            );
        }
    }

    async fn update_job_after_error(&self, job_id: &str, message: &str) {
        let run_count = match self.repo.get_by_id(job_id).await {
            Ok(Some(r)) => r.run_count,
            Ok(None) => return,
            Err(e) => {
                error!(job_id, error = %e, "Failed to read job for run_count");
                return;
            }
        };
        let now = now_ms();
        let params = UpdateCronJobParams {
            last_run_at: Some(Some(now)),
            last_status: Some(Some("error".into())),
            last_error: Some(Some(message.to_owned())),
            retry_count: Some(0),
            run_count: Some(run_count + 1),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(job_id, &params).await {
            error!(job_id, error = %e, "Failed to update job after error");
        }
    }

    async fn reschedule_after_execution(&self, job: &CronJob) {
        let is_at = matches!(job.schedule, CronSchedule::At { .. });
        if is_at {
            let params = UpdateCronJobParams {
                enabled: Some(false),
                next_run_at: Some(None),
                ..Default::default()
            };
            if let Err(e) = self.repo.update(&job.id, &params).await {
                error!(job_id = %job.id, error = %e, "Failed to disable at-type job");
            }
            self.scheduler.cancel_job(&job.id);

            let disabled = CronJob {
                enabled: false,
                next_run_at: None,
                ..job.clone()
            };
            self.emitter.emit_job_updated(&cron_job_to_response(&disabled));

            info!(job_id = %job.id, "At-type job executed, auto-disabled");
            return;
        }

        let now = now_ms();
        let next = compute_next_run(&job.schedule, now);
        let updated = CronJob {
            next_run_at: next,
            ..job.clone()
        };
        let params = UpdateCronJobParams {
            next_run_at: Some(next),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&job.id, &params).await {
            error!(job_id = %job.id, error = %e, "Failed to update next_run_at");
        }
        self.scheduler.reschedule_job(&updated);
    }

    async fn record_missed_execution(&self, job: &CronJob) {
        let params = UpdateCronJobParams {
            last_status: Some(Some("missed".into())),
            last_error: Some(None),
            retry_count: Some(0),
            ..Default::default()
        };
        if let Err(err) = self.repo.update(&job.id, &params).await {
            error!(
                job_id = %job.id,
                error = %err,
                "Failed to mark cron job as missed"
            );
        }
    }

    async fn insert_missed_job_tips(&self, job: &CronJob) {
        if job.conversation_id.trim().is_empty() {
            return;
        }

        let content = format!(
            "Scheduled task \"{}\" was missed while the system was unavailable. It was not run automatically.",
            job.name
        );

        match self
            .executor
            .insert_tips_message(&job.conversation_id, &content, "warning")
            .await
        {
            Ok(()) => self
                .emitter
                .emit_conversation_tips(&job.conversation_id, &content, "warning"),
            Err(err) => {
                warn!(
                    job_id = %job.id,
                    conversation_id = %job.conversation_id,
                    error = %err,
                    "Failed to persist missed-job tips message"
                );
            }
        }
    }

    async fn reschedule_after_missed(&self, job: &CronJob) {
        let is_at = matches!(job.schedule, CronSchedule::At { .. });
        if is_at {
            let params = UpdateCronJobParams {
                enabled: Some(false),
                next_run_at: Some(None),
                ..Default::default()
            };
            if let Err(err) = self.repo.update(&job.id, &params).await {
                error!(
                    job_id = %job.id,
                    error = %err,
                    "Failed to disable missed at-type job"
                );
            }
            self.scheduler.cancel_job(&job.id);
            return;
        }

        let next = compute_next_run(&job.schedule, now_ms());
        let params = UpdateCronJobParams {
            next_run_at: Some(next),
            ..Default::default()
        };
        if let Err(err) = self.repo.update(&job.id, &params).await {
            error!(
                job_id = %job.id,
                error = %err,
                "Failed to reschedule missed cron job"
            );
            return;
        }

        let updated = CronJob {
            next_run_at: next,
            ..job.clone()
        };
        self.scheduler.reschedule_job(&updated);
    }

    fn schedule_retry(&self, job_id: &str, _attempt: i64) {
        let next_run = now_ms() + RETRY_INTERVAL_MS as i64;
        let retry_job = CronJob {
            id: job_id.to_owned(),
            name: String::new(),
            enabled: true,
            schedule: CronSchedule::At {
                at_ms: next_run,
                description: None,
            },
            message: String::new(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: String::new(),
            conversation_title: None,
            agent_type: String::new(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 0,
            updated_at: 0,
            next_run_at: Some(next_run),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 0,
        };
        self.scheduler.schedule_job(&retry_job);
    }

    pub async fn delete_jobs_by_conversation(&self, conversation_id: &str) {
        let jobs = match self.repo.list_by_conversation(conversation_id).await {
            Ok(rows) => rows,
            Err(e) => {
                error!(conversation_id, error = %e, "Failed to list cron jobs for cascade delete");
                return;
            }
        };

        for row in &jobs {
            self.scheduler.cancel_job(&row.id);
            self.emitter.emit_job_removed(&row.id);
        }

        if let Err(e) = self.repo.delete_by_conversation(conversation_id).await {
            error!(conversation_id, error = %e, "Failed to cascade-delete cron jobs");
        } else if !jobs.is_empty() {
            info!(
                conversation_id,
                count = jobs.len(),
                "Cascade-deleted cron jobs for conversation"
            );
        }
    }

    async fn build_agent_config_from_conversation(
        &self,
        row: &aionui_db::models::ConversationRow,
    ) -> (
        String,
        Option<aionui_api_types::CronAgentConfigWriteDto>,
        Option<String>,
    ) {
        let extra = serde_json::from_str::<serde_json::Value>(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let assistant_snapshot = match self.executor.get_assistant_snapshot(&row.id).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(
                    conversation_id = %row.id,
                    error = %err,
                    "Failed to load conversation assistant snapshot for cron agent config"
                );
                None
            }
        };
        // Both interactive `send_message` and the cron executor parse
        // `conversation.model` via the same helper. Keeping the cron-side
        // `agent_config.model` derivation in sync with that parser prevents
        // the cached vendor-label fallback (`"aionrs"`) from sneaking back in
        // (Sentry ELECTRON-1HM).
        let model_resolved = aionui_conversation::task_options::provider_model_from_conversation_row(row);
        let model = (!model_resolved.provider_id.is_empty()).then_some(&model_resolved);
        let preset_assistant_id = get_string(&extra, &["preset_assistant_id", "presetAssistantId"]);
        let extra_assistant_id = get_string(&extra, &["assistant_id", "assistantId"]).or(preset_assistant_id);
        let snapshot_assistant_id = assistant_snapshot
            .as_ref()
            .map(|snapshot| snapshot.assistant_id.trim().to_owned())
            .filter(|value| !value.is_empty());
        let legacy_agent_label = if row.r#type == "aionrs" {
            Some("aionrs".to_owned())
        } else {
            model
                .map(|value| value.provider_id.clone())
                .filter(|value| !value.is_empty())
                .or_else(|| get_string(&extra, &["backend"]))
                .or_else(|| Some(row.r#type.clone()))
        };
        let legacy_assistant_id = match (
            snapshot_assistant_id.as_ref(),
            extra_assistant_id.as_ref(),
            legacy_agent_label,
        ) {
            (None, None, Some(label)) => self.resolve_assistant_id_for_agent_label(&label).await,
            _ => None,
        };
        let fallback_assistant_id = match (
            snapshot_assistant_id.as_ref(),
            extra_assistant_id.as_ref(),
            legacy_assistant_id.as_ref(),
        ) {
            (None, None, None) => self.resolve_default_assistant_id().await,
            _ => None,
        };
        let uses_default_assistant_fallback = fallback_assistant_id.is_some();
        let assistant_id = snapshot_assistant_id
            .or(extra_assistant_id)
            .or(legacy_assistant_id)
            .or(fallback_assistant_id);
        let assistant_name = match assistant_id.as_deref() {
            Some(assistant_id) => match self.resolve_assistant_name(Some(assistant_id)).await {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        conversation_id = %row.id,
                        assistant_id,
                        error = %err,
                        "Failed to resolve assistant name for cron agent config"
                    );
                    None
                }
            },
            None => None,
        };
        let snapshot_backend = match assistant_snapshot.as_ref() {
            Some(snapshot) => match self.runtime_backend_for_agent_id(snapshot.agent_id.trim()).await {
                Ok(value) => Some(value).filter(|value| !value.is_empty()),
                Err(err) => {
                    warn!(
                        conversation_id = %row.id,
                        error = %err,
                        "Failed to resolve assistant snapshot agent id for cron agent config"
                    );
                    None
                }
            },
            None => None,
        };
        let assistant_backend = if uses_default_assistant_fallback {
            None
        } else {
            snapshot_backend.clone().or(self
                .resolve_assistant_backend(assistant_id.as_deref())
                .await
                .unwrap_or(None))
        };

        let backend = if row.r#type == "aionrs" {
            model
                .map(|value| value.provider_id.clone())
                .filter(|value| !value.is_empty())
                .or_else(|| get_string(&extra, &["backend"]))
                .or_else(|| assistant_backend.clone())
                .unwrap_or_else(|| "aionrs".to_owned())
        } else {
            assistant_backend
                .clone()
                .or_else(|| {
                    model
                        .map(|value| value.provider_id.clone())
                        .filter(|value| !value.is_empty())
                })
                .or_else(|| get_string(&extra, &["backend"]))
                .unwrap_or_else(|| row.r#type.clone())
        };

        let agent_type_enum = serde_json::from_value::<AgentType>(serde_json::Value::String(row.r#type.clone())).ok();
        let full_auto_mode = agent_type_enum
            .unwrap_or(AgentType::Acp)
            .full_auto_mode_id(Some(backend.as_str()))
            .to_owned();
        let agent_config = aionui_api_types::CronAgentConfigWriteDto {
            name: assistant_name
                .or_else(|| get_string(&extra, &["agent_name", "agentName"]))
                .unwrap_or_else(|| row.name.clone()),
            cli_path: get_string(&extra, &["cli_path", "cliPath"]).or_else(|| {
                extra
                    .get("gateway")
                    .and_then(|gateway| gateway.get("cli_path").or_else(|| gateway.get("cliPath")))
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned)
            }),
            assistant_id,
            mode: Some(full_auto_mode),
            model_id: get_string(&extra, &["current_model_id", "currentModelId"])
                .or_else(|| {
                    model.and_then(|value| {
                        value
                            .use_model
                            .clone()
                            .or_else(|| (!value.model.is_empty()).then(|| value.model.clone()))
                    })
                })
                .or_else(|| {
                    assistant_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.resolved_model_id.clone())
                }),
            model: (row.r#type == "aionrs").then(|| model.cloned()).flatten(),
            config_options: None,
            workspace: get_string(&extra, &["workspace"]),
        };

        (row.r#type.clone(), Some(agent_config), snapshot_backend)
    }

    async fn build_cron_agent_config(
        &self,
        runtime_agent_type: &str,
        config: aionui_api_types::CronAgentConfigWriteDto,
        _assistant_backend_override: Option<&str>,
    ) -> Result<CronAgentConfig, CronError> {
        let Some(assistant_id) = config.assistant_id.as_deref().filter(|value| !value.trim().is_empty()) else {
            return Err(CronError::InvalidAgentConfig(
                "assistant_id is required for cron jobs".into(),
            ));
        };

        self.resolve_assistant_backend(Some(assistant_id))
            .await?
            .ok_or_else(|| {
                CronError::InvalidAgentConfig(format!(
                    "assistant '{assistant_id}' could not resolve a runtime backend"
                ))
            })?;

        Ok(CronAgentConfig {
            name: config.name,
            cli_path: config.cli_path,
            is_preset: None,
            assistant_id: config.assistant_id,
            custom_agent_id: None,
            mode: config.mode,
            model_id: config.model_id,
            model: normalize_model(config.model, runtime_agent_type)?,
            config_options: config.config_options,
            workspace: config.workspace,
        })
    }

    async fn resolve_assistant_backend(&self, assistant_id: Option<&str>) -> Result<Option<String>, CronError> {
        let Some(assistant_id) = assistant_id.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };

        let Some(definition) = self.assistant_definition_repo.get_by_assistant_id(assistant_id).await? else {
            return Ok(None);
        };
        let overlay = self.assistant_overlay_repo.get(&definition.id).await?;
        let effective_agent_id = overlay
            .as_ref()
            .and_then(|item| item.agent_id_override.as_deref())
            .unwrap_or(definition.agent_id.as_str());

        Ok(Some(self.runtime_backend_for_agent_id(effective_agent_id).await?))
    }

    async fn resolve_assistant_name(&self, assistant_id: Option<&str>) -> Result<Option<String>, CronError> {
        let Some(assistant_id) = assistant_id.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };

        Ok(self
            .assistant_definition_repo
            .get_by_assistant_id(assistant_id)
            .await?
            .map(|definition| definition.name.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    async fn resolve_assistant_id_for_agent_label(&self, agent_label: &str) -> Option<String> {
        let rows = self.agent_metadata_repo.list_all().await.ok()?;
        let binding = resolve_agent_binding_from_rows(&rows, agent_label)?;
        self.assistant_definition_repo
            .list()
            .await
            .ok()?
            .into_iter()
            .filter(|definition| definition.deleted_at.is_none() && definition.agent_id == binding.agent_id)
            .min_by_key(|definition| {
                let source_rank = match definition.source.as_str() {
                    "builtin" => 0,
                    "generated" => 1,
                    "user" => 2,
                    _ => 3,
                };
                (source_rank, definition.name.clone())
            })
            .map(|definition| definition.assistant_id)
    }

    async fn resolve_default_assistant_id(&self) -> Option<String> {
        self.assistant_definition_repo
            .list()
            .await
            .ok()?
            .into_iter()
            .filter(|definition| definition.deleted_at.is_none())
            .min_by_key(|definition| {
                let source_rank = match definition.source.as_str() {
                    "builtin" => 0,
                    "generated" => 1,
                    "user" => 2,
                    _ => 3,
                };
                (source_rank, definition.name.clone())
            })
            .map(|definition| definition.assistant_id)
    }

    async fn runtime_backend_for_agent_id(&self, agent_id: &str) -> Result<String, CronError> {
        let rows = self.agent_metadata_repo.list_all().await?;
        Ok(resolve_agent_binding_from_rows(&rows, agent_id)
            .map(|binding| binding.runtime_backend)
            .unwrap_or_else(|| agent_id.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// OnConversationDelete implementation (cascade delete)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl aionui_common::OnConversationDelete for CronService {
    async fn on_conversation_deleted(&self, conversation_id: &str) {
        self.delete_jobs_by_conversation(conversation_id).await;
    }
}

fn get_string(extra: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        extra
            .get(*key)
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
            .filter(|value| !value.is_empty())
    })
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn runtime_agent_type_for_backend(backend: &str) -> &'static str {
    if backend == "aionrs" { "aionrs" } else { "acp" }
}

fn normalize_model(
    model: Option<ProviderWithModel>,
    runtime_agent_type: &str,
) -> Result<Option<ProviderWithModel>, CronError> {
    let Some(mut model) = model else {
        return Ok(None);
    };

    model.provider_id = model.provider_id.trim().to_owned();
    model.model = model.model.trim().to_owned();
    model.use_model = model
        .use_model
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());

    if runtime_agent_type == "aionrs" && (model.provider_id.is_empty() || model.model.is_empty()) {
        return Err(CronError::InvalidAgentConfig(
            "aionrs cron jobs require agent_config.model.provider_id and agent_config.model.model".into(),
        ));
    }

    if model.provider_id.is_empty() || model.model.is_empty() {
        return Ok(None);
    }

    Ok(Some(model))
}

fn validate_aionrs_agent_config(
    agent_type: &str,
    agent_config: Option<&aionui_api_types::CronAgentConfigWriteDto>,
) -> Result<(), CronError> {
    if agent_type != "aionrs" {
        return Ok(());
    }
    let model_ok = agent_config.is_some_and(|c| {
        c.model
            .as_ref()
            .is_some_and(|value| !value.provider_id.trim().is_empty() && !value.model.trim().is_empty())
    });
    if !model_ok {
        return Err(CronError::InvalidAgentConfig(
            "aionrs cron jobs require agent_config.model.provider_id and agent_config.model.model".into(),
        ));
    }
    Ok(())
}

fn parse_execution_mode(mode: Option<&str>) -> Result<ExecutionMode, CronError> {
    match mode {
        None | Some("existing") => Ok(ExecutionMode::Existing),
        Some(s) => ExecutionMode::from_str(s),
    }
}

fn validate_skill_body_content(content: &str) -> Result<(), CronError> {
    let trimmed = content.trim();

    if trimmed.is_empty() {
        return Err(CronError::InvalidSkillContent("content must not be empty".into()));
    }

    let lower = trimmed.to_lowercase();
    for pattern in PLACEHOLDER_PATTERNS {
        if lower.starts_with(pattern) {
            return Err(CronError::InvalidSkillContent(
                "content appears to be placeholder text".into(),
            ));
        }
    }

    Ok(())
}

fn schedule_description(schedule: &CronSchedule) -> Option<&str> {
    match schedule {
        CronSchedule::At { description, .. }
        | CronSchedule::Every { description, .. }
        | CronSchedule::Cron { description, .. } => description.as_deref(),
    }
}

async fn persist_skill_file(data_dir: &Path, job: &CronJob, raw_content: &str) -> Result<(), CronError> {
    match write_raw_skill_file(data_dir, &job.id, raw_content).await {
        Ok(_) => Ok(()),
        Err(CronError::InvalidSkillContent(_)) => {
            let description = job
                .description
                .clone()
                .unwrap_or_else(|| format!("Saved cron skill for {}", job.name));
            write_skill_file(
                data_dir,
                &job.id,
                &job.name,
                &description,
                raw_content.trim(),
                schedule_description(&job.schedule),
            )
            .await
            .map(|_| ())
        }
        Err(err) => Err(err),
    }
}

fn build_update_params(job: &CronJob, req: &UpdateCronJobRequest) -> UpdateCronJobParams {
    let (schedule_kind, schedule_value, schedule_tz, schedule_description) = if req.schedule.is_some() {
        let (k, v, tz, d) = schedule_to_row_fields(&job.schedule);
        (Some(k), Some(v), Some(tz), Some(d))
    } else {
        (None, None, None, None)
    };

    let agent_config = req.agent_config.as_ref().and_then(|_| {
        job.agent_config
            .as_ref()
            .map(|config| Some(serde_json::to_string(config).unwrap_or_default()))
    });

    UpdateCronJobParams {
        name: req.name.clone(),
        enabled: req.enabled,
        schedule_kind,
        schedule_value,
        schedule_tz,
        schedule_description,
        payload_message: req.message.clone(),
        execution_mode: req.execution_mode.clone(),
        agent_config,
        conversation_id: None,
        conversation_title: req.conversation_title.as_ref().map(|t| Some(t.clone())),
        skill_content: None,
        description: req.description.as_ref().map(|value| Some(value.clone())),
        next_run_at: if req.schedule.is_some() || req.enabled.is_some() {
            Some(job.next_run_at)
        } else {
            None
        },
        last_run_at: None,
        last_status: None,
        last_error: None,
        run_count: None,
        retry_count: None,
    }
}

fn sanitize_agent_config_dto(
    mut config: aionui_api_types::CronAgentConfigWriteDto,
) -> aionui_api_types::CronAgentConfigWriteDto {
    if let Some(value) = config.assistant_id.as_mut() {
        let trimmed = value.trim().to_owned();
        if trimmed.is_empty() {
            config.assistant_id = None;
        } else {
            *value = trimmed;
        }
    }
    config
}

fn schedule_from_dto_with_existing_timezone(dto: &CronScheduleDto, existing: &CronSchedule) -> CronSchedule {
    match dto {
        CronScheduleDto::Cron { expr, tz, description } => CronSchedule::Cron {
            expr: expr.clone(),
            tz: tz.clone().or_else(|| match existing {
                CronSchedule::Cron { tz, .. } => tz.clone(),
                _ => None,
            }),
            description: description.clone(),
        },
        _ => schedule_from_dto(dto),
    }
}

fn schedule_to_row_fields(schedule: &CronSchedule) -> (String, String, Option<String>, Option<String>) {
    match schedule {
        CronSchedule::At { at_ms, description } => ("at".to_owned(), at_ms.to_string(), None, description.clone()),
        CronSchedule::Every { every_ms, description } => {
            ("every".to_owned(), every_ms.to_string(), None, description.clone())
        }
        CronSchedule::Cron { expr, tz, description } => {
            ("cron".to_owned(), expr.clone(), tz.clone(), description.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_skill_body_content -------------------------------------------

    #[test]
    fn validate_skill_empty_content() {
        let err = validate_skill_body_content("").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_whitespace_only() {
        let err = validate_skill_body_content("   \n  ").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_todo() {
        let err = validate_skill_body_content("TODO: fill in later").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_fill_in() {
        let err = validate_skill_body_content("Fill in your instructions here").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_replace() {
        let err = validate_skill_body_content("Replace this with your skill").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_valid_content() {
        assert!(validate_skill_body_content("---\nname: test\n---\nDo something useful").is_ok());
    }

    #[test]
    fn validate_skill_valid_short() {
        assert!(validate_skill_body_content("Run daily report").is_ok());
    }

    // -- validate_aionrs_agent_config ----------------------------------------

    fn agent_cfg_dto(provider_id: &str) -> aionui_api_types::CronAgentConfigWriteDto {
        aionui_api_types::CronAgentConfigWriteDto {
            name: "provider".into(),
            cli_path: None,
            assistant_id: Some("assistant-1".into()),
            mode: None,
            model_id: Some("gpt-4o".into()),
            model: Some(ProviderWithModel {
                provider_id: provider_id.to_owned(),
                model: "gpt-4o".into(),
                use_model: None,
            }),
            config_options: None,
            workspace: None,
        }
    }

    #[test]
    fn validate_aionrs_accepts_valid_config() {
        let cfg = agent_cfg_dto("4056cdea");
        assert!(validate_aionrs_agent_config("aionrs", Some(&cfg)).is_ok());
    }

    #[test]
    fn validate_aionrs_rejects_missing_config() {
        let err = validate_aionrs_agent_config("aionrs", None).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_aionrs_rejects_empty_provider_id() {
        let cfg = agent_cfg_dto("");
        let err = validate_aionrs_agent_config("aionrs", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_aionrs_rejects_whitespace_provider_id() {
        let cfg = agent_cfg_dto("   ");
        let err = validate_aionrs_agent_config("aionrs", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_aionrs_ignores_non_aionrs_type() {
        // ACP / other types may legitimately omit agent_config or leave model empty.
        assert!(validate_aionrs_agent_config("acp", None).is_ok());
        let cfg = agent_cfg_dto("");
        assert!(validate_aionrs_agent_config("claude", Some(&cfg)).is_ok());
    }

    #[test]
    fn sanitize_agent_config_dto_clears_legacy_ids_when_assistant_id_present() {
        let config = aionui_api_types::CronAgentConfigWriteDto {
            name: "Helper".into(),
            cli_path: None,
            assistant_id: Some("assistant-1".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        };

        let sanitized = sanitize_agent_config_dto(config);

        assert_eq!(sanitized.assistant_id.as_deref(), Some("assistant-1"));
    }

    #[test]
    fn sanitize_agent_config_dto_rejects_legacy_custom_agent_id_without_assistant_id() {
        let err = serde_json::from_value::<aionui_api_types::CronAgentConfigWriteDto>(serde_json::json!({
            "name": "Helper",
            "custom_agent_id": "legacy-assistant",
            "mode": "default",
            "model_id": "claude-sonnet-4",
        }))
        .expect_err("legacy custom_agent_id must be rejected");

        assert!(err.to_string().contains("custom_agent_id"));
    }

    // -- parse_execution_mode -------------------------------------------------

    #[test]
    fn parse_mode_none_defaults_to_existing() {
        assert_eq!(parse_execution_mode(None).unwrap(), ExecutionMode::Existing);
    }

    #[test]
    fn parse_mode_existing() {
        assert_eq!(parse_execution_mode(Some("existing")).unwrap(), ExecutionMode::Existing);
    }

    #[test]
    fn parse_mode_new_conversation() {
        assert_eq!(
            parse_execution_mode(Some("new_conversation")).unwrap(),
            ExecutionMode::NewConversation
        );
    }

    #[test]
    fn parse_mode_invalid() {
        let err = parse_execution_mode(Some("parallel")).unwrap_err();
        assert!(matches!(err, CronError::InvalidExecutionMode(_)));
    }

    // -- build_update_params --------------------------------------------------

    fn sample_job() -> CronJob {
        CronJob {
            id: "cron_test".into(),
            name: "Test".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: "conv_1".into(),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(61000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }

    #[test]
    fn build_update_params_name_only() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: Some("New Name".into()),
            description: None,
            enabled: None,
            schedule: None,
            message: None,
            execution_mode: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req);
        assert_eq!(params.name.as_deref(), Some("New Name"));
        assert!(params.enabled.is_none());
        assert!(params.schedule_kind.is_none());
        assert!(params.next_run_at.is_none());
    }

    #[test]
    fn build_update_params_with_schedule_change() {
        let job = CronJob {
            schedule: CronSchedule::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("UTC".into()),
                description: Some("daily".into()),
            },
            next_run_at: Some(99999),
            ..sample_job()
        };
        let req = UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: None,
            schedule: Some(CronScheduleDto::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("UTC".into()),
                description: Some("daily".into()),
            }),
            message: None,
            execution_mode: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req);
        assert_eq!(params.schedule_kind.as_deref(), Some("cron"));
        assert_eq!(params.schedule_value.as_deref(), Some("0 0 9 * * *"));
        assert!(params.next_run_at.is_some());
    }

    #[test]
    fn build_update_params_strips_legacy_ids_when_assistant_id_present() {
        let mut job = sample_job();
        job.agent_config = Some(CronAgentConfig {
            name: "Helper".into(),
            cli_path: None,
            is_preset: None,
            assistant_id: Some("assistant-1".into()),
            custom_agent_id: None,
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        });
        let req = UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: None,
            schedule: None,
            message: None,
            execution_mode: None,
            agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
                name: "Helper".into(),
                cli_path: None,
                assistant_id: Some("assistant-1".into()),
                mode: Some("default".into()),
                model_id: Some("claude-sonnet-4".into()),
                model: None,
                config_options: None,
                workspace: None,
            }),
            conversation_title: None,
            max_retries: None,
        };

        let params = build_update_params(&job, &req);
        let config_json = params.agent_config.flatten().expect("agent config json");
        let config: CronAgentConfig = serde_json::from_str(&config_json).expect("parse cron config");

        assert_eq!(config.assistant_id.as_deref(), Some("assistant-1"));
        assert!(config.custom_agent_id.is_none());
        assert!(config.is_preset.is_none());
    }

    #[test]
    fn build_update_params_rejects_legacy_custom_agent_id_without_assistant_id() {
        let err = serde_json::from_value::<aionui_api_types::CronAgentConfigWriteDto>(serde_json::json!({
            "name": "Helper",
            "custom_agent_id": "legacy-assistant",
            "mode": "default",
            "model_id": "claude-sonnet-4",
        }))
        .expect_err("legacy custom_agent_id must be rejected");

        assert!(err.to_string().contains("custom_agent_id"));
    }

    #[test]
    fn preserves_existing_cron_timezone_when_update_omits_tz() {
        let existing = CronSchedule::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: Some("daily".into()),
        };
        let dto = CronScheduleDto::Cron {
            expr: "0 30 9 * * *".into(),
            tz: None,
            description: Some("daily".into()),
        };

        let schedule = schedule_from_dto_with_existing_timezone(&dto, &existing);

        assert_eq!(
            schedule,
            CronSchedule::Cron {
                expr: "0 30 9 * * *".into(),
                tz: Some("Asia/Shanghai".into()),
                description: Some("daily".into()),
            }
        );
    }

    #[test]
    fn build_update_params_enabled_change_triggers_next_run() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: Some(false),
            schedule: None,
            message: None,
            execution_mode: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req);
        assert_eq!(params.enabled, Some(false));
        assert!(params.next_run_at.is_some());
    }

    #[test]
    fn build_update_params_description_only() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: None,
            description: Some("Updated description".into()),
            enabled: None,
            schedule: None,
            message: None,
            execution_mode: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req);
        assert_eq!(
            params.description.as_ref().and_then(|value| value.as_deref()),
            Some("Updated description")
        );
    }

    // -- schedule_to_row_fields -----------------------------------------------

    #[test]
    fn row_fields_at() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::At {
            at_ms: 5000,
            description: Some("once".into()),
        });
        assert_eq!(kind, "at");
        assert_eq!(value, "5000");
        assert!(tz.is_none());
        assert_eq!(desc.as_deref(), Some("once"));
    }

    #[test]
    fn row_fields_every() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::Every {
            every_ms: 30000,
            description: None,
        });
        assert_eq!(kind, "every");
        assert_eq!(value, "30000");
        assert!(tz.is_none());
        assert!(desc.is_none());
    }

    #[test]
    fn row_fields_cron() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::Cron {
            expr: "0 0 * * * *".into(),
            tz: Some("UTC".into()),
            description: Some("hourly".into()),
        });
        assert_eq!(kind, "cron");
        assert_eq!(value, "0 0 * * * *");
        assert_eq!(tz.as_deref(), Some("UTC"));
        assert_eq!(desc.as_deref(), Some("hourly"));
    }
}
