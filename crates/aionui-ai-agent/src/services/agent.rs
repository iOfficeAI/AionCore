//! Business-logic layer for the ai-agent crate.
//!
//! Per `AGENTS.md` "Domain Crate Structure", this is the sole location
//! for agent-related business logic. HTTP handlers in `routes/` should
//! only extract inputs, call methods on this service, and wrap the
//! result in `ApiResponse`.
//!
//! Session-scoped operations (mode/model/config/usage/capabilities/
//! slash-commands/side-question/workspace/openclaw-runtime) now live in
//! `aionui-conversation::ConversationService`, which dispatches through
//! `AgentInstance`. This service retains only agent-catalog and
//! ACP health-check responsibilities, plus support for the custom-agent
//! CRUD endpoints (see `services::custom`).

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{AgentLogoEntry, AgentManagementRow, ProviderHealthCheckRequest, ProviderHealthCheckResponse};
use aionui_db::IProviderRepository;
use aionui_realtime::EventBroadcaster;

use super::availability::{AgentAvailabilityFeedbackPort, AgentAvailabilityService};
use super::provider_health::ProviderHealthCheckService;
use crate::error::AgentError;
use crate::registry::AgentRegistry;

pub struct AgentService {
    registry: Arc<AgentRegistry>,
    broadcaster: Arc<dyn EventBroadcaster>,
    data_dir: PathBuf,
    provider_health: ProviderHealthCheckService,
    availability: AgentAvailabilityService,
}

impl AgentService {
    pub fn new(
        registry: Arc<AgentRegistry>,
        broadcaster: Arc<dyn EventBroadcaster>,
        provider_repo: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let provider_health = ProviderHealthCheckService::new(provider_repo.clone(), encryption_key, data_dir.clone());
        let availability = AgentAvailabilityService::new(registry.clone(), provider_repo, data_dir.clone());
        Arc::new(Self {
            registry,
            broadcaster,
            data_dir,
            provider_health,
            availability,
        })
    }

    /// Data directory used by the custom-agent probe to spawn CLI
    /// processes with a stable cwd.
    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Registry accessor consumed by the `services::custom` submodule
    /// for direct repository access (upsert / delete / enable toggle).
    pub(crate) fn registry(&self) -> &Arc<AgentRegistry> {
        &self.registry
    }

    pub(crate) fn broadcaster(&self) -> &Arc<dyn EventBroadcaster> {
        &self.broadcaster
    }

    pub fn availability_feedback_port(&self) -> Arc<dyn AgentAvailabilityFeedbackPort> {
        Arc::new(self.availability.clone())
    }
}

// Agent operations
impl AgentService {
    pub async fn list_management_agents(&self) -> Result<Vec<AgentManagementRow>, AgentError> {
        Ok(self.availability.list_management_rows().await)
    }

    /// Backend → logo URL catalog for business surfaces.
    ///
    /// Business pages (guid, team, cron, conversation lists) must render
    /// an agent logo from a backend identifier alone, without owning a
    /// hardcoded path map. This projects every known agent row — including
    /// user-disabled or currently-missing ones, so historical conversations
    /// still resolve a logo — down to its `backend` and stored `icon` URL.
    pub async fn list_agent_logos(&self) -> Result<Vec<AgentLogoEntry>, AgentError> {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        for agent in self.registry.list_all_including_hidden().await {
            let Some(logo) = agent.icon.filter(|value| !value.is_empty()) else {
                continue;
            };
            // Frontend rows resolve a logo from the conversation's runtime key,
            // which is the vendor `backend` for ACP agents but the `agent_type`
            // for backends without a vendor label (e.g. aionrs, where `backend`
            // is NULL). Key on `backend` when present, otherwise the agent_type.
            let key = agent
                .backend
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| agent.agent_type.serde_name().to_owned());
            if key.is_empty() {
                continue;
            }
            if seen.insert(key.clone()) {
                entries.push(AgentLogoEntry { backend: key, logo });
            }
        }
        Ok(entries)
    }

    pub async fn health_check_agent_by_id(&self, id: &str) -> Result<AgentManagementRow, AgentError> {
        self.availability.run_manual_health_check(id).await
    }

    pub async fn provider_health_check(
        &self,
        req: ProviderHealthCheckRequest,
    ) -> Result<ProviderHealthCheckResponse, AgentError> {
        self.provider_health.health_check(req).await
    }

    pub async fn set_agent_overrides(
        &self,
        id: &str,
        req: aionui_api_types::SetAgentOverridesRequest,
    ) -> Result<AgentManagementRow, AgentError> {
        let repo = self.registry.repo_handle();
        let row = repo
            .get(id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;

        let command_override = req
            .command_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let has_env_override = req
            .env_override
            .as_ref()
            .is_some_and(|entries| entries.iter().any(|entry| !entry.name.trim().is_empty()));

        if (command_override.is_some() || has_env_override) && is_internal_aion_cli_row(&row) {
            return Err(AgentError::bad_request("Internal Aion CLI does not support overrides"));
        }

        let env_json = match req.env_override {
            Some(entries) if !entries.is_empty() => Some(
                serde_json::to_string(&entries)
                    .map_err(|e| AgentError::internal(format!("encode env_override: {e}")))?,
            ),
            _ => None,
        };

        repo.update_agent_overrides(id, command_override.as_deref(), env_json.as_deref())
            .await
            .map_err(|e| AgentError::internal(format!("repo.update_agent_overrides: {e}")))?;

        self.availability.run_manual_health_check(id).await
    }

    pub async fn get_agent_overrides(&self, id: &str) -> Result<aionui_api_types::AgentOverridesResponse, AgentError> {
        let row = self
            .registry
            .repo_handle()
            .get(id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;

        let env_override = row
            .env_override
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<aionui_api_types::AgentEnvEntry>>(s).ok())
            .unwrap_or_default();

        Ok(aionui_api_types::AgentOverridesResponse {
            command_override: if is_internal_aion_cli_row(&row) {
                None
            } else {
                row.command_override
            },
            env_override,
        })
    }
}

fn is_internal_aion_cli_row(row: &aionui_db::AgentMetadataRow) -> bool {
    row.agent_type.eq_ignore_ascii_case("aionrs") && row.agent_source.eq_ignore_ascii_case("internal")
}
