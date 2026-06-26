use super::*;
use aionui_api_types::BehaviorPolicy;
use aionui_common::AgentType;
use aionui_common::constants::{TEAM_CAPABLE_BACKENDS, has_mcp_capability};
use aionui_db::models::AssistantOverlayRow;
use aionui_db::{IAgentMetadataRepository, resolve_agent_binding_from_rows};
use std::collections::HashMap;
use std::sync::Arc;

use crate::prompts::AvailableAssistant;

use crate::provisioning::PersistSpawnedAgentRequest;

/// Known ACP vendor labels. Kept in lockstep with the `agent_metadata`
/// seed in `005_agent_metadata.sql` — a caller hitting an unknown
/// vendor should trigger a schema drift discussion, not silently fall
/// through.
const ACP_VENDOR_LABELS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "qwen",
    "codebuddy",
    "droid",
    "goose",
    "auggie",
    "kimi",
    "opencode",
    "copilot",
    "qoder",
    "vibe",
    "cursor",
    "kiro",
    "hermes",
    "snow",
];

const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";

pub(crate) fn parse_agent_type(backend: &str) -> Result<AgentType, TeamError> {
    // Any registered ACP vendor label collapses to `AgentType::Acp`.
    if ACP_VENDOR_LABELS.contains(&backend) {
        return Ok(AgentType::Acp);
    }
    // Otherwise interpret as a top-level `AgentType` (e.g. "acp",
    // "nanobot", "aionrs", "remote", "openclaw-gateway").
    let quoted = format!("\"{backend}\"");
    if let Ok(agent_type) = serde_json::from_str::<AgentType>(&quoted) {
        if agent_type.is_deprecated_runtime() {
            return Err(TeamError::InvalidRequest(DEPRECATED_AGENT_TYPE_MESSAGE.into()));
        }
        return Ok(agent_type);
    }
    Err(TeamError::InvalidRequest(format!("unsupported backend: {backend}")))
}

/// Resolve the most permissive session mode for a given backend string.
/// Reuses `AgentType::full_auto_mode_id` from aionui-common.
pub(crate) fn resolve_full_auto_mode(backend: &str) -> &'static str {
    let agent_type = if ACP_VENDOR_LABELS.contains(&backend) {
        AgentType::Acp
    } else {
        let quoted = format!("\"{backend}\"");
        serde_json::from_str::<AgentType>(&quoted).unwrap_or(AgentType::Acp)
    };
    agent_type.full_auto_mode_id(Some(backend))
}

pub(crate) async fn resolve_runtime_backend(
    agent_metadata_repo: &Arc<dyn IAgentMetadataRepository>,
    agent_id: &str,
) -> Result<String, TeamError> {
    let rows = agent_metadata_repo.list_all().await?;
    Ok(resolve_agent_binding_from_rows(&rows, agent_id)
        .map(|binding| binding.runtime_backend)
        .unwrap_or_else(|| agent_id.to_owned()))
}

impl TeamSessionService {
    pub(crate) async fn resolve_spawn_backend_and_model(
        &self,
        assistant_id: Option<&str>,
        requested_model: Option<&str>,
        fallback_backend: &str,
        fallback_model: &str,
    ) -> Result<(String, String), TeamError> {
        if let Some(assistant_id) = assistant_id.map(str::trim).filter(|value| !value.is_empty()) {
            let definition = self
                .assistant_definition_repo
                .get_by_assistant_id(assistant_id)
                .await?
                .ok_or_else(|| TeamError::InvalidRequest(format!("Preset assistant not found: {assistant_id}")))?;
            let overlay = self.assistant_overlay_repo.get(&definition.id).await?;
            let effective_agent_id = overlay
                .as_ref()
                .and_then(|row| row.agent_id_override.as_deref())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(definition.agent_id.as_str());
            let backend = resolve_runtime_backend(&self.agent_metadata_repo, effective_agent_id).await?;
            let requested_model = requested_model
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let fixed_model = (definition.default_model_mode == "fixed")
                .then(|| definition.default_model_value.clone())
                .flatten()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            let backend_default_model = self.default_model_for_backend(&backend).await;
            let model = requested_model
                .or(fixed_model)
                .or(backend_default_model)
                .unwrap_or_else(|| fallback_model.to_owned());
            return Ok((backend, model));
        }

        let backend = fallback_backend.to_owned();
        let requested_model = requested_model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let backend_default_model = self.default_model_for_backend(&backend).await;
        let model = requested_model
            .or(backend_default_model)
            .unwrap_or_else(|| fallback_model.to_owned());
        Ok((backend, model))
    }

