use std::sync::Arc;

use aionui_api_types::{
    ChannelAssistantSettingRequest, ChannelAssistantSettingResponse, ChannelDefaultModelSetting,
    ChannelPlatformSettingsResponse,
};
use aionui_common::ProviderWithModel;
use aionui_db::{
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository, IClientPreferenceRepository,
    resolve_agent_binding_from_rows,
};
use tracing::debug;

use crate::error::ChannelError;
use crate::types::PluginType;

const DEFAULT_AGENT_TYPE: &str = "aionrs";

/// Per-plugin agent/model configuration read from `client_preferences`.
///
/// Keys follow the pattern established by the old Electron frontend:
/// - `assistant.{platform}.agent`       → JSON `{"backend":"claude","name":"Claude"}`
/// - `assistant.{platform}.defaultModel` → JSON `{"id":"provider_id","use_model":"model_name"}`
pub struct ChannelSettingsService {
    pref_repo: Arc<dyn IClientPreferenceRepository>,
    agent_metadata_repo: Option<Arc<dyn IAgentMetadataRepository>>,
    assistant_definition_repo: Option<Arc<dyn IAssistantDefinitionRepository>>,
    assistant_overlay_repo: Option<Arc<dyn IAssistantOverlayRepository>>,
}

/// Resolved agent configuration for a channel platform.
///
/// `backend` is only meaningful for ACP agents (claude, gemini, codex, …).
/// Non-ACP agent types (aionrs, nanobot, remote, …) have `backend = None`.
#[derive(Debug, Clone)]
pub struct ResolvedAgentConfig {
    pub agent_type: String,
    pub backend: Option<String>,
}

/// Resolved model configuration for a channel platform.
#[derive(Debug, Clone)]
pub struct ResolvedModelConfig {
    pub provider_id: String,
    pub model: String,
    pub use_model: Option<String>,
}

impl ChannelSettingsService {
    pub fn new(pref_repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self {
            pref_repo,
            agent_metadata_repo: None,
            assistant_definition_repo: None,
            assistant_overlay_repo: None,
        }
    }

    pub fn with_agent_metadata_repo(mut self, agent_metadata_repo: Arc<dyn IAgentMetadataRepository>) -> Self {
        self.agent_metadata_repo = Some(agent_metadata_repo);
        self
    }

    pub fn with_assistant_repos(
        mut self,
        assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
    ) -> Self {
        self.assistant_definition_repo = Some(assistant_definition_repo);
        self.assistant_overlay_repo = Some(assistant_overlay_repo);
        self
    }

    /// Reads the agent configuration for a platform from `client_preferences`.
    ///
    /// Supports two data formats:
    /// - **New:** `{"agent_type":"acp","backend":"claude","name":"Claude"}`
    /// - **Legacy:** `{"backend":"claude","name":"Claude"}` (no agent_type field)
    ///
    /// Falls back to `agent_type=aionrs, backend=None` when no config exists.
    pub async fn get_agent_config(&self, platform: PluginType) -> Result<ResolvedAgentConfig, ChannelError> {
        let key = agent_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(default_agent_config());
        };

        if let Some(setting) = parse_channel_assistant_setting(&pref.value) {
            if let Some(assistant_id) = setting.assistant_id.as_deref() {
                if let Some(resolved) = self.resolve_assistant_agent_config(assistant_id).await? {
                    debug!(
                        platform = %platform,
                        assistant_id,
                        agent_type = %resolved.agent_type,
                        backend = ?resolved.backend,
                        "resolved channel agent config from assistant identity"
                    );
                    return Ok(resolved);
                }

                return Err(ChannelError::InvalidConfig(format!(
                    "Channel assistant binding references unresolved assistant identity: {assistant_id}"
                )));
            }

            if let Some(at) = setting.agent_type.as_deref() {
                let backend = if at == "acp" { setting.backend.clone() } else { None };

                debug!(platform = %platform, agent_type = %at, backend = ?backend, "resolved channel agent config (new format)");

                return Ok(ResolvedAgentConfig {
                    agent_type: at.to_owned(),
                    backend,
                });
            }

            if let Some(raw_backend) = setting.backend.as_deref() {
                let raw_backend = raw_backend.to_owned();
                let agent_type = backend_to_agent_type(&raw_backend);
                let backend = if agent_type == "acp" { Some(raw_backend) } else { None };

                debug!(
                    platform = %platform,
                    agent_type = %agent_type,
                    backend = ?backend,
                    "resolved channel agent config (legacy format)"
                );

                return Ok(ResolvedAgentConfig { agent_type, backend });
            }
        }

