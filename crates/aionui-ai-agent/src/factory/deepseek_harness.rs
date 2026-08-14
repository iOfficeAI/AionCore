use std::collections::HashMap;
use std::path::Path;

use aionui_common::{CommandSpec, EnvVar, ProviderWithModel};
use aionui_db::{IProviderRepository, models::Provider};
use sha2::{Digest, Sha256};

use crate::error::AgentError;

pub(crate) const BACKEND: &str = "deepseek-harness";

pub(crate) struct DeepseekHarnessLaunch {
    pub(crate) command_spec: CommandSpec,
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    pub(crate) enabled_models: Vec<String>,
}

struct LaunchContext<'a> {
    encryption_key: &'a [u8; 32],
    data_dir: &'a Path,
    user_id: &'a str,
    conversation_id: &'a str,
    workspace: &'a str,
}

pub(crate) async fn resolve_launch(
    provider_repo: &dyn IProviderRepository,
    encryption_key: &[u8; 32],
    data_dir: &Path,
    user_id: &str,
    conversation_id: &str,
    workspace: &str,
    requested_model: &ProviderWithModel,
) -> Result<DeepseekHarnessLaunch, AgentError> {
    let provider = default_provider(provider_repo, user_id).await?;
    let model_id = requested_model
        .use_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(&requested_model.model)
        .trim()
        .to_owned();
    let enabled_models = enabled_models(&provider)?;
    validate_model_in(&enabled_models, &model_id)?;
    build_launch(
        provider,
        model_id,
        enabled_models,
        LaunchContext {
            encryption_key,
            data_dir,
            user_id,
            conversation_id,
            workspace,
        },
    )
}

pub(crate) async fn resolve_probe_launch(
    provider_repo: &dyn IProviderRepository,
    encryption_key: &[u8; 32],
    data_dir: &Path,
    user_id: &str,
) -> Result<DeepseekHarnessLaunch, AgentError> {
    let provider = default_provider(provider_repo, user_id).await?;
    let enabled_models = enabled_models(&provider)?;
    let model_id = enabled_models
        .first()
        .cloned()
        .ok_or_else(|| AgentError::bad_request("DeepSeek provider has no enabled models"))?;
    let workspace = std::env::temp_dir().to_string_lossy().into_owned();
    build_launch(
        provider,
        model_id,
        enabled_models,
        LaunchContext {
            encryption_key,
            data_dir,
            user_id,
            conversation_id: "health-check",
            workspace: &workspace,
        },
    )
}

async fn default_provider(provider_repo: &dyn IProviderRepository, user_id: &str) -> Result<Provider, AgentError> {
    provider_repo
        .list(user_id)
        .await
        .map_err(|error| AgentError::internal(format!("Failed to load DeepSeek providers: {error}")))?
        .into_iter()
        .find(|provider| provider.enabled && provider.platform.eq_ignore_ascii_case("deepseek"))
        .ok_or_else(|| AgentError::bad_request("No enabled DeepSeek provider is configured"))
}

#[cfg(test)]
fn validate_model(provider: &Provider, model_id: &str) -> Result<(), AgentError> {
    validate_model_in(&enabled_models(provider)?, model_id)
}

fn validate_model_in(enabled_models: &[String], model_id: &str) -> Result<(), AgentError> {
    if model_id.is_empty() {
        return Err(AgentError::bad_request("DeepSeek Harness requires a model"));
    }
    if enabled_models.iter().any(|model| model == model_id) {
        return Ok(());
    }
    Err(AgentError::bad_request(format!(
        "Model '{model_id}' is not enabled for the default DeepSeek provider"
    )))
}

fn enabled_models(provider: &Provider) -> Result<Vec<String>, AgentError> {
    let models: Vec<String> = serde_json::from_str(&provider.models)
        .map_err(|error| AgentError::internal(format!("Invalid DeepSeek provider model list: {error}")))?;
    let enabled: HashMap<String, bool> = provider
        .model_enabled
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| AgentError::internal(format!("Invalid DeepSeek provider model state: {error}")))?
        .unwrap_or_default();
    Ok(models
        .into_iter()
        .filter(|model| !model.trim().is_empty() && enabled.get(model).copied().unwrap_or(true))
        .collect())
}

