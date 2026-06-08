use std::sync::Arc;

use tracing::warn;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::remote::{RemoteAgentConfig, RemoteAgentManager};
use crate::session_context::RemoteSessionBuildContext;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: RemoteSessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let row = deps
        .remote_agent_repo
        .find_by_id(&build_context.remote_agent_id)
        .await
        .map_err(|e| AgentError::internal(format!("Failed to load remote agent config: {e}")))?
        .ok_or_else(|| AgentError::not_found(format!("Remote agent '{}' not found", build_context.remote_agent_id)))?;
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
