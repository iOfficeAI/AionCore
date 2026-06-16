use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{AddAgentRequest, TeamAgentInput};
use aionui_common::{AgentKillReason, AgentType, ProviderWithModel, generate_id};
use aionui_db::models::TeamRow;
use aionui_db::{IProviderRepository, ITeamRepository, UpdateTeamParams};
use async_trait::async_trait;
use tracing::{info, warn};

use crate::error::TeamError;
use crate::mcp::TeamMcpStdioConfig;
use crate::service::inherit_team_workspace;
use crate::service::spawn_support::{parse_agent_type, resolve_full_auto_mode};
use crate::types::{Team, TeamAgent, TeammateRole};
use crate::workspace::TeamWorkspaceResolver;

#[derive(Clone)]
pub struct TeamAgentProvisioner {
    repo: Arc<dyn ITeamRepository>,
    provider_repo: Arc<dyn IProviderRepository>,
    conversation_port: Arc<dyn TeamConversationProvisioningPort>,
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
    custom_agent_id: Option<String>,
    workspace: Option<String>,
}

pub(crate) struct PersistSpawnedAgentRequest {
    pub user_id: String,
    pub team_id: String,
    pub slot_id: String,
    pub name: String,
    pub backend: String,
    pub model: String,
    pub custom_agent_id: Option<String>,
}

pub struct TeamConversationCreateRequest {
    pub user_id: String,
    pub agent_type: AgentType,
    pub name: String,
    pub top_level_model: Option<ProviderWithModel>,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConversationCreateResult {
    pub conversation_id: String,
    pub workspace: String,
}

pub struct TeamConversationAdoptRequest {
    pub conversation_id: String,
    pub extra: serde_json::Value,
}

#[async_trait]
pub trait TeamConversationProvisioningPort: Send + Sync {
    async fn create_team_conversation(
        &self,
        request: TeamConversationCreateRequest,
    ) -> Result<TeamConversationCreateResult, TeamError>;

    async fn adopt_team_conversation(&self, request: TeamConversationAdoptRequest) -> Result<(), TeamError>;

    async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, TeamError>;

    async fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, TeamError>;

    async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError>;

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError>;

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError>;

    async fn delete_team_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), TeamError>;
}