fn build_launch(
    provider: Provider,
    model_id: String,
    enabled_models: Vec<String>,
    context: LaunchContext<'_>,
) -> Result<DeepseekHarnessLaunch, AgentError> {
    let runtime = aionui_runtime::probe_deepseek_harness_runtime().ok_or_else(|| {
        AgentError::bad_request("DeepSeek Harness runtime is not installed; install it from Agent settings first")
    })?;
    let api_key = aionui_common::decrypt_string(&provider.api_key_encrypted, context.encryption_key)
        .map_err(|error| AgentError::internal(format!("Failed to decrypt DeepSeek provider credential: {error}")))?;
    if api_key.trim().is_empty() {
        return Err(AgentError::bad_request("DeepSeek provider API key is empty"));
    }

    let scope = scope_hash(context.user_id, context.conversation_id);
    let runtime_data = context.data_dir.join("deepseek-harness");
    let mut env = vec![
        EnvVar {
            name: "DEEPSEEK_API_KEY".to_owned(),
            value: api_key,
        },
        EnvVar {
            name: "AIONUI_DSH_MODEL".to_owned(),
            value: model_id.clone(),
        },
        EnvVar {
            name: "AIONUI_DSH_HOME".to_owned(),
            value: runtime_data
                .join("home")
                .join(scope_hash(context.user_id, "home"))
                .to_string_lossy()
                .into_owned(),
        },
        EnvVar {
            name: "AIONUI_DSH_SESSIONS_ROOT".to_owned(),
            value: runtime_data
                .join("sessions")
                .join(&scope)
                .to_string_lossy()
                .into_owned(),
        },
        EnvVar {
            name: "AIONUI_DSH_SPILL_ROOT".to_owned(),
            value: runtime_data.join("spill").join(&scope).to_string_lossy().into_owned(),
        },
    ];
    if !provider.base_url.trim().is_empty() {
        env.push(EnvVar {
            name: "DEEPSEEK_BASE_URL".to_owned(),
            value: provider.base_url.clone(),
        });
    }

    Ok(DeepseekHarnessLaunch {
        command_spec: CommandSpec {
            command: runtime.node_path,
            args: vec![
                runtime.entry_path.to_string_lossy().into_owned(),
                "--config".to_owned(),
                runtime.config_path.to_string_lossy().into_owned(),
            ],
            env,
            cwd: Some(context.workspace.to_owned()),
        },
        provider_id: provider.id,
        model_id,
        enabled_models,
    })
}

fn scope_hash(user_id: &str, conversation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update([0]);
    hasher.update(conversation_id.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(models: &str, enabled: Option<&str>) -> Provider {
        Provider {
            id: "p1".into(),
            user_id: "u1".into(),
            platform: "deepseek".into(),
            name: "DeepSeek".into(),
            base_url: String::new(),
            api_key_encrypted: "encrypted".into(),
            models: models.into(),
            enabled: true,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_enabled: enabled.map(str::to_owned),
            model_health: None,
            model_settings: "{}".into(),
            bedrock_config: None,
            is_full_url: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn arbitrary_enabled_model_is_passed_without_a_static_allowlist() {
        let provider = provider(r#"["custom-deepseek-model"]"#, None);
        assert!(validate_model(&provider, "custom-deepseek-model").is_ok());
    }

    #[test]
    fn explicitly_disabled_model_is_rejected() {
        let provider = provider(r#"["deepseek-chat"]"#, Some(r#"{"deepseek-chat":false}"#));
        assert!(validate_model(&provider, "deepseek-chat").is_err());
    }

    #[test]
    fn scope_paths_do_not_embed_user_controlled_identifiers() {
        let hash = scope_hash("../user", "../../conversation");
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