    /// Check if a backend is allowed to participate in team mode.
    /// Hard whitelist passes immediately; then checks behavior_policy.supports_team;
    /// finally queries persisted `agent_capabilities` for MCP transport declarations.
    pub(crate) async fn is_backend_team_capable(&self, backend: &str) -> bool {
        if TEAM_CAPABLE_BACKENDS.contains(&backend) {
            return true;
        }
        let Ok(Some(row)) = self.agent_metadata_repo.find_builtin_by_backend(backend).await else {
            return false;
        };
        let bp_supports = row
            .behavior_policy
            .as_deref()
            .and_then(|s| serde_json::from_str::<BehaviorPolicy>(s).ok())
            .is_some_and(|bp| bp.supports_team);
        if bp_supports {
            return true;
        }
        let caps = row
            .agent_capabilities
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        has_mcp_capability(caps.as_ref())
    }

    /// Return all enabled assistants that can currently participate in team mode.
    /// This is the assistant-first candidate source for the leader prompt.
    pub(crate) async fn list_team_selectable_assistants(&self) -> Vec<AvailableAssistant> {
        let Ok(definitions) = self.assistant_definition_repo.list().await else {
            return Vec::new();
        };
        let Ok(overlays) = self.assistant_overlay_repo.list().await else {
            return Vec::new();
        };
        let Ok(agent_rows) = self.agent_metadata_repo.list_all().await else {
            return Vec::new();
        };

        let overlay_by_definition: HashMap<&str, &AssistantOverlayRow> = overlays
            .iter()
            .map(|row| (row.assistant_definition_id.as_str(), row))
            .collect();

        let mut assistants: Vec<(i32, AvailableAssistant)> = Vec::new();

        for definition in &definitions {
            let Some(source) = (match definition.source.as_str() {
                "builtin" | "generated" | "user" => Some(definition.source.as_str()),
                _ => None,
            }) else {
                continue;
            };
            let overlay = overlay_by_definition.get(definition.id.as_str()).copied();
            let enabled = overlay.is_none_or(|row| row.enabled);
            if !enabled {
                continue;
            }

            let effective_agent_id = overlay
                .and_then(|row| row.agent_id_override.as_deref())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(definition.agent_id.as_str());
            let effective_backend = resolve_runtime_backend(&self.agent_metadata_repo, effective_agent_id)
                .await
                .unwrap_or_else(|_| effective_agent_id.to_owned());

            let agent_row = if source == "generated" {
                definition
                    .source_ref
                    .as_deref()
                    .and_then(|source_ref| agent_rows.iter().find(|row| row.id == source_ref))
            } else {
                agent_rows
                    .iter()
                    .find(|row| {
                        row.backend.as_deref() == Some(effective_backend.as_str()) && row.agent_source != "custom"
                    })
                    .or_else(|| {
                        agent_rows
                            .iter()
                            .find(|row| row.backend.as_deref() == Some(effective_backend.as_str()))
                    })
            };

            let is_available = agent_row.is_some_and(|row| row.last_check_status.as_deref() != Some("unavailable"));
            let is_team_capable = self.is_backend_team_capable(&effective_backend).await;
            if !(is_available && is_team_capable) {
                continue;
            }

            let mut skills = decode_string_list(&definition.default_skill_ids);
            skills.extend(decode_string_list(&definition.custom_skill_names));

            assistants.push((
                overlay.map(|row| row.sort_order).unwrap_or(i32::MAX),
                AvailableAssistant {
                    assistant_id: definition.assistant_id.clone(),
                    name: definition.name.clone(),
                    backend: effective_backend.to_owned(),
                    description: definition.description.clone().unwrap_or_default(),
                    skills,
                },
            ));
        }

        assistants.sort_by(|(left_order, left), (right_order, right)| {
            left_order
                .cmp(right_order)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.assistant_id.cmp(&right.assistant_id))
        });

