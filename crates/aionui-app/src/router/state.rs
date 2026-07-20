//! Module-level router states + their builders.
//!
//! `ModuleStates` is the bundle returned by `build_module_states`; each
//! `build_*_state` constructs one `*RouterState` from `AppServices`.

use std::sync::Arc;
use std::time::Instant;

use aionui_ai_agent::{AgentRouterState, AgentService, RemoteAgentRouterState, RemoteAgentService};
use aionui_api_types::{CreateTeamRequest, TeamAgentInput};
use aionui_assistant::{
    AssistantAgentCatalogPort, AssistantError, AssistantRouterState, AssistantService, BuiltinAssistantRegistry,
};
use aionui_auth::extract_token_from_ws_headers;
use aionui_channel::error::ChannelError;
use aionui_channel::message_service::ChannelTeamSender;
use aionui_channel::{
    ChannelRouterState,
    action::{
        ChannelPersonalConversationSummary, ChannelPersonalDirectory, ChannelTeamCreateRequest, ChannelTeamDirectory,
        ChannelTeamSummary,
    },
    approval::{ChannelApprovalContext, ChannelApprovalPort},
    development::{ChannelDevelopmentCommand, ChannelDevelopmentContext, ChannelDevelopmentPort},
};
use aionui_common::now_ms;
use aionui_conversation::{ConversationRouterState, ConversationService};
use aionui_cron::{CronEventEmitter, CronRouterState, service::CronServiceDeps};
use aionui_db::models::MessageRow;
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, IAcpSessionRepository, IAgentMetadataRepository,
    IAgentWorkspaceLeaseRepository, IApprovalRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository, IConversationRepository,
    IDevelopmentRepository, IProjectRepository, IProviderRepository, ITeamRepository, MessagePageDirection,
    MessagePageParams, SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAgentWorkspaceLeaseRepository,
    SqliteApprovalRepository, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteClientPreferenceRepository, SqliteConversationRepository, SqliteDevelopmentOperationsRepository,
    SqliteDevelopmentRepository, SqliteProjectRepository, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteTeamRepository,
};
use aionui_development::{
    ApprovalOption, ApprovalRequestInput, ApprovalResolver, ApprovalRouterState, ApprovalService, ApprovalSource,
    DeliveryService, DevelopmentOperationsService, DevelopmentRouterState, DevelopmentService, GhCliDeliveryProvider,
    ResolveApprovalContext,
};
use aionui_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionStateStore, ExternalPathsManager,
    HubIndexManager, HubInstaller, HubRouterState, SkillRouterState, resolve_install_target_dir_for_data_dir,
    resolve_scan_paths_for_data_dir, resolve_state_file_path,
};
use aionui_file::{BrowseRoots, FileRouterState, FileService, FileWatchService, SnapshotService};
use aionui_mcp::{
    AionrsAdapter, AionuiAdapter, ClaudeAdapter, CodeBuddyAdapter, CodexAdapter, GeminiAdapter, McpAgentAdapter,
    McpConfigService, McpConnectionTestService, McpRouterState, McpSyncService, OpencodeAdapter, QwenAdapter,
};
use aionui_office::{
    ConversionService, OfficeRouterState, OfficecliWatchManager, ProxyService, SnapshotService as OfficeSnapshotService,
};
use aionui_project::{
    AgentCapabilitySnapshot, ProjectAgentCapabilityPort, ProjectError, ProjectRouterState, ProjectService,
};
use aionui_realtime::{NoopMessageRouter, WsHandlerState};
use aionui_shell::ShellRouterState;
use aionui_system::{
    ClientPrefService, ConnectionTestRouterState, ConnectionTestService, ModelFetchService, ProtocolDetectionService,
    ProviderService, RuntimePrepareService, SettingsService, SystemRouterState, VersionCheckService,
};
use aionui_team::{
    AgentTurnCancellationPort, AgentTurnExecutionPort, TeamConversationProvisioningPort, TeamProjectionMessageStore,
    TeamRouterState, TeamSessionService,
};

use crate::config::derive_encryption_key;
use crate::router::team_conversation_adapters::TeamConversationAdapters;
use crate::services::AppServices;

struct ChannelTeamDirectoryAdapter {
    service: Arc<TeamSessionService>,
    owner_user_id: String,
}

struct ChannelPersonalDirectoryAdapter {
    conversation_repo: Arc<dyn IConversationRepository>,
    owner_user_id: String,
}

struct ProjectAgentCapabilityAdapter {
    service: Arc<AgentService>,
}

struct ApprovalAgentResolver {
    task_manager: Arc<dyn aionui_ai_agent::IWorkerTaskManager>,
}

struct ChannelApprovalAdapter {
    service: Arc<ApprovalService>,
    owner_user_id: String,
    project_repo: Arc<dyn IProjectRepository>,
    development_service: Arc<DevelopmentService>,
}

struct ChannelDevelopmentAdapter {
    owner_user_id: String,
    project_repo: Arc<dyn IProjectRepository>,
    development_repo: Arc<dyn IDevelopmentRepository>,
    approval_repo: Arc<dyn IApprovalRepository>,
    service: Arc<DevelopmentService>,
}

#[async_trait::async_trait]
impl ApprovalResolver for ApprovalAgentResolver {
    async fn resolve(
        &self,
        conversation_id: &str,
        call_id: &str,
        value: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), String> {
        let agent = self
            .task_manager
            .get_task(conversation_id)
            .ok_or_else(|| "Conversation agent is no longer running".to_owned())?;
        agent
            .confirm(call_id, call_id, value, always_allow)
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl ChannelApprovalPort for ChannelApprovalAdapter {
    async fn create(
        &self,
        context: ChannelApprovalContext,
        confirmation: aionui_common::Confirmation,
    ) -> Result<String, ChannelError> {
        let project = self
            .project_repo
            .get_for_resource(&self.owner_user_id, "conversation", &context.conversation_id)
            .await
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
        let project_id = project.as_ref().map(|project| project.id.clone());
        let run_id = if let Some(project) = project.as_ref() {
            self.development_service
                .list_runs(&self.owner_user_id, Some(&project.id))
                .await
                .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?
                .into_iter()
                .find(|run| !matches!(run.status.as_str(), "succeeded" | "failed" | "cancelled"))
                .map(|run| run.id)
        } else {
            None
        };
        let risk_level = match confirmation.command_type.as_deref() {
            Some("read") => "low",
            Some("edit") | Some("execute") => "high",
            _ => "medium",
        };
        let action_type = confirmation.command_type.clone().unwrap_or_else(|| "tool_call".into());
        let row = self
            .service
            .create(ApprovalRequestInput {
                requester_user_id: self.owner_user_id.clone(),
                project_id,
                run_id,
                task_id: None,
                conversation_id: context.conversation_id,
                agent_id: context.agent_id,
                call_id: confirmation.call_id,
                action_type,
                command: Some(confirmation.description),
                working_directory: None,
                risk_level: risk_level.into(),
                options: confirmation
                    .options
                    .into_iter()
                    .map(|option| ApprovalOption {
                        label: option.label,
                        value: option.value,
                        params: option.params.map(|params| serde_json::json!(params)),
                    })
                    .collect(),
                source: Some(ApprovalSource {
                    channel: context.platform.to_string(),
                    user_id: context.source_user_id,
                    chat_id: context.chat_id,
                    thread_id: context.message_thread_id,
                }),
            })
            .await
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
        Ok(row.id)
    }

    async fn resolve(
        &self,
        source_user_id: &str,
        platform: aionui_channel::types::PluginType,
        chat_id: &str,
        message_thread_id: Option<i64>,
        approval_id: &str,
        option_index: usize,
    ) -> Result<String, ChannelError> {
        self.service
            .resolve(
                approval_id,
                option_index,
                ResolveApprovalContext::Channel {
                    user_id: self.owner_user_id.clone(),
                    channel: platform.to_string(),
                    source_user_id: source_user_id.to_owned(),
                    chat_id: chat_id.to_owned(),
                    thread_id: message_thread_id,
                },
            )
            .await
            .map(|row| row.status)
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))
    }
}