        Ok(default_agent_config())
    }

    /// Reads the model configuration for a platform from `client_preferences`.
    ///
    /// Returns `None` when no model is configured (common for ACP agents).
    pub async fn get_model_config(&self, platform: PluginType) -> Result<Option<ResolvedModelConfig>, ChannelError> {
        let key = model_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return Ok(None);
        };

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).unwrap_or_default();

        let provider_id = parsed["id"].as_str().unwrap_or_default().to_owned();
        let use_model = parsed["use_model"].as_str().map(|s| s.to_owned());

        if provider_id.is_empty() && use_model.is_none() {
            return Ok(None);
        }

        debug!(platform = %platform, provider_id = %provider_id, use_model = ?use_model, "resolved channel model config");

        Ok(Some(ResolvedModelConfig {
            provider_id: provider_id.clone(),
            model: use_model.clone().unwrap_or_default(),
            use_model,
        }))
    }

    pub async fn get_platform_settings(
        &self,
        platform: PluginType,
    ) -> Result<ChannelPlatformSettingsResponse, ChannelError> {
        let key_agent = agent_key(platform);
        let key_model = model_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key_agent, &key_model]).await?;

        let mut assistant = None;
        let mut default_model = None;

        for pref in prefs {
            if pref.key == key_agent {
                if let Some(parsed) = parse_channel_assistant_setting(&pref.value) {
                    assistant = Some(self.normalize_channel_assistant_setting_for_response(parsed).await?);
                }
            } else if pref.key == key_model {
                default_model = parse_channel_model_setting(&pref.value);
            }
        }

        if assistant.is_none() {
            assistant = self.resolve_default_channel_assistant_setting().await?;
        }

        Ok(ChannelPlatformSettingsResponse {
            platform: platform.to_string(),
            assistant,
            default_model,
        })
    }

    pub async fn get_assistant_setting(
        &self,
        platform: PluginType,
    ) -> Result<Option<ChannelAssistantSettingResponse>, ChannelError> {
        let key = agent_key(platform);
        let prefs = self.pref_repo.get_by_keys(&[&key]).await?;

        let Some(pref) = prefs.into_iter().next() else {
            return self.resolve_default_channel_assistant_setting().await;
        };

        let parsed = if let Some(assistant) = parse_channel_assistant_setting(&pref.value) {
            Some(self.normalize_channel_assistant_setting_for_response(assistant).await?)
        } else {
            None
        };

        Ok(parsed)
    }

    pub async fn set_assistant_setting(
        &self,
        platform: PluginType,
        assistant: &ChannelAssistantSettingRequest,
    ) -> Result<(), ChannelError> {
        let normalized = normalize_channel_assistant_setting_for_write(assistant);
        let payload = serde_json::to_string(&normalized).map_err(ChannelError::Json)?;
        let key = agent_key(platform);
        self.pref_repo.upsert_batch(&[(&key, payload.as_str())]).await?;
        Ok(())
    }

    pub async fn set_model_setting(
        &self,
        platform: PluginType,
        model: &ChannelDefaultModelSetting,
    ) -> Result<(), ChannelError> {
        let payload = serde_json::to_string(model).map_err(ChannelError::Json)?;
        let key = model_key(platform);
        self.pref_repo.upsert_batch(&[(&key, payload.as_str())]).await?;
        Ok(())
    }

    async fn resolve_assistant_agent_config(
        &self,
        assistant_id: &str,
    ) -> Result<Option<ResolvedAgentConfig>, ChannelError> {
        let (Some(definition_repo), Some(overlay_repo)) =
            (&self.assistant_definition_repo, &self.assistant_overlay_repo)
        else {
            return Ok(None);
        };

        let Some(definition) = definition_repo.get_by_assistant_id(assistant_id).await? else {
            return Ok(None);
        };

        let agent_id = overlay_repo
            .get(&definition.id)
            .await?
            .and_then(|row| row.agent_id_override)
            .unwrap_or(definition.agent_id);
        let agent_backend = self.runtime_backend_for_agent_id(&agent_id).await?;
        let agent_type = backend_to_agent_type(&agent_backend);
        let backend = if agent_type == "acp" { Some(agent_backend) } else { None };

        Ok(Some(ResolvedAgentConfig { agent_type, backend }))
    }

    async fn resolve_assistant_identity_for_legacy_binding(
        &self,
        assistant: &ChannelAssistantSettingResponse,
    ) -> Result<Option<String>, ChannelError> {
        let (Some(definition_repo), Some(_overlay_repo)) =
            (&self.assistant_definition_repo, &self.assistant_overlay_repo)
        else {
            return Ok(None);
        };

        let legacy_backend = assistant
            .backend
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| assistant.agent_type.as_deref().filter(|value| !value.trim().is_empty()));

        let Some(legacy_backend) = legacy_backend else {
            return Ok(None);
        };

        let definitions = definition_repo.list().await?;

        for definition in definitions {
            if definition.source != "generated" {
                continue;
            }
            let runtime_backend = self.runtime_backend_for_agent_id(&definition.agent_id).await?;
            if runtime_backend == legacy_backend {
                return Ok(Some(definition.assistant_id));
            }
        }

        Ok(None)
    }

    async fn normalize_channel_assistant_setting_for_response(
        &self,
        assistant: ChannelAssistantSettingResponse,
    ) -> Result<ChannelAssistantSettingResponse, ChannelError> {
        let assistant_id = assistant
            .assistant_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                assistant
                    .custom_agent_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(ToOwned::to_owned)
            });

        let canonical_assistant_id = if assistant_id.is_some() {
            assistant_id
        } else {
            self.resolve_assistant_identity_for_legacy_binding(&assistant).await?
        };

        if canonical_assistant_id.is_some() {
            Ok(ChannelAssistantSettingResponse {
                assistant_id: canonical_assistant_id,
                custom_agent_id: None,
                backend: None,
                agent_type: None,
                name: assistant.name,
            })
        } else {
            Ok(assistant)
        }
    }

    async fn resolve_default_channel_assistant_setting(
        &self,
    ) -> Result<Option<ChannelAssistantSettingResponse>, ChannelError> {
        let Some(assistant_id) = self.resolve_default_assistant_identity().await? else {
            return Ok(None);
        };

        Ok(Some(ChannelAssistantSettingResponse {
            assistant_id: Some(assistant_id),
            custom_agent_id: None,
            backend: None,
            agent_type: None,
            name: None,
        }))
    }

    async fn resolve_default_assistant_identity(&self) -> Result<Option<String>, ChannelError> {
        let (Some(definition_repo), Some(overlay_repo)) =
            (&self.assistant_definition_repo, &self.assistant_overlay_repo)
        else {
            return Ok(None);
        };

        let definitions = definition_repo.list().await?;
        let overlays = overlay_repo.list().await?;

        for definition in definitions.iter().filter(|definition| definition.source == "generated") {
            if self.effective_assistant_backend(definition, &overlays).await? == DEFAULT_AGENT_TYPE {
                return Ok(Some(definition.assistant_id.clone()));
            }
        }

        let mut any_aionrs = None;
        for definition in &definitions {
            if self.effective_assistant_backend(definition, &overlays).await? == DEFAULT_AGENT_TYPE {
                any_aionrs = Some(definition);
                break;
            }
        }
        if let Some(definition) = any_aionrs {
            return Ok(Some(definition.assistant_id.clone()));
        }

        Ok(None)
    }

    async fn effective_assistant_backend(
        &self,
        definition: &aionui_db::models::AssistantDefinitionRow,
        overlays: &[aionui_db::models::AssistantOverlayRow],
    ) -> Result<String, ChannelError> {
        let agent_id = overlays
            .iter()
            .find(|overlay| overlay.assistant_definition_id == definition.id)
            .and_then(|overlay| overlay.agent_id_override.as_deref())
            .unwrap_or(definition.agent_id.as_str());
        self.runtime_backend_for_agent_id(agent_id).await
    }

    async fn runtime_backend_for_agent_id(&self, agent_id: &str) -> Result<String, ChannelError> {
        let Some(agent_metadata_repo) = self.agent_metadata_repo.as_ref() else {
            return Ok(agent_id.to_owned());
        };
        let rows = agent_metadata_repo.list_all().await?;
        Ok(resolve_agent_binding_from_rows(&rows, agent_id)
            .map(|binding| binding.runtime_backend)
            .unwrap_or_else(|| agent_id.to_owned()))
    }
}

