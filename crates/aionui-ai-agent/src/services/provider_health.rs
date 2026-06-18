use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::AgentError;
use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::engine::AgentEngine;
use aion_agent::output::OutputSink;
use aion_agent::output::null_sink::NullSink;
use aion_config::config::{CliArgs, Config};
use aionui_api_types::{
    HealthStatus, ProviderHealthCheckErrorKind, ProviderHealthCheckRequest, ProviderHealthCheckResponse,
};
use aionui_db::{IProviderRepository, models::Provider};
use regex::Regex;
use tracing::{info, warn};

use crate::factory::aionrs::{map_aionrs_provider, resolve_aionrs_url_and_compat, resolve_bedrock_config};
use crate::types::AionrsResolvedConfig;

const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_CHECK_PROMPT: &str = "Reply with exactly OK.";
const HEALTH_CHECK_MSG_ID: &str = "provider-health-check";

pub struct ProviderHealthCheckService {
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    data_dir: PathBuf,
}

impl ProviderHealthCheckService {
    pub fn new(provider_repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32], data_dir: PathBuf) -> Self {
        Self {
            provider_repo,
            encryption_key,
            data_dir,
        }
    }

    pub async fn health_check(
        &self,
        req: ProviderHealthCheckRequest,
    ) -> Result<ProviderHealthCheckResponse, AgentError> {
        if req.provider_id.trim().is_empty() {
            return Err(AgentError::bad_request("provider_id is required"));
        }
        if req.model.trim().is_empty() {
            return Err(AgentError::bad_request("model is required"));
        }

        let provider_id = req.provider_id.trim();
        let model = req.model.trim();
        let row = self
            .provider_repo
            .find_by_id(provider_id)
            .await
            .map_err(|e| AgentError::internal(format!("Failed to load provider config: {e}")))?
            .ok_or_else(|| AgentError::bad_request(format!("Provider '{provider_id}' not found")))?;

        let config = self.resolve_probe_config(&row, model)?;
        run_probe(row.id, row.platform, config).await
    }

    fn resolve_probe_config(&self, row: &Provider, model_id: &str) -> Result<AionrsResolvedConfig, AgentError> {
        let api_key = aionui_common::decrypt_string(&row.api_key_encrypted, &self.encryption_key)
            .map_err(|e| AgentError::internal(e.to_string()))?;
        let provider = map_aionrs_provider(&row.platform, model_id, row.model_protocols.as_deref());
        let (base_url, compat_overrides) =
            resolve_aionrs_url_and_compat(&row.platform, &row.base_url, &provider, row.is_full_url);
        let bedrock_config = if row.platform == "bedrock" {
            resolve_bedrock_config(row.bedrock_config.as_deref())
        } else {
            None
        };

        Ok(AionrsResolvedConfig {
            provider,
            api_key,
            model: model_id.to_owned(),
            base_url,
            system_prompt: Some("You are a provider health probe. Reply with exactly OK and do not use tools.".into()),
            max_tokens: 16,
            max_turns: Some(1),
            max_malformed_tool_call_turns: Some(1),
            compat_overrides,
            session_directory: self.data_dir.join("aionrs-health-check-sessions"),
            session_mode: None,
            extra_mcp_servers: HashMap::new(),
            bedrock_config,
        })
    }
}

async fn run_probe(
    provider_id: String,
    platform: String,
    config_extra: AionrsResolvedConfig,
) -> Result<ProviderHealthCheckResponse, AgentError> {
    let started = Instant::now();
    let model = config_extra.model.clone();

    info!(
        provider_id = %provider_id,
        platform = %platform,
        model = %model,
        "Provider health check started"
    );

    let mut engine = match build_probe_engine(config_extra).await {
        Ok(engine) => engine,
        Err(error) => {
            let message = format!("Aionrs probe bootstrap failed: {error}");
            let response = unhealthy_response(provider_id, platform, model, started.elapsed(), message, None);
            log_health_check_result(&response);
            return Ok(response);
        }
    };

    match tokio::time::timeout(
        HEALTH_CHECK_TIMEOUT,
        engine.run(HEALTH_CHECK_PROMPT, HEALTH_CHECK_MSG_ID),
    )
    .await
    {
        Ok(Ok(_)) => {
            let response = ProviderHealthCheckResponse {
                provider_id,
                platform,
                model,
                status: HealthStatus::Healthy,
                elapsed_ms: elapsed_ms(started.elapsed()),
                message: None,
                error_kind: None,
                http_status: None,
                timeout_stage: None,
            };
            log_health_check_result(&response);
            Ok(response)
        }
        Ok(Err(error)) => {
            let message = error.to_string();
            let response = unhealthy_response(provider_id, platform, model, started.elapsed(), message, None);
            log_health_check_result(&response);
            Ok(response)
        }
        Err(_) => {
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                format!("Health check timeout ({}s)", HEALTH_CHECK_TIMEOUT.as_secs()),
                Some("engine_run".into()),
            );
            log_health_check_result(&response);
            Ok(response)
        }
    }
}

fn log_health_check_result(response: &ProviderHealthCheckResponse) {
    match response.status {
        HealthStatus::Healthy => info!(
            provider_id = %response.provider_id,
            platform = %response.platform,
            model = %response.model,
            elapsed_ms = response.elapsed_ms,
            "Provider health check succeeded"
        ),
        HealthStatus::Unhealthy | HealthStatus::Unknown => warn!(
            provider_id = %response.provider_id,
            platform = %response.platform,
            model = %response.model,
            elapsed_ms = response.elapsed_ms,
            error_kind = ?response.error_kind,
            http_status = ?response.http_status,
            timeout_stage = ?response.timeout_stage,
            "Provider health check failed"
        ),
    }
}