impl TeamAgentProvisioner {
    pub(crate) fn new(
        repo: Arc<dyn ITeamRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
    ) -> Self {
        Self {
            repo,
            provider_repo,
            conversation_port,
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
    ) -> Result<InitialProvisioningResult, TeamError> {
        let Some((leader_input, teammate_inputs)) = inputs.split_first() else {
            return Err(TeamError::InvalidRequest("at least one agent is required".into()));
        };

        let leader_slot_id = generate_id();
        let leader_role = TeammateRole::Lead;
        let leader_conversation = self
            .create_or_adopt_conversation(
                user_id,
                team_id,
                &leader_slot_id,
                leader_role,
                &leader_input.name,
                &leader_input.backend,
                &leader_input.model,
                leader_input.custom_agent_id.as_deref(),
                leader_input.conversation_id.as_deref(),
                shared_workspace,
            )
            .await?;

        let team_workspace = match shared_workspace {
            Some(workspace) => workspace.to_owned(),
            None => {
                self.resolve_initial_leader_workspace(
                    team_id,
                    &leader_conversation.conversation_id,
                    leader_conversation.workspace,
                )
                .await?
            }
        };

        let mut agents = Vec::with_capacity(inputs.len());
        agents.push(TeamAgent {
            slot_id: leader_slot_id.clone(),
            name: leader_input.name.clone(),
            role: leader_role,
            conversation_id: leader_conversation.conversation_id,
            backend: leader_input.backend.clone(),
            model: leader_input.model.clone(),
            custom_agent_id: leader_input.custom_agent_id.clone(),
            status: None,
            conversation_type: None,
            cli_path: None,
        });

        for input in teammate_inputs {
            let slot_id = generate_id();
            let role = TeammateRole::parse(&input.role).unwrap_or(TeammateRole::Teammate);
            let conversation = self
                .create_or_adopt_conversation(
                    user_id,
                    team_id,
                    &slot_id,
                    role,
                    &input.name,
                    &input.backend,
                    &input.model,
                    input.custom_agent_id.as_deref(),
                    input.conversation_id.as_deref(),
                    Some(&team_workspace),
                )
                .await?;
            agents.push(TeamAgent {
                slot_id,
                name: input.name.clone(),
                role,
                conversation_id: conversation.conversation_id,
                backend: input.backend.clone(),
                model: input.model.clone(),
                custom_agent_id: input.custom_agent_id.clone(),
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
        let role = TeammateRole::parse(&req.role).unwrap_or(TeammateRole::Teammate);
        let workspace = self.workspace_resolver().resolve_for_new_agent(row, team).await?;
        let agent = self
            .provision_new_agent(NewAgentProvisioning {
                user_id: user_id.to_owned(),
                team_id: team.id.clone(),
                slot_id: generate_id(),
                name: req.name,
                role,
                backend: req.backend,
                model: req.model,
                custom_agent_id: req.custom_agent_id,
                workspace: Some(workspace),
            })
            .await?;
        team.agents.push(agent.clone());
        self.persist_agents(&team.id, &team.agents).await?;
        Ok(agent)
    }

    pub(crate) async fn persist_spawned_agent(&self, req: PersistSpawnedAgentRequest) -> Result<TeamAgent, TeamError> {
        let row = self
            .repo
            .get_team(&req.team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(req.team_id.clone()))?;
        let mut team = Team::from_row(&row)?;
        let workspace = self.workspace_resolver().resolve_for_new_agent(&row, &team).await?;
        let agent = self
            .provision_new_agent(NewAgentProvisioning {
                user_id: req.user_id,
                team_id: req.team_id.clone(),
                slot_id: req.slot_id,
                name: req.name,
                role: TeammateRole::Teammate,
                backend: req.backend,
                model: req.model,
                custom_agent_id: req.custom_agent_id,
                workspace: Some(workspace),
            })
            .await?;
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
        self.write_team_mcp_runtime_config(agent, mcp_stdio_cfg).await?;
        let _ = task_manager.kill(&agent.conversation_id, Some(AgentKillReason::TeamMcpRebuild));
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
            outcome = "attached",
            "Team agent provisioner attached runtime process"
        );
        Ok(())
    }

    pub(crate) async fn write_team_mcp_runtime_config(
        &self,
        agent: &TeamAgent,
        mcp_stdio_cfg: TeamMcpStdioConfig,
    ) -> Result<(), TeamError> {
        let patch = serde_json::json!({
            "team_mcp_stdio_config": mcp_stdio_cfg,
            "session_mode": resolve_full_auto_mode(&agent.backend),
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
            .create_or_adopt_conversation(
                &input.user_id,
                &input.team_id,
                &input.slot_id,
                input.role,
                &input.name,
                &input.backend,
                &input.model,
                input.custom_agent_id.as_deref(),
                None,
                input.workspace.as_deref(),
            )
            .await?;
        Ok(TeamAgent {
            slot_id: input.slot_id,
            name: input.name,
            role: input.role,
            conversation_id: conversation.conversation_id,
            backend: input.backend,
            model: input.model,
            custom_agent_id: input.custom_agent_id,
            status: None,
            conversation_type: None,
            cli_path: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_or_adopt_conversation(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        role: TeammateRole,
        name: &str,
        backend: &str,
        model: &str,
        custom_agent_id: Option<&str>,
        existing_conversation_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<ProvisionedConversation, TeamError> {
        let extra = self
            .build_team_extra(team_id, slot_id, role, backend, model, custom_agent_id, workspace)
            .await?;
        if let Some(existing_id) = existing_conversation_id {
            self.conversation_port
                .adopt_team_conversation(TeamConversationAdoptRequest {
                    conversation_id: existing_id.to_owned(),
                    extra,
                })
                .await?;
            info!(
                team_id,
                slot_id,
                conversation_id = %existing_id,
                outcome = "adopted",
                "Team agent provisioned"
            );
            return Ok(ProvisionedConversation {
                conversation_id: existing_id.to_owned(),
                workspace: workspace.map(str::to_owned),
            });
        }

        let agent_type = parse_agent_type(backend)?;
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
                agent_type,
                name: name.to_owned(),
                top_level_model,
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

    pub(crate) async fn patch_guide_mcp_config(
        &self,
        agent: &TeamAgent,
        config: &aionui_api_types::GuideMcpConfig,
    ) -> Result<(), TeamError> {
        self.conversation_port
            .patch_runtime_config(
                &agent.conversation_id,
                serde_json::json!({ "guide_mcp_config": config }),
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_team_extra(
        &self,
        team_id: &str,
        slot_id: &str,
        role: TeammateRole,
        backend: &str,
        model: &str,
        custom_agent_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<serde_json::Value, TeamError> {
        let mut extra = serde_json::json!({
            "teamId": team_id,
            "slot_id": slot_id,
            "role": role.to_string(),
            "backend": backend,
            "session_mode": resolve_full_auto_mode(backend),
        });
        if parse_agent_type(backend)? != AgentType::Aionrs {
            extra["current_model_id"] = serde_json::Value::String(model.to_owned());
        }
        if let Some(custom_agent_id) = custom_agent_id {
            extra["custom_agent_id"] = serde_json::Value::String(custom_agent_id.to_owned());
            extra["preset_assistant_id"] = serde_json::Value::String(custom_agent_id.to_owned());
        }
        if let Some(workspace) = workspace {
            inherit_team_workspace(&mut extra, workspace);
        }
        Ok(extra)
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
}
