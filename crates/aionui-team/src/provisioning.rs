use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    AddAgentRequest, GetConfigOptionsResponse, McpRuntimeSnapshot, SetConfigOptionRequest, SetConfigOptionResponse,
    TeamAgentInput, TeamMcpSelection, TeamToolTransport, assistant_mcp_binding_fingerprint,
};
use aionui_common::{AgentKillReason, AgentType, ProviderWithModel, generate_id};
use aionui_db::models::{AgentMetadataRow, TeamRow};
use aionui_db::{IAgentMetadataRepository, IProviderRepository, ITeamRepository, UpdateTeamParams};
use async_trait::async_trait;
use tracing::{info, warn};

use crate::error::TeamError;
use crate::mcp::TeamMcpStdioConfig;
use crate::ports::TeamConversationBindingLookup;
use crate::ports::{TeamAssistantCatalogPort, TeamToolCapabilityPort};
use crate::service::inherit_team_workspace;
use crate::service::spawn_support::{agent_type_for_backend, cli_backend_metadata, session_mode_for_backend};
use crate::types::{Team, TeamAgent, TeammateRole};
use crate::workspace::TeamWorkspaceResolver;

#[derive(Clone)]
pub struct TeamAgentProvisioner {
    repo: Arc<dyn ITeamRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
    provider_repo: Arc<dyn IProviderRepository>,
    conversation_port: Arc<dyn TeamConversationProvisioningPort>,
    capability_port: Arc<dyn TeamToolCapabilityPort>,
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
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConversationCreateResult {
    pub conversation_id: String,
    pub workspace: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamMcpSnapshotResolution {
    pub snapshot: McpRuntimeSnapshot,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamConversationModelFacts {
    pub confirmed_model_id: Option<String>,
    pub runtime_seed_model_id: Option<String>,
}

#[async_trait]
pub trait TeamConversationProvisioningPort: Send + Sync {
    async fn create_team_conversation(
        &self,
        request: TeamConversationCreateRequest,
    ) -> Result<TeamConversationCreateResult, TeamError>;

    async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, TeamError>;

    async fn conversation_assistant_id(&self, conversation_id: &str) -> Result<Option<String>, TeamError>;

    async fn create_team_temp_workspace(&self, user_id: &str, team_id: &str) -> Result<String, TeamError>;

    async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError>;

    async fn persist_confirmed_model(&self, conversation_id: &str, model: &str) -> Result<(), TeamError> {
        self.patch_runtime_config(conversation_id, serde_json::json!({ "current_model_id": model }))
            .await
    }

    async fn conversation_model_facts(&self, _conversation_id: &str) -> Result<TeamConversationModelFacts, TeamError> {
        Ok(TeamConversationModelFacts::default())
    }

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError>;

    async fn get_config_options(&self, conversation_id: &str) -> Result<GetConfigOptionsResponse, TeamError>;

    async fn set_config_option(
        &self,
        _conversation_id: &str,
        _option_id: &str,
        _request: SetConfigOptionRequest,
    ) -> Result<SetConfigOptionResponse, TeamError> {
        Err(TeamError::InvalidRequest(
            "team conversation config updates are unavailable".to_owned(),
        ))
    }

    async fn supports_context_reset(&self, _user_id: &str, _conversation_id: &str) -> Result<bool, TeamError> {
        Ok(false)
    }

    async fn clear_context_anchor(&self, _user_id: &str, _conversation_id: &str) -> Result<bool, TeamError> {
        Ok(false)
    }

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError>;

    /// Resolve ONE assistant's effective MCP binding (its fixed default list, or
    /// the user's last selection in auto mode) into the two create-time request
    /// shapes: non-builtin rows by id + builtin rows as neutral session servers.
    /// Used by team provisioning so a member conversation carries the MCP set its
    /// own assistant is bound to — including builtin servers the generic
    /// enabled-set fallback hard-excludes.
    ///
    /// `Ok(None)` means the assistant does not exist. An empty selection is still
    /// `Ok(Some(..))` — an assistant deliberately bound to no MCP. A repo read
    /// failure is an `Err`, never a silently empty set, so provisioning aborts
    /// instead of dropping every MCP the assistant is bound to.
    ///
    /// Deliberately has NO default implementation: a silently empty selection
    /// would look identical to "this assistant has no MCP servers", so every
    /// implementor must state its own answer.
    async fn resolve_assistant_mcp_selection(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<TeamMcpSelection>, TeamError>;

    /// Resolve an existing member conversation's MCP binding into the full typed
    /// four-field runtime snapshot (names + statuses classified against the
    /// agent's transport support). Used by the attach and refresh paths before
    /// rebuilding a member session.
    ///
    /// `assistant_id` of `None` — or an `assistant_id` that no longer resolves —
    /// yields the snapshot already persisted on the conversation with a `None`
    /// fingerprint, so a member whose assistant was deleted keeps working
    /// instead of being stranded or silently downgraded to no MCP.
    ///
    /// Deliberately has NO default implementation, for the same reason as
    /// `resolve_assistant_mcp_selection`.
    async fn resolve_conversation_mcp_snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
        assistant_id: Option<&str>,
    ) -> Result<TeamMcpSnapshotResolution, TeamError>;

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

    pub(crate) fn new(
        repo: Arc<dyn ITeamRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
        provider_repo: Arc<dyn IProviderRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
        capability_port: Arc<dyn TeamToolCapabilityPort>,
    ) -> Self {
        Self {
            repo,
            agent_metadata_repo,
            assistant_catalog,
            provider_repo,
            conversation_port,
            capability_port,
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
        let leader_slot_id = generate_id();
        let leader_role = TeammateRole::Lead;
        let leader_assistant_id = Self::effective_assistant_id(leader_input.assistant_id.as_deref());
        // Resolve the leader assistant's MCP binding before any conversation is
        // created, so a read failure or an unknown assistant aborts here rather
        // than after an orphan conversation exists. Teammates resolve their own
        // bindings below — each member follows the assistant it is bound to, so
        // this result is NOT shared across members.
        let leader_mcp_selection = self
            .resolve_assistant_mcp_selection(user_id, leader_assistant_id.as_deref())
            .await?;
        let leader_backend = self
            .resolve_requested_backend(user_id, leader_input.backend.as_deref(), leader_assistant_id.as_deref())
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
                shared_workspace,
                None,
                Some(&leader_mcp_selection),
            )
            .await?;

        let team_workspace = match shared_workspace {
            Some(workspace) => workspace.to_owned(),
            None => {
                self.resolve_initial_leader_workspace(
                    user_id,
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
            backend: leader_backend,
            model: leader_input.model.clone(),
            assistant_id: leader_assistant_id,
            status: None,
            conversation_type: None,
            cli_path: None,
        });

        for (input, role) in inputs
            .iter()
            .zip(roles.iter())
            .filter(|(_, role)| **role == TeammateRole::Teammate)
        {
            let slot_id = generate_id();
            let assistant_id = Self::effective_assistant_id(input.assistant_id.as_deref());
            let backend = self
                .resolve_requested_backend(user_id, input.backend.as_deref(), assistant_id.as_deref())
                .await?;
            let mcp_selection = self
                .resolve_assistant_mcp_selection(user_id, assistant_id.as_deref())
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
                    Some(&team_workspace),
                    None,
                    Some(&mcp_selection),
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
        let workspace = self.workspace_resolver().resolve_for_new_agent(row, team).await?;
        let assistant_id = Self::effective_assistant_id(req.assistant_id.as_deref());
        let backend = self
            .resolve_requested_backend(user_id, req.backend.as_deref(), assistant_id.as_deref())
            .await?;
        // Resolve the global MCP selection once for this agent.
        let mcp_selection = self
            .resolve_assistant_mcp_selection(user_id, assistant_id.as_deref())
            .await?;
        let agent = self
            .provision_new_agent(
                NewAgentProvisioning {
                    user_id: user_id.to_owned(),
                    team_id: team.id.clone(),
                    slot_id: generate_id(),
                    name: req.name,
                    role,
                    backend,
                    model: req.model,
                    assistant_id,
                    workspace: Some(workspace),
                    session_mode: row.session_mode.clone(),
                },
                Some(&mcp_selection),
            )
            .await?;
        team.agents.push(agent.clone());
        self.persist_agents(&team.id, &team.agents).await?;
        Ok(agent)
    }

    async fn resolve_requested_backend(
        &self,
        user_id: &str,
        requested_backend: Option<&str>,
        assistant_id: Option<&str>,
    ) -> Result<String, TeamError> {
        let assistant_id = assistant_id.map(str::trim).filter(|value| !value.is_empty());
        if let Some(assistant_id) = assistant_id {
            return self
                .assistant_catalog
                .resolve_team_selectable_assistant(user_id, assistant_id)
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
            .get_team(&req.user_id, &req.team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(req.team_id.clone()))?;
        let mut team = Team::from_row(&row)?;
        let workspace = self.workspace_resolver().resolve_for_new_agent(&row, &team).await?;
        // Resolve the global MCP selection once for this spawned agent.
        let mcp_selection = self
            .resolve_assistant_mcp_selection(&req.user_id, req.assistant_id.as_deref())
            .await?;
        let agent = self
            .provision_new_agent(
                NewAgentProvisioning {
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
                },
                Some(&mcp_selection),
            )
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
        kill_existing: bool,
    ) -> Result<Option<String>, TeamError> {
        let team_id = mcp_stdio_cfg.team_id.clone();
        let (transport, capabilities) = self.resolve_team_tool_transport(user_id, agent).await?;
        info!(
            team_id = %team_id,
            slot_id = %agent.slot_id,
            conversation_id = %agent.conversation_id,
            backend = %agent.backend,
            capability_origin = capabilities.origin.as_str(),
            mcp_stdio = capabilities.mcp.stdio,
            mcp_sse = capabilities.mcp.sse,
            mcp_streamable_http = capabilities.mcp.streamable_http,
            cli_fallback = capabilities.cli_fallback,
            selected_transport = ?transport,
            "resolved Team tool transport"
        );
        // Re-resolve this member's assistant MCP binding (including builtin
        // servers) BEFORE composing the patch so the rebuilt session carries the
        // current selection. A repo read failure aborts the attach — it is never
        // treated as "the binding is empty", which would clear the stored
        // snapshot. A vanished assistant degrades to the persisted snapshot
        // rather than failing; see `resolve_conversation_mcp_snapshot`.
        let mcp_resolution = self
            .conversation_port
            .resolve_conversation_mcp_snapshot(user_id, &agent.conversation_id, agent.assistant_id.as_deref())
            .await?;
        match transport {
            TeamToolTransport::Mcp => {
                self.write_team_mcp_runtime_config(user_id, agent, mcp_stdio_cfg, !kill_existing, &mcp_resolution)
                    .await?
            }
            TeamToolTransport::CliAssumed => {
                self.write_team_cli_runtime_config(user_id, agent, !kill_existing, &mcp_resolution)
                    .await?
            }
        }
        if kill_existing {
            task_manager
                .kill_and_wait(&agent.conversation_id, Some(AgentKillReason::TeamMcpRebuild))
                .await;
        }
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
        Ok(mcp_resolution.fingerprint)
    }

    /// Resolve the MCP selection for an agent that is about to be CREATED.
    ///
    /// An unresolvable `assistant_id` is a hard error here, matching
    /// `resolve_requested_backend`: the caller passed an assistant that does not
    /// exist, and failing before anything is persisted is the correct response.
    /// This is deliberately asymmetric with the attach/refresh path — see
    /// `resolve_conversation_mcp_snapshot`, where a vanished assistant must
    /// degrade to the persisted snapshot instead of stranding a live member.
    async fn resolve_assistant_mcp_selection(
        &self,
        user_id: &str,
        assistant_id: Option<&str>,
    ) -> Result<TeamMcpSelection, TeamError> {
        let Some(assistant_id) = assistant_id else {
            return Ok(TeamMcpSelection::default());
        };
        self.conversation_port
            .resolve_assistant_mcp_selection(user_id, assistant_id)
            .await?
            .ok_or_else(|| TeamError::InvalidRequest(format!("Assistant MCP binding is unavailable: {assistant_id}")))
    }

    /// Persist the latest assistant MCP snapshot without disturbing a dormant
    /// or currently working runtime.
    pub(crate) async fn refresh_agent_mcp_snapshot(
        &self,
        user_id: &str,
        agent: &TeamAgent,
    ) -> Result<Option<String>, TeamError> {
        let resolution = self
            .conversation_port
            .resolve_conversation_mcp_snapshot(user_id, &agent.conversation_id, agent.assistant_id.as_deref())
            .await?;
        let mut patch = serde_json::json!({});
        merge_mcp_snapshot_into_patch(&mut patch, &resolution);
        self.conversation_port
            .patch_runtime_config(&agent.conversation_id, patch)
            .await?;
        Ok(resolution.fingerprint)
    }

    /// Pick how team tools reach this agent from the unified capability port.
    /// The Team domain deliberately does not know vendor ids or persistence
    /// formats; constructed descriptors and ACP handshakes are resolved outside.
    pub(crate) async fn team_tool_transport(
        &self,
        user_id: &str,
        agent: &TeamAgent,
    ) -> Result<TeamToolTransport, TeamError> {
        self.resolve_team_tool_transport(user_id, agent)
            .await
            .map(|(transport, _)| transport)
    }

    async fn resolve_team_tool_transport(
        &self,
        user_id: &str,
        agent: &TeamAgent,
    ) -> Result<(TeamToolTransport, aionui_common::ResolvedBackendCapabilities), TeamError> {
        let capabilities = self.capability_port.resolve(user_id, &agent.backend, None).await?;
        if capabilities.mcp.stdio {
            return Ok((TeamToolTransport::Mcp, capabilities));
        }
        if capabilities.cli_fallback {
            return Ok((TeamToolTransport::CliAssumed, capabilities));
        }
        Err(TeamError::InvalidRequest(format!(
            "agent backend is not eligible for Team transport: {}",
            agent.backend
        )))
    }

    pub(crate) async fn write_team_mcp_runtime_config(
        &self,
        user_id: &str,
        agent: &TeamAgent,
        mcp_stdio_cfg: TeamMcpStdioConfig,
        preserve_session_mode: bool,
        mcp_resolution: &TeamMcpSnapshotResolution,
    ) -> Result<(), TeamError> {
        let cli_metadata = cli_backend_metadata(&self.agent_metadata_repo, user_id, &agent.backend).await?;
        let agent_type = agent_type_for_backend(cli_metadata.as_ref(), &agent.backend)?;
        let session_mode = session_mode_for_backend(&agent.backend, agent_type, cli_metadata.as_ref());
        let mut patch = if preserve_session_mode {
            serde_json::json!({ "team_mcp_stdio_config": mcp_stdio_cfg })
        } else {
            serde_json::json!({
                "team_mcp_stdio_config": mcp_stdio_cfg,
                "session_mode": session_mode,
            })
        };
        merge_mcp_snapshot_into_patch(&mut patch, mcp_resolution);
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

    pub(crate) async fn write_team_cli_runtime_config(
        &self,
        user_id: &str,
        agent: &TeamAgent,
        preserve_session_mode: bool,
        mcp_resolution: &TeamMcpSnapshotResolution,
    ) -> Result<(), TeamError> {
        let cli_metadata = cli_backend_metadata(&self.agent_metadata_repo, user_id, &agent.backend).await?;
        let agent_type = agent_type_for_backend(cli_metadata.as_ref(), &agent.backend)?;
        let session_mode = session_mode_for_backend(&agent.backend, agent_type, cli_metadata.as_ref());
        let mut patch = if preserve_session_mode {
            serde_json::json!({ "team_mcp_stdio_config": null })
        } else {
            serde_json::json!({
                "team_mcp_stdio_config": null,
                "session_mode": session_mode,
            })
        };
        merge_mcp_snapshot_into_patch(&mut patch, mcp_resolution);
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

    async fn provision_new_agent(
        &self,
        input: NewAgentProvisioning,
        mcp_selection: Option<&TeamMcpSelection>,
    ) -> Result<TeamAgent, TeamError> {
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
                input.session_mode.as_deref(),
                mcp_selection,
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
        session_mode: Option<&str>,
        mcp_selection: Option<&TeamMcpSelection>,
    ) -> Result<ProvisionedConversation, TeamError> {
        let cli_metadata = cli_backend_metadata(&self.agent_metadata_repo, user_id, backend).await?;
        let agent_type = agent_type_for_backend(cli_metadata.as_ref(), backend)?;
        let extra = self.build_team_extra(
            team_id,
            slot_id,
            role,
            backend,
            model,
            assistant_id,
            workspace,
            agent_type,
            cli_metadata.as_ref(),
            session_mode,
            mcp_selection,
        );
        let provider_id = if agent_type == AgentType::Aionrs {
            self.resolve_provider_for_model(user_id, model)
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
        user_id: &str,
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

        let workspace = self
            .conversation_port
            .create_team_temp_workspace(user_id, team_id)
            .await?;
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
        cli_metadata: Option<&AgentMetadataRow>,
        session_mode: Option<&str>,
        mcp_selection: Option<&TeamMcpSelection>,
    ) -> serde_json::Value {
        let session_mode = session_mode
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| session_mode_for_backend(backend, agent_type, cli_metadata));
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
        if let Some(selection) = mcp_selection {
            // Explicit final snapshot inputs (possibly empty). `create()`
            // recognizes them for team conversations, classifies valid rows
            // per agent, and rewrites the full four-field snapshot. Empty arrays are
            // deliberate — "no user MCP" must win over preset defaults.
            extra["mcp_server_ids"] = serde_json::Value::Array(
                selection
                    .mcp_server_ids
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            );
            extra["session_mcp_servers"] =
                serde_json::to_value(&selection.session_mcp_servers).unwrap_or(serde_json::Value::Array(Vec::new()));
            extra["mcp_statuses"] =
                serde_json::to_value(&selection.mcp_statuses).unwrap_or(serde_json::Value::Array(Vec::new()));
            extra["assistant_mcp_fingerprint"] =
                serde_json::Value::String(assistant_mcp_binding_fingerprint(&selection.selected_ids));
        }
        extra
    }

    async fn persist_agents(&self, team_id: &str, agents: &[TeamAgent]) -> Result<(), TeamError> {
        let agents_json = serde_json::to_string(agents)?;
        let row = self
            .repo
            .get_team_for_restore(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_owned()))?;
        self.repo
            .update_team(
                &row.user_id,
                team_id,
                &UpdateTeamParams {
                    agents: Some(agents_json),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn resolve_provider_for_model(&self, user_id: &str, model: &str) -> Option<String> {
        let providers = self.provider_repo.list(user_id).await.ok()?;
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

/// Merge a resolved MCP snapshot into a conversation runtime-config patch.
///
/// The four snapshot fields are always written — an empty selection is a real
/// selection and must overwrite whatever was stored. `assistant_mcp_fingerprint`
/// is written ONLY when the resolution carries one: a `None` fingerprint means
/// the assistant binding could not be resolved and the snapshot came from what
/// was already persisted (see `resolve_conversation_mcp_snapshot`). Writing
/// `null` there would erase the last known binding identity and leave no way to
/// tell which MCP set a degraded member is actually running.
fn merge_mcp_snapshot_into_patch(patch: &mut serde_json::Value, resolution: &TeamMcpSnapshotResolution) {
    let Some(object) = patch.as_object_mut() else {
        return;
    };
    let snapshot = &resolution.snapshot;
    object.insert("mcp_server_ids".to_owned(), serde_json::json!(snapshot.mcp_server_ids));
    object.insert(
        "session_mcp_servers".to_owned(),
        serde_json::json!(snapshot.session_mcp_servers),
    );
    object.insert("mcp_servers".to_owned(), serde_json::json!(snapshot.mcp_servers));
    object.insert("mcp_statuses".to_owned(), serde_json::json!(snapshot.mcp_statuses));
    if let Some(fingerprint) = resolution.fingerprint.as_deref() {
        object.insert(
            "assistant_mcp_fingerprint".to_owned(),
            serde_json::Value::String(fingerprint.to_owned()),
        );
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
        /// Canned global MCP snapshot returned to the provisioner, so tests can
        /// assert the composed patch carries the four snapshot fields.
        mcp_snapshot: Option<McpRuntimeSnapshot>,
        mcp_error: Option<&'static str>,
        persisted_extra: Arc<Mutex<serde_json::Value>>,
    }

    #[async_trait]
    impl TeamConversationProvisioningPort for RecordingProvisioningPort {
        async fn create_team_conversation(
            &self,
            _request: TeamConversationCreateRequest,
        ) -> Result<TeamConversationCreateResult, TeamError> {
            self.events.lock().unwrap().push("create");
            Err(TeamError::InvalidRequest("unused".into()))
        }

        async fn resolve_assistant_mcp_selection(
            &self,
            _user_id: &str,
            _assistant_id: &str,
        ) -> Result<Option<TeamMcpSelection>, TeamError> {
            if let Some(message) = self.mcp_error {
                return Err(TeamError::InvalidRequest(message.into()));
            }
            Ok(Some(
                self.mcp_snapshot
                    .as_ref()
                    .map(|snapshot| TeamMcpSelection {
                        selected_ids: snapshot
                            .mcp_server_ids
                            .iter()
                            .cloned()
                            .chain(snapshot.session_mcp_servers.iter().map(|server| server.id.clone()))
                            .collect(),
                        mcp_server_ids: snapshot.mcp_server_ids.clone(),
                        session_mcp_servers: snapshot.session_mcp_servers.clone(),
                        mcp_statuses: snapshot.mcp_statuses.clone(),
                    })
                    .unwrap_or_default(),
            ))
        }

        async fn resolve_conversation_mcp_snapshot(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _assistant_id: Option<&str>,
        ) -> Result<TeamMcpSnapshotResolution, TeamError> {
            if let Some(message) = self.mcp_error {
                return Err(TeamError::InvalidRequest(message.into()));
            }
            Ok(TeamMcpSnapshotResolution {
                snapshot: self.mcp_snapshot.clone().unwrap_or_default(),
                fingerprint: Some("test-fingerprint".to_owned()),
            })
        }

        async fn conversation_workspace(&self, _conversation_id: &str) -> Result<Option<String>, TeamError> {
            Ok(None)
        }

        async fn conversation_assistant_id(&self, _conversation_id: &str) -> Result<Option<String>, TeamError> {
            Ok(None)
        }

        async fn create_team_temp_workspace(&self, _user_id: &str, _team_id: &str) -> Result<String, TeamError> {
            Err(TeamError::InvalidRequest("unused".into()))
        }

        async fn patch_runtime_config(
            &self,
            _conversation_id: &str,
            patch: serde_json::Value,
        ) -> Result<(), TeamError> {
            if let (Some(stored), Some(incoming)) =
                (self.persisted_extra.lock().unwrap().as_object_mut(), patch.as_object())
            {
                for (key, value) in incoming {
                    stored.insert(key.clone(), value.clone());
                }
            }
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

    struct NoopKillTaskManager;

    #[async_trait]
    impl IWorkerTaskManager for NoopKillTaskManager {
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
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async {})
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
            _user_id: &str,
        ) -> Result<Vec<crate::ports::TeamAssistantCatalogEntry>, TeamError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl IAgentMetadataRepository for UnusedAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
            Ok(Vec::new())
        }
        async fn list_all_for_user(&self, _user_id: &str) -> Result<Vec<AgentMetadataRow>, DbError> {
            self.list_all().await
        }
        async fn get(&self, _id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn get_for_user(&self, _user_id: &str, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            self.get(id).await
        }
        async fn find_by_source_and_name(
            &self,
            _agent_source: &str,
            _name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn find_by_source_and_name_for_user(
            &self,
            _user_id: &str,
            agent_source: &str,
            name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            self.find_by_source_and_name(agent_source, name).await
        }
        async fn find_builtin_by_backend(&self, _backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn find_builtin_by_backend_for_user(
            &self,
            _user_id: &str,
            backend: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            self.find_builtin_by_backend(backend).await
        }
        async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn upsert_for_user(
            &self,
            _user_id: &str,
            params: &UpsertAgentMetadataParams<'_>,
        ) -> Result<AgentMetadataRow, DbError> {
            self.upsert(params).await
        }
        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn apply_handshake_for_user(
            &self,
            _user_id: &str,
            id: &str,
            params: &UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            self.apply_handshake(id, params).await
        }
        async fn update_availability_snapshot(
            &self,
            _id: &str,
            _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }
        async fn update_availability_snapshot_for_user(
            &self,
            _user_id: &str,
            id: &str,
            params: &UpdateAgentAvailabilitySnapshotParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            self.update_availability_snapshot(id, params).await
        }
        async fn update_agent_overrides(
            &self,
            _id: &str,
            _command_override: Option<&str>,
            _env_override: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_agent_overrides_for_user(
            &self,
            _user_id: &str,
            id: &str,
            command_override: Option<&str>,
            env_override: Option<&str>,
        ) -> Result<(), DbError> {
            self.update_agent_overrides(id, command_override, env_override).await
        }
        async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
            Ok(false)
        }
        async fn set_enabled_for_user(&self, _user_id: &str, id: &str, enabled: bool) -> Result<bool, DbError> {
            self.set_enabled(id, enabled).await
        }
        async fn delete(&self, _id: &str) -> Result<bool, DbError> {
            Ok(false)
        }
        async fn delete_for_user(&self, _user_id: &str, id: &str) -> Result<bool, DbError> {
            self.delete(id).await
        }
    }

    struct EmptyProviderRepo;

    struct TestCapabilityPort;

    #[async_trait]
    impl TeamToolCapabilityPort for TestCapabilityPort {
        async fn resolve(
            &self,
            _user_id: &str,
            backend: &str,
            _agent_id: Option<&str>,
        ) -> Result<aionui_common::ResolvedBackendCapabilities, TeamError> {
            let direct = matches!(backend, "claude" | "codex" | "antigravity");
            let internal = backend == "aionrs";
            Ok(aionui_common::ResolvedBackendCapabilities {
                mcp: aionui_common::McpTransportCapabilities {
                    stdio: direct || internal,
                    ..Default::default()
                },
                cli_fallback: !internal,
                origin: if direct {
                    aionui_common::CapabilityOrigin::DirectDescriptor
                } else if internal {
                    aionui_common::CapabilityOrigin::InternalDescriptor
                } else {
                    aionui_common::CapabilityOrigin::Unknown
                },
            })
        }
    }

    #[async_trait]
    impl IProviderRepository for EmptyProviderRepo {
        async fn list(&self, _user_id: &str) -> Result<Vec<Provider>, DbError> {
            Ok(Vec::new())
        }
        async fn find_by_id(&self, _user_id: &str, _id: &str) -> Result<Option<Provider>, DbError> {
            Ok(None)
        }
        async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn update(
            &self,
            _user_id: &str,
            _id: &str,
            _params: UpdateProviderParams<'_>,
        ) -> Result<Provider, DbError> {
            Err(DbError::Init("unused".into()))
        }
        async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), DbError> {
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
        test_provisioner_with_snapshot(events, patches, None)
    }

    fn test_provisioner_with_snapshot(
        events: Arc<Mutex<Vec<&'static str>>>,
        patches: Arc<Mutex<Vec<serde_json::Value>>>,
        mcp_snapshot: Option<McpRuntimeSnapshot>,
    ) -> TeamAgentProvisioner {
        test_provisioner_with_port_state(
            events,
            patches,
            mcp_snapshot,
            None,
            Arc::new(Mutex::new(serde_json::json!({}))),
        )
    }

    fn test_provisioner_with_port_state(
        events: Arc<Mutex<Vec<&'static str>>>,
        patches: Arc<Mutex<Vec<serde_json::Value>>>,
        mcp_snapshot: Option<McpRuntimeSnapshot>,
        mcp_error: Option<&'static str>,
        persisted_extra: Arc<Mutex<serde_json::Value>>,
    ) -> TeamAgentProvisioner {
        TeamAgentProvisioner::new(
            Arc::new(crate::test_utils::MockTeamRepo::new()),
            Arc::new(UnusedAgentMetadataRepo),
            Arc::new(EmptyTeamAssistantCatalog),
            Arc::new(EmptyProviderRepo),
            Arc::new(RecordingProvisioningPort {
                events,
                patches,
                mcp_snapshot,
                mcp_error,
                persisted_extra,
            }),
            Arc::new(TestCapabilityPort),
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

    fn test_mcp_snapshot() -> McpRuntimeSnapshot {
        use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
        McpRuntimeSnapshot {
            mcp_server_ids: vec!["mcp-docs".into()],
            session_mcp_servers: vec![SessionMcpServer {
                id: "mcp-chrome".into(),
                name: "chrome-devtools".into(),
                transport: SessionMcpTransport::Stdio {
                    command: "/runtime/npx".into(),
                    args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
                    env: Default::default(),
                },
            }],
            mcp_servers: vec!["mcp-docs".into(), "chrome-devtools".into()],
            mcp_statuses: Vec::new(),
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

        let transport = provisioner.team_tool_transport("user-test", &agent).await.unwrap();

        assert_eq!(transport, TeamToolTransport::Mcp);
    }

    #[tokio::test]
    async fn direct_cli_transport_uses_mcp_without_historical_acp_snapshot() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));

        for backend in ["claude", "codex", "antigravity"] {
            let mut agent = test_agent();
            agent.backend = backend.into();

            let transport = provisioner.team_tool_transport("user-test", &agent).await.unwrap();

            assert_eq!(
                transport,
                TeamToolTransport::Mcp,
                "direct backend {backend} must use its adapter descriptor instead of an absent ACP snapshot"
            );
        }
    }

    #[tokio::test]
    async fn team_tool_transport_uses_cli_for_non_mcp_backend() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));
        let mut agent = test_agent();
        agent.backend = "custom-acp".into();

        let transport = provisioner.team_tool_transport("user-test", &agent).await.unwrap();

        assert_eq!(transport, TeamToolTransport::CliAssumed);
    }

    #[tokio::test]
    async fn cli_runtime_config_clears_mcp_config() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let provisioner = test_provisioner_with_patches(events, Arc::clone(&patches));

        provisioner
            .write_team_cli_runtime_config("user-test", &test_agent(), false, &TeamMcpSnapshotResolution::default())
            .await
            .unwrap();

        let patches = patches.lock().unwrap();
        assert_eq!(patches[0]["team_mcp_stdio_config"], serde_json::Value::Null);
        assert_eq!(patches[0]["mcp_server_ids"], serde_json::json!([]));
        assert_eq!(patches[0]["session_mcp_servers"], serde_json::json!([]));
        assert_eq!(patches[0]["mcp_servers"], serde_json::json!([]));
        assert_eq!(patches[0]["mcp_statuses"], serde_json::json!([]));
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
                .attach_agent_process("user-1", &agent, test_mcp_config(), &task_manager, true)
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
        // Single atomic patch: team config + the four MCP snapshot fields are
        // replaced together, before kill → warmup.
        assert!(patches[0]["team_mcp_stdio_config"].is_object());
        assert!(patches[0]["mcp_server_ids"].is_array());
        assert!(patches[0]["session_mcp_servers"].is_array());
        assert!(patches[0]["mcp_servers"].is_array());
        assert!(patches[0]["mcp_statuses"].is_array());
    }

    #[tokio::test]
    async fn attach_patch_carries_atomic_mcp_snapshot_on_mcp_transport() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(NoopKillTaskManager);
        let mut agent = test_agent();
        agent.backend = "aionrs".into();

        let snapshot = test_mcp_snapshot();
        let provisioner =
            test_provisioner_with_snapshot(Arc::clone(&events), Arc::clone(&patches), Some(snapshot.clone()));
        provisioner
            .attach_agent_process("user-1", &agent, test_mcp_config(), &task_manager, false)
            .await
            .unwrap();

        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["patch", "warmup"],
            "attach must compose ONE patch then warm up"
        );
        let patches = patches.lock().unwrap();
        assert_eq!(patches.len(), 1, "must be a single atomic patch, not two writes");
        assert!(patches[0]["team_mcp_stdio_config"].is_object());
        assert_eq!(patches[0]["mcp_server_ids"], serde_json::json!(["mcp-docs"]));
        assert_eq!(
            patches[0]["session_mcp_servers"][0]["name"],
            serde_json::json!("chrome-devtools")
        );
        assert_eq!(
            patches[0]["mcp_servers"],
            serde_json::json!(["mcp-docs", "chrome-devtools"])
        );
        assert!(patches[0]["mcp_statuses"].is_array());
    }

    #[tokio::test]
    async fn attach_refreshes_mcp_snapshot_on_cli_transport() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(NoopKillTaskManager);
        let mut agent = test_agent();
        // Generic ACP backend with no advertised capabilities → CLI transport.
        agent.backend = "acp".into();

        let provisioner =
            test_provisioner_with_snapshot(Arc::clone(&events), Arc::clone(&patches), Some(test_mcp_snapshot()));
        provisioner
            .attach_agent_process("user-1", &agent, test_mcp_config(), &task_manager, false)
            .await
            .unwrap();

        let patches = patches.lock().unwrap();
        assert_eq!(patches.len(), 1);
        // CLI-coordinated agents also get the refreshed user MCP snapshot.
        assert_eq!(patches[0]["team_mcp_stdio_config"], serde_json::Value::Null);
        assert_eq!(patches[0]["mcp_server_ids"], serde_json::json!(["mcp-docs"]));
        assert_eq!(
            patches[0]["session_mcp_servers"][0]["name"],
            serde_json::json!("chrome-devtools")
        );
    }

    #[tokio::test]
    async fn snapshot_only_refresh_preserves_team_coordination_config() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let persisted_extra = Arc::new(Mutex::new(serde_json::json!({
            "team_mcp_stdio_config": {"team_id": "team-1", "slot_id": "slot-1"},
        })));
        let provisioner = test_provisioner_with_port_state(
            events,
            patches,
            Some(test_mcp_snapshot()),
            None,
            Arc::clone(&persisted_extra),
        );

        provisioner
            .refresh_agent_mcp_snapshot("user-1", &test_agent())
            .await
            .unwrap();

        let extra = persisted_extra.lock().unwrap();
        assert_eq!(extra["team_mcp_stdio_config"]["team_id"], "team-1");
        assert_eq!(extra["mcp_server_ids"], serde_json::json!(["mcp-docs"]));
    }

    #[tokio::test]
    async fn provision_initial_agents_stops_before_conversation_creation_when_mcp_repo_is_unavailable() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let provisioner = test_provisioner_with_port_state(
            Arc::clone(&events),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some("MCP repository unavailable"),
            Arc::new(Mutex::new(serde_json::json!({}))),
        );
        let inputs = vec![TeamAgentInput {
            name: "Lead".into(),
            role: "lead".into(),
            backend: Some("aionrs".into()),
            model: "test-model".into(),
            assistant_id: Some("assistant-1".into()),
            conversation_id: None,
        }];

        let error = match provisioner
            .provision_initial_agents("user-1", "team-1", &inputs, Some("/workspace"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("repository failure must abort provisioning"),
        };

        assert!(error.to_string().contains("MCP repository unavailable"));
        assert!(
            events.lock().unwrap().is_empty(),
            "global MCP resolution must fail before any conversation is created"
        );
    }

    #[tokio::test]
    async fn attach_repo_error_preserves_snapshot_without_patch_kill_or_warmup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let patches = Arc::new(Mutex::new(Vec::new()));
        let existing_extra = serde_json::json!({
            "mcp_server_ids": ["old-docs"],
            "session_mcp_servers": [{
                "id": "old-chrome",
                "name": "chrome-devtools",
                "transport": { "type": "stdio", "command": "/old/npx", "args": [], "env": {} }
            }],
            "mcp_servers": ["old-docs", "chrome-devtools"],
            "mcp_statuses": [{ "id": "old-docs", "name": "old-docs", "status": "loaded" }]
        });
        let persisted_extra = Arc::new(Mutex::new(existing_extra.clone()));
        let provisioner = test_provisioner_with_port_state(
            Arc::clone(&events),
            Arc::clone(&patches),
            None,
            Some("MCP repository unavailable"),
            Arc::clone(&persisted_extra),
        );
        let (kill_started, _kill_started_rx) = watch::channel(false);
        let (_release_kill, release_kill_rx) = watch::channel(true);
        let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(BlockingKillTaskManager {
            events: Arc::clone(&events),
            kill_started,
            release_kill: release_kill_rx,
        });
        let mut agent = test_agent();
        agent.backend = "aionrs".into();

        let error = provisioner
            .attach_agent_process("user-1", &agent, test_mcp_config(), &task_manager, true)
            .await
            .expect_err("repository failure must abort attach");

        assert!(error.to_string().contains("MCP repository unavailable"));
        assert!(
            patches.lock().unwrap().is_empty(),
            "attach must not patch on repository error"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "attach must not patch, kill, or warm up"
        );
        assert_eq!(*persisted_extra.lock().unwrap(), existing_extra);
    }

    #[tokio::test]
    async fn build_team_extra_injects_explicit_mcp_selection() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));
        let selection = TeamMcpSelection {
            selected_ids: vec!["mcp-docs".into(), "builtin-1".into()],
            mcp_server_ids: vec!["mcp-docs".into()],
            session_mcp_servers: test_mcp_snapshot().session_mcp_servers,
            mcp_statuses: Vec::new(),
        };

        let extra = provisioner.build_team_extra(
            "team-1",
            "slot-1",
            TeammateRole::Teammate,
            "claude",
            "sonnet",
            None,
            Some("/ws"),
            AgentType::Acp,
            None,
            None,
            Some(&selection),
        );

        // Final snapshot inputs are explicit; no request-only fields leak in.
        assert_eq!(extra["mcp_server_ids"], serde_json::json!(["mcp-docs"]));
        assert_eq!(
            extra["session_mcp_servers"][0]["name"],
            serde_json::json!("chrome-devtools")
        );
        assert_eq!(extra["mcp_statuses"], serde_json::json!([]));
        assert!(extra.get("selected_mcp_server_ids").is_none());
        assert!(extra.get("selected_session_mcp_servers").is_none());
    }

    #[tokio::test]
    async fn build_team_extra_without_selection_writes_no_mcp_fields() {
        let provisioner = test_provisioner(Arc::new(Mutex::new(Vec::new())));
        let extra = provisioner.build_team_extra(
            "team-1",
            "slot-1",
            TeammateRole::Teammate,
            "claude",
            "sonnet",
            None,
            Some("/ws"),
            AgentType::Acp,
            None,
            None,
            None,
        );

        assert!(extra.get("selected_mcp_server_ids").is_none());
        assert!(extra.get("selected_session_mcp_servers").is_none());
    }

    fn snapshot_with_one_server() -> McpRuntimeSnapshot {
        McpRuntimeSnapshot {
            mcp_server_ids: vec!["mcp-a".to_owned()],
            session_mcp_servers: Vec::new(),
            mcp_servers: vec!["server-a".to_owned()],
            mcp_statuses: Vec::new(),
        }
    }

    #[test]
    fn resolved_fingerprint_is_written_into_the_patch() {
        let mut patch = serde_json::json!({ "session_mode": "build" });

        merge_mcp_snapshot_into_patch(
            &mut patch,
            &TeamMcpSnapshotResolution {
                snapshot: snapshot_with_one_server(),
                fingerprint: Some(r#"["mcp-a"]"#.to_owned()),
            },
        );

        assert_eq!(patch["session_mode"], "build", "pre-existing keys must survive");
        assert_eq!(patch["mcp_server_ids"], serde_json::json!(["mcp-a"]));
        assert_eq!(patch["mcp_servers"], serde_json::json!(["server-a"]));
        assert_eq!(patch["assistant_mcp_fingerprint"], r#"["mcp-a"]"#);
    }

    #[test]
    fn degraded_snapshot_keeps_the_stored_fingerprint_instead_of_nulling_it() {
        // A `None` fingerprint means the assistant vanished and the snapshot was
        // reused from what was already persisted. Writing the key as `null` here
        // would erase the only record of which MCP set the member is running.
        let mut patch = serde_json::json!({ "team_mcp_stdio_config": null });

        merge_mcp_snapshot_into_patch(
            &mut patch,
            &TeamMcpSnapshotResolution {
                snapshot: snapshot_with_one_server(),
                fingerprint: None,
            },
        );

        assert!(
            !patch.as_object().unwrap().contains_key("assistant_mcp_fingerprint"),
            "the key must be absent, not null: {patch}"
        );
        // The snapshot itself is still refreshed — only the fingerprint is held back.
        assert_eq!(patch["mcp_server_ids"], serde_json::json!(["mcp-a"]));
        assert_eq!(patch["mcp_servers"], serde_json::json!(["server-a"]));
    }

    #[test]
    fn empty_snapshot_fields_still_overwrite_stored_values() {
        let mut patch = serde_json::json!({});

        merge_mcp_snapshot_into_patch(
            &mut patch,
            &TeamMcpSnapshotResolution {
                snapshot: McpRuntimeSnapshot::default(),
                fingerprint: Some("[]".to_owned()),
            },
        );

        // "no MCP" must be persisted as an explicit empty selection, never skipped.
        assert_eq!(patch["mcp_server_ids"], serde_json::json!([]));
        assert_eq!(patch["session_mcp_servers"], serde_json::json!([]));
        assert_eq!(patch["mcp_servers"], serde_json::json!([]));
        assert_eq!(patch["mcp_statuses"], serde_json::json!([]));
        assert_eq!(patch["assistant_mcp_fingerprint"], "[]");
    }
}