        assistants.into_iter().map(|(_, assistant)| assistant).collect()
    }

    /// Return the `team_list_models` response built from DB rows.
    /// Falls back to the hardcoded response if the DB query fails.
    /// For internal agents (like aionrs with backend=NULL), enriches
    /// with models from the providers table.
    pub(crate) async fn list_models_from_db(
        &self,
        assistant_id_filter: Option<&str>,
    ) -> Result<serde_json::Value, TeamError> {
        let Ok(rows) = self.agent_metadata_repo.list_all().await else {
            return Ok(crate::mcp::tools::handle_team_list_models(&serde_json::Value::Null));
        };
        let backend_filter = match assistant_id_filter.map(str::trim).filter(|value| !value.is_empty()) {
            Some(assistant_id) => {
                let definition = self
                    .assistant_definition_repo
                    .get_by_assistant_id(assistant_id)
                    .await?
                    .ok_or_else(|| TeamError::InvalidRequest(format!("Assistant not found: {assistant_id}")))?;
                let overlay = self.assistant_overlay_repo.get(&definition.id).await?;
                Some(
                    resolve_runtime_backend(
                        &self.agent_metadata_repo,
                        overlay
                            .as_ref()
                            .and_then(|row| row.agent_id_override.as_deref())
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or(definition.agent_id.as_str()),
                    )
                    .await?,
                )
            }
            None => None,
        };
        let provider_models = self.collect_provider_models().await;
        Ok(crate::mcp::tools::build_list_models_from_rows(
            &rows,
            backend_filter.as_deref(),
            &provider_models,
        ))
    }

    /// Collect all enabled provider model IDs grouped by provider name.
    /// Returns a flat list of model IDs for use by internal agents (aionrs).
    async fn collect_provider_models(&self) -> Vec<String> {
        let Ok(providers) = self.provider_repo.list().await else {
            return vec![];
        };
        providers
            .into_iter()
            .filter(|p| p.enabled)
            .flat_map(|p| serde_json::from_str::<Vec<String>>(&p.models).unwrap_or_default())
            .collect()
    }

    pub(crate) async fn default_model_for_backend(&self, backend: &str) -> Option<String> {
        if backend == "aionrs" {
            return self.collect_provider_models().await.into_iter().next();
        }
        let row = self.agent_metadata_repo.find_builtin_by_backend(backend).await.ok()??;
        let json: serde_json::Value = serde_json::from_str(row.available_models.as_deref()?).ok()?;
        if let Some(id) = json.get("current_model_id").and_then(|v| v.as_str())
            && !id.is_empty()
        {
            return Some(id.to_owned());
        }
        let arr = json
            .get("available_models")
            .and_then(|v| v.as_array())
            .or_else(|| json.as_array())?;
        arr.first()
            .and_then(|e| e.get("id").and_then(|v| v.as_str()))
            .map(|s| s.to_owned())
    }

    pub async fn spawn_agent_in_session(
        &self,
        team_id: &str,
        caller_slot_id: &str,
        req: crate::session::SpawnAgentRequest,
    ) -> Result<TeamAgent, TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        entry.session.spawn_agent(caller_slot_id, req).await
    }

    pub fn dispose_all(&self) {
        let keys: Vec<String> = self.sessions.iter().map(|entry| entry.key().clone()).collect();
        for key in keys {
            self.stop_session_unchecked(&key);
        }
        info!("All team sessions disposed");
    }

    /// Create the conversation + persist the new agent slot for a spawn.
    ///
    /// Holds the per-team `add_agent` lock for the entirety of the
    /// read-modify-write on `teams.agents`, matching [`TeamSessionService::add_agent`]
    /// (W4-D23) so concurrent spawns cannot race and drop slots.
    ///
    /// The lock is *not* held across the process warmup step — callers
    /// (`TeamSession::spawn_agent`) wire that up separately so a slow
    /// `warmup` never stalls other spawns against the same team.
    pub(crate) async fn persist_spawned_agent(&self, req: PersistSpawnedAgentRequest) -> Result<TeamAgent, TeamError> {
        let lock = self
            .add_agent_locks
            .entry(req.team_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        self.provisioner().persist_spawned_agent(req).await
    }
}