async fn build_probe_engine(config_extra: AionrsResolvedConfig) -> Result<AgentEngine, AgentError> {
    let workspace = config_extra
        .session_directory
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sink: Arc<dyn OutputSink> = Arc::new(NullSink);
    let cli_args = CliArgs {
        provider: Some(config_extra.provider),
        api_key: Some(config_extra.api_key),
        base_url: config_extra.base_url,
        model: Some(config_extra.model),
        max_tokens: Some(config_extra.max_tokens),
        max_turns: config_extra.max_turns,
        max_malformed_tool_call_turns: config_extra.max_malformed_tool_call_turns,
        system_prompt: config_extra.system_prompt,
        profile: None,
        auto_approve: false,
        project_dir: Some(PathBuf::from(&workspace)),
    };
    let mut config =
        Config::resolve(&cli_args).map_err(|error| AgentError::internal(format!("Config resolve failed: {error}")))?;

    config.bedrock = config_extra.bedrock_config;
    config.session.enabled = false;
    config.mcp.servers.clear();
    config.file_cache.enabled = false;
    if let Some(field) = config_extra.compat_overrides.max_tokens_field {
        config.compat.max_tokens_field = Some(field);
    }
    if let Some(path) = config_extra.compat_overrides.api_path {
        config.compat.api_path = Some(path);
    }

    AgentBootstrap::new(config, workspace, sink)
        .build()
        .await
        .map(|result| result.engine)
        .map_err(|error| AgentError::internal(error.to_string()))
}

fn unhealthy_response(
    provider_id: String,
    platform: String,
    model: String,
    elapsed: Duration,
    message: String,
    timeout_stage: Option<String>,
) -> ProviderHealthCheckResponse {
    let error_kind = classify_error(&message, timeout_stage.is_some());
    let http_status = extract_http_status(&message);
    ProviderHealthCheckResponse {
        provider_id,
        platform,
        model,
        status: HealthStatus::Unhealthy,
        elapsed_ms: elapsed_ms(elapsed),
        message: Some(message),
        error_kind: Some(error_kind),
        http_status,
        timeout_stage,
    }
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub(crate) fn classify_error(message: &str, is_timeout: bool) -> ProviderHealthCheckErrorKind {
    if is_timeout {
        return ProviderHealthCheckErrorKind::Timeout;
    }

    let lower = message.to_lowercase();
    if lower.contains("invalid authorization header") || lower.contains("invalid x-api-key header") {
        return ProviderHealthCheckErrorKind::InvalidAuthorizationHeader;
    }
    if lower.contains("rate limited") || lower.contains(" 429") || lower.contains("api error 429") {
        return ProviderHealthCheckErrorKind::RateLimited;
    }
    if lower.contains("insufficient_quota")
        || lower.contains("insufficient quota")
        || lower.contains("credit balance is too low")
        || lower.contains("billing")
    {
        return ProviderHealthCheckErrorKind::InsufficientQuota;
    }
    if lower.contains("aws credential")
        || lower.contains("loading credentials")
        || lower.contains("invalid refresh token")
        || lower.contains("session token not found")
    {
        return ProviderHealthCheckErrorKind::AwsCredentials;
    }
    if lower.contains("api error 401") || lower.contains("unauthorized") || lower.contains("invalid api key") {
        return ProviderHealthCheckErrorKind::Unauthorized;
    }
    if lower.contains("api error 403") || lower.contains("forbidden") {
        return ProviderHealthCheckErrorKind::Forbidden;
    }
    if lower.contains("api error 404") || lower.contains("not found") {
        return ProviderHealthCheckErrorKind::NotFound;
    }
    if lower.contains("api error 400") || lower.contains("invalid_request") || lower.contains("invalid request") {
        return ProviderHealthCheckErrorKind::InvalidRequest;
    }
    if lower.contains("connection error") || lower.contains("http error") {
        return ProviderHealthCheckErrorKind::ConnectionError;
    }
    if lower.contains("api error") || lower.contains("provider error") {
        return ProviderHealthCheckErrorKind::ApiError;
    }

    ProviderHealthCheckErrorKind::Unknown
}

pub(crate) fn extract_http_status(message: &str) -> Option<u16> {
    let re = Regex::new(r"(?i)api error\s+(\d{3})").ok()?;
    re.captures(message)
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_detects_quota_message() {
        let message = r#"Provider error: API error 400: {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low"}}"#;
        assert_eq!(
            classify_error(message, false),
            ProviderHealthCheckErrorKind::InsufficientQuota
        );
        assert_eq!(extract_http_status(message), Some(400));
    }

    #[test]
    fn classify_error_detects_invalid_header() {
        assert_eq!(
            classify_error(
                "Connection error: Invalid authorization header: invalid header value",
                false
            ),
            ProviderHealthCheckErrorKind::InvalidAuthorizationHeader
        );
    }

    #[test]
    fn classify_error_detects_aws_credentials() {
        assert_eq!(
            classify_error(
                "Provider error: Connection error: AWS credential error: an error occurred while loading credentials",
                false
            ),
            ProviderHealthCheckErrorKind::AwsCredentials
        );
        assert_eq!(
            classify_error(
                "service error: UnauthorizedException: Session token not found or invalid",
                false
            ),
            ProviderHealthCheckErrorKind::AwsCredentials
        );
    }

    #[test]
    fn classify_error_detects_timeout() {
        assert_eq!(
            classify_error("Health check timeout (30s)", true),
            ProviderHealthCheckErrorKind::Timeout
        );
    }
}
