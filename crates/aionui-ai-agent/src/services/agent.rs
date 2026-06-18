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

use aionui_api_types::{
    AgentLogoEntry, AgentManagementRow, AgentMetadata, ProviderHealthCheckRequest, ProviderHealthCheckResponse,
};
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
        let provider_health = ProviderHealthCheckService::new(provider_repo, encryption_key, data_dir.clone());
        let availability = AgentAvailabilityService::new(registry.clone(), data_dir.clone());
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

    pub fn start_background_scheduler(&self) {
        self.availability.start_background_scheduler();
    }

    pub fn availability_feedback_port(&self) -> Arc<dyn AgentAvailabilityFeedbackPort> {
        Arc::new(self.availability.clone())
    }
}

// Agent operations
impl AgentService {
    pub async fn refresh_agents(&self) -> Result<Vec<AgentMetadata>, AgentError> {
        self.registry.refresh_availability().await;
        Ok(self
            .registry
            .list_all()
            .await
            .into_iter()
            .filter(|agent| agent.agent_type.supports_new_conversation())
            .collect())
    }

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
            let (Some(backend), Some(logo)) = (agent.backend, agent.icon) else {
                continue;
            };
            if backend.is_empty() || logo.is_empty() {
                continue;
            }
            if seen.insert(backend.clone()) {
                entries.push(AgentLogoEntry { backend, logo });
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
}
