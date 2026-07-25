use super::*;
use aionui_common::AgentType;
use aionui_db::models::AgentMetadataRow;
use aionui_db::{IAgentMetadataRepository, resolve_agent_binding_from_rows};
use std::sync::Arc;

use crate::ports::TeamAssistantCatalogEntry;
use crate::prompts::AvailableAssistant;

use crate::provisioning::PersistSpawnedAgentRequest;

const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";

pub(crate) fn parse_agent_type(backend: &str) -> Result<AgentType, TeamError> {
    let quoted = format!("\"{backend}\"");
    if let Ok(agent_type) = serde_json::from_str::<AgentType>(&quoted) {
        if agent_type.is_deprecated_runtime() {
            return Err(TeamError::InvalidRequest(DEPRECATED_AGENT_TYPE_MESSAGE.into()));
        }
        return Ok(agent_type);
    }
    Err(TeamError::InvalidRequest(format!("unsupported backend: {backend}")))
}

fn find_acp_backend_metadata(rows: &[AgentMetadataRow], backend: &str) -> Option<AgentMetadataRow> {
    rows.iter()
        .find(|row| row.agent_type == AgentType::Acp.serde_name() && row.backend.as_deref() == Some(backend))
        .cloned()
}

pub(crate) async fn acp_backend_metadata(
    agent_metadata_repo: &Arc<dyn IAgentMetadataRepository>,
    backend: &str,
) -> Result<Option<AgentMetadataRow>, TeamError> {
    let rows = agent_metadata_repo.list_all().await?;
    Ok(find_acp_backend_metadata(&rows, backend))
}

