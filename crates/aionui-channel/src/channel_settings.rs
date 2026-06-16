use std::sync::Arc;

use aionui_api_types::{ChannelAssistantSetting, ChannelDefaultModelSetting, ChannelPlatformSettingsResponse};
use aionui_common::ProviderWithModel;
use aionui_db::IClientPreferenceRepository;
use tracing::debug;

use crate::error::ChannelError;
use crate::types::PluginType;

const DEFAULT_BACKEND: &str = "aionrs";
const DEFAULT_AGENT_TYPE: &str = "aionrs";

/// Per-plugin agent/model configuration read from `client_preferences`.
///
/// Keys follow the pattern established by the old Electron frontend:
/// - `assistant.{platform}.agent`       → JSON `{"backend":"claude","name":"Claude"}`
/// - `assistant.{platform}.defaultModel` → JSON `{"id":"provider_id","use_model":"model_name"}`
pub struct ChannelSettingsService {
    pref_repo: Arc<dyn IClientPreferenceRepository>,
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
        Self { pref_repo }
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

        let parsed: serde_json::Value = serde_json::from_str(&pref.value).unwrap_or_default();

        if let Some(at) = parsed["agent_type"].as_str() {
            let backend = if at == "acp" {
                parsed["backend"].as_str().map(|s| s.to_owned())
            } else {
                None
            };

            debug!(platform = %platform, agent_type = %at, backend = ?backend, "resolved channel agent config (new format)");

            return Ok(ResolvedAgentConfig {
                agent_type: at.to_owned(),
                backend,
            });
        }

        let raw_backend = parsed["backend"].as_str().unwrap_or(DEFAULT_BACKEND).to_owned();
        let agent_type = backend_to_agent_type(&raw_backend);
        let backend = if agent_type == "acp" { Some(raw_backend) } else { None };

        debug!(platform = %platform, agent_type = %agent_type, backend = ?backend, "resolved channel agent config (legacy format)");

        Ok(ResolvedAgentConfig { agent_type, backend })
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
                assistant = parse_channel_assistant_setting(&pref.value);
            } else if pref.key == key_model {
                default_model = parse_channel_model_setting(&pref.value);
            }
        }

        Ok(ChannelPlatformSettingsResponse {
            platform: platform.to_string(),
            assistant,
            default_model,
        })
    }

    pub async fn set_assistant_setting(
        &self,
        platform: PluginType,
        assistant: &ChannelAssistantSetting,
    ) -> Result<(), ChannelError> {
        let payload = serde_json::to_string(assistant).map_err(ChannelError::Json)?;
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

fn parse_channel_assistant_setting(value: &str) -> Option<ChannelAssistantSetting> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;

    if let Some(raw) = parsed.as_str() {
        return Some(ChannelAssistantSetting {
            assistant_id: None,
            custom_agent_id: None,
            backend: Some(raw.to_owned()),
            agent_type: Some(backend_to_agent_type(raw)),
            name: None,
        });
    }

    Some(ChannelAssistantSetting {
        assistant_id: parsed["assistant_id"].as_str().map(|s| s.to_owned()),
        custom_agent_id: parsed["custom_agent_id"].as_str().map(|s| s.to_owned()),
        backend: parsed["backend"].as_str().map(|s| s.to_owned()),
        agent_type: parsed["agent_type"].as_str().map(|s| s.to_owned()),
        name: parsed["name"].as_str().map(|s| s.to_owned()),
    })
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
    use aionui_db::models::ClientPreference;
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
