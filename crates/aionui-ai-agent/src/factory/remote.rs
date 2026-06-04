use std::sync::Arc;

use aionui_api_types::RemoteBuildExtra;
use tracing::warn;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::remote::{RemoteAgentConfig, RemoteAgentManager};
use crate::types::BuildTaskOptions;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    options: BuildTaskOptions,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let extra: RemoteBuildExtra = serde_json::from_value(options.extra)
        .map_err(|e| AgentError::bad_request(format!("Invalid Remote build options: {e}")))?;
    let row = deps
        .remote_agent_repo
        .find_by_id(&extra.remote_agent_id)
        .await
        .map_err(|e| AgentError::internal(format!("Failed to load remote agent config: {e}")))?
        .ok_or_else(|| AgentError::not_found(format!("Remote agent '{}' not found", extra.remote_agent_id)))?;
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
    let config = RemoteAgentConfig {
        remote_agent_id: row.id.clone(),
        url: row.url.clone(),
        auth_type: row.auth_type.clone(),
        auth_token,
        allow_insecure: row.allow_insecure,
    };
    let agent = RemoteAgentManager::new(ctx.conversation_id, ctx.workspace, config).await?;
    Ok(AgentInstance::Remote(Arc::new(agent)))
}