impl ChannelDevelopmentAdapter {
    async fn project_for_context(
        &self,
        context: &ChannelDevelopmentContext,
    ) -> Result<aionui_db::models::ProjectRow, ChannelError> {
        if let Some(conversation_id) = context.conversation_id.as_deref()
            && let Some(project) = self
                .project_repo
                .get_for_resource(&self.owner_user_id, "conversation", conversation_id)
                .await
                .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?
        {
            return Ok(project);
        }
        let projects = self
            .project_repo
            .list_for_user(&self.owner_user_id)
            .await
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
        match projects.as_slice() {
            [project] => Ok(project.clone()),
            [] => Err(ChannelError::InvalidConfig(
                "当前没有项目，请先在 Web 中创建项目并绑定当前会话。".into(),
            )),
            _ => Err(ChannelError::InvalidConfig(
                "当前会话尚未绑定项目，请先在 Web 项目页完成绑定。".into(),
            )),
        }
    }

    async fn active_run(&self, project_id: &str) -> Result<Option<aionui_db::models::DevelopmentRunRow>, ChannelError> {
        Ok(self
            .service
            .list_runs(&self.owner_user_id, Some(project_id))
            .await
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?
            .into_iter()
            .find(|run| !matches!(run.status.as_str(), "succeeded" | "failed" | "cancelled")))
    }

    fn require_run(
        run: Option<aionui_db::models::DevelopmentRunRow>,
    ) -> Result<aionui_db::models::DevelopmentRunRow, ChannelError> {
        run.ok_or_else(|| ChannelError::InvalidConfig("当前项目没有进行中的开发运行。".into()))
    }
}

#[async_trait::async_trait]
impl ChannelDevelopmentPort for ChannelDevelopmentAdapter {
    async fn execute(
        &self,
        context: ChannelDevelopmentContext,
        command: ChannelDevelopmentCommand,
    ) -> Result<String, ChannelError> {
        let project = self.project_for_context(&context).await?;
        let active_run = self.active_run(&project.id).await?;
        if command == ChannelDevelopmentCommand::Project {
            return Ok(format!(
                "项目：{}\n类型：{}\n目录：{}\n当前运行：{}",
                project.name,
                project.project_type,
                project.local_path,
                active_run.as_ref().map(|run| run.status.as_str()).unwrap_or("无")
            ));
        }
        let run = Self::require_run(active_run)?;
        let tasks = self
            .service
            .list_tasks(&self.owner_user_id, &run.id)
            .await
            .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
        match command {
            ChannelDevelopmentCommand::Project => unreachable!(),
            ChannelDevelopmentCommand::RunInfo => {
                let pending = self
                    .approval_repo
                    .list_for_user(&self.owner_user_id, Some(&run.id))
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?
                    .into_iter()
                    .filter(|approval| approval.status == "pending")
                    .count();
                let completed = tasks.iter().filter(|task| task.status == "completed").count();
                Ok(format!(
                    "运行：{}\n状态：{}\n模式：{}\n任务：{}/{} 已完成\n待审批：{}\n目标：{}",
                    run.id,
                    run.status,
                    run.execution_mode,
                    completed,
                    tasks.len(),
                    pending,
                    run.request_summary
                ))
            }
            ChannelDevelopmentCommand::DiffSummary => {
                let artifacts = self
                    .service
                    .list_artifacts(&self.owner_user_id, &run.id, None)
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                let gates = self
                    .service
                    .list_gates(&self.owner_user_id, &run.id, None)
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                let passed = gates.iter().filter(|gate| gate.status == "passed").count();
                let failed = gates
                    .iter()
                    .filter(|gate| matches!(gate.status.as_str(), "failed" | "timed_out"))
                    .count();
                let evidence = artifacts
                    .iter()
                    .rev()
                    .take(5)
                    .map(|artifact| format!("- {}: {}", artifact.artifact_type, artifact.path_or_uri))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(format!(
                    "变更证据：{} 项\n质量门禁：{} 通过 / {} 失败\n{}",
                    artifacts.len(),
                    passed,
                    failed,
                    if evidence.is_empty() { "暂无证据" } else { &evidence }
                ))
            }
            ChannelDevelopmentCommand::Test => {
                let task = tasks
                    .iter()
                    .find(|task| !matches!(task.status.as_str(), "completed" | "cancelled" | "deleted"));
                let gate = self
                    .service
                    .execute_gate(
                        &self.owner_user_id,
                        &run.id,
                        task.map(|task| task.id.as_str()),
                        "unit_test",
                        task.and_then(|task| task.assigned_workspace_lease_id.as_deref()),
                        true,
                    )
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                Ok(format!(
                    "单元测试门禁：{}\n退出码：{}\n耗时：{} ms",
                    gate.status,
                    gate.exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "无".into()),
                    gate.duration_ms.unwrap_or_default()
                ))
            }
            ChannelDevelopmentCommand::Stop => {
                self.development_repo
                    .update_run_status(&run.id, &self.owner_user_id, "cancelled", Some(now_ms()))
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                Ok(format!("运行 {} 已停止。", run.id))
            }
            ChannelDevelopmentCommand::Retry => {
                let gates = self
                    .service
                    .list_gates(&self.owner_user_id, &run.id, None)
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                let previous = gates
                    .iter()
                    .rev()
                    .find(|gate| matches!(gate.status.as_str(), "failed" | "timed_out"))
                    .ok_or_else(|| ChannelError::InvalidConfig("没有可重试的失败门禁。".into()))?;
                let task = previous
                    .task_id
                    .as_deref()
                    .and_then(|task_id| tasks.iter().find(|task| task.id == task_id));
                let gate = self
                    .service
                    .execute_gate(
                        &self.owner_user_id,
                        &run.id,
                        previous.task_id.as_deref(),
                        &previous.gate_type,
                        task.and_then(|task| task.assigned_workspace_lease_id.as_deref()),
                        previous.required,
                    )
                    .await
                    .map_err(|error| ChannelError::MessageSendFailed(error.to_string()))?;
                Ok(format!("已重试 {} 门禁，结果：{}。", gate.gate_type, gate.status))
            }
            ChannelDevelopmentCommand::Handoff => Ok(format!(
                "Web 接力入口：/#/projects?projectId={}&runId={}\n打开后可查看任务、证据、审批和质量门禁。",
                project.id, run.id
            )),
        }
    }
}

#[async_trait::async_trait]
impl ProjectAgentCapabilityPort for ProjectAgentCapabilityAdapter {
    async fn snapshot(&self, id: &str, refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError> {
        let row = if refresh {
            match self.service.health_check_agent_by_id(id).await {
                Ok(row) => Some(row),
                Err(aionui_ai_agent::AgentError::NotFound(_)) => None,
                Err(error) => return Err(ProjectError::Internal(error.to_string())),
            }
        } else {
            self.service
                .list_management_agents()
                .await
                .map_err(|error| ProjectError::Internal(error.to_string()))?
                .into_iter()
                .find(|row| row.id == id)
        };
        Ok(row.map(agent_management_row_to_project_snapshot))
    }
}

fn serialized_enum<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn agent_management_row_to_project_snapshot(row: aionui_api_types::AgentManagementRow) -> AgentCapabilitySnapshot {
    AgentCapabilitySnapshot {
        id: row.id,
        agent_type: serialized_enum(&row.agent_type),
        enabled: row.enabled,
        installed: row.installed,
        status: serialized_enum(&row.status),
        last_check_status: row.last_check_status.as_ref().map(serialized_enum),
        last_check_at: row.last_check_at,
        last_success_at: row.last_success_at,
        agent_capabilities: row.agent_capabilities,
        available_models: row.available_models,
        available_modes: row.available_modes,
        available_commands: row.available_commands,
        dynamic_probe: row.dynamic_probe,
    }
}

#[async_trait::async_trait]
impl ChannelTeamDirectory for ChannelTeamDirectoryAdapter {
    async fn list_teams(&self, user_id: &str) -> Result<Vec<ChannelTeamSummary>, ChannelError> {
        let teams = self
            .service
            .list_teams(user_id)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;
        if teams.is_empty() && user_id != self.owner_user_id {
            let owner_teams = self
                .service
                .list_teams(&self.owner_user_id)
                .await
                .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;
            return Ok(owner_teams.into_iter().map(team_response_to_channel_summary).collect());
        }
        Ok(teams.into_iter().map(team_response_to_channel_summary).collect())
    }