fn agent_key(platform: PluginType) -> String {
    format!("assistant.{platform}.agent")
}

fn model_key(platform: PluginType) -> String {
    format!("assistant.{platform}.defaultModel")
}

fn default_agent_config() -> ResolvedAgentConfig {
    ResolvedAgentConfig {
        agent_type: DEFAULT_AGENT_TYPE.to_owned(),
        backend: None,
    }
}

fn parse_channel_assistant_setting(value: &str) -> Option<ChannelAssistantSettingResponse> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;

    if let Some(raw) = parsed.as_str() {
        return Some(ChannelAssistantSettingResponse {
            assistant_id: None,
            custom_agent_id: None,
            backend: Some(raw.to_owned()),
            agent_type: Some(backend_to_agent_type(raw)),
            name: None,
        });
    }

    Some(ChannelAssistantSettingResponse {
        assistant_id: parsed["assistant_id"].as_str().map(|s| s.to_owned()),
        custom_agent_id: parsed["custom_agent_id"].as_str().map(|s| s.to_owned()),
        backend: parsed["backend"].as_str().map(|s| s.to_owned()),
        agent_type: parsed["agent_type"].as_str().map(|s| s.to_owned()),
        name: parsed["name"].as_str().map(|s| s.to_owned()),
    })
}

