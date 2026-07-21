use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    AddAgentRequest, CreateTeamRequest, GetConfigOptionsResponse, TeamAgentInput, TeamToolTransport,
};
use aionui_common::{AgentKillReason, AgentType, ConversationSource, ProviderWithModel, generate_id, now_ms};
use aionui_db::models::{AgentMetadataRow, TeamRow};
use aionui_db::{
    IAgentMetadataRepository, IProviderRepository, ITeamRepository, UpdateTeamParams, resolve_agent_binding_from_rows,
};
use async_trait::async_trait;
use tracing::{info, warn};

use crate::capability::{supports_team_cli_fallback_backend, supports_team_mcp_backend};
use crate::error::TeamError;
use crate::mcp::TeamMcpStdioConfig;
use crate::ports::TeamAssistantCatalogPort;
use crate::ports::TeamConversationBindingLookup;
use crate::service::inherit_team_workspace;
use crate::service::spawn_support::{acp_backend_metadata, parse_agent_type, session_mode_for_backend};
use crate::types::{Team, TeamAgent, TeammateRole};
use crate::workspace::TeamWorkspaceResolver;
use crate::{PreparedTeamWorkspaces, TeamWorkspaceManager, WorkspaceAgentSpec};

#[derive(Clone)]
pub struct TeamAgentProvisioner {
    repo: Arc<dyn ITeamRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
    provider_repo: Arc<dyn IProviderRepository>,
    conversation_port: Arc<dyn TeamConversationProvisioningPort>,
    workspace_manager: Option<Arc<dyn TeamWorkspaceManager>>,
}

pub(crate) struct InitialProvisioningResult {
    pub agents: Vec<TeamAgent>,
    pub lead_agent_id: Option<String>,
    pub team_workspace: String,
}

struct ProvisionedConversation {
    conversation_id: String,
    workspace: Option<String>,
}

struct NewAgentProvisioning {
    user_id: String,
    team_id: String,
    slot_id: String,
    name: String,
    role: TeammateRole,
    backend: String,
    model: String,
    assistant_id: Option<String>,
    workspace: Option<String>,
    session_mode: Option<String>,
}

pub(crate) struct PersistSpawnedAgentRequest {
    pub user_id: String,
    pub team_id: String,
    pub slot_id: String,
    pub name: String,
    pub backend: String,
    pub model: String,
    pub assistant_id: Option<String>,
}

pub struct TeamConversationCreateRequest {
    pub user_id: String,
    pub agent_type: Option<AgentType>,
    pub name: String,
    pub top_level_model: Option<ProviderWithModel>,
    pub assistant_id: Option<String>,
    pub source: Option<ConversationSource>,
    pub channel_chat_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_channel_id: Option<String>,
    pub source_chat_id: Option<String>,
    pub source_user_id: Option<String>,
    pub source_label: Option<String>,
    pub created_from: Option<String>,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamSourceMetadata {
    pub source_channel: Option<String>,
    pub source_channel_id: Option<String>,
    pub source_chat_id: Option<String>,
    pub source_user_id: Option<String>,
    pub source_label: Option<String>,
    pub created_from: Option<String>,
}

impl TeamSourceMetadata {
    pub fn from_create_request(req: &CreateTeamRequest) -> Self {
        Self {
            source_channel: clean_source(req.source_channel.as_deref()).or_else(|| Some("webui".into())),
            source_channel_id: clean_source(req.source_channel_id.as_deref()),
            source_chat_id: clean_source(req.source_chat_id.as_deref()),
            source_user_id: clean_source(req.source_user_id.as_deref()),
            source_label: clean_source(req.source_label.as_deref()).or_else(|| Some("WebUI".into())),
            created_from: clean_source(req.created_from.as_deref()).or_else(|| Some("webui".into())),
        }
    }

    pub fn top_level_source(&self) -> ConversationSource {
        match self.source_channel.as_deref() {
            Some("telegram") => ConversationSource::Telegram,
            Some("lark") => ConversationSource::Lark,
            Some("dingtalk") => ConversationSource::Dingtalk,
            Some("weixin") | Some("wecom") => ConversationSource::Weixin,
            _ => ConversationSource::Aionui,
        }
    }

    pub fn apply_to_extra(&self, extra: &mut serde_json::Value) {
        set_extra_string(extra, "source_channel", self.source_channel.as_deref());
        set_extra_string(extra, "source_channel_id", self.source_channel_id.as_deref());
        set_extra_string(extra, "source_chat_id", self.source_chat_id.as_deref());
        set_extra_string(extra, "source_user_id", self.source_user_id.as_deref());
        set_extra_string(extra, "source_label", self.source_label.as_deref());
        set_extra_string(extra, "created_from", self.created_from.as_deref());
    }
}

#[cfg(test)]
mod source_metadata_tests {
    use super::*;