    async fn get_team(&self, user_id: &str, team_id: &str) -> Result<Option<ChannelTeamSummary>, ChannelError> {
        Ok(self
            .list_teams(user_id)
            .await?
            .into_iter()
            .find(|team| team.id == team_id))
    }

    async fn ensure_team_session(&self, user_id: &str, team_id: &str) -> Result<(), ChannelError> {
        match self.service.ensure_session(user_id, team_id).await {
            Ok(()) => Ok(()),
            Err(primary_error) if user_id != self.owner_user_id => self
                .service
                .ensure_session(&self.owner_user_id, team_id)
                .await
                .map_err(|fallback_error| {
                    ChannelError::MessageSendFailed(format!(
                        "primary user failed: {primary_error}; owner fallback failed: {fallback_error}"
                    ))
                }),
            Err(error) => Err(ChannelError::MessageSendFailed(error.to_string())),
        }
    }

    async fn create_team(
        &self,
        user_id: &str,
        request: ChannelTeamCreateRequest,
    ) -> Result<ChannelTeamSummary, ChannelError> {
        let owner_user_id = if user_id == self.owner_user_id {
            user_id
        } else {
            &self.owner_user_id
        };
        let team = self
            .service
            .create_team(
                owner_user_id,
                CreateTeamRequest {
                    name: request.name,
                    agents: vec![TeamAgentInput {
                        name: request.lead_name,
                        role: request.lead_role,
                        backend: None,
                        model: request.model,
                        assistant_id: Some(request.assistant_id),
                        conversation_id: None,
                    }],
                    workspace: None,
                    source_channel: request.source_channel,
                    source_channel_id: request.source_channel_id,
                    source_chat_id: request.source_chat_id,
                    source_user_id: request.source_user_id,
                    source_label: request.source_label,
                    created_from: request.created_from,
                },
            )
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;
        Ok(team_response_to_channel_summary(team))
    }
}

#[async_trait::async_trait]
impl ChannelPersonalDirectory for ChannelPersonalDirectoryAdapter {
    async fn list_personal_conversations(
        &self,
        user_id: &str,
        platform: aionui_channel::types::PluginType,
        chat_id: &str,
    ) -> Result<Vec<ChannelPersonalConversationSummary>, ChannelError> {
        let conversations = self.list_for_user(user_id, platform, chat_id).await?;
        if conversations.is_empty() && user_id != self.owner_user_id {
            return self.list_for_user(&self.owner_user_id, platform, chat_id).await;
        }
        Ok(conversations)
    }

