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

    // Register AionUi as agy's PreToolUse gate for THIS workspace, and mint the
    // token that authenticates the hook's callback. Without this the session
    // still runs, but with agy's gate wide open and no per-call approval — so a
    // failure here must be loud.
    let hook_env = match deps.antigravity_hook_base_url.as_deref() {
        Some(base_url) => {
            let hook_binary = deps.backend_binary_path.as_path();
            crate::antigravity_hook::write_hooks_json(std::path::Path::new(&ctx.workspace), hook_binary).map_err(
                |e| {
                    AgentError::internal(format!(
                        "antigravity: could not register the permission hook in {}: {e}",
                        ctx.workspace
                    ))
                },
            )?;
            // Minting here also invalidates any token left over from a prior
            // build of this conversation, so a stale hook cannot answer for the
            // new session.
            let token = deps.antigravity_hook_tokens.issue(&ctx.conversation_id);
            vec![
                (
                    aionui_api_types::AntigravityHookConfig::ENV_BASE_URL.to_owned(),
                    base_url.to_owned(),
                ),
                (aionui_api_types::AntigravityHookConfig::ENV_TOKEN.to_owned(), token),
                (
                    aionui_api_types::AntigravityHookConfig::ENV_CONVERSATION_ID.to_owned(),
                    ctx.conversation_id.clone(),
                ),
            ]
        }
        None => {
            tracing::warn!(
                conversation_id = %ctx.conversation_id,
                "antigravity: no hook callback address configured — tools will run WITHOUT per-call approval"
            );
            Vec::new()
        }
    };

    let mut runtime_env = ctx.runtime_env.clone();
    runtime_env.extend(hook_env);

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
            runtime_env: &runtime_env,
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