pub(crate) fn session_mode_for_backend(
    backend: &str,
    agent_type: AgentType,
    acp_metadata: Option<&AgentMetadataRow>,
) -> String {
    if let Some(row) = acp_metadata
        && let Some(yolo_id) = row.yolo_id.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        return yolo_id.to_owned();
    }
    agent_type.full_auto_mode_id(Some(backend)).to_owned()
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
    pub(crate) async fn resolve_team_selectable_assistant(
        &self,
        assistant_id: &str,
    ) -> Result<TeamAssistantCatalogEntry, TeamError> {
        self.assistant_catalog
            .resolve_team_selectable_assistant(assistant_id)
            .await?
            .ok_or_else(|| {
                TeamError::InvalidRequest(format!("Assistant is not available for team mode: {assistant_id}"))
            })
    }

    pub(crate) async fn resolve_spawn_backend_and_model(
        &self,
        assistant_id: Option<&str>,
        requested_model: Option<&str>,
        fallback_backend: &str,
        fallback_model: &str,
    ) -> Result<(String, String), TeamError> {
        if let Some(assistant_id) = assistant_id.map(str::trim).filter(|value| !value.is_empty()) {
            let selectable = self.resolve_team_selectable_assistant(assistant_id).await?;
            let definition = self
                .assistant_definition_repo
                .get_by_assistant_id(assistant_id)
                .await?
                .ok_or_else(|| TeamError::InvalidRequest(format!("Preset assistant not found: {assistant_id}")))?;
            let backend = selectable.backend;
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

    /// Return all enabled assistants that can currently participate in team mode.
    /// This consumes the same assistant projection as the Team creation UI, so
    /// `team_selectable` has a single source of truth.
    pub(crate) async fn list_team_selectable_assistants(&self) -> Vec<AvailableAssistant> {
        let Ok(assistants) = self.assistant_catalog.list_team_selectable_assistants().await else {
            return Vec::new();
        };

        assistants
            .into_iter()
            .map(|assistant| AvailableAssistant {
                assistant_id: assistant.assistant_id,
                name: assistant.name,
                backend: assistant.backend,
                description: assistant.description,
                skills: assistant.skills,
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::workspace_harness::{
        force_team_workspace, setup_with_factory_metadata_team_repo_and_conversation_repo, single_agent_team_request,
    };
    use aionui_db::models::{AgentMetadataRow, AssistantDefinitionRow, AssistantOverlayRow, Provider};
    use aionui_db::{
        DbError, IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
        IProviderRepository, UpdateAgentHandshakeParams, UpsertAgentMetadataParams, UpsertAssistantDefinitionParams,
        UpsertAssistantOverlayParams,
    };
    use std::sync::Arc;

    fn agent_metadata_row(backend: &str, yolo_id: Option<&str>) -> AgentMetadataRow {
        AgentMetadataRow {
            id: format!("agent-{backend}"),
            icon: None,
            name: format!("{backend} agent"),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some(backend.to_owned()),
            agent_type: AgentType::Acp.serde_name().to_owned(),
            agent_source: "builtin".to_owned(),
            agent_source_info: None,
            enabled: true,
            command: None,
            args: None,
            env: None,
            native_skills_dirs: None,
            behavior_policy: None,
            yolo_id: yolo_id.map(ToOwned::to_owned),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 0,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            command_override: None,
            env_override: None,
            created_at: 0,
            updated_at: 0,
        }
    }

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
    struct MultiAssistantDefinitionRepo {
        rows: Vec<AssistantDefinitionRow>,
    }

    #[async_trait::async_trait]
    impl IAssistantDefinitionRepository for MultiAssistantDefinitionRepo {
        async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
            Ok(self.rows.clone())
        }

        async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok(self.rows.iter().find(|row| row.assistant_id == assistant_id).cloned())
        }

        async fn get_by_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok(self.rows.iter().find(|row| row.id == definition_id).cloned())
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

    #[derive(Clone)]
    struct MultiAssistantOverlayRepo {
        rows: Vec<AssistantOverlayRow>,
    }

    #[async_trait::async_trait]
    impl IAssistantOverlayRepository for MultiAssistantOverlayRepo {
        async fn get(&self, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.assistant_definition_id == definition_id)
                .cloned())
        }

        async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
            Ok(self.rows.clone())
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
            model_settings: "{}".into(),
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    struct RowsAgentMetadataRepo {
        rows: Vec<AgentMetadataRow>,
    }

    #[async_trait::async_trait]
    impl IAgentMetadataRepository for RowsAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
            Ok(self.rows.clone())
        }

        async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self.rows.iter().find(|row| row.id == id).cloned())
        }

        async fn find_by_source_and_name(
            &self,
            agent_source: &str,
            name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.agent_source == agent_source && row.name == name)
                .cloned())
        }

        async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.agent_source == "builtin" && row.backend.as_deref() == Some(backend))
                .cloned())
        }

        async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
            Err(DbError::Init("not implemented".into()))
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
            _params: &aionui_db::models::UpdateAgentAvailabilitySnapshotParams<'_>,
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

    struct RowsTeamAssistantCatalog {
        rows: Vec<crate::ports::TeamAssistantCatalogEntry>,
    }

    #[async_trait::async_trait]
    impl crate::ports::TeamAssistantCatalogPort for RowsTeamAssistantCatalog {
        async fn list_team_selectable_assistants(
            &self,
        ) -> Result<Vec<crate::ports::TeamAssistantCatalogEntry>, TeamError> {
            Ok(self.rows.clone())
        }
    }

    fn team_assistant_entry(assistant_id: &str, name: &str, backend: &str) -> crate::ports::TeamAssistantCatalogEntry {
        crate::ports::TeamAssistantCatalogEntry {
            assistant_id: assistant_id.into(),
            name: name.into(),
            backend: backend.into(),
            description: String::new(),
            skills: Vec::new(),
        }
    }

    fn assistant_definition(assistant_id: &str, agent_id: &str) -> AssistantDefinitionRow {
        AssistantDefinitionRow {
            id: format!("def-{assistant_id}"),
            assistant_id: assistant_id.into(),
            source: "builtin".into(),
            owner_type: "system".into(),
            source_ref: Some(assistant_id.into()),
            name: assistant_id.into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: None,
            agent_id: agent_id.into(),
            rule_resource_type: "none".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        }
    }

    fn service_with_selectable_catalog(
        assistants: Vec<crate::ports::TeamAssistantCatalogEntry>,
        definitions: Vec<AssistantDefinitionRow>,
    ) -> Arc<TeamSessionService> {
        let (base, _, _, _) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        TeamSessionService::new(
            base.repo.clone(),
            base.agent_metadata_repo.clone(),
            Arc::new(RowsTeamAssistantCatalog { rows: assistants }),
            Arc::new(MultiAssistantDefinitionRepo { rows: definitions }),
            Arc::new(MultiAssistantOverlayRepo { rows: vec![] }),
            Arc::new(SingleProviderRepo {
                rows: vec![provider_row("openai", &["gpt-5-mini"])],
            }),
            base.conversation_port.clone(),
            base.projection_store.clone(),
            base.broadcaster.clone(),
            base.task_manager.clone(),
            base.turn_port.clone(),
            base.cancellation_port.clone(),
            base.backend_binary_path.clone(),
        )
    }

    #[test]
    fn parse_agent_type_accepts_top_level_supported_runtimes() {
        assert_eq!(parse_agent_type("acp").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("aionrs").unwrap(), AgentType::Aionrs);
    }

    #[test]
    fn parse_agent_type_rejects_deprecated_runtime_types() {
        for backend in ["codex", "gemini", "nanobot", "remote", "openclaw-gateway"] {
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
    fn session_mode_for_backend_uses_codex_metadata_agent_full_access() {
        let row = agent_metadata_row("codex", Some("agent-full-access"));

        assert_eq!(
            session_mode_for_backend("codex", AgentType::Acp, Some(&row)),
            "agent-full-access"
        );
    }

    #[test]
    fn session_mode_for_backend_falls_back_to_codex_agent_full_access() {
        assert_eq!(
            session_mode_for_backend("codex", AgentType::Acp, None),
            "agent-full-access"
        );
    }

    #[tokio::test]
    async fn list_team_selectable_assistants_uses_assistant_projection_catalog() {
        let (base, _, _, _) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let svc = TeamSessionService::new(
            base.repo.clone(),
            Arc::new(RowsAgentMetadataRepo { rows: vec![] }),
            Arc::new(RowsTeamAssistantCatalog {
                rows: vec![team_assistant_entry(
                    "assistant-unchecked",
                    "Unchecked Assistant",
                    "cursor",
                )],
            }),
            Arc::new(MultiAssistantDefinitionRepo { rows: vec![] }),
            Arc::new(MultiAssistantOverlayRepo { rows: vec![] }),
            Arc::new(SingleProviderRepo { rows: vec![] }),
            base.conversation_port.clone(),
            base.projection_store.clone(),
            base.broadcaster.clone(),
            base.task_manager.clone(),
            base.turn_port.clone(),
            base.cancellation_port.clone(),
            base.backend_binary_path.clone(),
        );

        let assistants = svc.list_team_selectable_assistants().await;
        let ids: Vec<&str> = assistants
            .iter()
            .map(|assistant| assistant.assistant_id.as_str())
            .collect();

        assert_eq!(ids, vec!["assistant-unchecked"]);
    }

    #[tokio::test]
    async fn create_team_rejects_assistant_missing_from_team_selectable_catalog() {
        let svc = service_with_selectable_catalog(vec![], vec![assistant_definition("word-creator", "aionrs")]);
        let mut req = single_agent_team_request("Catalog Gate");
        req.agents[0].assistant_id = Some("word-creator".into());
        req.agents[0].backend = None;
        req.agents[0].model = "gpt-5-mini".into();

        let err = svc
            .create_team("user1", req)
            .await
            .expect_err("assistant must be rejected when not team selectable");

        assert!(
            matches!(&err, TeamError::InvalidRequest(msg) if msg.contains("not available for team mode")),
            "expected team-selectable assistant error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn add_agent_rejects_assistant_missing_from_team_selectable_catalog() {
        let svc = service_with_selectable_catalog(vec![], vec![assistant_definition("word-creator", "aionrs")]);
        let created = svc
            .create_team("user1", single_agent_team_request("Catalog Gate Add"))
            .await
            .unwrap();

        let err = svc
            .add_agent(
                "user1",
                &created.id,
                aionui_api_types::AddAgentRequest {
                    name: "Worker".into(),
                    role: "teammate".into(),
                    backend: None,
                    model: "gpt-5-mini".into(),
                    assistant_id: Some("word-creator".into()),
                },
            )
            .await
            .expect_err("assistant must be rejected when not team selectable");

        assert!(
            matches!(&err, TeamError::InvalidRequest(msg) if msg.contains("not available for team mode")),
            "expected team-selectable assistant error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_spawn_backend_and_model_rejects_assistant_missing_from_team_selectable_catalog() {
        let svc = service_with_selectable_catalog(vec![], vec![assistant_definition("word-creator", "aionrs")]);

        let err = svc
            .resolve_spawn_backend_and_model(Some("word-creator"), None, "gemini", "gemini-2.5-pro")
            .await
            .expect_err("spawn must reject assistants outside the team-selectable catalog");

        assert!(
            matches!(&err, TeamError::InvalidRequest(msg) if msg.contains("not available for team mode")),
            "expected team-selectable assistant error, got {err:?}"
        );
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
            Arc::new(RowsTeamAssistantCatalog {
                rows: vec![team_assistant_entry("word-creator", "Word Creator", "aionrs")],
            }),
            Arc::new(SingleAssistantDefinitionRepo {
                row: AssistantDefinitionRow {
                    id: "def-1".into(),
                    assistant_id: "word-creator".into(),
                    source: "builtin".into(),
                    owner_type: "system".into(),
                    source_ref: Some("word-creator".into()),
                    name: "Word Creator".into(),
                    name_i18n: "{}".into(),
                    description: None,
                    description_i18n: "{}".into(),
                    avatar_type: "emoji".into(),
                    avatar_value: None,
                    agent_id: "aionrs".into(),
                    rule_resource_type: "none".into(),
                    rule_resource_ref: None,
                    recommended_prompts: "[]".into(),
                    recommended_prompts_i18n: "{}".into(),
                    default_model_mode: "auto".into(),
                    default_model_value: None,
                    default_permission_mode: "auto".into(),
                    default_permission_value: None,
                    default_thought_level_mode: "auto".into(),
                    default_thought_level_value: None,
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
