use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_ai_agent::AgentRegistry;
use aionui_ai_agent::task_manager::IWorkerTaskManager;
use aionui_api_types::{AssistantConversationRequest, CreateConversationRequest};
use aionui_common::{
    AgentType, ProviderWithModel, WorkspacePathValidationError, now_ms, validate_workspace_path_availability,
};
use aionui_conversation::{
    ConversationAgentTurnOutcome, ConversationAgentTurnRequest, ConversationAgentTurnStarted,
    ConversationAgentTurnStartedCallback, ConversationAgentTurnStatus, ConversationError, ConversationService,
};
use aionui_db::models::MessageRow;
use aionui_db::{ConversationRowUpdate, IConversationRepository};
use aionui_realtime::EventBroadcaster;
use tracing::{error, info, warn};

use crate::artifacts::{broadcast_artifact, build_cron_trigger_artifact};
use crate::error::CronError;
use crate::prompt::{
    build_existing_conversation_prompt, build_new_conversation_prompt_with_skill_suggest,
    build_new_conversation_with_skill_prompt,
};
use crate::skill_file::{cron_skill_name, read_skill_content, write_raw_skill_file, write_skill_file};
use crate::skill_suggest::SkillSuggestDetector;
use crate::types::{CronJob, ExecutionMode};

