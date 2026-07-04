use std::sync::Arc;

use crate::AgentError;
use aionui_api_types::OpenClawBuildExtra;
use aionui_common::AgentType;
use tracing::warn;

use crate::agent_task::AgentInstance;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::openclaw::OpenClawAgentManager;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    mut config: OpenClawBuildExtra,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    // If this is a remote-agent row, resolve the gateway details from the DB.
    if let Some(remote_agent_id) = config.remote_agent_id.as_deref().filter(|id| !id.is_empty()) {
        let row = deps
            .remote_agent_repo
            .find_by_id(remote_agent_id)
            .await
            .map_err(|e| AgentError::Internal(format!("Failed to load remote agent config: {e}")))?
            .ok_or_else(|| AgentError::NotFound(format!("Remote agent '{remote_agent_id}' not found")))?;
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

        let (host, port) = parse_ws_url(&row.url).unwrap_or_else(|| {
            warn!(url = %row.url, "Failed to parse remote agent URL, using as-is");
            (row.url.clone(), None)
        });

        config.backend = Some("remote".to_owned());
        config.agent_name = Some(row.name.clone());
        config.gateway.host = Some(host);
        config.gateway.port = port;
        config.gateway.token = auth_token;
        config.gateway.password = None;
        config.gateway.use_external_gateway = true;
        config.gateway.cli_path = Some(row.url);
    }

    // OpenClaw lives in the catalog as an internal row; reuse
    // the registry-resolved path instead of re-running `which()`.
    if config.gateway.cli_path.is_none()
        && !config.gateway.use_external_gateway
        && let Some(cli) = deps
            .agent_registry
            .list_by_agent_type(AgentType::OpenclawGateway)
            .await
            .into_iter()
            .find_map(|m| m.resolved_command)
            .map(|p| p.to_string_lossy().into_owned())
    {
        config.gateway.cli_path = Some(cli);
    }

    let resume_session_key = config.session_key.clone();
    let agent = OpenClawAgentManager::new(
        ctx.conversation_id,
        ctx.workspace,
        config,
        resume_session_key,
        deps.data_dir.clone(),
    )
    .await?;
    let mut arc = Arc::new(agent);
    let weak = Arc::downgrade(&arc);
    if let Some(manager) = Arc::get_mut(&mut arc) {
        manager.set_weak_self(weak).await;
    }
    arc.start_event_relay();
    Ok(AgentInstance::OpenClaw(arc))
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