fn normalize_channel_assistant_setting_for_write(
    assistant: &ChannelAssistantSettingRequest,
) -> ChannelAssistantSettingResponse {
    ChannelAssistantSettingResponse {
        assistant_id: Some(assistant.assistant_id.trim().to_owned()),
        custom_agent_id: None,
        backend: None,
        agent_type: None,
        name: assistant.name.clone(),
    }
}

fn parse_channel_model_setting(value: &str) -> Option<ChannelDefaultModelSetting> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    let id = parsed["id"].as_str()?.to_owned();
    let use_model = parsed["use_model"].as_str()?.to_owned();
    Some(ChannelDefaultModelSetting { id, use_model })
}

/// Maps a backend identifier to the corresponding `AgentType` serde name.
///
/// ACP-style backends (claude, gemini, codex, etc.) all map to "acp".
/// Non-ACP backends map to their specific agent type.
fn backend_to_agent_type(backend: &str) -> String {
    match backend {
        "aionrs" | "aion-cli" => "aionrs".to_owned(),
        "openclaw-gateway" => "openclaw-gateway".to_owned(),
        "nanobot" => "nanobot".to_owned(),
        "remote" => "remote".to_owned(),
        _ => {
            // All ACP-compatible backends: claude, gemini, codex, codebuddy, opencode, qwen, copilot, droid, kimi, etc.
            "acp".to_owned()
        }
    }
}

