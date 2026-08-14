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
use std::sync::{Arc, RwLock};

use aionui_api_types::{
    AgentLogoEntry, AgentManagementRow, ManagedRuntimeState, ManagedRuntimeStatus, ProviderHealthCheckRequest,
    ProviderHealthCheckResponse,
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
    provider_health: ProviderHealthCheckService,
    availability: AgentAvailabilityService,
    deepseek_runtime: Arc<RwLock<ManagedRuntimeStatus>>,
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
        let availability = AgentAvailabilityService::new_with_runtime_credentials(
            registry.clone(),
            provider_repo,
            encryption_key,
            data_dir,
        );
        let manifest = aionui_runtime::deepseek_harness_manifest()
            .expect("embedded DeepSeek Harness runtime manifest must be valid");
        let current_runtime = aionui_runtime::probe_deepseek_harness_current_runtime();
        let installed_runtime = aionui_runtime::probe_deepseek_harness_runtime();
        let update_available = current_runtime.is_none() && installed_runtime.is_some();
        let deepseek_runtime = Arc::new(RwLock::new(ManagedRuntimeStatus {
            runtime_id: manifest.runtime_id,
            release: installed_runtime
                .as_ref()
                .map_or(manifest.release, |runtime| runtime.release.clone()),
            state: if installed_runtime.is_some() {
                ManagedRuntimeState::Ready
            } else {
                ManagedRuntimeState::NotInstalled
            },
            phase: update_available.then(|| "update_available".to_owned()),
            progress: None,
            error_code: None,
            error_message: None,
        }));
        Arc::new(Self {
            registry,
            broadcaster,
            provider_health,
            availability,
            deepseek_runtime,
        })
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
    pub async fn list_management_agents(&self, user_id: &str) -> Result<Vec<AgentManagementRow>, AgentError> {
        let rows = self.availability.list_management_rows(user_id).await?;
        Ok(rows.into_iter().map(|row| self.overlay_runtime_status(row)).collect())
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

    pub async fn health_check_agent_by_id(&self, user_id: &str, id: &str) -> Result<AgentManagementRow, AgentError> {
        let row = self.availability.run_manual_health_check(user_id, id).await?;
        Ok(self.overlay_runtime_status(row))
    }

    pub async fn prepare_agent_runtime(&self, user_id: &str, id: &str) -> Result<AgentManagementRow, AgentError> {
        let row = self
            .availability
            .management_row_by_id(user_id, id)
            .await?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;
        let source = row
            .agent_source_info
            .managed_runtime
            .as_ref()
            .ok_or_else(|| AgentError::bad_request("Agent does not use a managed application runtime"))?;
        if source.runtime_id != aionui_runtime::DEEPSEEK_HARNESS_RUNTIME_ID {
            return Err(AgentError::bad_request("Managed application runtime is not supported"));
        }

        if aionui_runtime::probe_deepseek_harness_current_runtime().is_some() {
            self.set_runtime_status(ManagedRuntimeState::Ready, Some("ready"), Some(100), None, None);
            return Ok(self.overlay_runtime_status(row));
        }

        {
            let mut status = self
                .deepseek_runtime
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if status.state != ManagedRuntimeState::Installing {
                status.state = ManagedRuntimeState::Installing;
                status.phase = Some("waiting_for_lock".to_owned());
                status.progress = Some(0);
                status.error_code = None;
                status.error_message = None;

                let shared_status = self.deepseek_runtime.clone();
                let registry = self.registry.clone();
                tokio::spawn(async move {
                    let reporter_status = shared_status.clone();
                    let reporter = move |update: aionui_runtime::ManagedNpmAppProgress| {
                        let (phase, progress) = match update.phase {
                            aionui_runtime::ManagedNpmAppProgressPhase::WaitingForLock => ("waiting_for_lock", 0),
                            aionui_runtime::ManagedNpmAppProgressPhase::Installing => ("installing", 25),
                            aionui_runtime::ManagedNpmAppProgressPhase::Validating => ("validating", 90),
                            aionui_runtime::ManagedNpmAppProgressPhase::Ready => ("ready", 100),
                            aionui_runtime::ManagedNpmAppProgressPhase::Failed => ("failed", 0),
                        };
                        let mut status = reporter_status.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                        status.state = if update.phase == aionui_runtime::ManagedNpmAppProgressPhase::Failed {
                            ManagedRuntimeState::Failed
                        } else if update.phase == aionui_runtime::ManagedNpmAppProgressPhase::Ready {
                            ManagedRuntimeState::Ready
                        } else {
                            ManagedRuntimeState::Installing
                        };
                        status.phase = Some(phase.to_owned());
                        status.progress = Some(progress);
                    };
                    match aionui_runtime::ensure_deepseek_harness_runtime(Some(&reporter), None).await {
                        Ok(runtime) => {
                            let candidate_release = aionui_runtime::deepseek_harness_manifest()
                                .map(|manifest| manifest.release)
                                .unwrap_or_default();
                            let rolled_back = runtime.release != candidate_release;
                            {
                                let mut status = shared_status.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                                status.state = ManagedRuntimeState::Ready;
                                status.release = runtime.release;
                                status.phase = Some(if rolled_back { "rollback" } else { "ready" }.to_owned());
                                status.progress = Some(100);
                                status.error_code = rolled_back.then(|| "runtime_update_failed".to_owned());
                                status.error_message = rolled_back.then(|| {
                                    "The candidate runtime failed validation; AionCore kept the previous verified release."
                                        .to_owned()
                                });
                            }
                            registry.refresh_availability().await;
                        }
                        Err(error) => {
                            tracing::warn!(
                                runtime_id = aionui_runtime::DEEPSEEK_HARNESS_RUNTIME_ID,
                                error_code = "runtime_install_failed",
                                error = %error,
                                "Managed runtime installation failed"
                            );
                            let mut status = shared_status.write().unwrap_or_else(|poisoned| poisoned.into_inner());
                            status.state = ManagedRuntimeState::Failed;
                            status.phase = Some("failed".to_owned());
                            status.progress = None;
                            status.error_code = Some("runtime_install_failed".to_owned());
                            status.error_message = Some(
                                "DeepSeek Harness runtime installation failed. Retry the installation or inspect server logs."
                                    .to_owned(),
                            );
                        }
                    }
                });
            }
        }
        Ok(self.overlay_runtime_status(row))
    }

    pub async fn provider_health_check(
        &self,
        user_id: &str,
        req: ProviderHealthCheckRequest,
    ) -> Result<ProviderHealthCheckResponse, AgentError> {
        self.provider_health.health_check(user_id, req).await
    }

    pub async fn set_agent_overrides(
        &self,
        user_id: &str,
        id: &str,
        req: aionui_api_types::SetAgentOverridesRequest,
    ) -> Result<AgentManagementRow, AgentError> {
        let repo = self.registry.repo_handle();
        let row = repo
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
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
            return Err(AgentError::bad_request(
                "Internal CSBU WorkMate agent does not support overrides",
            ));
        }

        // Launch-path overrides only make sense for direct-CLI rows. Bridge-launched
        // rows (e.g. `npx`) keep the bridge's own arguments in `args` (such as
        // `-y <package> acp`); swapping `command` for a launch path would feed those
        // bridge arguments to the target binary and break startup. Reject the write so
        // the stored spawn command stays coherent (env overrides remain allowed).
        if command_override.is_some() && is_bridge_launched_row(&row) {
            return Err(AgentError::bad_request(
                "This agent launches through a package runner (npx); its launch path cannot be overridden. Use environment variables instead.",
            ));
        }

        let env_json = match req.env_override {
            Some(entries) if !entries.is_empty() => Some(
                serde_json::to_string(&entries)
                    .map_err(|e| AgentError::internal(format!("encode env_override: {e}")))?,
            ),
            _ => None,
        };

        repo.update_agent_overrides_for_user(user_id, id, command_override.as_deref(), env_json.as_deref())
            .await
            .map_err(|e| AgentError::internal(format!("repo.update_agent_overrides_for_user: {e}")))?;

        self.availability.run_manual_health_check(user_id, id).await
    }

    pub async fn get_agent_overrides(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<aionui_api_types::AgentOverridesResponse, AgentError> {
        let row = self
            .registry
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
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

impl AgentService {
    fn overlay_runtime_status(&self, mut row: AgentManagementRow) -> AgentManagementRow {
        if row.agent_source_info.managed_runtime.is_some() {
            let status = self
                .deepseek_runtime
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            row.installed = status.state == ManagedRuntimeState::Ready;
            row.runtime = Some(status);
        }
        row
    }

    fn set_runtime_status(
        &self,
        state: ManagedRuntimeState,
        phase: Option<&str>,
        progress: Option<u8>,
        error_code: Option<&str>,
        error_message: Option<String>,
    ) {
        let mut status = self
            .deepseek_runtime
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.state = state;
        status.phase = phase.map(str::to_owned);
        status.progress = progress;
        status.error_code = error_code.map(str::to_owned);
        status.error_message = error_message;
    }
}

/// True when the row is launched through a bridge binary (e.g. `npx`) rather
/// than a direct CLI. Such rows store the bridge's own arguments in `args`
/// (e.g. `-y <package> acp`), so replacing `command` with a launch path would
/// forward those bridge arguments to the target binary. Launch-path overrides
/// are therefore only valid for direct-CLI rows (`command == binary_name`, no
/// bridge). Unparseable or absent `agent_source_info` is treated as direct.
fn is_bridge_launched_row(row: &aionui_db::AgentMetadataRow) -> bool {
    let Some(raw) = row.agent_source_info.as_deref() else {
        return false;
    };
    let Ok(info) = serde_json::from_str::<aionui_api_types::AgentSourceInfo>(raw) else {
        return false;
    };
    match info.bridge_binary.as_deref() {
        Some(bridge) => info.binary_name.as_deref() != Some(bridge),
        None => false,
    }
}

fn is_internal_aion_cli_row(row: &aionui_db::AgentMetadataRow) -> bool {
    row.agent_type.eq_ignore_ascii_case("aionrs") && row.agent_source.eq_ignore_ascii_case("internal")
}