fn decode_string_list(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::workspace_harness::{
        force_team_workspace, setup_with_factory_metadata_team_repo_and_conversation_repo, single_agent_team_request,
    };
    use aionui_db::models::{AssistantDefinitionRow, AssistantOverlayRow, Provider};
    use aionui_db::{
        DbError, IAssistantDefinitionRepository, IAssistantOverlayRepository, IProviderRepository,
        UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    };
    use std::sync::Arc;

    #[derive(Clone)]
    struct SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow,
    }

    #[async_trait::async_trait]
    impl IAssistantDefinitionRepository for SingleAssistantDefinitionRepo {
        async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
            Ok(vec![self.row.clone()])
        }

        async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok((self.row.assistant_id == assistant_id).then_some(self.row.clone()))
        }

        async fn get_by_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok((self.row.id == definition_id).then_some(self.row.clone()))
        }

        async fn get_by_source_ref(
            &self,
            _source: &str,
            _source_ref: &str,
        ) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _params: &UpsertAssistantDefinitionParams<'_>,
        ) -> Result<AssistantDefinitionRow, DbError> {
            Err(DbError::Init("not implemented".into()))
        }

        async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
            Ok(false)
        }
    }

    #[derive(Clone)]
    struct SingleAssistantOverlayRepo {
        row: AssistantOverlayRow,
    }

    #[async_trait::async_trait]
    impl IAssistantOverlayRepository for SingleAssistantOverlayRepo {
        async fn get(&self, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
            Ok((self.row.assistant_definition_id == definition_id).then_some(self.row.clone()))
        }

        async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
            Ok(vec![self.row.clone()])
        }

        async fn upsert(&self, _params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
            Err(DbError::Init("not implemented".into()))
        }

        async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
            Ok(false)
        }
    }

    struct SingleProviderRepo {
        rows: Vec<Provider>,
    }

    #[async_trait::async_trait]
    impl IProviderRepository for SingleProviderRepo {
        async fn list(&self) -> Result<Vec<Provider>, DbError> {
            Ok(self.rows.clone())
        }

        async fn find_by_id(&self, _id: &str) -> Result<Option<Provider>, DbError> {
            Ok(None)
        }

        async fn create(&self, _params: aionui_db::CreateProviderParams<'_>) -> Result<Provider, DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }

        async fn update(&self, _id: &str, _params: aionui_db::UpdateProviderParams<'_>) -> Result<Provider, DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }

        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }
    }

    fn provider_row(id: &str, models: &[&str]) -> Provider {
        Provider {
            id: id.into(),
            platform: "openai".into(),
            name: id.into(),
            base_url: "https://example.com".into(),
            api_key_encrypted: String::new(),
            models: serde_json::to_string(models).unwrap(),
            enabled: true,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn parse_agent_type_known_backends() {
        assert_eq!(parse_agent_type("acp").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("gemini").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("aionrs").unwrap(), AgentType::Aionrs);
    }

    #[test]
    fn parse_agent_type_rejects_deprecated_runtime_types() {
        for backend in ["nanobot", "remote", "openclaw-gateway"] {
            let err = parse_agent_type(backend).unwrap_err();
            assert!(matches!(err, TeamError::InvalidRequest(_)));
            assert!(
                err.to_string()
                    .contains("This agent type is no longer supported for new conversations."),
                "unexpected error for {backend}: {err}"
            );
        }
    }

    #[test]
    fn parse_agent_type_unknown_backend_returns_error() {
        let err = parse_agent_type("unknown").unwrap_err();
        assert!(matches!(err, TeamError::InvalidRequest(_)));
    }

    #[test]
    fn resolve_full_auto_mode_keeps_hermes_on_default() {
        assert_eq!(resolve_full_auto_mode("hermes"), "default");
    }

    #[tokio::test]
    async fn persist_spawned_agent_uses_team_workspace_resolver() {
        let (svc, team_repo, _, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user1", single_agent_team_request("Spawn Legacy"))
            .await
            .unwrap();
        let leader_workspace = conv_repo.get_extra(&created.assistants[0].conversation_id).unwrap()["workspace"]
            .as_str()
            .unwrap()
            .to_owned();

        force_team_workspace(&team_repo, &created.id, "").await;

        let spawned = svc
            .persist_spawned_agent(PersistSpawnedAgentRequest {
                team_id: created.id.clone(),
                user_id: "user1".into(),
                slot_id: "spawn-slot-1".into(),
                name: "Spawned".into(),
                backend: "acp".into(),
                model: "claude".into(),
                assistant_id: None,
            })
            .await
            .unwrap();

        let got = svc.get_team("user1", &created.id).await.unwrap();
        assert_eq!(got.workspace, leader_workspace);
        let spawned_extra = conv_repo.get_extra(&spawned.conversation_id).unwrap();
        assert_eq!(
            spawned_extra.get("workspace").and_then(serde_json::Value::as_str),
            Some(leader_workspace.as_str())
        );
    }

    #[tokio::test]
    async fn resolve_spawn_backend_and_model_prefers_assistant_identity_over_caller_backend() {
        let (svc, _, _, _) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let svc = TeamSessionService::new(
            svc.repo.clone(),
            svc.agent_metadata_repo.clone(),
            Arc::new(SingleAssistantDefinitionRepo {
                row: AssistantDefinitionRow {
                    id: "def-1".into(),
                    assistant_id: "word-creator".into(),
                    source: "builtin".into(),
                    owner_type: "system".into(),
                    source_ref: Some("word-creator".into()),
                    source_version: None,
                    source_hash: None,
                    name: "Word Creator".into(),
                    name_i18n: "{}".into(),
                    description: None,
                    description_i18n: "{}".into(),
                    avatar_type: "emoji".into(),
                    avatar_value: None,
                    agent_id: "aionrs".into(),
                    rule_resource_type: "inline".into(),
                    rule_resource_ref: None,
                    rule_inline_content: None,
                    recommended_prompts: "[]".into(),
                    recommended_prompts_i18n: "{}".into(),
                    default_model_mode: "auto".into(),
                    default_model_value: None,
                    default_permission_mode: "auto".into(),
                    default_permission_value: None,
                    default_skills_mode: "auto".into(),
                    default_skill_ids: "[]".into(),
                    custom_skill_names: "[]".into(),
                    default_disabled_builtin_skill_ids: "[]".into(),
                    default_mcps_mode: "auto".into(),
                    default_mcp_ids: "[]".into(),
                    created_at: 0,
                    updated_at: 0,
                    deleted_at: None,
                },
            }),
            Arc::new(SingleAssistantOverlayRepo {
                row: AssistantOverlayRow {
                    assistant_definition_id: "def-1".into(),
                    enabled: true,
                    sort_order: 0,
                    agent_id_override: None,
                    last_used_at: None,
                    created_at: 0,
                    updated_at: 0,
                },
            }),
            Arc::new(SingleProviderRepo {
                rows: vec![provider_row("openai", &["gpt-5-mini"])],
            }),
            svc.conversation_port.clone(),
            svc.projection_store.clone(),
            svc.broadcaster.clone(),
            svc.task_manager.clone(),
            svc.turn_port.clone(),
            svc.cancellation_port.clone(),
            svc.backend_binary_path.clone(),
        );

        let (backend, model) = svc
            .resolve_spawn_backend_and_model(Some("word-creator"), None, "gemini", "gemini-2.5-pro")
            .await
            .unwrap();

        assert_eq!(backend, "aionrs");
        assert_eq!(model, "gpt-5-mini");
    }
}