/// Builds a `ProviderWithModel` from the resolved config, or returns
/// the empty default when no model is configured.
pub fn resolved_model_to_provider(model: Option<&ResolvedModelConfig>) -> ProviderWithModel {
    match model {
        Some(m) => ProviderWithModel {
            provider_id: m.provider_id.clone(),
            model: m.model.clone(),
            use_model: m.use_model.clone(),
        },
        None => ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::DbError;
    use aionui_db::models::{
        AssistantDefinitionRow, AssistantOverlayRow, ClientPreference, UpsertAssistantDefinitionParams,
        UpsertAssistantOverlayParams,
    };
    use aionui_db::{IAssistantDefinitionRepository, IAssistantOverlayRepository};
    use std::sync::Mutex;

    struct MockPrefRepo {
        data: Mutex<Vec<(String, String)>>,
    }

    impl MockPrefRepo {
        fn new() -> Self {
            Self {
                data: Mutex::new(Vec::new()),
            }
        }

        fn with_data(entries: Vec<(&str, &str)>) -> Self {
            Self {
                data: Mutex::new(entries.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for MockPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            let data = self.data.lock().unwrap();
            Ok(data
                .iter()
                .map(|(k, v)| ClientPreference {
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }

        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            let data = self.data.lock().unwrap();
            Ok(data
                .iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .map(|(k, v)| ClientPreference {
                    key: k.clone(),
                    value: v.clone(),
                    updated_at: 0,
                })
                .collect())
        }

        async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError> {
            let mut data = self.data.lock().unwrap();
            for (key, value) in entries {
                if let Some(existing) = data.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = value.to_string();
                } else {
                    data.push((key.to_string(), value.to_string()));
                }
            }
            Ok(())
        }

        async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError> {
            let mut data = self.data.lock().unwrap();
            data.retain(|(k, _)| !keys.contains(&k.as_str()));
            Ok(())
        }
    }

    struct MockAssistantDefinitionRepo {
        rows: Vec<AssistantDefinitionRow>,
    }

    #[async_trait::async_trait]
    impl IAssistantDefinitionRepository for MockAssistantDefinitionRepo {
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
            source: &str,
            source_ref: &str,
        ) -> Result<Option<AssistantDefinitionRow>, DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.source == source && row.source_ref.as_deref() == Some(source_ref))
                .cloned())
        }

        async fn upsert(
            &self,
            _params: &UpsertAssistantDefinitionParams<'_>,
        ) -> Result<AssistantDefinitionRow, DbError> {
            panic!("unused in channel settings tests")
        }

        async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
            panic!("unused in channel settings tests")
        }
    }

    struct MockAssistantOverlayRepo {
        rows: Vec<AssistantOverlayRow>,
    }

    #[async_trait::async_trait]
    impl IAssistantOverlayRepository for MockAssistantOverlayRepo {
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
            panic!("unused in channel settings tests")
        }

        async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
            panic!("unused in channel settings tests")
        }
    }

    fn make_definition(assistant_id: &str, agent_id: &str) -> AssistantDefinitionRow {
        AssistantDefinitionRow {
            id: format!("def-{assistant_id}"),
            assistant_id: assistant_id.to_owned(),
            source: "generated".to_owned(),
            owner_type: "system".to_owned(),
            source_ref: Some(assistant_id.to_owned()),
            name: assistant_id.to_owned(),
            name_i18n: "{}".to_owned(),
            description: None,
            description_i18n: "{}".to_owned(),
            avatar_type: "emoji".to_owned(),
            avatar_value: None,
            agent_id: agent_id.to_owned(),
            rule_resource_type: "user_file".to_owned(),
            rule_resource_ref: None,
            recommended_prompts: "[]".to_owned(),
            recommended_prompts_i18n: "{}".to_owned(),
            default_model_mode: "auto".to_owned(),
            default_model_value: None,
            default_permission_mode: "auto".to_owned(),
            default_permission_value: None,
            default_thought_level_mode: "auto".to_owned(),
            default_thought_level_value: None,
            default_skills_mode: "auto".to_owned(),
            default_skill_ids: "[]".to_owned(),
            custom_skill_names: "[]".to_owned(),
            default_disabled_builtin_skill_ids: "[]".to_owned(),
            default_mcps_mode: "auto".to_owned(),
            default_mcp_ids: "[]".to_owned(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        }
    }

    fn make_overlay(definition_id: &str, agent_id_override: &str) -> AssistantOverlayRow {
        AssistantOverlayRow {
            assistant_definition_id: definition_id.to_owned(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some(agent_id_override.to_owned()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    // ── backend_to_agent_type ─────────────────────────────────────────

    #[test]
    fn acp_backends_map_to_acp() {
        for backend in &[
            "claude",
            "gemini",
            "codex",
            "codebuddy",
            "opencode",
            "qwen",
            "copilot",
            "droid",
            "kimi",
        ] {
            assert_eq!(backend_to_agent_type(backend), "acp", "backend: {backend}");
        }
    }

    #[test]
    fn aionrs_backends_map_to_aionrs() {
        assert_eq!(backend_to_agent_type("aionrs"), "aionrs");
        assert_eq!(backend_to_agent_type("aion-cli"), "aionrs");
    }

    #[test]
    fn non_acp_backends_map_correctly() {
        assert_eq!(backend_to_agent_type("openclaw-gateway"), "openclaw-gateway");
        assert_eq!(backend_to_agent_type("nanobot"), "nanobot");
        assert_eq!(backend_to_agent_type("remote"), "remote");
    }

    #[test]
    fn unknown_backend_defaults_to_acp() {
        assert_eq!(backend_to_agent_type("unknown"), "acp");
    }

    // ── get_agent_config ──────────────────────────────────────────────

    #[tokio::test]
    async fn agent_config_returns_default_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "aionrs");
        assert!(config.backend.is_none());
    }

    #[tokio::test]
    async fn agent_config_reads_acp_from_preferences() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn agent_config_aionrs_has_no_backend() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"backend":"aionrs","name":"Aion CLI"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Lark).await.unwrap();
        assert_eq!(config.agent_type, "aionrs");
        assert!(config.backend.is_none());
    }

    // ── get_agent_config (new format) ──────────────────────────────────

    #[tokio::test]
    async fn agent_config_reads_new_format_acp() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"agent_type":"acp","backend":"claude","name":"Claude"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn agent_config_reads_new_format_aionrs() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"agent_type":"aionrs","name":"Aion CLI"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Lark).await.unwrap();
        assert_eq!(config.agent_type, "aionrs");
        assert!(config.backend.is_none());
    }

    #[tokio::test]
    async fn agent_config_reads_new_format_openclaw() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.weixin.agent",
            r#"{"agent_type":"openclaw-gateway","name":"OpenClaw"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_agent_config(PluginType::Weixin).await.unwrap();
        assert_eq!(config.agent_type, "openclaw-gateway");
        assert!(config.backend.is_none());
    }

    #[tokio::test]
    async fn agent_config_resolves_backend_from_assistant_identity() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"assistant_id":"bare-claude","name":"Claude"}"#,
        )]));
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![make_definition("bare-claude", "claude")],
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn agent_config_prefers_overlay_backend_for_assistant_identity() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"assistant_id":"bare-claude","name":"Claude"}"#,
        )]));
        let definition = make_definition("bare-claude", "claude");
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![definition.clone()],
        });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(MockAssistantOverlayRepo {
            rows: vec![make_overlay(&definition.id, "codex")],
        });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let config = svc.get_agent_config(PluginType::Telegram).await.unwrap();
        assert_eq!(config.agent_type, "acp");
        assert_eq!(config.backend.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn agent_config_errors_when_assistant_identity_cannot_resolve() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"assistant_id":"missing-assistant","name":"Missing"}"#,
        )]));
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
            Arc::new(MockAssistantDefinitionRepo { rows: vec![] });
        let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let err = svc.get_agent_config(PluginType::Telegram).await.unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));
        assert!(
            err.to_string().contains("missing-assistant"),
            "error should name the unresolved assistant identity"
        );
    }

    // ── get_model_config ──────────────────────────────────────────────

    #[tokio::test]
    async fn model_config_returns_none_when_no_pref() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Telegram).await.unwrap();
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn model_config_reads_from_preferences() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.weixin.defaultModel",
            r#"{"id":"490fdb4e","use_model":"global.anthropic.claude-opus-4-6-v1"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Weixin).await.unwrap().unwrap();
        assert_eq!(config.provider_id, "490fdb4e");
        assert_eq!(config.use_model.as_deref(), Some("global.anthropic.claude-opus-4-6-v1"));
    }

    #[tokio::test]
    async fn model_config_returns_none_for_empty_values() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.defaultModel",
            r#"{"id":"","use_model":null}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let config = svc.get_model_config(PluginType::Telegram).await.unwrap();
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn set_assistant_setting_persists_assistant_only_payload() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo.clone());

        svc.set_assistant_setting(
            PluginType::Telegram,
            &ChannelAssistantSettingRequest {
                assistant_id: "assistant-1".into(),
                name: Some("Claude".into()),
            },
        )
        .await
        .unwrap();

        let stored = repo.get_by_keys(&["assistant.telegram.agent"]).await.unwrap();
        let payload = serde_json::from_str::<serde_json::Value>(&stored[0].value).unwrap();

        assert_eq!(payload["assistant_id"], "assistant-1");
        assert_eq!(payload["name"], "Claude");
        assert!(payload.get("custom_agent_id").is_none());
        assert!(payload.get("backend").is_none());
        assert!(payload.get("agent_type").is_none());
    }

    #[tokio::test]
    async fn set_assistant_setting_trims_assistant_id_before_persisting() {
        let repo = Arc::new(MockPrefRepo::new());
        let svc = ChannelSettingsService::new(repo.clone());

        svc.set_assistant_setting(
            PluginType::Lark,
            &ChannelAssistantSettingRequest {
                assistant_id: "  legacy-custom  ".into(),
                name: Some("Codex".into()),
            },
        )
        .await
        .unwrap();

        let stored = repo.get_by_keys(&["assistant.lark.agent"]).await.unwrap();
        let payload = serde_json::from_str::<serde_json::Value>(&stored[0].value).unwrap();

        assert_eq!(payload["assistant_id"], "legacy-custom");
        assert_eq!(payload["name"], "Codex");
        assert!(payload.get("custom_agent_id").is_none());
        assert!(payload.get("backend").is_none());
        assert!(payload.get("agent_type").is_none());
    }

    #[tokio::test]
    async fn get_assistant_setting_promotes_legacy_custom_agent_id_in_response() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"custom_agent_id":"legacy-custom","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let setting = svc.get_assistant_setting(PluginType::Telegram).await.unwrap().unwrap();

        assert_eq!(setting.assistant_id.as_deref(), Some("legacy-custom"));
        assert!(setting.custom_agent_id.is_none());
        assert!(setting.backend.is_none());
        assert!(setting.agent_type.is_none());
        assert_eq!(setting.name.as_deref(), Some("Codex"));
    }

    #[tokio::test]
    async fn get_assistant_setting_defaults_to_generated_aionrs_assistant() {
        let repo = Arc::new(MockPrefRepo::new());
        let definition_repo = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![
                make_definition("bare-claude", "claude"),
                make_definition("bare-aionrs", "aionrs"),
            ],
        });
        let overlay_repo = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let setting = svc.get_assistant_setting(PluginType::Telegram).await.unwrap().unwrap();

        assert_eq!(setting.assistant_id.as_deref(), Some("bare-aionrs"));
        assert!(setting.custom_agent_id.is_none());
        assert!(setting.backend.is_none());
        assert!(setting.agent_type.is_none());
        assert!(setting.name.is_none());
    }

    #[tokio::test]
    async fn get_assistant_setting_preserves_backend_only_legacy_response() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let setting = svc.get_assistant_setting(PluginType::Lark).await.unwrap().unwrap();

        assert!(setting.assistant_id.is_none());
        assert!(setting.custom_agent_id.is_none());
        assert_eq!(setting.backend.as_deref(), Some("codex"));
        assert!(setting.agent_type.is_none());
        assert_eq!(setting.name.as_deref(), Some("Codex"));
    }

    #[tokio::test]
    async fn get_assistant_setting_canonicalizes_backend_only_legacy_response_when_assistant_repos_exist() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.lark.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let definition_repo = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![make_definition("bare-codex", "codex")],
        });
        let overlay_repo = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let setting = svc.get_assistant_setting(PluginType::Lark).await.unwrap().unwrap();

        assert_eq!(setting.assistant_id.as_deref(), Some("bare-codex"));
        assert!(setting.custom_agent_id.is_none());
        assert!(setting.backend.is_none());
        assert!(setting.agent_type.is_none());
        assert_eq!(setting.name.as_deref(), Some("Codex"));
    }

    #[tokio::test]
    async fn get_platform_settings_promotes_legacy_custom_agent_id_in_response() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"custom_agent_id":"legacy-custom","name":"Codex"}"#,
        )]));
        let svc = ChannelSettingsService::new(repo);

        let settings = svc.get_platform_settings(PluginType::Telegram).await.unwrap();
        let assistant = settings.assistant.expect("assistant settings");

        assert_eq!(assistant.assistant_id.as_deref(), Some("legacy-custom"));
        assert!(assistant.custom_agent_id.is_none());
        assert!(assistant.backend.is_none());
        assert!(assistant.agent_type.is_none());
        assert_eq!(assistant.name.as_deref(), Some("Codex"));
    }

    #[tokio::test]
    async fn get_platform_settings_defaults_to_generated_aionrs_assistant() {
        let repo = Arc::new(MockPrefRepo::new());
        let definition_repo = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![
                make_definition("bare-claude", "claude"),
                make_definition("bare-aionrs", "aionrs"),
            ],
        });
        let overlay_repo = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let settings = svc.get_platform_settings(PluginType::Telegram).await.unwrap();
        let assistant = settings.assistant.expect("assistant settings");

        assert_eq!(assistant.assistant_id.as_deref(), Some("bare-aionrs"));
        assert!(assistant.custom_agent_id.is_none());
        assert!(assistant.backend.is_none());
        assert!(assistant.agent_type.is_none());
        assert!(assistant.name.is_none());
    }

    #[tokio::test]
    async fn get_platform_settings_canonicalizes_backend_only_legacy_response_when_assistant_repos_exist() {
        let repo = Arc::new(MockPrefRepo::with_data(vec![(
            "assistant.telegram.agent",
            r#"{"backend":"codex","name":"Codex"}"#,
        )]));
        let definition_repo = Arc::new(MockAssistantDefinitionRepo {
            rows: vec![make_definition("bare-codex", "codex")],
        });
        let overlay_repo = Arc::new(MockAssistantOverlayRepo { rows: vec![] });
        let svc = ChannelSettingsService::new(repo).with_assistant_repos(definition_repo, overlay_repo);

        let settings = svc.get_platform_settings(PluginType::Telegram).await.unwrap();
        let assistant = settings.assistant.expect("assistant settings");

        assert_eq!(assistant.assistant_id.as_deref(), Some("bare-codex"));
        assert!(assistant.custom_agent_id.is_none());
        assert!(assistant.backend.is_none());
        assert!(assistant.agent_type.is_none());
        assert_eq!(assistant.name.as_deref(), Some("Codex"));
    }

    // ── resolved_model_to_provider ────────────────────────────────────

    #[test]
    fn resolved_model_converts_to_provider() {
        let model = ResolvedModelConfig {
            provider_id: "abc".into(),
            model: "gpt-5".into(),
            use_model: Some("gpt-5".into()),
        };
        let p = resolved_model_to_provider(Some(&model));
        assert_eq!(p.provider_id, "abc");
        assert_eq!(p.model, "gpt-5");
        assert_eq!(p.use_model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn none_model_produces_empty_provider() {
        let p = resolved_model_to_provider(None);
        assert!(p.provider_id.is_empty());
        assert!(p.model.is_empty());
        assert!(p.use_model.is_none());
    }
}