    #[test]
    fn team_source_metadata_applies_telegram_source_to_extra() {
        let req = CreateTeamRequest {
            name: "Telegram Team".into(),
            agents: vec![],
            workspace: None,
            source_channel: Some("telegram".into()),
            source_channel_id: Some("bot-1".into()),
            source_chat_id: Some("chat-1".into()),
            source_user_id: Some("user-1".into()),
            source_label: Some("Telegram".into()),
            created_from: Some("telegram".into()),
        };
        let metadata = TeamSourceMetadata::from_create_request(&req);
        let mut extra = serde_json::json!({});

        metadata.apply_to_extra(&mut extra);

        assert_eq!(extra["source_channel"], "telegram");
        assert_eq!(extra["source_channel_id"], "bot-1");
        assert_eq!(extra["source_chat_id"], "chat-1");
        assert_eq!(extra["source_user_id"], "user-1");
        assert_eq!(extra["source_label"], "Telegram");
        assert_eq!(extra["created_from"], "telegram");
        assert_eq!(metadata.top_level_source(), ConversationSource::Telegram);
    }

    #[test]
    fn formal_team_preflight_rejects_stale_dynamic_probe() {
        let now = aionui_common::now_ms();
        let raw = serde_json::json!({
            "agent_id": "codex",
            "checked_at": now - 25 * 60 * 60 * 1000,
            "available_models": ["gpt-5"],
            "steps": [
                {"step":"spawn","status":"passed","started_at":now,"duration_ms":1},
                {"step":"initialize","status":"passed","started_at":now,"duration_ms":1},
                {"step":"models","status":"passed","started_at":now,"duration_ms":1},
                {"step":"minimal_prompt","status":"passed","started_at":now,"duration_ms":1},
                {"step":"cancel","status":"passed","started_at":now,"duration_ms":1},
                {"step":"resume","status":"unsupported","started_at":now,"duration_ms":1,
                 "error_category":"protocol","error_message":"unsupported"}
            ]
        })
        .to_string();

        let error = validate_dynamic_probe_result(&raw, "gpt-5", now).unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[test]
    fn formal_team_preflight_rejects_unobserved_selected_model() {
        let now = aionui_common::now_ms();
        let mut result = healthy_probe("codex", now);
        result.available_models = vec!["gpt-5".into()];
        let raw = serde_json::to_string(&result).unwrap();

        let error = validate_dynamic_probe_result(&raw, "gpt-5.6", now).unwrap_err();
        assert!(error.to_string().contains("was not observed"));
    }

    #[test]
    fn formal_team_preflight_rejects_latest_failed_check_even_with_prior_success() {
        let now = aionui_common::now_ms();
        let raw = serde_json::to_string(&healthy_probe("codex", now)).unwrap();

        let error = validate_agent_dynamic_preflight(Some("offline"), Some(&raw), "gpt-5", now).unwrap_err();

        assert!(error.to_string().contains("latest health check failed"));
    }

    fn healthy_probe(agent_id: &str, checked_at: i64) -> aionui_api_types::AgentDynamicProbeResult {
        use aionui_api_types::{AgentProbeStatus, AgentProbeStep, AgentProbeStepResult};

        aionui_api_types::AgentDynamicProbeResult {
            agent_id: agent_id.into(),
            checked_at,
            available_models: vec!["gpt-5".into()],
            steps: [
                AgentProbeStep::Spawn,
                AgentProbeStep::Initialize,
                AgentProbeStep::Models,
                AgentProbeStep::MinimalPrompt,
                AgentProbeStep::Cancel,
                AgentProbeStep::Resume,
            ]
            .into_iter()
            .map(|step| AgentProbeStepResult {
                step,
                status: if step == AgentProbeStep::Resume {
                    AgentProbeStatus::Unsupported
                } else {
                    AgentProbeStatus::Passed
                },
                started_at: checked_at,
                duration_ms: 1,
                error_category: None,
                error_message: None,
            })
            .collect(),
        }
    }
}

fn clean_source(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn set_extra_string(extra: &mut serde_json::Value, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        extra[key] = serde_json::Value::String(value.to_owned());
    }
}

const DYNAMIC_PROBE_STALE_MS: i64 = 24 * 60 * 60 * 1000;

fn validate_dynamic_probe_result(raw: &str, selected_model: &str, checked_at: i64) -> Result<(), TeamError> {
    let probe: aionui_api_types::AgentDynamicProbeResult = serde_json::from_str(raw).map_err(|_| {
        TeamError::InvalidRequest("Agent dynamic probe snapshot is invalid; run health check again".into())
    })?;
    if checked_at.saturating_sub(probe.checked_at) > DYNAMIC_PROBE_STALE_MS {
        return Err(TeamError::InvalidRequest(format!(
            "Agent '{}' dynamic probe is stale; run health check again",
            probe.agent_id
        )));
    }
    if !probe.is_usable() {
        return Err(TeamError::InvalidRequest(format!(
            "Agent '{}' failed dynamic capability preflight",
            probe.agent_id
        )));
    }
    let selected_model = selected_model.trim();
    if !selected_model.is_empty()
        && !selected_model.eq_ignore_ascii_case("default")
        && !probe.available_models.iter().any(|model| model == selected_model)
    {
        return Err(TeamError::InvalidRequest(format!(
            "Model '{selected_model}' was not observed during Agent '{}' dynamic probe",
            probe.agent_id
        )));
    }
    Ok(())
}

fn validate_agent_dynamic_preflight(
    last_check_status: Option<&str>,
    dynamic_probe_result: Option<&str>,
    selected_model: &str,
    checked_at: i64,
) -> Result<(), TeamError> {
    if !last_check_status.is_some_and(|status| matches!(status, "online" | "available")) {
        return Err(TeamError::InvalidRequest(
            "Agent latest health check failed; run health check again before creating a Team".into(),
        ));
    }
    let raw = dynamic_probe_result.ok_or_else(|| {
        TeamError::InvalidRequest(
            "Agent has no successful dynamic probe; run health check before creating a Team".into(),
        )
    })?;
    validate_dynamic_probe_result(raw, selected_model, checked_at)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConversationCreateResult {
    pub conversation_id: String,
    pub workspace: String,
}

#[async_trait]
pub trait TeamConversationProvisioningPort: Send + Sync {
    async fn create_team_conversation(
        &self,
        request: TeamConversationCreateRequest,
    ) -> Result<TeamConversationCreateResult, TeamError>;

    async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, TeamError>;

    async fn conversation_assistant_id(&self, conversation_id: &str) -> Result<Option<String>, TeamError>;

    async fn conversation_source_metadata(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<TeamSourceMetadata>, TeamError> {
        Ok(None)
    }

    async fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, TeamError>;

    async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError>;

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError>;

    async fn get_config_options(&self, conversation_id: &str) -> Result<GetConfigOptionsResponse, TeamError>;

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError>;

    async fn delete_team_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), TeamError>;

    async fn lookup_team_binding_by_conversation(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError> {
        Err(TeamError::InvalidRequest(
            "team conversation lookup is unavailable".to_owned(),
        ))
    }
}

impl TeamAgentProvisioner {
    fn normalized_role(input: &TeamAgentInput) -> Result<TeammateRole, TeamError> {
        TeammateRole::parse(input.role.trim())
            .ok_or_else(|| TeamError::InvalidRequest(format!("invalid team agent role: {}", input.role)))
    }

    fn effective_assistant_id(assistant_id: Option<&str>) -> Option<String> {
        assistant_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    pub(crate) async fn validate_dynamic_preflight(&self, agents: &[TeamAgentInput]) -> Result<(), TeamError> {
        for agent in agents {
            let Some(assistant_id) = Self::effective_assistant_id(agent.assistant_id.as_deref()) else {
                continue;
            };
            let assistant = self
                .assistant_catalog
                .resolve_team_selectable_assistant(&assistant_id)
                .await?
                .ok_or_else(|| {
                    TeamError::InvalidRequest(format!("Assistant is not available for team mode: {assistant_id}"))
                })?;
            let rows = self.agent_metadata_repo.list_all().await?;
            let resolved = resolve_agent_binding_from_rows(&rows, &assistant.backend)
                .ok_or_else(|| TeamError::InvalidRequest(format!("Agent not found: {}", assistant.backend)))?;
            let row = rows
                .iter()
                .find(|row| row.id == resolved.agent_id)
                .ok_or_else(|| TeamError::InvalidRequest(format!("Agent not found: {}", assistant.backend)))?;
            if row.agent_type == "acp" {
                validate_agent_dynamic_preflight(
                    row.last_check_status.as_deref(),
                    row.dynamic_probe_result.as_deref(),
                    &agent.model,
                    now_ms(),
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn new(
        repo: Arc<dyn ITeamRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
        provider_repo: Arc<dyn IProviderRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
        workspace_manager: Option<Arc<dyn TeamWorkspaceManager>>,
    ) -> Self {
        Self {
            repo,
            agent_metadata_repo,
            assistant_catalog,
            provider_repo,
            conversation_port,
            workspace_manager,
        }
    }

    fn workspace_resolver(&self) -> TeamWorkspaceResolver {
        TeamWorkspaceResolver::new(self.repo.clone(), self.conversation_port.clone())
    }

    pub(crate) async fn provision_initial_agents(
        &self,
        user_id: &str,
        team_id: &str,
        inputs: &[TeamAgentInput],
        shared_workspace: Option<&str>,
        isolated_workspaces: Option<&PreparedTeamWorkspaces>,
        source_metadata: &TeamSourceMetadata,
    ) -> Result<InitialProvisioningResult, TeamError> {
        if inputs.is_empty() {
            return Err(TeamError::InvalidRequest("at least one agent is required".into()));
        };

        let roles = inputs
            .iter()
            .map(Self::normalized_role)
            .collect::<Result<Vec<_>, _>>()?;
        let leaders = roles
            .iter()
            .enumerate()
            .filter_map(|(idx, role)| (*role == TeammateRole::Lead).then_some(idx))
            .collect::<Vec<_>>();
        let [leader_idx] = leaders.as_slice() else {
            return Err(TeamError::InvalidRequest(
                "exactly one team agent must have role lead".into(),
            ));
        };

        let leader_input = &inputs[*leader_idx];
        let leader_lease = isolated_workspaces.and_then(|plan| plan.agent_leases.get(*leader_idx));
        let leader_slot_id = leader_lease
            .map(|lease| lease.slot_id.clone())
            .unwrap_or_else(generate_id);
        let leader_role = TeammateRole::Lead;
        let leader_assistant_id = Self::effective_assistant_id(leader_input.assistant_id.as_deref());
        let leader_backend = self
            .resolve_requested_backend(leader_input.backend.as_deref(), leader_assistant_id.as_deref())
            .await?;
        let leader_conversation = self
            .create_team_conversation_for_agent(
                user_id,
                team_id,
                &leader_slot_id,
                leader_role,
                &leader_input.name,
                &leader_backend,
                &leader_input.model,
                leader_assistant_id.as_deref(),
                leader_lease
                    .map(|lease| lease.worktree_path.as_str())
                    .or(shared_workspace),
                source_metadata,
                None,
            )
            .await?;

        let team_workspace = match isolated_workspaces {
            Some(plan) => plan.integration.worktree_path.clone(),
            None => match shared_workspace {
                Some(workspace) => workspace.to_owned(),
                None => {
                    self.resolve_initial_leader_workspace(
                        team_id,
                        &leader_conversation.conversation_id,
                        leader_conversation.workspace,
                    )
                    .await?
                }
            },
        };

        let mut agents = Vec::with_capacity(inputs.len());
        agents.push(TeamAgent {
            slot_id: leader_slot_id.clone(),
            name: leader_input.name.clone(),
            role: leader_role,
            conversation_id: leader_conversation.conversation_id,
            backend: leader_backend,
            model: leader_input.model.clone(),
            assistant_id: leader_assistant_id,
            status: None,
            conversation_type: None,
            cli_path: None,
        });

        for (index, (input, role)) in inputs
            .iter()
            .zip(roles.iter())
            .enumerate()
            .filter(|(_, (_, role))| **role == TeammateRole::Teammate)
        {
            let isolated_lease = isolated_workspaces.and_then(|plan| plan.agent_leases.get(index));
            let slot_id = isolated_lease
                .map(|lease| lease.slot_id.clone())
                .unwrap_or_else(generate_id);
            let assistant_id = Self::effective_assistant_id(input.assistant_id.as_deref());
            let backend = self
                .resolve_requested_backend(input.backend.as_deref(), assistant_id.as_deref())
                .await?;
            let conversation = self
                .create_team_conversation_for_agent(
                    user_id,
                    team_id,
                    &slot_id,
                    *role,
                    &input.name,
                    &backend,
                    &input.model,
                    assistant_id.as_deref(),
                    isolated_lease
                        .map(|lease| lease.worktree_path.as_str())
                        .or(Some(&team_workspace)),
                    source_metadata,
                    None,
                )
                .await?;
            agents.push(TeamAgent {
                slot_id,
                name: input.name.clone(),
                role: *role,
                conversation_id: conversation.conversation_id,
                backend,
                model: input.model.clone(),
                assistant_id,
                status: None,
                conversation_type: None,
                cli_path: None,
            });
        }

        let lead_agent_id = Some(leader_slot_id);
        info!(
            team_id,
            count = agents.len(),
            workspace_source = if shared_workspace.is_some() {
                "user_supplied"
            } else {
                "auto_from_leader"
            },
            "Team agents provisioned"
        );
        Ok(InitialProvisioningResult {
            agents,
            lead_agent_id,
            team_workspace,
        })
    }

    pub(crate) async fn add_agent(
        &self,
        user_id: &str,
        row: &TeamRow,
        team: &mut Team,
        req: AddAgentRequest,
    ) -> Result<TeamAgent, TeamError> {
        let role = TeammateRole::parse(req.role.trim())
            .ok_or_else(|| TeamError::InvalidRequest(format!("invalid team agent role: {}", req.role)))?;
        if role != TeammateRole::Teammate {
            return Err(TeamError::InvalidRequest(
                "add_agent only supports teammate role".into(),
            ));
        }
        let slot_id = generate_id();
        let workspace = self
            .resolve_workspace_for_new_agent(user_id, row, team, &slot_id, &req.name)
            .await?;
        let assistant_id = Self::effective_assistant_id(req.assistant_id.as_deref());
        let backend = self
            .resolve_requested_backend(req.backend.as_deref(), assistant_id.as_deref())
            .await?;
        let agent = self
            .provision_new_agent(NewAgentProvisioning {
                user_id: user_id.to_owned(),
                team_id: team.id.clone(),
                slot_id: slot_id.clone(),
                name: req.name,
                role,
                backend,
                model: req.model,
                assistant_id,
                workspace: Some(workspace),
                session_mode: row.session_mode.clone(),
            })
            .await;
        let agent = match agent {
            Ok(agent) => agent,
            Err(error) => {
                self.release_failed_isolated_slot(row, &slot_id).await;
                return Err(error);
            }
        };
        team.agents.push(agent.clone());
        self.persist_agents(&team.id, &team.agents).await?;
        Ok(agent)
    }

    async fn resolve_requested_backend(
        &self,
        requested_backend: Option<&str>,
        assistant_id: Option<&str>,
    ) -> Result<String, TeamError> {
        let assistant_id = assistant_id.map(str::trim).filter(|value| !value.is_empty());
        if let Some(assistant_id) = assistant_id {
            return self
                .assistant_catalog
                .resolve_team_selectable_assistant(assistant_id)
                .await?
                .map(|assistant| assistant.backend)
                .ok_or_else(|| {
                    TeamError::InvalidRequest(format!("Assistant is not available for team mode: {assistant_id}"))
                });
        }

        let Some(requested_backend) = requested_backend.map(str::trim).filter(|value| !value.is_empty()) else {
            return Err(TeamError::InvalidRequest(
                "backend is required when assistant_id is absent".into(),
            ));
        };
        Ok(requested_backend.to_owned())
    }

    pub(crate) async fn persist_spawned_agent(&self, req: PersistSpawnedAgentRequest) -> Result<TeamAgent, TeamError> {
        let row = self
            .repo
            .get_team(&req.team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(req.team_id.clone()))?;
        let mut team = Team::from_row(&row)?;
        let slot_id = req.slot_id.clone();
        let workspace = self
            .resolve_workspace_for_new_agent(&req.user_id, &row, &team, &slot_id, &req.name)
            .await?;
        let agent = self
            .provision_new_agent(NewAgentProvisioning {
                user_id: req.user_id,
                team_id: req.team_id.clone(),
                slot_id: req.slot_id,
                name: req.name,
                role: TeammateRole::Teammate,
                backend: req.backend,
                model: req.model,
                assistant_id: req.assistant_id,
                workspace: Some(workspace),
                session_mode: row.session_mode.clone(),
            })
            .await;
        let agent = match agent {
            Ok(agent) => agent,
            Err(error) => {
                self.release_failed_isolated_slot(&row, &slot_id).await;
                return Err(error);
            }
        };
        team.agents.push(agent.clone());
        self.persist_agents(&req.team_id, &team.agents).await?;
        Ok(agent)
    }

    pub(crate) async fn attach_agent_process(
        &self,
        user_id: &str,
        agent: &TeamAgent,
        mcp_stdio_cfg: TeamMcpStdioConfig,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError> {
        let team_id = mcp_stdio_cfg.team_id.clone();
        let transport = self.team_tool_transport(agent).await?;
        match transport {
            TeamToolTransport::Mcp => self.write_team_mcp_runtime_config(agent, mcp_stdio_cfg).await?,
            TeamToolTransport::CliAssumed => self.write_team_cli_runtime_config(agent).await?,
        }
        task_manager
            .kill_and_wait(&agent.conversation_id, Some(AgentKillReason::TeamMcpRebuild))
            .await;
        self.conversation_port
            .warmup_agent_process(user_id, &agent.conversation_id, task_manager)
            .await
            .map_err(|e| {
                TeamError::InvalidRequest(format!("failed to warm up rebuilt agent {}: {e}", agent.slot_id))
            })?;
        info!(
            team_id = %team_id,
            slot_id = %agent.slot_id,
            conversation_id = %agent.conversation_id,
            backend = %agent.backend,
            transport = ?transport,
            outcome = "attached",
            "Team agent provisioner attached runtime process"
        );
        Ok(())
    }

    pub(crate) async fn team_tool_transport(&self, agent: &TeamAgent) -> Result<TeamToolTransport, TeamError> {
        let capabilities = self.agent_capabilities(&agent.backend).await?;
        if supports_team_mcp_backend(&agent.backend, capabilities.as_ref()) {
            return Ok(TeamToolTransport::Mcp);
        }
        if supports_team_cli_fallback_backend(capabilities.as_ref()) {
            return Ok(TeamToolTransport::CliAssumed);
        }
        Err(TeamError::InvalidRequest(format!(
            "agent backend is not eligible for Team transport: {}",
            agent.backend
        )))
    }

    async fn agent_capabilities(&self, backend: &str) -> Result<Option<serde_json::Value>, TeamError> {
        let Some(metadata) = acp_backend_metadata(&self.agent_metadata_repo, backend).await? else {
            return Ok(None);
        };
        let Some(raw) = metadata
            .agent_capabilities
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        Ok(serde_json::from_str(raw).ok())
    }

    pub(crate) async fn write_team_mcp_runtime_config(
        &self,
        agent: &TeamAgent,
        mcp_stdio_cfg: TeamMcpStdioConfig,
    ) -> Result<(), TeamError> {
        let acp_metadata = acp_backend_metadata(&self.agent_metadata_repo, &agent.backend).await?;
        let agent_type = if acp_metadata.is_some() {
            AgentType::Acp
        } else {
            parse_agent_type(&agent.backend)?
        };
        let session_mode = session_mode_for_backend(&agent.backend, agent_type, acp_metadata.as_ref());
        let patch = serde_json::json!({
            "team_mcp_stdio_config": mcp_stdio_cfg,
            "session_mode": session_mode,
        });
        self.conversation_port
            .patch_runtime_config(&agent.conversation_id, patch)
            .await
            .map_err(|e| {
                TeamError::InvalidRequest(format!(
                    "failed to persist team_mcp_stdio_config for {}: {e}",
                    agent.slot_id
                ))
            })
    }

    pub(crate) async fn write_team_cli_runtime_config(&self, agent: &TeamAgent) -> Result<(), TeamError> {
        let acp_metadata = acp_backend_metadata(&self.agent_metadata_repo, &agent.backend).await?;
        let agent_type = if acp_metadata.is_some() {
            AgentType::Acp
        } else {
            parse_agent_type(&agent.backend)?
        };
        let session_mode = session_mode_for_backend(&agent.backend, agent_type, acp_metadata.as_ref());
        let patch = serde_json::json!({
            "team_mcp_stdio_config": null,
            "session_mode": session_mode,
        });
        self.conversation_port
            .patch_runtime_config(&agent.conversation_id, patch)
            .await
            .map_err(|e| {
                TeamError::InvalidRequest(format!(
                    "failed to persist Team CLI runtime config for {}: {e}",
                    agent.slot_id
                ))
            })
    }

    pub(crate) async fn update_session_mode_seed(&self, agent: &TeamAgent, mode: &str) -> Result<(), TeamError> {
        self.conversation_port
            .patch_runtime_config(&agent.conversation_id, serde_json::json!({ "session_mode": mode }))
            .await
            .map_err(|e| {
                TeamError::InvalidRequest(format!("failed to persist session_mode for {}: {e}", agent.slot_id))
            })?;
        self.conversation_port
            .save_acp_runtime_mode(&agent.conversation_id, mode)
            .await
            .map_err(|e| {
                TeamError::InvalidRequest(format!("failed to persist ACP runtime mode for {}: {e}", agent.slot_id))
            })?;
        Ok(())
    }

    async fn provision_new_agent(&self, input: NewAgentProvisioning) -> Result<TeamAgent, TeamError> {
        let conversation = self
            .create_team_conversation_for_agent(
                &input.user_id,
                &input.team_id,
                &input.slot_id,
                input.role,
                &input.name,
                &input.backend,
                &input.model,
                input.assistant_id.as_deref(),
                input.workspace.as_deref(),
                &TeamSourceMetadata::default(),
                input.session_mode.as_deref(),
            )
            .await?;
        Ok(TeamAgent {
            slot_id: input.slot_id,
            name: input.name,
            role: input.role,
            conversation_id: conversation.conversation_id,
            backend: input.backend,
            model: input.model,
            assistant_id: input.assistant_id,
            status: None,
            conversation_type: None,
            cli_path: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_team_conversation_for_agent(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        role: TeammateRole,
        name: &str,
        backend: &str,
        model: &str,
        assistant_id: Option<&str>,
        workspace: Option<&str>,
        source_metadata: &TeamSourceMetadata,
        session_mode: Option<&str>,
    ) -> Result<ProvisionedConversation, TeamError> {
        let acp_metadata = acp_backend_metadata(&self.agent_metadata_repo, backend).await?;
        let agent_type = if acp_metadata.is_some() {
            AgentType::Acp
        } else {
            parse_agent_type(backend)?
        };
        let extra = self.build_team_extra(
            team_id,
            slot_id,
            role,
            backend,
            model,
            assistant_id,
            workspace,
            agent_type,
            acp_metadata.as_ref(),
            session_mode,
            source_metadata,
        );
        let provider_id = if agent_type == AgentType::Aionrs {
            self.resolve_provider_for_model(model)
                .await
                .unwrap_or_else(|| backend.to_owned())
        } else {
            backend.to_owned()
        };
        let (top_level_model, extra) = if agent_type == AgentType::Aionrs {
            (
                Some(ProviderWithModel {
                    provider_id,
                    model: model.to_owned(),
                    use_model: None,
                }),
                extra,
            )
        } else {
            let mut extra = extra;
            extra["provider_id"] = serde_json::Value::String(provider_id);
            extra["current_model_id"] = serde_json::Value::String(model.to_owned());
            (None, extra)
        };
        let created = self
            .conversation_port
            .create_team_conversation(TeamConversationCreateRequest {
                user_id: user_id.to_owned(),
                agent_type: if assistant_id.is_some() { None } else { Some(agent_type) },
                name: name.to_owned(),
                top_level_model,
                assistant_id: assistant_id.map(str::to_owned),
                source: Some(source_metadata.top_level_source()),
                channel_chat_id: source_metadata.source_chat_id.clone(),
                source_channel: source_metadata.source_channel.clone(),
                source_channel_id: source_metadata.source_channel_id.clone(),
                source_chat_id: source_metadata.source_chat_id.clone(),
                source_user_id: source_metadata.source_user_id.clone(),
                source_label: source_metadata.source_label.clone(),
                created_from: source_metadata.created_from.clone(),
                extra,
            })
            .await?;
        let conv_id = created.conversation_id;
        let resolved_workspace = created.workspace;
        info!(
            team_id,
            slot_id,
            conversation_id = %conv_id,
            outcome = "created",
            "Team agent provisioned"
        );
        Ok(ProvisionedConversation {
            conversation_id: conv_id,
            workspace: Some(resolved_workspace),
        })
    }

    async fn resolve_initial_leader_workspace(
        &self,
        team_id: &str,
        leader_conversation_id: &str,
        created_workspace: Option<String>,
    ) -> Result<String, TeamError> {
        if let Some(workspace) = created_workspace
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(workspace.to_owned());
        }

        if let Some(workspace) = self
            .conversation_port
            .conversation_workspace(leader_conversation_id)
            .await?
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            return Ok(workspace);
        }

        let workspace = self.conversation_port.create_team_temp_workspace(team_id).await?;
        if let Err(e) = self
            .conversation_port
            .patch_runtime_config(leader_conversation_id, serde_json::json!({ "workspace": workspace }))
            .await
        {
            warn!(
                team_id,
                conversation_id = %leader_conversation_id,
                error = %e,
                "failed to patch leader workspace during initial team provisioning"
            );
        }
        Ok(workspace)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_team_extra(
        &self,
        team_id: &str,
        slot_id: &str,
        role: TeammateRole,
        backend: &str,
        model: &str,
        assistant_id: Option<&str>,
        workspace: Option<&str>,
        agent_type: AgentType,
        acp_metadata: Option<&AgentMetadataRow>,
        session_mode: Option<&str>,
        source_metadata: &TeamSourceMetadata,
    ) -> serde_json::Value {
        let session_mode = session_mode
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| session_mode_for_backend(backend, agent_type, acp_metadata));
        let mut extra = serde_json::json!({
            "teamId": team_id,
            "slot_id": slot_id,
            "role": role.to_string(),
            "backend": backend,
            "session_mode": session_mode,
        });
        if agent_type != AgentType::Aionrs {
            extra["current_model_id"] = serde_json::Value::String(model.to_owned());
        }
        if let Some(assistant_id) = assistant_id {
            extra["assistant_id"] = serde_json::Value::String(assistant_id.to_owned());
        }
        if let Some(workspace) = workspace {
            inherit_team_workspace(&mut extra, workspace);
        }
        source_metadata.apply_to_extra(&mut extra);
        extra
    }

    async fn persist_agents(&self, team_id: &str, agents: &[TeamAgent]) -> Result<(), TeamError> {
        let agents_json = serde_json::to_string(agents)?;
        self.repo
            .update_team(
                team_id,
                &UpdateTeamParams {
                    agents: Some(agents_json),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn resolve_provider_for_model(&self, model: &str) -> Option<String> {
        let providers = self.provider_repo.list().await.ok()?;
        for provider in providers {
            if !provider.enabled {
                continue;
            }
            let models: Vec<String> = serde_json::from_str(&provider.models).unwrap_or_default();
            if models.iter().any(|candidate| candidate == model) {
                return Some(provider.id);
            }
        }
        None
    }

    async fn resolve_workspace_for_new_agent(
        &self,
        user_id: &str,
        row: &TeamRow,
        team: &Team,
        slot_id: &str,
        name: &str,
    ) -> Result<String, TeamError> {
        if matches!(row.workspace_mode.as_str(), "isolated" | "isolated_worktree") {
            let manager = self
                .workspace_manager
                .as_ref()
                .ok_or_else(|| TeamError::WorkspaceOperation("isolated worktree support is unavailable".into()))?;
            return Ok(manager
                .allocate_agent(user_id, &row.id, &WorkspaceAgentSpec::new(slot_id, name))
                .await?
                .worktree_path);
        }
        self.workspace_resolver().resolve_for_new_agent(row, team).await
    }

    async fn release_failed_isolated_slot(&self, row: &TeamRow, slot_id: &str) {
        if matches!(row.workspace_mode.as_str(), "isolated" | "isolated_worktree")
            && let Some(manager) = &self.workspace_manager
        {
            let _ = manager.release_slot(&row.id, slot_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::types::BuildTaskOptions;
    use aionui_ai_agent::{AgentError, AgentInstance};
    use aionui_db::models::{
        AgentMetadataRow, Provider, UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams,
        UpsertAgentMetadataParams,
    };
    use aionui_db::{CreateProviderParams, DbError, UpdateProviderParams};
    use std::sync::Mutex;
    use tokio::sync::watch;

    struct RecordingProvisioningPort {
        events: Arc<Mutex<Vec<&'static str>>>,
        patches: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait]
    impl TeamConversationProvisioningPort for RecordingProvisioningPort {
        async fn create_team_conversation(
            &self,
            _request: TeamConversationCreateRequest,
        ) -> Result<TeamConversationCreateResult, TeamError> {
            Err(TeamError::InvalidRequest("unused".into()))
        }

        async fn conversation_workspace(&self, _conversation_id: &str) -> Result<Option<String>, TeamError> {
            Ok(None)
        }

        async fn conversation_assistant_id(&self, _conversation_id: &str) -> Result<Option<String>, TeamError> {
            Ok(None)
        }

        async fn create_team_temp_workspace(&self, _team_id: &str) -> Result<String, TeamError> {
            Err(TeamError::InvalidRequest("unused".into()))
        }

        async fn patch_runtime_config(
            &self,
            _conversation_id: &str,
            patch: serde_json::Value,
        ) -> Result<(), TeamError> {
            self.patches.lock().unwrap().push(patch);
            self.events.lock().unwrap().push("patch");
            Ok(())
        }

        async fn save_acp_runtime_mode(&self, _conversation_id: &str, _mode: &str) -> Result<(), TeamError> {
            Ok(())
        }

        async fn get_config_options(&self, _conversation_id: &str) -> Result<GetConfigOptionsResponse, TeamError> {
            Ok(GetConfigOptionsResponse {
                config_options: Vec::new(),
            })
        }

        async fn warmup_agent_process(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _task_manager: &Arc<dyn IWorkerTaskManager>,
        ) -> Result<(), TeamError> {
            self.events.lock().unwrap().push("warmup");
            Ok(())
        }

        async fn delete_team_conversation(&self, _user_id: &str, _conversation_id: &str) -> Result<(), TeamError> {
            Ok(())
        }
    }

    struct BlockingKillTaskManager {
        events: Arc<Mutex<Vec<&'static str>>>,
        kill_started: watch::Sender<bool>,
        release_kill: watch::Receiver<bool>,
    }

    #[async_trait]
    impl IWorkerTaskManager for BlockingKillTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
            None
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, AgentError> {
            Err(AgentError::internal("unused"))
        }

        fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            self.events.lock().unwrap().push("kill_sync");
            let _ = self.kill_started.send(true);
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            let events = Arc::clone(&self.events);
            let kill_started = self.kill_started.clone();
            let mut release_kill = self.release_kill.clone();
            Box::pin(async move {
                events.lock().unwrap().push("kill_wait_start");
                let _ = kill_started.send(true);
                while !*release_kill.borrow() {
                    if release_kill.changed().await.is_err() {
                        break;
                    }
                }
                events.lock().unwrap().push("kill_wait_done");
            })
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            0
        }

        fn collect_idle(&self, _idle_threshold_ms: aionui_common::TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct UnusedAgentMetadataRepo;

    struct EmptyTeamAssistantCatalog;

    #[async_trait]
    impl TeamAssistantCatalogPort for EmptyTeamAssistantCatalog {
        async fn list_team_selectable_assistants(
            &self,
        ) -> Result<Vec<crate::ports::TeamAssistantCatalogEntry>, TeamError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl IAgentMetadataRepository for UnusedAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
            Ok(Vec::new())
        }
        async fn get(&self, _id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn find_by_source_and_name(
            &self,
            _agent_source: &str,
            _name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn find_builtin_by_backend(&self, _backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn update_availability_snapshot(
            &self,
            _id: &str,
            _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn update_agent_overrides(
            &self,
            _id: &str,
            _command_override: Option<&str>,
            _env_override: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
            Ok(false)
        }
        async fn delete(&self, _id: &str) -> Result<bool, DbError> {
            Ok(false)
        }
    }

    struct EmptyProviderRepo;

    #[async_trait]
    impl IProviderRepository for EmptyProviderRepo {
        async fn list(&self) -> Result<Vec<Provider>, DbError> {
            Ok(Vec::new())
        }
        async fn find_by_id(&self, _id: &str) -> Result<Option<Provider>, DbError> {
            Ok(None)
        }
        async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn update(&self, _id: &str, _params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }
    }

    fn test_provisioner(events: Arc<Mutex<Vec<&'static str>>>) -> TeamAgentProvisioner {
        test_provisioner_with_patches(events, Arc::new(Mutex::new(Vec::new())))
    }

    fn test_provisioner_with_patches(
        events: Arc<Mutex<Vec<&'static str>>>,
        patches: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> TeamAgentProvisioner {
        TeamAgentProvisioner::new(
            Arc::new(crate::test_utils::MockTeamRepo::new()),
            Arc::new(UnusedAgentMetadataRepo),
            Arc::new(EmptyTeamAssistantCatalog),
            Arc::new(EmptyProviderRepo),
            Arc::new(RecordingProvisioningPort { events, patches }),
            None,
        )
    }

    fn test_agent() -> TeamAgent {
        TeamAgent {
            slot_id: "slot-1".into(),
            name: "Agent".into(),
            role: TeammateRole::Teammate,
            conversation_id: "conv-1".into(),
            backend: "acp".into(),
            model: "sonnet".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    fn test_mcp_config() -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            team_id: "team-1".into(),
            port: 12345,
            token: "token".into(),
            slot_id: "slot-1".into(),
            binary_path: "/tmp/aioncore".into(),
        }
    }

    #[tokio::test]
    async fn team_tool_transport_prefers_mcp_for_builtin_aionrs_backend() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));
        let mut agent = test_agent();
        agent.backend = "aionrs".into();

        let transport = provisioner.team_tool_transport(&agent).await.unwrap();

        assert_eq!(transport, TeamToolTransport::Mcp);
    }

    #[tokio::test]
    async fn team_tool_transport_uses_cli_for_non_mcp_backend() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));
        let mut agent = test_agent();
        agent.backend = "custom-acp".into();

        let transport = provisioner.team_tool_transport(&agent).await.unwrap();

        assert_eq!(transport, TeamToolTransport::CliAssumed);
    }

    #[tokio::test]
    async fn cli_runtime_config_clears_mcp_config() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let provisioner = test_provisioner_with_patches(events, Arc::clone(&patches));

        provisioner.write_team_cli_runtime_config(&test_agent()).await.unwrap();

        let patches = patches.lock().unwrap();
        assert_eq!(patches[0]["team_mcp_stdio_config"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn attach_agent_process_waits_for_kill_before_warmup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let (kill_started_tx, mut kill_started_rx) = watch::channel(false);
        let (release_kill_tx, release_kill_rx) = watch::channel(false);
        let provisioner = test_provisioner_with_patches(Arc::clone(&events), Arc::clone(&patches));
        let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(BlockingKillTaskManager {
            events: Arc::clone(&events),
            kill_started: kill_started_tx,
            release_kill: release_kill_rx,
        });
        let mut agent = test_agent();
        agent.backend = "aionrs".into();

        let attach = tokio::spawn(async move {
            provisioner
                .attach_agent_process("user-1", &agent, test_mcp_config(), &task_manager)
                .await
        });
        while !*kill_started_rx.borrow() {
            kill_started_rx.changed().await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            !events.lock().unwrap().contains(&"warmup"),
            "agent warmup must wait until the previous task is fully killed"
        );

        release_kill_tx.send(true).unwrap();
        attach.await.unwrap().unwrap();

        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["patch", "kill_wait_start", "kill_wait_done", "warmup"]
        );
        let patches = patches.lock().unwrap();
        assert!(patches[0]["team_mcp_stdio_config"].is_object());
    }
}