pub const RETRY_INTERVAL_MS: u64 = 30_000;
pub const MAX_RETRIES_DEFAULT: i64 = 3;
const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";
const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionResult {
    Success { conversation_id: String },
    Retrying { attempt: i64 },
    Skipped,
    Error { message: String },
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedExecution {
    pub conversation_id: String,
    saved_skill: Option<SavedSkillContext>,
}

pub(crate) enum PreparedRunNow {
    Ready(PreparedExecution),
    AlreadyRunning { conversation_id: String },
}

struct SkillSuggestContext {
    conversation_id: String,
    job_id: String,
    workspace: String,
}

pub struct JobExecutor {
    conversation_repo: Arc<dyn IConversationRepository>,
    conversation_service: Arc<ConversationService>,
    work_dir: PathBuf,
    data_dir: PathBuf,
    broadcaster: Arc<dyn EventBroadcaster>,
    agent_registry: Arc<AgentRegistry>,
    skill_suggest_detector: SkillSuggestDetector,
}

impl JobExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _task_manager: Arc<dyn IWorkerTaskManager>,
        conversation_repo: Arc<dyn IConversationRepository>,
        conversation_service: Arc<ConversationService>,
        work_dir: PathBuf,
        data_dir: PathBuf,
        broadcaster: Arc<dyn EventBroadcaster>,
        agent_registry: Arc<AgentRegistry>,
    ) -> Self {
        let skill_suggest_detector =
            SkillSuggestDetector::new(Arc::clone(&broadcaster), conversation_repo.clone(), data_dir.clone());
        Self {
            conversation_repo,
            conversation_service,
            work_dir,
            data_dir,
            broadcaster,
            agent_registry,
            skill_suggest_detector,
        }
    }

    pub async fn execute(&self, job: &CronJob) -> ExecutionResult {
        let prepared = match self.prepare_scheduled(job).await {
            Ok(prepared) => prepared,
            Err(result) => return result,
        };

        self.execute_prepared_scheduled(job, prepared).await
    }

    pub(crate) async fn prepare_scheduled(&self, job: &CronJob) -> Result<PreparedExecution, ExecutionResult> {
        let saved_skill = match self.prepare_saved_skill(job).await {
            Ok(skill) => skill,
            Err(e) => {
                error!(job_id = %job.id, error = %e, "Failed to prepare saved cron skill");
                return Err(ExecutionResult::Error { message: e.to_string() });
            }
        };

        if let Err(e) = self.validate_runtime_job_workspace(job).await {
            error!(job_id = %job.id, error = %e, "Failed cron workspace validation");
            return Err(ExecutionResult::Error { message: e.to_string() });
        }

        let conversation_id = match self.resolve_conversation(job, saved_skill.as_ref()).await {
            Ok(id) => id,
            Err(e) => {
                error!(job_id = %job.id, error = %e, "Failed to resolve conversation");
                return Err(ExecutionResult::Error { message: e.to_string() });
            }
        };

        if self.is_conversation_claimed(&conversation_id) {
            info!(
                job_id = %job.id,
                conversation_id = %conversation_id,
                "Cron target conversation already has an active turn; scheduling retry"
            );
            return Err(self.handle_busy(job));
        }

        Ok(PreparedExecution {
            conversation_id,
            saved_skill,
        })
    }

    pub(crate) async fn prepare_run_now(&self, job: &CronJob) -> Result<PreparedRunNow, CronError> {
        let saved_skill = match self.prepare_saved_skill(job).await {
            Ok(skill) => skill,
            Err(err) => {
                error!(
                    job_id = %job.id,
                    error = %err,
                    "Failed to prepare saved cron skill for run-now"
                );
                return Err(err);
            }
        };

        self.validate_runtime_job_workspace(job).await?;
        let conversation_id = self.resolve_conversation(job, saved_skill.as_ref()).await?;

        if self.is_conversation_claimed(&conversation_id) {
            info!(
                job_id = %job.id,
                conversation_id = %conversation_id,
                "Run-now target conversation already has an active turn; returning it for navigation"
            );
            return Ok(PreparedRunNow::AlreadyRunning { conversation_id });
        }

        Ok(PreparedRunNow::Ready(PreparedExecution {
            conversation_id,
            saved_skill,
        }))
    }

    pub(crate) async fn execute_prepared(&self, job: &CronJob, prepared: PreparedExecution) -> ExecutionResult {
        self.execute_inner_with_busy_retry(job, &prepared.conversation_id, prepared.saved_skill.as_ref(), false)
            .await
    }

    pub(crate) async fn execute_prepared_scheduled(
        &self,
        job: &CronJob,
        prepared: PreparedExecution,
    ) -> ExecutionResult {
        self.execute_inner_with_busy_retry(job, &prepared.conversation_id, prepared.saved_skill.as_ref(), true)
            .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    async fn execute_inner(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> ExecutionResult {
        self.execute_inner_with_busy_retry(job, conversation_id, saved_skill, true)
            .await
    }

    pub async fn conversation_exists(&self, conversation_id: &str) -> Result<bool, CronError> {
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await
            .map_err(CronError::Database)?;
        Ok(row.is_some())
    }

    pub fn is_conversation_claimed(&self, conversation_id: &str) -> bool {
        self.conversation_service.runtime_state().is_claimed(conversation_id)
    }

    pub async fn get_conversation_row(
        &self,
        conversation_id: &str,
    ) -> Result<Option<aionui_db::models::ConversationRow>, CronError> {
        self.conversation_repo
            .get(conversation_id)
            .await
            .map_err(CronError::Database)
    }

    pub async fn auto_workspace_to_delete_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<PathBuf>, CronError> {
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Ok(None);
        };
        Ok(self
            .conversation_service
            .auto_workspace_to_delete_for_row(&row, conversation_id))
    }

    pub async fn get_assistant_snapshot(
        &self,
        conversation_id: &str,
    ) -> Result<Option<aionui_db::models::ConversationAssistantSnapshotRow>, CronError> {
        self.conversation_repo
            .get_assistant_snapshot(conversation_id)
            .await
            .map_err(CronError::Database)
    }

    pub(crate) async fn resolve_job_workspace_raw(&self, job: &CronJob) -> Result<String, CronError> {
        self.resolve_execution_workspace_raw(job, &job.conversation_id).await
    }

    pub(crate) async fn validate_runtime_job_workspace(&self, job: &CronJob) -> Result<(), CronError> {
        let workspace = self.resolve_job_workspace_raw(job).await?;
        match validate_workspace_path_availability(&workspace) {
            Ok(_) => Ok(()),
            Err(WorkspacePathValidationError::Empty) => Ok(()),
            Err(WorkspacePathValidationError::DoesNotExist(path))
            | Err(WorkspacePathValidationError::NotDirectory(path))
            | Err(WorkspacePathValidationError::NotAccessible { path, .. }) => {
                Err(CronError::WorkspacePathRuntimeUnavailable(path))
            }
        }
    }

    pub async fn insert_tips_message(
        &self,
        conversation_id: &str,
        content: &str,
        tip_type: &str,
    ) -> Result<(), CronError> {
        // `id` must stay in the short-id family so the frontend message list
        // sees a uniform shape across all sources (see ConversationService's
        // mint_msg_id contract). A follow-up PR will move this insert behind
        // ConversationService entirely; for now we reuse the mint function to
        // keep the id format consistent without reshuffling ownership.
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: conversation_id.to_owned(),
            msg_id: None,
            r#type: "tips".into(),
            content: serde_json::json!({
                "content": content,
                "type": tip_type,
            })
            .to_string(),
            position: Some("center".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: aionui_common::now_ms(),
        };

        self.conversation_repo
            .insert_message(&row)
            .await
            .map_err(CronError::Database)
    }

    pub async fn bind_cron_job_to_conversation(
        &self,
        conversation_id: &str,
        cron_job_id: &str,
    ) -> Result<(), CronError> {
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Ok(());
        };

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let Some(obj) = extra.as_object_mut() else {
            extra = serde_json::json!({});
            extra.as_object_mut().expect("json object").insert(
                "cron_job_id".to_owned(),
                serde_json::Value::String(cron_job_id.to_owned()),
            );
            extra.as_object_mut().expect("json object").insert(
                "cronJobId".to_owned(),
                serde_json::Value::String(cron_job_id.to_owned()),
            );
            let update = ConversationRowUpdate {
                extra: Some(extra.to_string()),
                updated_at: Some(now_ms()),
                ..Default::default()
            };
            return self
                .conversation_repo
                .update(conversation_id, &update)
                .await
                .map_err(CronError::Database);
        };

        let current = obj
            .get("cron_job_id")
            .and_then(|value| value.as_str())
            .or_else(|| obj.get("cronJobId").and_then(|value| value.as_str()));

        if current == Some(cron_job_id) {
            return Ok(());
        }

        obj.insert(
            "cron_job_id".to_owned(),
            serde_json::Value::String(cron_job_id.to_owned()),
        );
        obj.insert(
            "cronJobId".to_owned(),
            serde_json::Value::String(cron_job_id.to_owned()),
        );

        let update = ConversationRowUpdate {
            extra: Some(extra.to_string()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo
            .update(conversation_id, &update)
            .await
            .map_err(CronError::Database)
    }

    pub async fn unbind_cron_job_from_conversation(
        &self,
        conversation_id: &str,
        cron_job_id: &str,
    ) -> Result<(), CronError> {
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Ok(());
        };

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let Some(obj) = extra.as_object_mut() else {
            return Ok(());
        };

        let snake_matches = obj.get("cron_job_id").and_then(|value| value.as_str()) == Some(cron_job_id);
        let camel_matches = obj.get("cronJobId").and_then(|value| value.as_str()) == Some(cron_job_id);
        if !snake_matches && !camel_matches {
            return Ok(());
        }

        if snake_matches {
            obj.remove("cron_job_id");
        }
        if camel_matches {
            obj.remove("cronJobId");
        }

        let update = ConversationRowUpdate {
            extra: Some(extra.to_string()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo
            .update(conversation_id, &update)
            .await
            .map_err(CronError::Database)
    }

    pub async fn persist_workspace_if_missing(
        &self,
        conversation_id: &str,
        resolved_workspace: &str,
    ) -> Result<(), CronError> {
        let resolved_workspace = resolved_workspace.trim();
        if resolved_workspace.is_empty() {
            return Ok(());
        }

        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Ok(());
        };

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        let Some(obj) = extra.as_object_mut() else {
            extra = serde_json::json!({});
            extra.as_object_mut().expect("json object").insert(
                "workspace".to_owned(),
                serde_json::Value::String(resolved_workspace.to_owned()),
            );
            let update = ConversationRowUpdate {
                extra: Some(extra.to_string()),
                updated_at: Some(now_ms()),
                ..Default::default()
            };
            return self
                .conversation_repo
                .update(conversation_id, &update)
                .await
                .map_err(CronError::Database);
        };

        let current_workspace = obj
            .get("workspace")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default();

        if !current_workspace.is_empty() {
            return Ok(());
        }

        obj.insert(
            "workspace".to_owned(),
            serde_json::Value::String(resolved_workspace.to_owned()),
        );

        let update = ConversationRowUpdate {
            extra: Some(extra.to_string()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo
            .update(conversation_id, &update)
            .await
            .map_err(CronError::Database)
    }
}

impl JobExecutor {
    fn handle_busy(&self, job: &CronJob) -> ExecutionResult {
        let max_retries = job.max_retries;
        let current_retry = job.retry_count;

        if current_retry >= max_retries {
            warn!(
                job_id = %job.id,
                retries = current_retry,
                "Max retries exceeded, skipping"
            );
            return ExecutionResult::Skipped;
        }

        let attempt = current_retry + 1;
        info!(
            job_id = %job.id,
            attempt,
            max_retries,
            "Conversation busy, scheduling retry"
        );
        ExecutionResult::Retrying { attempt }
    }

    async fn resolve_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
    ) -> Result<String, CronError> {
        match job.execution_mode {
            ExecutionMode::Existing => {
                // A job created without an anchor conversation (the frontend
                // creates "continue-this-conversation" jobs from the cron page
                // before any conversation exists), or whose anchor was later
                // deleted, materializes a replacement conversation on demand.
                // The service layer then persists the new id back onto the job
                // so subsequent runs reuse it.
                let conversation_id = job.conversation_id.trim();
                if conversation_id.is_empty() {
                    return self
                        .create_new_conversation(job, saved_skill, ConversationPurpose::ExistingReplacement)
                        .await;
                }
                if !self.conversation_exists(conversation_id).await? {
                    warn!(
                        job_id = %job.id,
                        conversation_id,
                        "Cron existing-mode conversation is missing; creating a replacement conversation"
                    );
                    return self
                        .create_new_conversation(job, saved_skill, ConversationPurpose::ExistingReplacement)
                        .await;
                }
                Ok(job.conversation_id.clone())
            }
            ExecutionMode::NewConversation => {
                self.create_new_conversation(job, saved_skill, ConversationPurpose::NewConversationExecution)
                    .await
            }
        }
    }

    async fn create_new_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
        purpose: ConversationPurpose,
    ) -> Result<String, CronError> {
        let agent_type = parse_agent_type(&self.agent_registry, &job.agent_type).await?;
        let model = resolve_model(job);
        let user_id = self.resolve_conversation_owner_user_id(job).await?;

        let extra = build_conversation_extra(&self.agent_registry, job, saved_skill, purpose).await;
        let assistant = build_assistant_request(job);

        let req = CreateConversationRequest {
            r#type: if assistant.is_some() { None } else { Some(agent_type) },
            name: Some(job.name.clone()),
            model,
            assistant,
            source: None,
            channel_chat_id: None,
            extra,
        };

        let response = self
            .conversation_service
            .create(&user_id, req)
            .await
            .map_err(CronError::from_conversation_create)?;

        let response_workspace = response
            .extra
            .get("workspace")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default();

        if response_workspace.is_empty() {
            let fallback_workspace = default_temp_workspace_path(&self.work_dir, &agent_type, job, &response.id);
            std::fs::create_dir_all(&fallback_workspace).map_err(|err| {
                CronError::Scheduler(format!(
                    "create fallback cron workspace {}: {err}",
                    fallback_workspace.display()
                ))
            })?;
            self.persist_workspace_if_missing(&response.id, &fallback_workspace.to_string_lossy())
                .await?;
        }

        info!(
            job_id = %job.id,
            conversation_id = %response.id,
            "Created new conversation for cron job"
        );

        Ok(response.id)
    }

    async fn resolve_conversation_owner_user_id(&self, job: &CronJob) -> Result<String, CronError> {
        if !job.conversation_id.trim().is_empty()
            && let Some(row) = self.get_conversation_row(&job.conversation_id).await?
            && !row.user_id.trim().is_empty()
        {
            return Ok(row.user_id);
        }

        Ok(SYSTEM_DEFAULT_USER_ID.to_owned())
    }

    async fn execute_inner_with_busy_retry(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
        retry_on_busy: bool,
    ) -> ExecutionResult {
        match self.get_conversation_row(conversation_id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return ExecutionResult::Error {
                    message: format!("conversation {conversation_id} not found"),
                };
            }
            Err(e) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to load conversation row for cron model resolution"
                );
                return ExecutionResult::Error { message: e.to_string() };
            }
        }

        let skill_names = match self.resolve_task_skill_names(job, conversation_id, saved_skill).await {
            Ok(names) => names,
            Err(e) => {
                error!(job_id = %job.id, error = %e, "Failed to resolve task skills");
                return ExecutionResult::Error { message: e.to_string() };
            }
        };

        let prompt = build_prompt(job, saved_skill);
        let user_id = match self.resolve_target_conversation_user_id(conversation_id).await {
            Ok(user_id) => user_id,
            Err(e) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to resolve cron conversation owner before dispatch"
                );
                return ExecutionResult::Error { message: e.to_string() };
            }
        };

        let on_started = {
            let job = job.clone();
            let conversation_repo = Arc::clone(&self.conversation_repo);
            let broadcaster = Arc::clone(&self.broadcaster);
            Some(Arc::new(move |started: ConversationAgentTurnStarted| {
                let job = job.clone();
                let conversation_repo = Arc::clone(&conversation_repo);
                let broadcaster = Arc::clone(&broadcaster);
                let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = Box::pin(async move {
                    let created_at = now_ms();
                    let row = build_cron_trigger_artifact(&started.conversation_id, &job, created_at);
                    match conversation_repo.upsert_artifact(&row).await {
                        Ok(row) => {
                            if let Err(e) = broadcast_artifact(&broadcaster, &row) {
                                warn!(
                                    job_id = %job.id,
                                    conversation_id = %started.conversation_id,
                                    error = %e,
                                    "Failed to broadcast cron trigger artifact"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                job_id = %job.id,
                                conversation_id = %started.conversation_id,
                                error = %e,
                                "Failed to persist cron trigger artifact"
                            );
                        }
                    }
                });
                fut
            }) as ConversationAgentTurnStartedCallback)
        };

        let turn_req = ConversationAgentTurnRequest {
            user_id,
            conversation_id: conversation_id.to_owned(),
            content: prompt,
            files: vec![],
            inject_skills: skill_names.clone(),
            persist_user_message: true,
            user_message_hidden: true,
            on_started,
        };

        match self.conversation_service.run_agent_turn(turn_req).await {
            Ok(outcome) => {
                if outcome.status == ConversationAgentTurnStatus::Failed {
                    return ExecutionResult::Error {
                        message: self.conversation_turn_failed_message(conversation_id, outcome).await,
                    };
                }
                if saved_skill.is_none() && matches!(job.execution_mode, ExecutionMode::NewConversation) {
                    let workspace = self
                        .resolve_execution_workspace(job, conversation_id)
                        .await
                        .unwrap_or_else(|err| {
                            warn!(
                                job_id = %job.id,
                                conversation_id,
                                error = %err,
                                "Failed to resolve cron workspace for skill suggestion check"
                            );
                            String::new()
                        });
                    self.schedule_skill_suggest_check(SkillSuggestContext {
                        conversation_id: conversation_id.to_owned(),
                        job_id: job.id.clone(),
                        workspace,
                    });
                }
                info!(
                    job_id = %job.id,
                    conversation_id,
                    "Cron job message sent successfully"
                );
                ExecutionResult::Success {
                    conversation_id: conversation_id.to_owned(),
                }
            }
            Err(ConversationError::Busy { reason }) if retry_on_busy => {
                warn!(
                    job_id = %job.id,
                    conversation_id,
                    reason,
                    "Cron target conversation became busy during send; scheduling retry"
                );
                self.handle_busy(job)
            }
            Err(e) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to send cron job message"
                );
                ExecutionResult::Error { message: e.to_string() }
            }
        }
    }

    async fn conversation_turn_failed_message(
        &self,
        conversation_id: &str,
        outcome: ConversationAgentTurnOutcome,
    ) -> String {
        if let Some(message) = outcome
            .error_message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            return message.to_owned();
        }

        match self
            .conversation_service
            .latest_conversation_error_message(conversation_id)
            .await
        {
            Ok(Some(message)) => message,
            Ok(None) => format!(
                "Conversation turn {} failed without agent error details",
                outcome.turn_id
            ),
            Err(err) => {
                warn!(
                    conversation_id,
                    turn_id = %outcome.turn_id,
                    error = %err,
                    "Failed to load latest conversation error message"
                );
                format!(
                    "Conversation turn {} failed without agent error details",
                    outcome.turn_id
                )
            }
        }
    }

    async fn resolve_target_conversation_user_id(&self, conversation_id: &str) -> Result<String, CronError> {
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} not found"
            )));
        };

        if row.user_id.trim().is_empty() {
            return Ok(SYSTEM_DEFAULT_USER_ID.to_owned());
        }

        Ok(row.user_id)
    }

    pub async fn mark_skill_suggest_artifacts_saved(&self, job_id: &str) -> Result<(), CronError> {
        let rows = self
            .conversation_repo
            .mark_skill_suggest_artifacts_saved(job_id, now_ms())
            .await
            .map_err(CronError::Database)?;

        for row in rows {
            broadcast_artifact(&self.broadcaster, &row)?;
        }

        Ok(())
    }

    async fn resolve_execution_workspace_raw(&self, job: &CronJob, conversation_id: &str) -> Result<String, CronError> {
        if let Some(workspace) = job.agent_config.as_ref().and_then(|config| config.workspace.as_deref()) {
            return Ok(workspace.to_owned());
        }

        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Ok(String::new());
        };

        let extra = serde_json::from_str::<serde_json::Value>(&row.extra).unwrap_or_default();
        Ok(extra
            .get("workspace")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned())
    }

    async fn resolve_execution_workspace(&self, job: &CronJob, conversation_id: &str) -> Result<String, CronError> {
        Ok(self
            .resolve_execution_workspace_raw(job, conversation_id)
            .await?
            .trim()
            .to_owned())
    }

    fn schedule_skill_suggest_check(&self, ctx: SkillSuggestContext) {
        self.skill_suggest_detector
            .schedule_check(ctx.conversation_id, ctx.job_id, ctx.workspace);
    }

    async fn prepare_saved_skill(&self, job: &CronJob) -> Result<Option<SavedSkillContext>, CronError> {
        if let Some(raw_content) = read_skill_content(&self.data_dir, &job.id).await?
            && !raw_content.trim().is_empty()
        {
            return Ok(Some(SavedSkillContext {
                name: cron_skill_name(&job.id)?,
                raw_content,
            }));
        }

        let legacy_content = job
            .skill_content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty());

        let Some(legacy_content) = legacy_content else {
            return Ok(None);
        };

        persist_legacy_skill_file(&self.data_dir, job, legacy_content).await?;
        let raw_content = read_skill_content(&self.data_dir, &job.id)
            .await?
            .unwrap_or_else(|| legacy_content.to_owned());

        Ok(Some(SavedSkillContext {
            name: cron_skill_name(&job.id)?,
            raw_content,
        }))
    }

    async fn resolve_task_skill_names(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> Result<Vec<String>, CronError> {
        let mut skills = match job.execution_mode {
            ExecutionMode::Existing => self.load_conversation_skill_names(conversation_id).await?,
            ExecutionMode::NewConversation => Vec::new(),
        };

        if matches!(job.execution_mode, ExecutionMode::NewConversation)
            && let Some(saved_skill) = saved_skill
            && !skills.iter().any(|name| name == &saved_skill.name)
        {
            skills.push(saved_skill.name.clone());
        }

        Ok(skills)
    }

    async fn load_conversation_skill_names(&self, conversation_id: &str) -> Result<Vec<String>, CronError> {
        let Some(row) = self
            .conversation_repo
            .get(conversation_id)
            .await
            .map_err(CronError::Database)?
        else {
            return Ok(Vec::new());
        };

        let Ok(extra) = serde_json::from_str::<serde_json::Value>(&row.extra) else {
            return Ok(Vec::new());
        };

        Ok(extra
            .get("skills")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Resolve a cron job's stored `agent_type` string into an [`AgentType`].
///
/// Cron persists this field as a free-form string because legacy rows
/// carry a vendor label (e.g. `"claude"`, `"gemini"`) instead of the
/// canonical `"acp"`. Resolution order:
/// 1. Exact [`AgentType`] serde match. Deprecated runtime variants are
///    rejected before any conversation is created.
/// 2. ACP vendor lookup via the registry — any builtin ACP row's
///    `backend` aliases to [`AgentType::Acp`].
/// 3. Fallback to [`AgentType::Acp`] to preserve the prior default.
async fn parse_agent_type(registry: &AgentRegistry, agent_type_str: &str) -> Result<AgentType, CronError> {
    if let Ok(agent_type) = serde_json::from_value::<AgentType>(serde_json::Value::String(agent_type_str.to_owned())) {
        if agent_type.is_deprecated_runtime() {
            return Err(CronError::InvalidAgentConfig(DEPRECATED_AGENT_TYPE_MESSAGE.into()));
        }
        return Ok(agent_type);
    }

    if registry.find_builtin_by_backend(agent_type_str).await.is_some() {
        return Ok(AgentType::Acp);
    }

    Ok(AgentType::Acp)
}

/// Only aionrs conversations carry meaningful model info in `conversations.model`;
/// ACP and other agent types ignore this field and resolve the model via their own
/// mechanisms (catalog defaults, CLI flags, etc.). Returning `None` lets the
/// `CreateConversationRequest.model` stay `None` for those types, which is the
/// correct semantic.
///
fn resolve_model(job: &CronJob) -> Option<ProviderWithModel> {
    if job.agent_type != "aionrs" {
        return None;
    }
    job.agent_config.as_ref()?.model.clone()
}

/// Fill `extra` with the agent identity the factory should use.
///
/// Preferred path: resolve a builtin ACP catalog row via the
/// registry and emit `agent_id` (exact factory lookup) alongside
/// `backend` (convenience for other consumers).
async fn inject_agent_identity(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    registry: &AgentRegistry,
    job: &CronJob,
) {
    let lookup_label = job.agent_type.trim();
    if lookup_label.is_empty() {
        return;
    }

    if let Some(meta) = registry.find_builtin_by_backend(lookup_label).await {
        extra.insert("agent_id".to_owned(), serde_json::Value::String(meta.id.clone()));
        if let Some(backend) = meta.backend {
            extra.insert("backend".to_owned(), serde_json::Value::String(backend));
        }
    }
}

#[cfg(test)]
async fn build_task_extra(registry: &AgentRegistry, job: &CronJob, skills: &[String]) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert("cron_job_id".to_owned(), serde_json::Value::String(job.id.clone()));
    extra.insert("cronJobId".to_owned(), serde_json::Value::String(job.id.clone()));
    if !skills.is_empty() {
        extra.insert(
            "skills".to_owned(),
            serde_json::Value::Array(skills.iter().cloned().map(serde_json::Value::String).collect()),
        );
    }

    inject_agent_identity(&mut extra, registry, job).await;

    if let Some(config) = &job.agent_config {
        if let Some(cli_path) = &config.cli_path {
            extra.insert("cli_path".to_owned(), serde_json::Value::String(cli_path.clone()));
        }
        if !config.name.is_empty() {
            extra.insert("agent_name".to_owned(), serde_json::Value::String(config.name.clone()));
        }
        let has_assistant_id = config
            .assistant_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        if let Some(assistant_id) = &config.assistant_id {
            extra.insert(
                "assistant_id".to_owned(),
                serde_json::Value::String(assistant_id.clone()),
            );
        }
        if !has_assistant_id && let Some(custom_agent_id) = &config.custom_agent_id {
            extra.insert(
                "custom_agent_id".to_owned(),
                serde_json::Value::String(custom_agent_id.clone()),
            );
            if config.is_preset.unwrap_or(false) {
                extra.insert(
                    "preset_assistant_id".to_owned(),
                    serde_json::Value::String(custom_agent_id.clone()),
                );
            }
        }
        if let Some(mode) = &config.mode {
            extra.insert("session_mode".to_owned(), serde_json::Value::String(mode.clone()));
        }
    }

    serde_json::Value::Object(extra)
}

fn build_prompt(job: &CronJob, saved_skill: Option<&SavedSkillContext>) -> String {
    let schedule_desc = schedule_description_text(&job.schedule);

    match job.execution_mode {
        ExecutionMode::Existing => build_existing_conversation_prompt(&job.name, &schedule_desc, &job.message),
        ExecutionMode::NewConversation => {
            if saved_skill.is_some() {
                build_new_conversation_with_skill_prompt(&job.name, &job.message)
            } else {
                build_new_conversation_prompt_with_skill_suggest(&job.name, &schedule_desc, &job.message)
            }
        }
    }
}

fn build_assistant_request(job: &CronJob) -> Option<AssistantConversationRequest> {
    let config = job.agent_config.as_ref()?;
    let assistant_id = config
        .assistant_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            config
                .custom_agent_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
        })?;

    Some(AssistantConversationRequest {
        id: assistant_id,
        locale: None,
        conversation_overrides: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedSkillContext {
    name: String,
    raw_content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationPurpose {
    NewConversationExecution,
    ExistingReplacement,
}

async fn build_conversation_extra(
    registry: &AgentRegistry,
    job: &CronJob,
    saved_skill: Option<&SavedSkillContext>,
    purpose: ConversationPurpose,
) -> serde_json::Value {
    let assistant_backed = build_assistant_request(job).is_some();
    let mut extra = serde_json::Map::new();
    extra.insert("cron_job_id".to_owned(), serde_json::Value::String(job.id.clone()));
    extra.insert("cronJobId".to_owned(), serde_json::Value::String(job.id.clone()));
    if matches!(purpose, ConversationPurpose::NewConversationExecution) {
        extra.insert(
            "exclude_auto_inject_skills".to_owned(),
            serde_json::Value::Array(vec![serde_json::Value::String("cron".to_owned())]),
        );
    }

    if let Some(saved_skill) = saved_skill {
        extra.insert(
            "preset_enabled_skills".to_owned(),
            serde_json::Value::Array(vec![serde_json::Value::String(saved_skill.name.clone())]),
        );
    }

    if !assistant_backed {
        inject_agent_identity(&mut extra, registry, job).await;
    }

    if let Some(config) = &job.agent_config {
        if let Some(cli_path) = &config.cli_path {
            extra.insert("cli_path".to_owned(), serde_json::Value::String(cli_path.clone()));
        }
        if !assistant_backed && !config.name.is_empty() {
            extra.insert("agent_name".to_owned(), serde_json::Value::String(config.name.clone()));
        }
        if let Some(mode) = &config.mode {
            extra.insert("session_mode".to_owned(), serde_json::Value::String(mode.clone()));
        }
        if let Some(workspace) = &config.workspace
            && !workspace.trim().is_empty()
        {
            extra.insert("workspace".to_owned(), serde_json::Value::String(workspace.clone()));
        }
    }

    serde_json::Value::Object(extra)
}

fn schedule_description_text(schedule: &crate::types::CronSchedule) -> String {
    match schedule {
        crate::types::CronSchedule::At { at_ms, description } => {
            description.clone().unwrap_or_else(|| format!("At {at_ms}"))
        }
        crate::types::CronSchedule::Every { every_ms, description } => {
            description.clone().unwrap_or_else(|| format!("Every {every_ms} ms"))
        }
        crate::types::CronSchedule::Cron { expr, tz, description } => description.clone().unwrap_or_else(|| match tz {
            Some(tz) => format!("{expr} ({tz})"),
            None => expr.clone(),
        }),
    }
}

fn default_temp_workspace_path(
    data_dir: &std::path::Path,
    agent_type: &AgentType,
    _job: &CronJob,
    conversation_id: &str,
) -> std::path::PathBuf {
    let label = if *agent_type == AgentType::Acp {
        "acp".to_owned()
    } else {
        agent_type.serde_name().to_owned()
    };

    data_dir
        .join("conversations")
        .join(format!("{label}-temp-{conversation_id}"))
}

fn schedule_description_ref(schedule: &crate::types::CronSchedule) -> Option<&str> {
    match schedule {
        crate::types::CronSchedule::At { description, .. }
        | crate::types::CronSchedule::Every { description, .. }
        | crate::types::CronSchedule::Cron { description, .. } => description.as_deref(),
    }
}

async fn persist_legacy_skill_file(data_dir: &Path, job: &CronJob, raw_content: &str) -> Result<(), CronError> {
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
                schedule_description_ref(&job.schedule),
            )
            .await
            .map(|_| ())
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreatedBy, CronAgentConfig, CronSchedule};
    use aionui_ai_agent::AgentStreamEvent;
    use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
    use aionui_ai_agent::protocol::events::{FinishEventData, TextEventData};
    use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
    use aionui_api_types::{AgentModeResponse, ConfigOptionConfirmation, SetConfigOptionResponse, WebSocketMessage};
    use aionui_common::{AgentKillReason, ConversationStatus, PaginatedResult, TimestampMs};
    use aionui_db::{
        ConversationArtifactRow, ConversationFilters, ConversationRowUpdate, MessagePageParams, MessagePageResult,
        MessageRowUpdate, MessageSearchRow,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{RwLock, broadcast};
    use tokio::time::timeout;

    fn ensure_named_workspace_path(name: &str) -> String {
        let workspace = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&workspace).unwrap();
        workspace.to_string_lossy().to_string()
    }

    fn sample_job() -> CronJob {
        CronJob {
            id: "cron_test1".into(),
            name: "Test Job".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: Some(CronAgentConfig {
                name: "Claude".into(),
                cli_path: Some("/usr/bin/claude".into()),
                is_preset: None,
                assistant_id: Some("assistant-sample".into()),
                custom_agent_id: None,
                mode: None,
                model_id: Some("claude-sonnet-4".into()),
                model: None,
                config_options: None,
                workspace: Some(ensure_named_workspace_path("aionui-cron-sample-job-workspace")),
            }),
            conversation_id: "conv_1".into(),
            conversation_title: Some("Test Conv".into()),
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(3000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }

    async fn wait_for_agent_send(agent: &RecordingAgent, expected_calls: usize) {
        timeout(std::time::Duration::from_secs(1), async {
            loop {
                if agent.send_calls() >= expected_calls {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("agent send should complete");
    }

    // -- handle_busy tests ---------------------------------------------------

    #[tokio::test]
    async fn handle_busy_returns_retrying_when_under_limit() {
        let executor = make_executor_for_busy_tests();

        let job = CronJob {
            retry_count: 1,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Retrying { attempt: 2 });
    }

    #[tokio::test]
    async fn handle_busy_returns_skipped_when_at_limit() {
        let executor = make_executor_for_busy_tests();

        let job = CronJob {
            retry_count: 3,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Skipped);
    }

    #[tokio::test]
    async fn handle_busy_returns_skipped_when_over_limit() {
        let executor = make_executor_for_busy_tests();

        let job = CronJob {
            retry_count: 5,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Skipped);
    }

    #[tokio::test]
    async fn handle_busy_first_retry_returns_attempt_1() {
        let executor = make_executor_for_busy_tests();

        let job = CronJob {
            retry_count: 0,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Retrying { attempt: 1 });
    }

    #[tokio::test]
    async fn execute_returns_retrying_when_runtime_state_is_already_claimed() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let job = sample_job();
        let claim = executor
            .conversation_service
            .runtime_state()
            .try_claim_turn(&job.conversation_id, "turn-existing")
            .expect("runtime claim should succeed");

        let result = executor.execute(&job).await;

        assert_eq!(result, ExecutionResult::Retrying { attempt: 1 });
        assert_eq!(agent.send_calls(), 0, "busy precheck should avoid send attempts");

        drop(claim);
    }

    #[tokio::test]
    async fn prepare_run_now_returns_active_conversation_when_runtime_state_is_already_claimed() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent));
        let job = sample_job();
        let claim = executor
            .conversation_service
            .runtime_state()
            .try_claim_turn(&job.conversation_id, "turn-existing")
            .expect("runtime claim should succeed");

        let prepared = executor
            .prepare_run_now(&job)
            .await
            .expect("run-now should return the claimed conversation");

        assert!(matches!(
            prepared,
            PreparedRunNow::AlreadyRunning { conversation_id }
                if conversation_id == job.conversation_id
        ));

        drop(claim);
    }

    // -- build_prompt tests --------------------------------------------------

    #[test]
    fn build_prompt_existing_mode_no_skill() {
        let job = sample_job();
        let prompt = build_prompt(&job, None);
        assert!(prompt.contains("[Scheduled Task Execution]"));
        assert!(prompt.contains("Task instruction:\ndo something"));
    }

    #[test]
    fn build_prompt_existing_mode_with_skill_does_not_append_saved_skill() {
        let job = sample_job();
        let prompt = build_prompt(
            &job,
            Some(&SavedSkillContext {
                name: "cron-cron_test1".into(),
                raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
            }),
        );
        assert!(prompt.contains("[Scheduled Task Execution]"));
        assert!(!prompt.contains("## Skill Instructions"));
        assert!(!prompt.contains("Do X"));
    }

    #[test]
    fn build_prompt_new_conv_with_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(
            &job,
            Some(&SavedSkillContext {
                name: "cron-cron_test1".into(),
                raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
            }),
        );
        assert!(prompt.contains("A skill file with detailed instructions has been loaded"));
        assert!(prompt.contains("do something"));
    }

    #[test]
    fn build_prompt_new_conv_no_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(&job, None);
        assert!(prompt.contains("create a file named \"SKILL_SUGGEST.md\""));
    }

    #[test]
    fn build_prompt_new_conv_empty_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(&job, None);
        assert!(prompt.contains("SKILL_SUGGEST.md"));
    }

    // -- registry helper ------------------------------------------------------

    /// Build a registry backed by an in-memory DB seeded from the
    /// production migrations, so backend-lookup tests exercise the
    /// same catalog rows the server would see at runtime.
    async fn hydrated_registry() -> Arc<AgentRegistry> {
        let db = aionui_db::init_database_memory().await.unwrap();
        let repo = Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        registry
    }

    // -- parse_agent_type tests -----------------------------------------------

    #[tokio::test]
    async fn parse_agent_type_known_types() {
        let registry = hydrated_registry().await;
        assert_eq!(parse_agent_type(&registry, "acp").await.unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type(&registry, "aionrs").await.unwrap(), AgentType::Aionrs);
    }

    #[tokio::test]
    async fn parse_agent_type_rejects_deprecated_runtime_types() {
        let registry = hydrated_registry().await;
        for agent_type in ["openclaw-gateway", "nanobot", "remote", "gemini", "codex"] {
            let err = parse_agent_type(&registry, agent_type).await.unwrap_err();
            assert!(matches!(err, CronError::InvalidAgentConfig(_)));
            assert!(
                err.to_string()
                    .contains("This agent type is no longer supported for new conversations."),
                "unexpected error for {agent_type}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn parse_agent_type_acp_backend_aliases_to_acp() {
        let registry = hydrated_registry().await;
        assert_eq!(parse_agent_type(&registry, "claude").await.unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type(&registry, "qwen").await.unwrap(), AgentType::Acp);
    }

    #[tokio::test]
    async fn parse_agent_type_unknown_defaults_to_acp() {
        let registry = hydrated_registry().await;
        assert_eq!(
            parse_agent_type(&registry, "unknown_type").await.unwrap(),
            AgentType::Acp
        );
    }

    // -- resolve_model tests -------------------------------------------------

    #[test]
    fn resolve_model_returns_none_for_acp() {
        // Model info only applies to aionrs; ACP ignores it.
        let job = sample_job();
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_acp_without_config() {
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_non_aionrs_type() {
        let job = CronJob {
            agent_type: "claude".into(),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_aionrs_with_full_config() {
        let job = CronJob {
            agent_type: "aionrs".into(),
            agent_config: Some(CronAgentConfig {
                name: "OpenAI".into(),
                cli_path: None,
                is_preset: None,
                assistant_id: None,
                custom_agent_id: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                model: Some(ProviderWithModel {
                    provider_id: "4056cdea".into(),
                    model: "gpt-5".into(),
                    use_model: None,
                }),
                config_options: None,
                workspace: None,
            }),
            ..sample_job()
        };
        let model = resolve_model(&job).expect("aionrs + full config returns Some");
        assert_eq!(model.provider_id, "4056cdea");
        assert_eq!(model.model, "gpt-5");
    }

    #[test]
    fn resolve_model_aionrs_uses_model_payload() {
        let job = CronJob {
            agent_type: "aionrs".into(),
            agent_config: Some(CronAgentConfig {
                name: "OpenAI".into(),
                cli_path: None,
                is_preset: None,
                assistant_id: None,
                custom_agent_id: None,
                mode: None,
                model_id: None,
                model: Some(ProviderWithModel {
                    provider_id: "4056cdea".into(),
                    model: "gpt-5".into(),
                    use_model: Some("gpt-5".into()),
                }),
                config_options: None,
                workspace: None,
            }),
            ..sample_job()
        };
        let model = resolve_model(&job).expect("aionrs model payload returns Some");
        assert_eq!(model.provider_id, "4056cdea");
        assert_eq!(model.model, "gpt-5");
    }

    #[test]
    fn resolve_model_aionrs_without_config_returns_none() {
        // Defensive: `add_job` rejects this shape, but resolve_model must not
        // fabricate a provider_id from the agent_type like the old code did.
        let job = CronJob {
            agent_type: "aionrs".into(),
            agent_config: None,
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_aionrs_without_model_returns_none() {
        let job = CronJob {
            agent_type: "aionrs".into(),
            agent_config: Some(CronAgentConfig {
                name: "Bogus".into(),
                cli_path: None,
                is_preset: None,
                assistant_id: None,
                custom_agent_id: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                model: None,
                config_options: None,
                workspace: None,
            }),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    // -- build_task_extra tests -----------------------------------------------

    #[tokio::test]
    async fn build_task_extra_includes_cron_job_id() {
        let registry = hydrated_registry().await;
        let job = sample_job();
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["cron_job_id"], "cron_test1");
    }

    #[tokio::test]
    async fn build_task_extra_with_config_fields() {
        let registry = hydrated_registry().await;
        let job = sample_job();
        let extra = build_task_extra(&registry, &job, &["cron-cron_test1".into()]).await;
        assert_eq!(extra["cli_path"], "/usr/bin/claude");
        assert_eq!(extra["agent_name"], "Claude");
        assert_eq!(extra["assistant_id"], "assistant-sample");
        assert_eq!(extra["skills"], serde_json::json!(["cron-cron_test1"]));
    }

    #[tokio::test]
    async fn build_task_extra_omits_legacy_assistant_identity_when_assistant_id_is_present() {
        let registry = hydrated_registry().await;
        let mut job = sample_job();
        let config = job.agent_config.as_mut().expect("sample job should carry config");
        config.assistant_id = Some("assistant-sample".into());
        config.custom_agent_id = Some("legacy-custom".into());
        config.is_preset = Some(true);

        let extra = build_task_extra(&registry, &job, &[]).await;

        assert_eq!(extra["assistant_id"], "assistant-sample");
        assert!(extra.get("custom_agent_id").is_none());
        assert!(extra.get("preset_assistant_id").is_none());
    }

    #[tokio::test]
    async fn build_task_extra_without_config() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["cron_job_id"], "cron_test1");
        assert!(extra.get("backend").is_none());
    }

    #[tokio::test]
    async fn build_task_extra_falls_back_to_agent_type_for_acp_backend() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["backend"], "claude");
        // Vendor label must resolve to a catalog row so the factory can
        // skip the `find_builtin_by_backend` fallback.
        assert!(extra.get("agent_id").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn build_conversation_extra_without_saved_skill_excludes_cron_auto_inject_only() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let extra =
            build_conversation_extra(&registry, &job, None, ConversationPurpose::NewConversationExecution).await;

        assert_eq!(extra["cron_job_id"], "cron_test1");
        assert_eq!(extra["exclude_auto_inject_skills"], serde_json::json!(["cron"]));
        assert!(extra.get("preset_enabled_skills").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_for_existing_replacement_keeps_cron_auto_inject() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::Existing,
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None, ConversationPurpose::ExistingReplacement).await;

        assert_eq!(extra["cron_job_id"], "cron_test1");
        assert!(extra.get("exclude_auto_inject_skills").is_none());
        assert!(extra.get("preset_enabled_skills").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_with_saved_skill_enables_preset_skill() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let saved_skill = SavedSkillContext {
            name: "cron-cron_test1".into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let extra = build_conversation_extra(
            &registry,
            &job,
            Some(&saved_skill),
            ConversationPurpose::NewConversationExecution,
        )
        .await;

        assert_eq!(extra["exclude_auto_inject_skills"], serde_json::json!(["cron"]));
        assert_eq!(extra["preset_enabled_skills"], serde_json::json!(["cron-cron_test1"]));
    }

    #[tokio::test]
    async fn build_conversation_extra_omits_legacy_agent_identity_fields_for_assistant_backed_new_conversations() {
        let registry = hydrated_registry().await;
        let mut job = sample_job();
        job.execution_mode = ExecutionMode::NewConversation;
        let config = job.agent_config.as_mut().expect("sample job should carry config");
        config.assistant_id = Some("assistant-preset".into());
        config.is_preset = Some(true);
        config.custom_agent_id = None;

        let extra =
            build_conversation_extra(&registry, &job, None, ConversationPurpose::NewConversationExecution).await;

        assert!(extra.get("assistant_id").is_none());
        assert!(extra.get("preset_assistant_id").is_none());
        assert!(extra.get("custom_agent_id").is_none());
        assert!(extra.get("backend").is_none());
        assert!(extra.get("agent_id").is_none());
        assert!(extra.get("agent_name").is_none());
    }

    #[test]
    fn build_assistant_request_uses_legacy_custom_agent_id_when_assistant_id_missing() {
        let mut job = sample_job();
        let config = job.agent_config.as_mut().expect("sample job should carry config");
        config.assistant_id = None;
        config.custom_agent_id = Some("custom-assistant".into());

        let assistant = build_assistant_request(&job).expect("legacy custom assistant should map to request");

        assert_eq!(assistant.id, "custom-assistant");
        assert!(assistant.locale.is_none());
        assert!(assistant.conversation_overrides.is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_preserves_agent_workspace() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let extra =
            build_conversation_extra(&registry, &job, None, ConversationPurpose::NewConversationExecution).await;

        assert_eq!(
            extra["workspace"],
            ensure_named_workspace_path("aionui-cron-sample-job-workspace")
        );
    }

    #[tokio::test]
    async fn build_conversation_extra_falls_back_to_agent_type_for_acp_backend() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };

        let extra =
            build_conversation_extra(&registry, &job, None, ConversationPurpose::NewConversationExecution).await;

        assert_eq!(extra["backend"], "claude");
    }

    // -- execution_result display ---------------------------------------------

    #[test]
    fn execution_result_variants() {
        let success = ExecutionResult::Success {
            conversation_id: "conv_1".into(),
        };
        assert_eq!(
            success,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );

        let retrying = ExecutionResult::Retrying { attempt: 2 };
        assert_eq!(retrying, ExecutionResult::Retrying { attempt: 2 });

        assert_eq!(ExecutionResult::Skipped, ExecutionResult::Skipped);

        let error = ExecutionResult::Error { message: "oops".into() };
        assert_eq!(error, ExecutionResult::Error { message: "oops".into() });
    }

    #[tokio::test]
    async fn execute_inner_leaves_session_mode_to_conversation_runtime() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "default");
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_does_not_directly_set_mode_for_uninitialized_agent() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", false));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "default");
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_skips_mode_update_when_already_matching() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "yolo", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_new_conversation_without_saved_skill_requests_skill_suggest() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let executor = make_executor_with_task_manager(task_manager.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(
            sent_messages[0]
                .content
                .contains("create a file named \"SKILL_SUGGEST.md\"")
        );
        assert!(sent_messages[0].inject_skills.is_empty());

        let options = task_manager
            .last_options()
            .expect("task manager should capture build options");
        assert!(options.context.skills.is_empty());
    }

    #[tokio::test]
    async fn execute_inner_builds_agent_task_once_through_conversation_service() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let executor = make_executor_with_task_manager(task_manager.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(
            task_manager.recorded_options().len(),
            1,
            "cron execution should let the conversation layer build the agent task exactly once"
        );
    }

    #[tokio::test]
    async fn execute_inner_returns_conversation_turn_error_message() {
        let executor = make_executor_with_task_manager(Arc::new(FailingTaskManager::new(
            "ACP init failed: config file is invalid",
        )));
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Error {
                message: "ACP init failed: config file is invalid".into()
            }
        );
    }

    #[tokio::test]
    async fn execute_inner_new_conversation_with_saved_skill_injects_saved_skill() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let executor = make_executor_with_task_manager(task_manager.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let saved_skill = SavedSkillContext {
            name: "cron-cron_test1".into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let result = executor.execute_inner(&job, "conv_1", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(
            sent_messages[0]
                .content
                .contains("A skill file with detailed instructions has been loaded")
        );
        assert!(!sent_messages[0].content.contains("SKILL_SUGGEST.md"));
        assert_eq!(sent_messages[0].inject_skills, vec!["cron-cron_test1".to_owned()]);

        let options = task_manager
            .recorded_options()
            .into_iter()
            .next()
            .expect("task manager should capture build options");
        assert!(options.context.skills.is_empty());
    }

    #[tokio::test]
    async fn execute_inner_existing_with_saved_skill_keeps_saved_skill_out_of_prompt_and_turn() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let job = sample_job();
        let saved_skill = SavedSkillContext {
            name: "cron-cron_test1".into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let result = executor.execute_inner(&job, "conv_1", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(!sent_messages[0].content.contains("## Skill Instructions"));
        assert!(!sent_messages[0].content.contains("Do X"));
        assert!(sent_messages[0].inject_skills.is_empty());
    }

    #[tokio::test]
    async fn execute_inner_existing_without_saved_skill_does_not_send_skill_suggest_follow_up() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let job = sample_job();

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let _ = agent
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData::default()));
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            agent.send_calls(),
            1,
            "existing-mode cron should not send a follow-up SKILL_SUGGEST prompt"
        );
    }

    #[tokio::test]
    async fn execute_inner_uses_conversation_workspace_when_job_workspace_missing() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let executor = make_executor_with_task_manager(task_manager.clone());
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let options = task_manager
            .last_options()
            .expect("task manager should capture build options");
        assert_eq!(
            options.context.workspace.path,
            ensure_named_workspace_path("aionui-cron-existing-conversation-workspace")
        );
        assert!(options.context.workspace.is_custom);
    }

    #[tokio::test]
    async fn execute_inner_persists_agent_workspace_when_conversation_workspace_missing() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new("conv_1", serde_json::json!({})));
        let executor = make_executor_with_task_manager_and_repo(task_manager.clone(), repo.clone());
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let options = task_manager
            .last_options()
            .expect("task manager should capture build options");
        assert!(options.context.workspace.path.ends_with("acp-temp-conv_1"));
        assert!(options.context.workspace.stored_path.is_empty());
        assert!(!options.context.workspace.is_custom);

        let update = repo
            .last_update_with_extra()
            .expect("conversation workspace should be persisted");
        let extra = update.extra.expect("workspace update should write extra");
        let value: serde_json::Value = serde_json::from_str(&extra).expect("valid extra json");
        assert_eq!(
            value["workspace"],
            ensure_named_workspace_path("aionui-cron-agent-workspace")
        );
    }

    #[tokio::test]
    async fn execute_inner_inserts_right_side_user_message_for_cron_prompt() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_1",
            serde_json::json!({
                "workspace": ensure_named_workspace_path("aionui-cron-existing-conversation-workspace")
            }),
        ));
        let executor = make_executor_with_task_manager_and_repo(task_manager, repo.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let messages = repo.inserted_messages();
        assert!(!messages.is_empty(), "cron execution should insert a user message");
        let right_message = messages
            .iter()
            .find(|message| message.position.as_deref() == Some("right"))
            .expect("cron execution should insert a right-side prompt message");
        assert_eq!(right_message.r#type, "text");
        assert!(right_message.hidden);
        assert!(right_message.content.contains("SKILL_SUGGEST.md"));
    }

    #[tokio::test]
    async fn execute_inner_upserts_cron_trigger_artifact_and_broadcasts_event() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_1",
            serde_json::json!({
                "workspace": ensure_named_workspace_path("aionui-cron-existing-conversation-workspace")
            }),
        ));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let executor =
            make_executor_with_task_manager_repo_and_broadcaster(task_manager, repo.clone(), broadcaster.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let messages = repo.inserted_messages();
        assert!(
            messages.iter().all(|message| message.r#type != "cron_trigger"),
            "cron execution should no longer persist cron trigger as a message"
        );

        let events = broadcaster.events();
        let trigger_event = events
            .iter()
            .find(|event| event["name"] == "conversation.artifact" && event["data"]["kind"] == "cron_trigger")
            .expect("cron execution should broadcast cron trigger artifact");
        assert_eq!(trigger_event["data"]["conversation_id"], "conv_1");
        assert_eq!(trigger_event["data"]["payload"]["cron_job_id"], "cron_test1");
        assert_eq!(trigger_event["data"]["payload"]["cron_job_name"], "Test Job");
        assert!(trigger_event["data"]["payload"]["triggered_at"].as_i64().is_some());
    }

    #[tokio::test]
    async fn execute_inner_places_cron_trigger_artifact_before_assistant_response() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true).with_text_response("assistant reply"));
        let task_manager = Arc::new(RecordingTaskManager::new(AgentInstance::Mock(agent.clone())));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_1",
            serde_json::json!({
                "workspace": ensure_named_workspace_path("aionui-cron-existing-conversation-workspace")
            }),
        ));
        let executor = make_executor_with_task_manager_and_repo(task_manager, repo.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_1", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_1".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let operations = repo.operations();
        let trigger_index = operations
            .iter()
            .position(|operation| operation == "upsert_artifact:cron_trigger")
            .expect("cron trigger artifact should be upserted");
        let assistant_index = operations
            .iter()
            .position(|operation| operation == "insert_message:left:text")
            .expect("assistant response should be persisted");
        assert!(
            trigger_index < assistant_index,
            "cron trigger card must be created before assistant responses so it renders before the AI reply"
        );
    }

    #[tokio::test]
    async fn execute_inner_returns_retrying_when_send_message_reports_busy() {
        let agent = Arc::new(RecordingAgent::new("conv_1", "default", true));
        let executor = make_executor_with_agent(AgentInstance::Mock(agent.clone()));
        let job = sample_job();
        let claim = executor
            .conversation_service
            .runtime_state()
            .try_claim_turn(&job.conversation_id, "turn-existing")
            .expect("runtime claim should succeed");

        let result = executor.execute_inner(&job, &job.conversation_id, None).await;

        assert_eq!(result, ExecutionResult::Retrying { attempt: 1 });
        assert_eq!(agent.send_calls(), 0, "busy send failure should not reach the agent");

        drop(claim);
    }

    // -- helper ---------------------------------------------------------------

    fn make_executor_for_busy_tests() -> JobExecutor {
        struct StubTaskManager;
        #[async_trait::async_trait]
        impl IWorkerTaskManager for StubTaskManager {
            fn get_task(&self, _: &str) -> Option<AgentInstance> {
                None
            }
            async fn get_or_build_task(
                &self,
                _: &str,
                _: BuildTaskOptions,
            ) -> Result<AgentInstance, aionui_ai_agent::AgentError> {
                Err(aionui_ai_agent::AgentError::internal("stub"))
            }
            fn kill(
                &self,
                _: &str,
                _: Option<aionui_common::AgentKillReason>,
            ) -> Result<(), aionui_ai_agent::AgentError> {
                Ok(())
            }
            fn kill_and_wait(
                &self,
                _: &str,
                _: Option<aionui_common::AgentKillReason>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(std::future::ready(()))
            }
            async fn clear(&self) {}
            fn active_count(&self) -> usize {
                0
            }
            fn collect_idle(&self, _: aionui_common::TimestampMs) -> Vec<String> {
                vec![]
            }
        }

        struct StubConvRepo;

        #[async_trait::async_trait]
        impl IConversationRepository for StubConvRepo {
            async fn get(&self, _id: &str) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
                Ok(None)
            }
            async fn create(&self, _row: &aionui_db::models::ConversationRow) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn update(&self, _id: &str, _updates: &ConversationRowUpdate) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn list_paginated(
                &self,
                _user_id: &str,
                _filters: &ConversationFilters,
            ) -> Result<PaginatedResult<aionui_db::models::ConversationRow>, aionui_db::DbError> {
                Ok(PaginatedResult {
                    items: vec![],
                    total: 0,
                    has_more: false,
                })
            }
            async fn find_by_source_and_chat(
                &self,
                _user_id: &str,
                _source: &str,
                _chat_id: &str,
                _agent_type: &str,
            ) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
                Ok(None)
            }
            async fn list_by_cron_job(
                &self,
                _user_id: &str,
                _cron_job_id: &str,
            ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
                Ok(vec![])
            }
            async fn list_associated(
                &self,
                _user_id: &str,
                _conversation_id: &str,
            ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
                Ok(vec![])
            }
            async fn list_messages_page(
                &self,
                _conv_id: &str,
                _params: &MessagePageParams,
            ) -> Result<MessagePageResult, aionui_db::DbError> {
                Ok(MessagePageResult {
                    items: vec![],
                    has_more_before: false,
                    has_more_after: false,
                })
            }
            async fn insert_message(&self, _message: &aionui_db::models::MessageRow) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), aionui_db::DbError> {
                Ok(())
            }
            async fn get_message_by_msg_id(
                &self,
                _conv_id: &str,
                _msg_id: &str,
                _msg_type: &str,
            ) -> Result<Option<aionui_db::models::MessageRow>, aionui_db::DbError> {
                Ok(None)
            }
            async fn search_messages(
                &self,
                _user_id: &str,
                _keyword: &str,
                _page: u32,
                _page_size: u32,
            ) -> Result<PaginatedResult<MessageSearchRow>, aionui_db::DbError> {
                Ok(PaginatedResult {
                    items: vec![],
                    total: 0,
                    has_more: false,
                })
            }
        }

        struct StubBroadcaster;
        impl aionui_realtime::EventBroadcaster for StubBroadcaster {
            fn broadcast(&self, _: WebSocketMessage<serde_json::Value>) {}
        }

        struct StubSkillResolver;
        #[async_trait::async_trait]
        impl aionui_conversation::skill_resolver::SkillResolver for StubSkillResolver {
            async fn auto_inject_names(&self) -> Vec<String> {
                Vec::new()
            }

            async fn resolve_skills(
                &self,
                _names: &[String],
            ) -> Vec<aionui_conversation::skill_resolver::ResolvedAgentSkill> {
                Vec::new()
            }

            async fn link_workspace_skills(
                &self,
                _workspace: &std::path::Path,
                _rel_dirs: &[&str],
                _skills: &[aionui_conversation::skill_resolver::ResolvedAgentSkill],
            ) -> usize {
                0
            }
        }

        let stub_broadcaster: Arc<dyn aionui_realtime::EventBroadcaster> = Arc::new(StubBroadcaster);
        let stub_repo: Arc<dyn IConversationRepository> = Arc::new(StubConvRepo);
        let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
        let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> = Arc::new(StubAcpSessionRepo);
        let conv_service = Arc::new(ConversationService::new(
            std::env::temp_dir(),
            stub_broadcaster,
            Arc::new(StubSkillResolver),
            Arc::new(StubTaskManager),
            Arc::clone(&stub_repo),
            Arc::clone(&agent_metadata_repo),
            acp_session_repo,
        ));

        let agent_registry = AgentRegistry::new(agent_metadata_repo);

        JobExecutor::new(
            Arc::new(StubTaskManager),
            stub_repo,
            conv_service,
            std::env::temp_dir(),
            std::env::temp_dir(),
            Arc::new(StubBroadcaster),
            agent_registry,
        )
    }

    struct RecordingAgent {
        conversation_id: String,
        workspace: String,
        event_tx: broadcast::Sender<AgentStreamEvent>,
        mode: RwLock<String>,
        sent_messages: RwLock<Vec<SendMessageData>>,
        response_text: Option<String>,
        initialized: bool,
        set_mode_calls: AtomicUsize,
        send_calls: AtomicUsize,
    }

    impl RecordingAgent {
        fn new(conversation_id: &str, mode: &str, initialized: bool) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                conversation_id: conversation_id.to_owned(),
                workspace: ensure_named_workspace_path("aionui-cron-agent-workspace"),
                event_tx,
                mode: RwLock::new(mode.to_owned()),
                sent_messages: RwLock::new(Vec::new()),
                response_text: None,
                initialized,
                set_mode_calls: AtomicUsize::new(0),
                send_calls: AtomicUsize::new(0),
            }
        }

        fn with_text_response(mut self, content: &str) -> Self {
            self.response_text = Some(content.to_owned());
            self
        }

        async fn mode(&self) -> String {
            self.mode.read().await.clone()
        }

        fn set_mode_calls(&self) -> usize {
            self.set_mode_calls.load(Ordering::Relaxed)
        }

        fn send_calls(&self) -> usize {
            self.send_calls.load(Ordering::Relaxed)
        }

        async fn sent_messages(&self) -> Vec<SendMessageData> {
            self.sent_messages.read().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl IAgentTask for RecordingAgent {
        fn agent_type(&self) -> AgentType {
            AgentType::Acp
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            &self.workspace
        }

        fn status(&self) -> Option<ConversationStatus> {
            Some(ConversationStatus::Pending)
        }

        fn last_activity_at(&self) -> TimestampMs {
            0
        }

        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }

        async fn send_message(&self, data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
            self.send_calls.fetch_add(1, Ordering::Relaxed);
            self.sent_messages.write().await.push(data);
            if let Some(content) = self.response_text.as_ref() {
                let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
                    content: content.clone(),
                }));
            }
            let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
            Ok(())
        }

        async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
            Ok(())
        }

        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl IMockAgent for RecordingAgent {
        async fn mode(&self) -> Result<AgentModeResponse, aionui_ai_agent::AgentError> {
            Ok(AgentModeResponse {
                mode: self.mode().await,
                initialized: self.initialized,
            })
        }

        async fn set_config_option(
            &self,
            option_id: &str,
            value: &str,
        ) -> Result<SetConfigOptionResponse, aionui_ai_agent::AgentError> {
            if option_id != "mode" {
                return Err(aionui_ai_agent::AgentError::bad_request(format!(
                    "unsupported config option: {option_id}"
                )));
            }
            self.set_mode_calls.fetch_add(1, Ordering::Relaxed);
            let mut guard = self.mode.write().await;
            *guard = value.to_owned();
            Ok(SetConfigOptionResponse {
                confirmation: ConfigOptionConfirmation::Observed,
                config_options: None,
            })
        }
    }

    struct FixedTaskManager {
        agent: AgentInstance,
    }

    struct FailingTaskManager {
        message: String,
    }

    impl FailingTaskManager {
        fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for FailingTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
            None
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, aionui_ai_agent::AgentError> {
            Err(aionui_ai_agent::AgentError::bad_gateway(self.message.clone()))
        }

        fn kill(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> Result<(), aionui_ai_agent::AgentError> {
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            0
        }

        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for FixedTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
            Some(self.agent.clone())
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, aionui_ai_agent::AgentError> {
            Ok(self.agent.clone())
        }

        fn kill(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> Result<(), aionui_ai_agent::AgentError> {
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            1
        }

        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct RecordingTaskManager {
        agent: AgentInstance,
        options: Mutex<Vec<BuildTaskOptions>>,
    }

    impl RecordingTaskManager {
        fn new(agent: AgentInstance) -> Self {
            Self {
                agent,
                options: Mutex::new(Vec::new()),
            }
        }

        fn last_options(&self) -> Option<BuildTaskOptions> {
            self.options.lock().ok().and_then(|items| items.last().cloned())
        }

        fn recorded_options(&self) -> Vec<BuildTaskOptions> {
            self.options.lock().map(|items| items.clone()).unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for RecordingTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
            Some(self.agent.clone())
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            options: BuildTaskOptions,
        ) -> Result<AgentInstance, aionui_ai_agent::AgentError> {
            self.options.lock().unwrap().push(options);
            Ok(self.agent.clone())
        }

        fn kill(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> Result<(), aionui_ai_agent::AgentError> {
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            1
        }

        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct ExistingConversationRepo;

    #[async_trait::async_trait]
    impl IConversationRepository for ExistingConversationRepo {
        async fn get(&self, id: &str) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(Some(aionui_db::models::ConversationRow {
                id: id.to_owned(),
                user_id: "cron".into(),
                name: "Cron Conversation".into(),
                r#type: "acp".into(),
                extra: serde_json::json!({
                    "workspace": ensure_named_workspace_path("aionui-cron-existing-conversation-workspace")
                })
                .to_string(),
                model: None,
                status: Some("finished".into()),
                source: None,
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 0,
                updated_at: 0,
            }))
        }

        async fn create(&self, _row: &aionui_db::models::ConversationRow) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn update(&self, _id: &str, _updates: &ConversationRowUpdate) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &ConversationFilters,
        ) -> Result<PaginatedResult<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }

        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn list_by_cron_job(
            &self,
            _user_id: &str,
            _cron_job_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(vec![])
        }

        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(vec![])
        }

        async fn list_messages_page(
            &self,
            _conv_id: &str,
            _params: &MessagePageParams,
        ) -> Result<MessagePageResult, aionui_db::DbError> {
            Ok(MessagePageResult {
                items: vec![],
                has_more_before: false,
                has_more_after: false,
            })
        }

        async fn insert_message(&self, _message: &aionui_db::models::MessageRow) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<aionui_db::models::MessageRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<PaginatedResult<MessageSearchRow>, aionui_db::DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
    }

    struct MissingWorkspaceConversationRepo {
        row: aionui_db::models::ConversationRow,
        updates: Mutex<Vec<ConversationRowUpdate>>,
        inserted_messages: Mutex<Vec<aionui_db::models::MessageRow>>,
        artifacts: Mutex<Vec<ConversationArtifactRow>>,
        operations: Mutex<Vec<String>>,
    }

    impl MissingWorkspaceConversationRepo {
        fn new(conversation_id: &str, extra: serde_json::Value) -> Self {
            Self {
                row: aionui_db::models::ConversationRow {
                    id: conversation_id.to_owned(),
                    user_id: "cron".into(),
                    name: "Cron Conversation".into(),
                    r#type: "acp".into(),
                    extra: extra.to_string(),
                    model: None,
                    status: Some("finished".into()),
                    source: None,
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: 0,
                    updated_at: 0,
                },
                updates: Mutex::new(Vec::new()),
                inserted_messages: Mutex::new(Vec::new()),
                artifacts: Mutex::new(Vec::new()),
                operations: Mutex::new(Vec::new()),
            }
        }

        fn last_update_with_extra(&self) -> Option<ConversationRowUpdate> {
            self.updates
                .lock()
                .ok()
                .and_then(|items| items.iter().rev().find(|update| update.extra.is_some()).cloned())
        }

        fn inserted_messages(&self) -> Vec<aionui_db::models::MessageRow> {
            self.inserted_messages
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default()
        }

        fn operations(&self) -> Vec<String> {
            self.operations.lock().map(|items| items.clone()).unwrap_or_default()
        }
    }

    struct RecordingBroadcaster {
        events: Mutex<Vec<serde_json::Value>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<serde_json::Value> {
            self.events.lock().map(|items| items.clone()).unwrap_or_default()
        }
    }

    impl aionui_realtime::EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(serde_json::json!({
                "name": event.name,
                "data": event.data,
            }));
        }
    }

    struct StubBroadcaster;

    impl aionui_realtime::EventBroadcaster for StubBroadcaster {
        fn broadcast(&self, _: WebSocketMessage<serde_json::Value>) {}
    }

    #[async_trait::async_trait]
    impl IConversationRepository for MissingWorkspaceConversationRepo {
        async fn get(&self, _id: &str) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(Some(self.row.clone()))
        }

        async fn create(&self, _row: &aionui_db::models::ConversationRow) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn update(&self, _id: &str, updates: &ConversationRowUpdate) -> Result<(), aionui_db::DbError> {
            self.updates.lock().unwrap().push(updates.clone());
            Ok(())
        }

        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &ConversationFilters,
        ) -> Result<PaginatedResult<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }

        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn list_by_cron_job(
            &self,
            _user_id: &str,
            _cron_job_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(vec![])
        }

        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
            Ok(vec![])
        }

        async fn list_messages_page(
            &self,
            _conv_id: &str,
            _params: &MessagePageParams,
        ) -> Result<MessagePageResult, aionui_db::DbError> {
            Ok(MessagePageResult {
                items: vec![],
                has_more_before: false,
                has_more_after: false,
            })
        }

        async fn insert_message(&self, message: &aionui_db::models::MessageRow) -> Result<(), aionui_db::DbError> {
            self.operations.lock().unwrap().push(format!(
                "insert_message:{}:{}",
                message.position.as_deref().unwrap_or("none"),
                message.r#type
            ));
            self.inserted_messages.lock().unwrap().push(message.clone());
            Ok(())
        }

        async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<aionui_db::models::MessageRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<PaginatedResult<MessageSearchRow>, aionui_db::DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }

        async fn upsert_artifact(
            &self,
            artifact: &ConversationArtifactRow,
        ) -> Result<ConversationArtifactRow, aionui_db::DbError> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("upsert_artifact:{}", artifact.kind));
            let mut artifacts = self.artifacts.lock().unwrap();
            if let Some(existing) = artifacts.iter_mut().find(|row| row.id == artifact.id) {
                *existing = artifact.clone();
                return Ok(existing.clone());
            }
            artifacts.push(artifact.clone());
            Ok(artifact.clone())
        }
    }

    fn make_executor_with_agent(agent: AgentInstance) -> JobExecutor {
        make_executor_with_task_manager(Arc::new(FixedTaskManager { agent }))
    }

    fn make_executor_with_task_manager(task_manager: Arc<dyn IWorkerTaskManager>) -> JobExecutor {
        make_executor_with_task_manager_and_repo(task_manager, Arc::new(ExistingConversationRepo))
    }

    fn make_executor_with_task_manager_and_repo(
        task_manager: Arc<dyn IWorkerTaskManager>,
        repo: Arc<dyn IConversationRepository>,
    ) -> JobExecutor {
        let broadcaster: Arc<dyn aionui_realtime::EventBroadcaster> = Arc::new(StubBroadcaster);
        make_executor_with_task_manager_repo_and_broadcaster(task_manager, repo, broadcaster)
    }

    fn make_executor_with_task_manager_repo_and_broadcaster(
        task_manager: Arc<dyn IWorkerTaskManager>,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
    ) -> JobExecutor {
        struct StubSkillResolver;

        #[async_trait::async_trait]
        impl aionui_conversation::skill_resolver::SkillResolver for StubSkillResolver {
            async fn auto_inject_names(&self) -> Vec<String> {
                Vec::new()
            }

            async fn resolve_skills(
                &self,
                _names: &[String],
            ) -> Vec<aionui_conversation::skill_resolver::ResolvedAgentSkill> {
                Vec::new()
            }

            async fn link_workspace_skills(
                &self,
                _workspace: &std::path::Path,
                _rel_dirs: &[&str],
                _skills: &[aionui_conversation::skill_resolver::ResolvedAgentSkill],
            ) -> usize {
                0
            }
        }

        let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
        let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> = Arc::new(StubAcpSessionRepo);
        let conversation_service = Arc::new(ConversationService::new(
            std::env::temp_dir(),
            Arc::clone(&broadcaster),
            Arc::new(StubSkillResolver),
            Arc::clone(&task_manager),
            Arc::clone(&repo),
            Arc::clone(&agent_metadata_repo),
            acp_session_repo,
        ));

        let agent_registry = AgentRegistry::new(agent_metadata_repo);

        JobExecutor::new(
            task_manager,
            repo,
            conversation_service,
            std::env::temp_dir(),
            std::env::temp_dir(),
            broadcaster,
            agent_registry,
        )
    }

    struct StubAcpSessionRepo;

    #[async_trait::async_trait]
    impl aionui_db::IAcpSessionRepository for StubAcpSessionRepo {
        async fn get(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<aionui_db::models::AcpSessionRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn create(
            &self,
            _params: &aionui_db::CreateAcpSessionParams<'_>,
        ) -> Result<aionui_db::models::AcpSessionRow, aionui_db::DbError> {
            Err(aionui_db::DbError::Init("stub".into()))
        }
        async fn update_session_id(
            &self,
            _conversation_id: &str,
            _session_id: &str,
        ) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
        async fn delete(&self, _conversation_id: &str) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
        async fn load_runtime_state(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<aionui_db::PersistedSessionState>, aionui_db::DbError> {
            Ok(None)
        }
        async fn save_runtime_state(
            &self,
            _conversation_id: &str,
            _params: &aionui_db::SaveRuntimeStateParams<'_>,
        ) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
    }

    struct StubAgentMetadataRepo;

    #[async_trait::async_trait]
    impl aionui_db::IAgentMetadataRepository for StubAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(Vec::new())
        }
        async fn get(&self, _id: &str) -> Result<Option<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn find_by_source_and_name(
            &self,
            _agent_source: &str,
            _name: &str,
        ) -> Result<Option<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn find_builtin_by_backend(
            &self,
            _backend: &str,
        ) -> Result<Option<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn upsert(
            &self,
            _params: &aionui_db::models::UpsertAgentMetadataParams<'_>,
        ) -> Result<aionui_db::models::AgentMetadataRow, aionui_db::DbError> {
            Err(aionui_db::DbError::Init("stub".into()))
        }
        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &aionui_db::models::UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn update_availability_snapshot(
            &self,
            _id: &str,
            _params: &aionui_db::models::UpdateAgentAvailabilitySnapshotParams<'_>,
        ) -> Result<Option<aionui_db::models::AgentMetadataRow>, aionui_db::DbError> {
            Ok(None)
        }
        async fn update_agent_overrides(
            &self,
            _id: &str,
            _command_override: Option<&str>,
            _env_override: Option<&str>,
        ) -> Result<(), aionui_db::DbError> {
            Ok(())
        }
        async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
        async fn delete(&self, _id: &str) -> Result<bool, aionui_db::DbError> {
            Ok(false)
        }
    }
}
