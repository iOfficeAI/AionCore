//! Factory branch for Antigravity (`agy` CLI) sessions.
//!
//! Antigravity is a direct-CLI backend that does NOT speak ACP, so it does not
//! go through `factory::acp` (nor through `session_agent::build_session_instance`,
//! which carries claude/codex-private assembly). It assembles here and hands the
//! opened backend to the generic `SessionAgentTask`.

use std::sync::Arc;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::session_context::AntigravitySessionBuildContext;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AntigravitySessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let mut config = build_context.config;

    // Trust the catalog row over a client-supplied backend label, mirroring the
    // ACP factory: the frontend collapses row-scoped rows to a shared slot
    // string that downstream consumers would misread.
    let meta = crate::factory::acp::resolve_catalog_metadata(&deps.agent_registry, &config, &ctx.user_id).await?;
    if config.agent_id.is_some() || config.backend.is_none() {
        config.backend.clone_from(&meta.backend);
    }

    let instance = crate::session_agent::build_antigravity_instance(
        crate::session_agent::SessionBuildInputs {
            conversation_id: ctx.conversation_id.clone(),
            user_id: ctx.user_id.clone(),
            workspace: ctx.workspace.clone(),
            config: &config,
            metadata: &meta,
            session_snapshot: build_context.session_snapshot.as_ref(),
            backend_session_id: build_context.session_id.clone(),
            mcp_server_repo: deps.mcp_server_repo.as_ref(),
            runtime_env: &ctx.runtime_env,
            broadcaster: deps.broadcaster.clone(),
            // Keyed by the resolved catalog row so discovered models/modes
            // refresh the `/api/agents` picker.
            catalog_writeback: Some((meta.id.clone(), deps.agent_registry.catalog_sender())),
            // Persists the resume anchor + observed mode/model from the pump.
            acp_session_repo: Some(deps.acp_agent_service.repo()),
            prompt_dump_dir: None,
        },
        deps.session_spawner.clone(),
    )
    .await?;

    tracing::info!(
        conversation_id = %ctx.conversation_id,
        "antigravity: routing conversation through the direct-CLI SessionAgentTask"
    );
    Ok(instance)
}
