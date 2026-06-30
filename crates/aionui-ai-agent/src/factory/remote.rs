use std::sync::Arc;

use aionui_api_types::{OpenClawBuildExtra, OpenClawGatewayConfig, RemoteBuildExtra};
use crate::AgentError;
use tracing::warn;

use crate::agent_task::AgentInstance;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::types::BuildTaskOptions;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    options: BuildTaskOptions,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let extra: RemoteBuildExtra = serde_json::from_value(options.extra.clone())
        .map_err(|e| AgentError::BadRequest(format!("Invalid Remote build options: {e}")))?;
    let row = deps
        .remote_agent_repo
        .find_by_id(&extra.remote_agent_id)
        .await
        .map_err(|e| AgentError::Internal(format!("Failed to load remote agent config: {e}")))?
        .ok_or_else(|| AgentError::NotFound(format!("Remote agent '{}' not found", extra.remote_agent_id)))?;
    let auth_token = row
        .auth_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .and_then(|encrypted| {
            aionui_common::decrypt_string(encrypted, &deps.encryption_key)
                .map_err(|e| {
                    warn!(error = %e, "Failed to decrypt remote agent auth_token");
                })
                .ok()
        });

    // Delegate to OpenClaw gateway — RemoteAgentManager's WebSocket is broken.
    // Build gateway config from the remote agent's URL and auth token.
    let (host, port) = parse_ws_url(&row.url).unwrap_or_else(|| {
        warn!(url = %row.url, "Failed to parse remote agent URL, using as-is");
        (row.url.clone(), None)
    });

    // Start from the original extra so fields like `skills`, `cron_job_id`,
    // `session_key`, etc. set by the cron executor or frontend are preserved.
    let mut openclaw_extra: OpenClawBuildExtra =
        serde_json::from_value(options.extra.clone()).unwrap_or_else(|_| OpenClawBuildExtra {
            backend: None,
            agent_name: None,
            gateway: OpenClawGatewayConfig::default(),
            skills: Vec::new(),
            preset_assistant_id: None,
            cron_job_id: None,
            session_key: None,
        });

    openclaw_extra.backend = Some("remote".to_owned());
    openclaw_extra.agent_name = Some(row.name.clone());
    openclaw_extra.gateway.host = Some(host);
    openclaw_extra.gateway.port = port;
    openclaw_extra.gateway.token = auth_token.clone();
    openclaw_extra.gateway.password = None;
    openclaw_extra.gateway.use_external_gateway = true;
    openclaw_extra.gateway.cli_path = Some(row.url.clone());

    let mut openclaw_options = options;
    openclaw_options.extra = serde_json::to_value(&openclaw_extra)
        .map_err(|e| AgentError::Internal(format!("Failed to serialize OpenClaw build extra: {e}")))?;

    crate::factory::openclaw::build(deps, openclaw_options, ctx).await
}

/// Parse a WebSocket URL like `ws://127.0.0.1:18790` into (host, port).
fn parse_ws_url(url: &str) -> Option<(String, Option<u16>)> {
    // Strip ws:// or wss:// prefix
    let without_scheme = url.strip_prefix("ws://").or_else(|| url.strip_prefix("wss://"))?;

    // Split host:port
    if let Some(colon_pos) = without_scheme.rfind(':') {
        let host = without_scheme[..colon_pos].to_owned();
        let port = without_scheme[colon_pos + 1..].parse::<u16>().ok();
        Some((host, port))
    } else {
        Some((without_scheme.to_owned(), None))
    }
}
