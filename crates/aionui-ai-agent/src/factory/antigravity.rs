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

/// The mode id that means "run without asking" for this agent.
///
/// A SENTINEL, not one of agy's modes: agy has no full-auto mode (all three of
/// its modes behave identically on permissions — only
/// `--dangerously-skip-permissions` decides). Callers that mean "unattended"
/// — a teammate via `session_mode_for_backend`, a scheduled run via
/// `fallback_full_auto_mode` — arrive here carrying this value, the same way
/// every other agent expresses full auto. A user can also pick it deliberately
/// from the mode list.
///
/// Deriving this from where the session came from instead (team? cron?) would
/// need a new branch for every future caller; this needs none.
fn yolo_mode_id(catalog_yolo_id: Option<&str>) -> &str {
    catalog_yolo_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("yolo")
}

/// Whether this session runs without a human to answer permission prompts.
pub(crate) fn is_full_auto(session_mode: Option<&str>, yolo_id: &str) -> bool {
    session_mode.is_some_and(|m| m.eq_ignore_ascii_case(yolo_id))
}

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
    // Resolve the mode the same way the session itself will (snapshot over
    // create-time seed, aliases normalized). Reading `config.session_mode`
    // directly saw only the value the conversation was created with, so a
    // runtime switch to full auto — and the mode a scheduled run resolves —
    // never reached this decision.
    let resolved_mode =
        crate::session_agent::resolved_session_mode(&config, build_context.session_snapshot.as_ref(), &meta);
    let full_auto = is_full_auto(resolved_mode.as_deref(), yolo_mode_id(meta.yolo_id.as_deref()));
    let hook_env = match deps.antigravity_hook_base_url.as_deref() {
        _ if full_auto => {
            tracing::info!(
                conversation_id = %ctx.conversation_id,
                "antigravity: full-auto mode — tools execute without per-call approval"
            );
            // Drop any hook an earlier non-full-auto build left in this
            // workspace, and revoke its token, or agy would keep calling back
            // and block on approval nobody is there to give.
            crate::antigravity_hook::remove_hooks_json(std::path::Path::new(&ctx.workspace));
            deps.antigravity_hook_tokens.revoke(&ctx.conversation_id);
            Vec::new()
        }
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

    // Prepared in every branch: full auto does not install it now, but the user
    // may switch out of full auto later and the backend needs it to restore the
    // gate.
    let permission_hook_body = deps
        .antigravity_hook_base_url
        .as_deref()
        .map(|_| crate::antigravity_hook::hooks_json_body(deps.backend_binary_path.as_path()));

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
            permission_hook_body,
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

#[cfg(test)]
mod tests {
    use super::{is_full_auto, yolo_mode_id};

    #[test]
    fn the_catalog_sentinel_turns_off_the_approval_hook() {
        // How a teammate and a scheduled run both arrive: their full-auto mode id.
        assert!(is_full_auto(Some("yolo"), yolo_mode_id(Some("yolo"))));
    }

    #[test]
    fn agys_real_modes_keep_their_approval_prompts() {
        // Measured on 1.1.9: none of these is full auto — `accept-edits` only
        // auto-approves file edits, `plan` is research-only, and all three are
        // refused the "command" permission without the flag.
        for mode in ["default", "accept-edits", "plan"] {
            assert!(!is_full_auto(Some(mode), yolo_mode_id(Some("yolo"))), "mode={mode}");
        }
        assert!(!is_full_auto(None, yolo_mode_id(Some("yolo"))));
    }

    #[test]
    fn a_catalog_without_a_yolo_id_still_recognises_the_sentinel() {
        // An older row (or a hand-edited one) must not silently lose full auto.
        assert!(is_full_auto(Some("yolo"), yolo_mode_id(None)));
        assert!(!is_full_auto(Some("default"), yolo_mode_id(None)));
    }

    #[test]
    fn a_catalog_may_rename_the_sentinel() {
        // The id is data, not a constant: honour whatever the row declares.
        assert!(is_full_auto(Some("full-access"), yolo_mode_id(Some("full-access"))));
        assert!(!is_full_auto(Some("yolo"), yolo_mode_id(Some("full-access"))));
    }
}
