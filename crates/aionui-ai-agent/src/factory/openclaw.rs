use std::sync::Arc;

use aionui_common::AgentType;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::openclaw::OpenClawAgentManager;
use crate::session_context::OpenClawSessionBuildContext;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: OpenClawSessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let mut config = build_context.config;

    // OpenClaw lives in the catalog as an internal row; reuse
    // the registry-resolved path instead of re-running `which()`.
    if config.gateway.cli_path.is_none()
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
    let arc = Arc::new(agent);
    arc.start_event_relay();
    Ok(AgentInstance::OpenClaw(arc))
}