    async fn get_personal_conversation(
        &self,
        user_id: &str,
        platform: aionui_channel::types::PluginType,
        chat_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError> {
        Ok(self
            .list_personal_conversations(user_id, platform, chat_id)
            .await?
            .into_iter()
            .find(|conversation| conversation.id == conversation_id))
    }

    async fn rename_personal_conversation(
        &self,
        user_id: &str,
        platform: aionui_channel::types::PluginType,
        chat_id: &str,
        conversation_id: &str,
        title: &str,
    ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError> {
        if self
            .get_personal_conversation(user_id, platform, chat_id, conversation_id)
            .await?
            .is_none()
        {
            return Ok(None);
        }

        let Some(row) = self
            .conversation_repo
            .get(conversation_id)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?
        else {
            return Ok(None);
        };
        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        if !extra.is_object() {
            extra = serde_json::json!({});
        }
        extra["title_source"] = serde_json::Value::String("manual".to_owned());
        extra["auto_title"] = serde_json::Value::Bool(false);

        self.conversation_repo
            .update(
                conversation_id,
                &ConversationRowUpdate {
                    name: Some(title.to_owned()),
                    extra: Some(extra.to_string()),
                    updated_at: Some(now_ms()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        self.get_personal_conversation(user_id, platform, chat_id, conversation_id)
            .await
    }
}

impl ChannelPersonalDirectoryAdapter {
    async fn list_for_user(
        &self,
        user_id: &str,
        platform: aionui_channel::types::PluginType,
        chat_id: &str,
    ) -> Result<Vec<ChannelPersonalConversationSummary>, ChannelError> {
        let platform_key = platform.to_string();
        let page = self
            .conversation_repo
            .list_paginated(
                user_id,
                &ConversationFilters {
                    limit: 50,
                    source: Some(platform_key.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        let mut conversations = Vec::new();
        for row in page.items {
            let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
            if extra
                .get("teamId")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
            {
                continue;
            }

            let source_channel = row
                .source_channel
                .as_deref()
                .or_else(|| extra.get("source_channel").and_then(serde_json::Value::as_str))
                .or(row.source.as_deref())
                .unwrap_or_default();
            if source_channel != platform_key {
                continue;
            }

            let source_chat_id = row
                .source_chat_id
                .as_deref()
                .or_else(|| extra.get("source_chat_id").and_then(serde_json::Value::as_str))
                .or(row.channel_chat_id.as_deref());
            if let Some(source_chat_id) = source_chat_id
                && !source_chat_id.trim().is_empty()
                && source_chat_id != chat_id
            {
                continue;
            }

            let backend = extra.get("backend").and_then(serde_json::Value::as_str);
            let agent_label = agent_label_for_channel_conversation(&row.r#type, backend);
            let recent_message = self.recent_user_message_for_conversation(&row.id).await?;
            conversations.push(ChannelPersonalConversationSummary {
                id: row.id,
                name: row.name,
                agent_type: row.r#type,
                agent_label: Some(agent_label),
                recent_message,
            });
        }

        Ok(conversations)
    }

    async fn recent_user_message_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, ChannelError> {
        let page = self
            .conversation_repo
            .list_messages_page(
                conversation_id,
                &MessagePageParams {
                    limit: 12,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;
        Ok(page.items.iter().rev().find_map(channel_user_message_preview))
    }
}

fn channel_user_message_preview(row: &MessageRow) -> Option<String> {
    if row.hidden || row.r#type != "text" || row.position.as_deref() != Some("right") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&row.content).ok()?;
    value
        .get("content")
        .or_else(|| value.get("text"))
        .and_then(serde_json::Value::as_str)
        .and_then(short_preview_text)
}

fn short_preview_text(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, 48))
}

fn agent_label_for_channel_conversation(agent_type: &str, backend: Option<&str>) -> String {
    match (agent_type, backend.map(str::trim).filter(|value| !value.is_empty())) {
        ("aionrs", _) => "Aion CLI".to_owned(),
        ("acp", Some("claude")) => "Claude Code".to_owned(),
        ("acp", Some("codex")) => "Codex CLI".to_owned(),
        ("acp", Some("cursor")) => "Cursor".to_owned(),
        ("acp", Some("hermes")) => "Hermes".to_owned(),
        ("acp", Some("openclaw")) => "OpenClaw".to_owned(),
        ("acp", Some(other)) => other.to_owned(),
        ("acp", None) => "ACP Agent".to_owned(),
        (other, _) => other.to_owned(),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_owned();
    }
    let head: String = chars.into_iter().take(max_chars).collect();
    format!("{head}...")
}

fn team_response_to_channel_summary(team: aionui_api_types::TeamResponse) -> ChannelTeamSummary {
    let lead_conversation_id = team
        .assistants
        .iter()
        .find(|agent| {
            agent.role == "lead"
                || team
                    .leader_assistant_id
                    .as_deref()
                    .is_some_and(|leader| leader == agent.slot_id)
        })
        .or_else(|| team.assistants.first())
        .map(|agent| agent.conversation_id.clone());
    ChannelTeamSummary {
        id: team.id,
        name: team.name,
        lead_conversation_id,
        agent_count: team.assistants.len(),
    }
}

#[derive(Debug)]
pub struct RouterBuildError {
    stage: &'static str,
    message: &'static str,
    source: Option<anyhow::Error>,
}

impl RouterBuildError {
    pub fn new(stage: &'static str, message: &'static str) -> Self {
        Self {
            stage,
            message,
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<anyhow::Error>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn stage(&self) -> &'static str {
        self.stage
    }

    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl std::fmt::Display for RouterBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for RouterBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// All module-level router states bundled into a single struct.
///
/// Reduces parameter bloat on router constructors and makes it easy for
/// tests to override individual modules.
pub struct ModuleStates {
    pub system: SystemRouterState,
    pub conversation: ConversationRouterState,
    pub remote_agent: RemoteAgentRouterState,
    pub agent: AgentRouterState,

    pub connection_test: ConnectionTestRouterState,
    pub file: FileRouterState,
    pub mcp: McpRouterState,
    pub extension: ExtensionRouterState,
    pub hub: HubRouterState,
    pub skill: SkillRouterState,
    pub channel: ChannelRouterState,
    pub team: TeamRouterState,
    pub cron: CronRouterState,
    pub project: ProjectRouterState,
    pub development: DevelopmentRouterState,
    pub office: OfficeRouterState,
    pub shell: ShellRouterState,
    pub assistant: AssistantRouterState,
}

fn default_allowed_roots(work_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{label}-temp-{id}/`. On Windows the
    // operator may put `work_dir` on a separate drive (e.g. `X:\AionUi`)
    // that's neither under `temp_dir` nor `home_dir`. Including `work_dir`
    // keeps temp workspaces on the default allowlist without widening it
    // to unrelated paths.
    if let Some(wd) = work_dir
        && !wd.as_os_str().is_empty()
        && !roots.iter().any(|r| r == wd)
    {
        roots.push(wd.to_path_buf());
    }
    roots
}

fn build_module_state_phase<T>(boot: &Instant, phase: &'static str, build: impl FnOnce() -> T) -> T {
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        phase,
        "startup: module state phase started"
    );
    let value = build();
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        phase,
        "startup: module state phase completed"
    );
    value
}

/// Components needed to start the channel orchestrator.
///
/// Returned alongside `ChannelRouterState` by `build_channel_state`.
/// The caller must spawn the orchestrator as a background task.
pub struct ChannelOrchestratorComponents {
    pub orchestrator: aionui_channel::orchestrator::ChannelOrchestrator,
    pub message_rx: tokio::sync::mpsc::Receiver<aionui_channel::types::UnifiedIncomingMessage>,
    pub confirm_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    pub manager: Arc<aionui_channel::manager::ChannelManager>,
    pub plugin_factory: Arc<aionui_channel::manager::PluginFactory>,
}

/// Build all default `ModuleStates` from application services.
pub async fn build_module_states(
    services: &AppServices,
) -> Result<(ModuleStates, ChannelOrchestratorComponents), RouterBuildError> {
    let boot = Instant::now();
    tracing::info!("startup: module state build started");

    let (ext_state, hub_state, mut skill_state) = build_extension_states(services).await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension states built"
    );

    let scan_paths = resolve_scan_paths_for_data_dir(&services.data_dir);
    if let Err(error) = ext_state.registry.initialize_with_scan_paths(scan_paths).await {
        tracing::warn!(
            code = "BOOTSTRAP_DEGRADED_EXTENSION_REGISTRY",
            stage = "extension.registry.initialize",
            error = %error,
            "extension registry initialize failed"
        );
    }
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension registry initialized"
    );

    let assistant = build_assistant_state(services);
    assistant.service.bootstrap_assistant_storage().await.map_err(|error| {
        RouterBuildError::new("router.assistant.bootstrap", "failed to bootstrap assistant storage").with_source(error)
    })?;
    let cron = build_cron_state(services);
    cron.cron_service.init().await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: cron state initialized"
    );

    // The agent catalog already hydrated at startup (see `lib.rs`).
    // Extension-contributed rows will land in `agent_metadata` in a
    // later step; for now we rely on the builtin + internal seed rows.

    let dispatcher: Arc<dyn AssistantRuleDispatcher> = assistant.service.clone();
    skill_state.assistant_dispatcher = Some(dispatcher);

    let backend_binary_path = Arc::new(
        std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("aioncore")),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: backend binary path resolved"
    );

    let team_state = build_module_state_phase(&boot, "team", || {
        build_team_state(services, Some(cron.cron_service.clone()), backend_binary_path.clone())
    });

    let (channel_state, channel_components) =
        build_channel_state(services, ext_state.registry.clone(), Some(team_state.service.clone())).await;
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: channel state built");

    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let agent_service = AgentService::new(
        services.agent_registry.clone(),
        services.event_bus.clone(),
        provider_repo,
        encryption_key,
        services.data_dir.clone(),
    );
    services
        .conversation_service
        .with_agent_availability_feedback(agent_service.availability_feedback_port());
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: agent service built");

    let project = build_module_state_phase(&boot, "project", || {
        build_project_state(services, agent_service.clone())
    });
    let development = build_module_state_phase(&boot, "development", || build_development_state(services));

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module states bundle started"
    );
    let states = ModuleStates {
        system: build_module_state_phase(&boot, "system", || build_system_state(services)),
        conversation: build_module_state_phase(&boot, "conversation", || {
            build_conversation_state(
                services,
                Some(cron.cron_service.clone()),
                Some(assistant.service.clone() as Arc<dyn AssistantRuleDispatcher>),
            )
        }),
        remote_agent: build_module_state_phase(&boot, "remote_agent", || build_remote_agent_state(services)),
        agent: build_module_state_phase(&boot, "agent", || AgentRouterState {
            agent_registry: services.agent_registry.clone(),
            service: agent_service.clone(),
        }),
        connection_test: build_module_state_phase(&boot, "connection_test", build_connection_test_state),
        file: build_module_state_phase(&boot, "file", || build_file_state(services))?,
        mcp: build_module_state_phase(&boot, "mcp", || build_mcp_state(services)),
        extension: ext_state,
        hub: hub_state,
        skill: skill_state,
        channel: channel_state,
        team: team_state,
        cron,
        project,
        development,
        office: build_module_state_phase(&boot, "office", || build_office_state(services)),
        shell: build_module_state_phase(&boot, "shell", || build_shell_state(services)),
        assistant,
    };
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module state build completed"
    );
    states
        .conversation
        .service
        .recover_stale_runtime_state_on_startup()
        .await;
    if let Err(error) = states
        .development
        .operations_service
        .reconcile_stale_runs(30 * 60 * 1000)
        .await
    {
        tracing::warn!(error = %error, "development recovery reconciliation failed");
    }

    Ok((states, channel_components))
}

fn build_project_state(services: &AppServices, agent_service: Arc<AgentService>) -> ProjectRouterState {
    let pool = services.database.pool().clone();
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(pool.clone()));
    let conversation_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(SqliteTeamRepository::new(pool));
    let agent_port: Arc<dyn ProjectAgentCapabilityPort> =
        Arc::new(ProjectAgentCapabilityAdapter { service: agent_service });
    ProjectRouterState {
        service: Arc::new(ProjectService::new(
            project_repo,
            conversation_repo,
            team_repo,
            agent_port,
        )),
    }
}

fn build_development_state(services: &AppServices) -> DevelopmentRouterState {
    let pool = services.database.pool().clone();
    let development_repo: Arc<dyn IDevelopmentRepository> = Arc::new(SqliteDevelopmentRepository::new(pool.clone()));
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(pool.clone()));
    let lease_repo: Arc<dyn IAgentWorkspaceLeaseRepository> =
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(pool.clone()));
    let operations_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(pool));
    let operations_service = Arc::new(DevelopmentOperationsService::new(
        operations_repo,
        development_repo.clone(),
        project_repo.clone(),
        lease_repo.clone(),
    ));
    DevelopmentRouterState {
        service: Arc::new(
            DevelopmentService::new(
                development_repo.clone(),
                project_repo.clone(),
                lease_repo,
                services.data_dir.join("development-artifacts"),
            )
            .with_operations(operations_service.clone()),
        ),
        delivery_service: Arc::new(
            DeliveryService::new(development_repo, project_repo, Arc::new(GhCliDeliveryProvider))
                .with_operations(operations_service.clone()),
        ),
        operations_service,
    }
}

pub fn build_approval_state(services: &AppServices) -> ApprovalRouterState {
    let repository: Arc<dyn IApprovalRepository> =
        Arc::new(SqliteApprovalRepository::new(services.database.pool().clone()));
    let resolver: Arc<dyn ApprovalResolver> = Arc::new(ApprovalAgentResolver {
        task_manager: services.worker_task_manager.clone(),
    });
    ApprovalRouterState {
        service: Arc::new(ApprovalService::new(repository, resolver)),
    }
}

/// Build the default `AssistantRouterState` from application services.
pub fn build_assistant_state(services: &AppServices) -> AssistantRouterState {
    #[derive(Clone)]
    struct RegistryAssistantAgentCatalog {
        registry: Arc<aionui_ai_agent::AgentRegistry>,
    }

    #[async_trait::async_trait]
    impl AssistantAgentCatalogPort for RegistryAssistantAgentCatalog {
        async fn list_management_agents(&self) -> Result<Vec<aionui_api_types::AgentManagementRow>, AssistantError> {
            self.registry.refresh_availability().await;
            Ok(self.registry.list_management_rows().await)
        }
    }

    let pool = services.database.pool().clone();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
        Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let state_repo: Arc<dyn IAssistantOverlayRepository> =
        Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()));
    let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(pool.clone()));
    let override_repo: Arc<dyn IAssistantOverrideRepository> =
        Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()));
    // Used by `AssistantService::resolve_default_agent_type` to infer a
    // working `agent_id` from the configured provider list when
    // the caller does not supply one (ELECTRON-1J1 / 1KV).
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let builtin = Arc::new(BuiltinAssistantRegistry::load());
    // Pin user_data_dir to the runtime-resolved data directory so dev /
    // packaged / multi-instance launches all keep their assistant rule files
    // alongside the matching SQLite database (avoiding the historical bug
    // where dev wrote rules to the release `~/.aionui/` while the db lived
    // under `~/.aionui-dev/`).
    let service = Arc::new(AssistantService::new(
        pool,
        aionui_assistant::service::AssistantServiceDeps {
            definition_repo,
            state_repo,
            preference_repo,
            repo,
            override_repo,
            provider_repo,
            builtin,
            agent_catalog: Some(Arc::new(RegistryAssistantAgentCatalog {
                registry: services.agent_registry.clone(),
            })),
        },
        services.data_dir.clone(),
    ));
    AssistantRouterState { service }
}

/// Build the default `SystemRouterState` from application services.
pub fn build_system_state(services: &AppServices) -> SystemRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let http_client = reqwest::Client::new();

    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(pool.clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(pool))),
        provider_service: ProviderService::new(provider_repo.clone(), encryption_key),
        model_fetch_service: ModelFetchService::new(provider_repo, encryption_key, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, env!("CARGO_PKG_VERSION").to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(services.event_bus.clone()),
    }
}

/// Build the default `ConversationRouterState` from application services.
pub fn build_conversation_state(
    services: &AppServices,
    cron_service: Option<Arc<aionui_cron::service::CronService>>,
    assistant_dispatcher: Option<Arc<dyn AssistantRuleDispatcher>>,
) -> ConversationRouterState {
    let conversation_service = services.conversation_service.clone();
    if let Some(dispatcher) = assistant_dispatcher {
        conversation_service.with_assistant_dispatcher(dispatcher);
    }
    if let Some(cron_service) = cron_service {
        conversation_service.with_delete_hook(cron_service.clone());
        conversation_service.with_cron_service(Some(cron_service));
    }
    ConversationRouterState {
        service: conversation_service,
        task_manager: services.worker_task_manager.clone(),
    }
}

/// Build the default `RemoteAgentRouterState` from application services.
pub fn build_remote_agent_state(services: &AppServices) -> RemoteAgentRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let repo = Arc::new(SqliteRemoteAgentRepository::new(pool));
    RemoteAgentRouterState {
        service: Arc::new(RemoteAgentService::new(repo, encryption_key)),
    }
}

/// Build the default `ConnectionTestRouterState`.
pub fn build_connection_test_state() -> ConnectionTestRouterState {
    ConnectionTestRouterState {
        service: ConnectionTestService::new(reqwest::Client::new()),
    }
}

/// Build the default `FileRouterState` from application services.
pub fn build_file_state(services: &AppServices) -> Result<FileRouterState, RouterBuildError> {
    let broadcaster = services.event_bus.clone();
    let allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));
    let browse_roots = BrowseRoots::new();
    let file_service = Arc::new(FileService::new(broadcaster.clone(), allowed_roots.clone()));
    let watch_service = Arc::new(FileWatchService::new(broadcaster).map_err(file_watch_init_error)?);
    let snapshot_service = Arc::new(SnapshotService::new());
    Ok(FileRouterState {
        file_service,
        watch_service,
        snapshot_service,
        allowed_roots,
        browse_roots,
    })
}

fn file_watch_init_error(error: aionui_file::FileError) -> RouterBuildError {
    RouterBuildError::new("router.file_watch", "failed to initialize file watch service").with_source(error)
}

/// Build the default `McpRouterState` from application services.
pub fn build_mcp_state(services: &AppServices) -> McpRouterState {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn aionui_db::IMcpServerRepository> = Arc::new(aionui_db::SqliteMcpServerRepository::new(pool));

    let adapters: Vec<Arc<dyn McpAgentAdapter>> = vec![
        Arc::new(ClaudeAdapter),
        Arc::new(GeminiAdapter),
        Arc::new(QwenAdapter),
        Arc::new(CodexAdapter),
        Arc::new(CodeBuddyAdapter),
        Arc::new(OpencodeAdapter),
        Arc::new(AionrsAdapter),
        Arc::new(AionuiAdapter::new(repo.clone())),
    ];

    let oauth_token_repo: Arc<dyn aionui_db::IOAuthTokenRepository> = Arc::new(
        aionui_db::SqliteOAuthTokenRepository::new(services.database.pool().clone()),
    );
    let http_client = reqwest::Client::new();

    McpRouterState {
        config_service: McpConfigService::new(repo.clone()),
        sync_service: McpSyncService::new(repo, adapters),
        connection_test_service: McpConnectionTestService::new(http_client.clone(), services.event_bus.clone()),
        oauth_service: aionui_mcp::McpOAuthService::new(oauth_token_repo, http_client),
    }
}

fn build_channel_settings_service(
    services: &AppServices,
) -> Arc<aionui_channel::channel_settings::ChannelSettingsService> {
    let pref_repo: Arc<dyn aionui_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone()));

    Arc::new(
        aionui_channel::channel_settings::ChannelSettingsService::new(pref_repo)
            .with_agent_metadata_repo(Arc::new(SqliteAgentMetadataRepository::new(
                services.database.pool().clone(),
            )))
            .with_provider_repo(Arc::new(SqliteProviderRepository::new(
                services.database.pool().clone(),
            )))
            .with_assistant_repos(
                Arc::new(SqliteAssistantDefinitionRepository::new(
                    services.database.pool().clone(),
                )),
                Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
            ),
    )
}

async fn build_channel_message_service(
    services: &AppServices,
    channel_settings: Arc<aionui_channel::channel_settings::ChannelSettingsService>,
    team_service: Option<Arc<TeamSessionService>>,
) -> Arc<aionui_channel::message_service::ChannelMessageService> {
    let owner_user_id = get_channel_owner_user_id(services).await;

    let pool = services.database.pool().clone();
    let mut service = aionui_channel::message_service::ChannelMessageService::new(
        Arc::new(services.conversation_service.clone()),
        services.worker_task_manager.clone(),
        channel_settings,
        owner_user_id,
    )
    .with_conversation_repo(Arc::new(SqliteConversationRepository::new(pool)));

    if let Some(team_service) = &team_service {
        service = service.with_team_sender(Arc::new(ChannelTeamSenderAdapter {
            service: Arc::clone(team_service),
        }));
    }

    Arc::new(service)
}

async fn get_channel_owner_user_id(services: &AppServices) -> String {
    services
        .user_repo
        .get_primary_webui_user()
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
        .unwrap_or_else(|| "system_default_user".to_string())
}

/// Build the default `ChannelRouterState` and orchestrator components.
pub async fn build_channel_state(
    services: &AppServices,
    extension_registry: ExtensionRegistry,
    team_service: Option<Arc<TeamSessionService>>,
) -> (ChannelRouterState, ChannelOrchestratorComponents) {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn aionui_db::IChannelRepository> = Arc::new(aionui_db::SqliteChannelRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);

    let (message_tx, message_rx) = tokio::sync::mpsc::channel(256);
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::channel(256);

    let manager = Arc::new(aionui_channel::manager::ChannelManager::new(
        repo.clone(),
        services.event_bus.clone(),
        encryption_key,
        message_tx,
        confirm_tx,
    ));

    let pairing_service = Arc::new(aionui_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
    ));

    let session_manager = Arc::new(aionui_channel::session::SessionManager::new(repo.clone()));

    let plugin_factory: Arc<aionui_channel::manager::PluginFactory> =
        Arc::new(Box::new(aionui_channel::plugins::create_plugin));

    // Build channel settings service for per-plugin agent/model configuration.
    let channel_settings = build_channel_settings_service(services);
    let owner_user_id = get_channel_owner_user_id(services).await;
    let project_repo: Arc<dyn IProjectRepository> =
        Arc::new(SqliteProjectRepository::new(services.database.pool().clone()));
    let development_state = build_development_state(services);
    let approval_port: Arc<dyn ChannelApprovalPort> = Arc::new(ChannelApprovalAdapter {
        service: build_approval_state(services).service,
        owner_user_id: owner_user_id.clone(),
        project_repo: project_repo.clone(),
        development_service: development_state.service.clone(),
    });
    let development_port: Arc<dyn ChannelDevelopmentPort> = Arc::new(ChannelDevelopmentAdapter {
        owner_user_id: owner_user_id.clone(),
        project_repo,
        development_repo: Arc::new(SqliteDevelopmentRepository::new(services.database.pool().clone())),
        approval_repo: Arc::new(SqliteApprovalRepository::new(services.database.pool().clone())),
        service: development_state.service,
    });

    // Build orchestrator dependencies
    let mut action_executor = aionui_channel::action::ActionExecutor::new(
        Arc::clone(&pairing_service),
        Arc::clone(&session_manager),
        Arc::clone(&channel_settings),
    )
    .with_approval_port(approval_port.clone())
    .with_development_port(development_port);
    if let Some(team_service) = &team_service {
        let team_directory = Arc::new(ChannelTeamDirectoryAdapter {
            service: Arc::clone(team_service),
            owner_user_id: owner_user_id.clone(),
        }) as Arc<dyn ChannelTeamDirectory>;
        action_executor = action_executor.with_team_directory(team_directory);
    }
    let personal_directory = Arc::new(ChannelPersonalDirectoryAdapter {
        conversation_repo: services.conversation_repo.clone(),
        owner_user_id: owner_user_id.clone(),
    }) as Arc<dyn ChannelPersonalDirectory>;
    action_executor = action_executor.with_personal_directory(personal_directory);
    let action_executor = Arc::new(action_executor);

    let message_service = build_channel_message_service(services, Arc::clone(&channel_settings), team_service).await;

    let team_event_relay = aionui_channel::team_event_relay::ChannelTeamEventRelay::new(
        services.event_bus.subscribe(),
        repo.clone(),
        services.conversation_repo.clone(),
        manager.clone() as Arc<dyn aionui_channel::stream_relay::ChannelSender>,
    );
    tokio::spawn(team_event_relay.run());

    let orchestrator = aionui_channel::orchestrator::ChannelOrchestrator::new(
        action_executor,
        message_service,
        Arc::clone(&session_manager),
        manager.clone() as Arc<dyn aionui_channel::stream_relay::ChannelSender>,
    )
    .with_approval_port(approval_port);

    let state = ChannelRouterState {
        manager: Arc::clone(&manager),
        pairing_service,
        session_manager,
        repo,
        plugin_factory: Arc::clone(&plugin_factory),
        settings_service: channel_settings,
        extension_registry,
    };

    let components = ChannelOrchestratorComponents {
        orchestrator,
        message_rx,
        confirm_rx,
        manager,
        plugin_factory,
    };

    (state, components)
}

struct ChannelTeamSenderAdapter {
    service: Arc<TeamSessionService>,
}

#[async_trait::async_trait]
impl ChannelTeamSender for ChannelTeamSenderAdapter {
    async fn send_team_lead_message(&self, user_id: &str, team_id: &str, content: &str) -> Result<(), ChannelError> {
        self.service
            .send_message(user_id, team_id, content, None)
            .await
            .map(|_| ())
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))
    }

    async fn send_team_agent_message(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        content: &str,
    ) -> Result<(), ChannelError> {
        self.service
            .send_message_to_agent(user_id, team_id, slot_id, content, None)
            .await
            .map(|_| ())
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))
    }
}

/// Build the default `TeamRouterState` from application services.
///
/// `backend_binary_path` is resolved once in `build_module_states` via
/// `std::env::current_exe()` and cloned into each builder that needs it,
/// per `docs/teams/phase1/interface-contracts.md` §10.
pub fn build_team_state(
    services: &AppServices,
    _cron_service: Option<Arc<aionui_cron::service::CronService>>,
    backend_binary_path: Arc<std::path::PathBuf>,
) -> TeamRouterState {
    let pool = services.database.pool().clone();
    let team_repo: Arc<dyn aionui_db::ITeamRepository> = Arc::new(aionui_db::SqliteTeamRepository::new(pool.clone()));
    let conv_service = services.conversation_service.clone();
    let conv_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool));
    let adapters = Arc::new(TeamConversationAdapters::new(
        conv_service,
        conv_repo,
        services.worker_task_manager.clone(),
    ));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = adapters.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = adapters.clone();
    let turn_port: Arc<dyn AgentTurnExecutionPort> = adapters.clone();
    let cancellation_port: Arc<dyn AgentTurnCancellationPort> = adapters;
    let workspace_manager: Arc<dyn aionui_team::TeamWorkspaceManager> =
        Arc::new(aionui_team::GitTeamWorkspaceManager::new(
            Arc::new(aionui_db::SqliteAgentWorkspaceLeaseRepository::new(
                services.database.pool().clone(),
            )),
            services.data_dir.join("team-worktrees"),
        ));
    let service = TeamSessionService::new_with_workspace_manager(
        team_repo,
        Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone())),
        Arc::new(SqliteAssistantDefinitionRepository::new(
            services.database.pool().clone(),
        )),
        Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        conversation_port,
        projection_store,
        services.event_bus.clone(),
        services.worker_task_manager.clone(),
        turn_port,
        cancellation_port,
        backend_binary_path,
        workspace_manager,
    );
    TeamRouterState { service }
}

/// Build the default `CronRouterState` from application services.
pub fn build_cron_state(services: &AppServices) -> CronRouterState {
    let pool = services.database.pool().clone();
    let cron_repo: Arc<dyn aionui_db::ICronRepository> = Arc::new(aionui_db::SqliteCronRepository::new(pool.clone()));

    let conv_repo: Arc<dyn aionui_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let skill_resolver = Arc::new(aionui_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
        services.skill_repo.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo.clone(),
        agent_metadata_repo.clone(),
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(aionui_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_definition_repo(Arc::new(SqliteAssistantDefinitionRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_state_repo(Arc::new(SqliteAssistantOverlayRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_preference_repo(Arc::new(SqliteAssistantPreferenceRepository::new(
        services.database.pool().clone(),
    )));

    let executor = Arc::new(aionui_cron::executor::JobExecutor::new(
        services.worker_task_manager.clone(),
        conv_repo,
        Arc::new(conv_service.clone()),
        services.work_dir.clone(),
        services.data_dir.clone(),
        services.event_bus.clone(),
        services.agent_registry.clone(),
    ));

    let tick_service_ref: Arc<CronServiceTickRef> = Arc::new(CronServiceTickRef::default());
    let tick_ref = tick_service_ref.clone();
    let scheduler = Arc::new(aionui_cron::scheduler::CronScheduler::new(Arc::new(
        move |job_id: String| {
            let svc = tick_ref.0.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Some(svc) = svc {
                    svc.tick(&job_id).await;
                }
            });
        },
    )));

    let emitter = CronEventEmitter::new(services.event_bus.clone());
    let assistant_definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(
        services.database.pool().clone(),
    ));
    let assistant_overlay_repo = Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone()));
    let cron_service = Arc::new(aionui_cron::service::CronService::new(CronServiceDeps {
        repo: cron_repo,
        agent_metadata_repo,
        assistant_definition_repo,
        assistant_overlay_repo,
        scheduler,
        executor,
        emitter,
        data_dir: services.data_dir.clone(),
    }));

    tick_service_ref.0.lock().unwrap().replace(cron_service.clone());

    CronRouterState {
        cron_service,
        conversation_service: conv_service,
    }
}

/// Build the default `OfficeRouterState` from application services.
pub fn build_office_state(services: &AppServices) -> OfficeRouterState {
    let data_dir = services.data_dir.as_path();
    let allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));

    let spawner: Arc<dyn aionui_office::ProcessSpawner> =
        Arc::new(aionui_office::DefaultProcessSpawner::new(data_dir.to_path_buf()));
    let watch_manager = Arc::new(OfficecliWatchManager::new(spawner, services.event_bus.clone()));

    let snapshot_service = Arc::new(OfficeSnapshotService::new(data_dir));
    let conversion_service = Arc::new(ConversionService::with_data_dir(None, data_dir.to_path_buf()));
    let proxy_service = Arc::new(ProxyService::new(watch_manager.clone()));

    OfficeRouterState {
        watch_manager,
        snapshot_service,
        conversion_service,
        proxy_service,
        allowed_roots,
    }
}

/// Build the default `ShellRouterState` from application services.
pub fn build_shell_state(services: &AppServices) -> ShellRouterState {
    let pool = services.database.pool().clone();
    let client_pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool));
    let client_pref_service = ClientPrefService::new(client_pref_repo);

    ShellRouterState {
        shell_service: Arc::new(aionui_shell::ShellService::new(Arc::new(
            aionui_shell::DefaultSystemOpener,
        ))),
        stt_service: Arc::new(aionui_shell::SttService::new(reqwest::Client::new())),
        client_pref_service,
    }
}

/// Helper to break the circular reference between CronScheduler and CronService.
#[derive(Default)]
struct CronServiceTickRef(std::sync::Mutex<Option<Arc<aionui_cron::service::CronService>>>);

/// Build the default extension-related router states.
///
/// Returns `(ExtensionRouterState, HubRouterState, SkillRouterState)`.
pub async fn build_extension_states(
    services: &AppServices,
) -> (ExtensionRouterState, HubRouterState, SkillRouterState) {
    let skill_data_dir = services.data_dir.clone();

    let state_store = ExtensionStateStore::new(resolve_state_file_path(&skill_data_dir));
    let registry = ExtensionRegistry::new(state_store, services.event_bus.clone(), services.app_version.clone());

    let hub_dir = resolve_install_target_dir_for_data_dir(&skill_data_dir);
    let index_manager = HubIndexManager::new(hub_dir, registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());

    let ext_paths_mgr = Arc::new(ExternalPathsManager::new(&skill_data_dir).await);

    let ext_state = ExtensionRouterState {
        registry: registry.clone(),
    };

    let hub_state = HubRouterState {
        index_manager,
        installer,
    };

    let skill_state = SkillRouterState {
        skill_paths: services.skill_paths.as_ref().clone(),
        skill_repo: services.skill_repo.clone(),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None,
    };

    (ext_state, hub_state, skill_state)
}

/// Build the default `WsHandlerState` from application services.
pub fn build_ws_state(services: &AppServices) -> WsHandlerState {
    if services.local {
        return WsHandlerState {
            manager: services.ws_manager.clone(),
            router: Arc::new(NoopMessageRouter),
            token_validator: Arc::new(|_| true),
            token_extractor: Arc::new(|_| Some("local".into())),
        };
    }

    let jwt_service = services.jwt_service.clone();
    let token_validator = Arc::new(move |token: &str| jwt_service.verify(token).is_ok());

    let token_extractor = Arc::new(|headers: &axum::http::HeaderMap| extract_token_from_ws_headers(headers));

    WsHandlerState {
        manager: services.ws_manager.clone(),
        router: Arc::new(NoopMessageRouter),
        token_validator,
        token_extractor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::AppConfig;
    use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
    use aionui_ai_agent::{
        AgentError, AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent, IWorkerTaskManager,
        WorkerTaskManagerImpl,
    };
    use aionui_channel::types::PluginType;
    use aionui_common::{AgentKillReason, AgentType, ConversationStatus, TimestampMs};
    use aionui_db::models::{AssistantSessionRow, UpsertAssistantDefinitionParams};
    use aionui_db::{
        IAssistantDefinitionRepository, IClientPreferenceRepository, IConversationRepository,
        SqliteAssistantDefinitionRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    };
    use aionui_extension::{ExtensionSource, ScanPath};

    struct ChannelStateNoopAgent {
        conversation_id: String,
        workspace: String,
    }

    #[async_trait::async_trait]
    impl IAgentTask for ChannelStateNoopAgent {
        fn agent_type(&self) -> AgentType {
            AgentType::Aionrs
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            &self.workspace
        }

        fn status(&self) -> Option<ConversationStatus> {
            Some(ConversationStatus::Finished)
        }

        fn last_activity_at(&self) -> TimestampMs {
            0
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentStreamEvent> {
            let (tx, _) = tokio::sync::broadcast::channel(1);
            tx.subscribe()
        }

        async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
            Ok(())
        }

        async fn cancel(&self) -> Result<(), AgentError> {
            Ok(())
        }

        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            Ok(())
        }
    }

    impl IMockAgent for ChannelStateNoopAgent {}

    fn mock_worker_task_manager() -> Arc<dyn IWorkerTaskManager> {
        let factory = Arc::new(|opts: BuildTaskOptions| {
            Box::pin(async move {
                Ok(AgentInstance::Mock(Arc::new(ChannelStateNoopAgent {
                    conversation_id: opts.conversation_id().to_owned(),
                    workspace: opts.context.workspace.path,
                })))
            }) as futures_util::future::BoxFuture<'static, Result<AgentInstance, AgentError>>
        });

        Arc::new(WorkerTaskManagerImpl::new(factory))
    }

    fn channel_state_assistant_definition() -> UpsertAssistantDefinitionParams<'static> {
        UpsertAssistantDefinitionParams {
            id: "asstdef-channel-state-aionrs",
            assistant_id: "bare-channel-aionrs",
            source: "generated",
            owner_type: "system",
            source_ref: Some("bare-channel-aionrs"),
            source_version: None,
            source_hash: None,
            name: "Bare Channel Aionrs",
            name_i18n: "{}",
            description: Some("Channel state regression assistant"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("A"),
            agent_id: "632f31d2",
            rule_resource_type: "inline",
            rule_resource_ref: None,
            rule_inline_content: Some(""),
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "auto",
            default_skill_ids: "[]",
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        }
    }

    #[tokio::test]
    async fn build_channel_message_service_uses_app_conversation_service_for_assistant_bindings() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default())
            .await
            .unwrap()
            .with_worker_task_manager(mock_worker_task_manager());

        let pool = services.database.pool().clone();
        let definition_repo = SqliteAssistantDefinitionRepository::new(pool.clone());
        definition_repo
            .upsert(&channel_state_assistant_definition())
            .await
            .unwrap();

        let pref_repo = SqliteClientPreferenceRepository::new(pool.clone());
        pref_repo
            .upsert_batch(&[
                (
                    "assistant.weixin.agent",
                    r#"{"assistant_id":"bare-channel-aionrs","name":"Weixin Aionrs"}"#,
                ),
                (
                    "assistant.weixin.defaultModel",
                    r#"{"id":"test-provider","use_model":"test-model"}"#,
                ),
            ])
            .await
            .unwrap();

        let settings = build_channel_settings_service(&services);
        let message_service = build_channel_message_service(&services, settings, None).await;
        let session = AssistantSessionRow {
            id: "session-channel-state".to_owned(),
            user_id: "channel-user-state".to_owned(),
            agent_type: "aionrs".to_owned(),
            conversation_id: None,
            workspace: None,
            chat_id: Some("wx-chat-state".to_owned()),
            message_thread_id: None,
            bound_agent_id: None,
            bound_backend: None,
            bound_provider_id: None,
            bound_model: None,
            created_at: 1,
            last_activity: 1,
        };

        let first = message_service
            .send_to_agent(&session, "hello", PluginType::Weixin)
            .await
            .unwrap();

        let conversation_repo = SqliteConversationRepository::new(pool);
        let snapshot = conversation_repo
            .get_assistant_snapshot(&first.conversation_id)
            .await
            .unwrap()
            .expect("channel-created conversation should persist assistant snapshot");
        let conversation = conversation_repo
            .get(&first.conversation_id)
            .await
            .unwrap()
            .expect("channel-created conversation should be persisted");

        assert_eq!(snapshot.assistant_id, "bare-channel-aionrs");
        assert_eq!(snapshot.agent_id, "632f31d2");
        assert_eq!(conversation.r#type, AgentType::Aionrs.serde_name());
        assert_eq!(conversation.name, "Weixin Aionrs");

        let second_session = AssistantSessionRow {
            conversation_id: Some(first.conversation_id.clone()),
            ..session
        };
        let second = message_service
            .send_to_agent(&second_session, "again", PluginType::Weixin)
            .await
            .unwrap();
        assert_eq!(second.conversation_id, first.conversation_id);

        services.database.close().await;
    }

    #[tokio::test]
    async fn build_extension_states_uses_host_app_version_for_engine_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("demo-ext");

        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "demo-ext",
                "version": "1.0.0",
                "engine": {
                    "aionui": "^2.0.0"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let db = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            data_dir: data_dir.clone(),
            work_dir: data_dir,
            app_version: "2.1.0".to_string(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        let (ext_state, _hub_state, _skill_state) = build_extension_states(&services).await;
        ext_state
            .registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();

        let loaded = ext_state.registry.get_loaded_extensions().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo-ext");

        services.database.close().await;
    }

    #[tokio::test]
    async fn channel_development_adapter_reports_bound_project_and_run() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
        project_repo
            .create(&aionui_db::models::ProjectRow {
                id: "project-channel".into(),
                user_id: "system_default_user".into(),
                name: "Aion".into(),
                local_path: project_dir.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: Some("main".into()),
                project_type: "single".into(),
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        project_repo
            .bind_resource(
                "project-channel",
                "system_default_user",
                "conversation",
                "conversation-channel",
            )
            .await
            .unwrap();
        let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
        development_repo
            .create_run(&aionui_db::models::DevelopmentRunRow {
                id: "run-channel".into(),
                user_id: "system_default_user".into(),
                project_id: "project-channel".into(),
                team_id: None,
                source_channel: Some("telegram".into()),
                source_user_id: Some("assistant-user".into()),
                execution_mode: "single".into(),
                status: "running".into(),
                request_summary: "Implement approvals".into(),
                acceptance_criteria: "[]".into(),
                baseline_commit: None,
                integration_branch: None,
                started_at: Some(1),
                finished_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        let service = Arc::new(DevelopmentService::new(
            development_repo.clone(),
            project_repo.clone(),
            Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
            project_dir.path().join("artifacts"),
        ));
        let adapter = ChannelDevelopmentAdapter {
            owner_user_id: "system_default_user".into(),
            project_repo,
            development_repo,
            approval_repo: Arc::new(SqliteApprovalRepository::new(db.pool().clone())),
            service,
        };
        let context = ChannelDevelopmentContext {
            source_user_id: "assistant-user".into(),
            conversation_id: Some("conversation-channel".into()),
            platform: PluginType::Telegram,
            chat_id: "chat".into(),
            message_thread_id: Some(5),
        };

        let project = adapter
            .execute(context.clone(), ChannelDevelopmentCommand::Project)
            .await
            .unwrap();
        assert!(project.contains("项目：Aion"));
        assert!(project.contains("当前运行：running"));
        let run = adapter
            .execute(context.clone(), ChannelDevelopmentCommand::RunInfo)
            .await
            .unwrap();
        assert!(run.contains("运行：run-channel"));
        let handoff = adapter
            .execute(context, ChannelDevelopmentCommand::Handoff)
            .await
            .unwrap();
        assert!(handoff.contains("/#/projects?projectId=project-channel&runId=run-channel"));
    }

    #[tokio::test]
    async fn channel_approval_adapter_links_bound_project_and_active_run() {
        struct NoopApprovalResolver;

        #[async_trait::async_trait]
        impl ApprovalResolver for NoopApprovalResolver {
            async fn resolve(
                &self,
                _conversation_id: &str,
                _call_id: &str,
                _value: serde_json::Value,
                _always_allow: bool,
            ) -> Result<(), String> {
                Ok(())
            }
        }

        let db = aionui_db::init_database_memory().await.unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        sqlx::query(
            "INSERT INTO conversations \
             (id, user_id, name, type, extra, pinned, created_at, updated_at) \
             VALUES ('conversation-approval', 'system_default_user', 'Approval', 'acp', '{}', 0, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
        project_repo
            .create(&aionui_db::models::ProjectRow {
                id: "project-approval".into(),
                user_id: "system_default_user".into(),
                name: "Aion".into(),
                local_path: project_dir.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: Some("main".into()),
                project_type: "single".into(),
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        project_repo
            .bind_resource(
                "project-approval",
                "system_default_user",
                "conversation",
                "conversation-approval",
            )
            .await
            .unwrap();
        let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
        development_repo
            .create_run(&aionui_db::models::DevelopmentRunRow {
                id: "run-approval".into(),
                user_id: "system_default_user".into(),
                project_id: "project-approval".into(),
                team_id: None,
                source_channel: Some("telegram".into()),
                source_user_id: Some("telegram-user".into()),
                execution_mode: "single".into(),
                status: "running".into(),
                request_summary: "Exercise remote approval".into(),
                acceptance_criteria: "[]".into(),
                baseline_commit: None,
                integration_branch: None,
                started_at: Some(1),
                finished_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        let approval_repo = Arc::new(SqliteApprovalRepository::new(db.pool().clone()));
        let development_service = Arc::new(DevelopmentService::new(
            development_repo,
            project_repo.clone(),
            Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
            project_dir.path().join("artifacts"),
        ));
        let adapter = ChannelApprovalAdapter {
            service: Arc::new(ApprovalService::new(
                approval_repo.clone(),
                Arc::new(NoopApprovalResolver),
            )),
            owner_user_id: "system_default_user".into(),
            project_repo,
            development_service,
        };

        let approval_id = adapter
            .create(
                ChannelApprovalContext {
                    source_user_id: "telegram-user".into(),
                    conversation_id: "conversation-approval".into(),
                    agent_id: Some("codex".into()),
                    platform: PluginType::Telegram,
                    chat_id: "chat".into(),
                    message_thread_id: Some(3),
                },
                aionui_common::Confirmation {
                    id: "confirmation-1".into(),
                    call_id: "call-1".into(),
                    title: Some("Run tests?".into()),
                    action: None,
                    description: "cargo test".into(),
                    command_type: Some("execute".into()),
                    options: vec![aionui_common::ConfirmationOption {
                        label: "Allow once".into(),
                        value: serde_json::json!(true),
                        params: None,
                    }],
                },
            )
            .await
            .unwrap();

        let row = approval_repo.get(&approval_id).await.unwrap().unwrap();
        assert_eq!(row.project_id.as_deref(), Some("project-approval"));
        assert_eq!(row.run_id.as_deref(), Some("run-approval"));
    }

    #[test]
    fn file_watch_init_error_maps_to_bootstrap_server_failed() {
        let err = file_watch_init_error(aionui_file::FileError::Internal("watch backend unavailable".into()));

        assert_eq!(err.stage(), "router.file_watch");
        assert_eq!(err.message(), "failed to initialize file watch service");
        assert!(!err.to_string().contains("watch backend unavailable"));
    }
}
