pub mod acp_assembler;

mod acp;
mod acp_launch_policy;
pub(crate) mod aionrs;
mod antigravity;
mod context;

use std::path::PathBuf;
use std::sync::Arc;

use aionui_db::{IMcpServerRepository, IProviderRepository, IUserRepository, SiteRole};
use aionui_realtime::EventBroadcaster;
use futures_util::FutureExt;

use crate::agent_task::AgentInstance;
use crate::capability::skill_manager::AcpSkillManager;
use crate::error::AgentError;
use crate::factory::context::FactoryContext;
use crate::persistence::AcpSessionSyncService;
use crate::registry::AgentRegistry;
use crate::session_context::AgentSessionKind;
use crate::task_manager::AgentFactory;
use crate::types::BuildTaskOptions;

/// Dependencies needed by the agent factory to construct agents.
pub struct AgentFactoryDeps {
    pub skill_manager: Arc<AcpSkillManager>,
    pub provider_repo: Arc<dyn IProviderRepository>,
    /// Live user records used to enforce the hosted-runtime trust boundary.
    pub user_repo: Arc<dyn IUserRepository>,
    pub encryption_key: [u8; 32],
    pub agent_registry: Arc<AgentRegistry>,
    pub acp_agent_service: Arc<AcpSessionSyncService>,
    pub data_dir: PathBuf,
    pub dump_prompts: bool,
    pub broadcaster: Arc<dyn EventBroadcaster>,
    /// Absolute path to the backend binary, reused as the `command` of the
    /// stdio MCP bridge injected into ACP `session/new` for team sessions.
    /// Captured once at app startup (`std::env::current_exe()`).
    pub backend_binary_path: Arc<PathBuf>,
    /// User-configured MCP servers repository. Used by ACP factory to
    /// inject enabled servers into `session/new` (ELECTRON-1JG fix).
    /// `None` for tests/composition paths that do not need MCP injection.
    pub mcp_server_repo: Option<Arc<dyn IMcpServerRepository>>,
    /// In hosted WebUI mode, only live local administrators may start agents
    /// with host-process or unrestricted filesystem tools. Members retain the
    /// built-in conversational agent with its tool surface disabled.
    pub restrict_member_host_tools: bool,
    /// Subprocess spawner for the clean-slate session model. claude/codex always
    /// run through `SessionAgentTask` (direct-CLI) instead of the ACP manager, so
    /// the spawner is unconditionally wired — there is no fallback to the ACP path.
    pub session_spawner: Arc<dyn aionui_process::Spawner>,
    /// Base URL the Antigravity permission hook calls back on (e.g.
    /// `http://127.0.0.1:25808`). agy cannot prompt for permission in headless
    /// mode, so AionUi registers its own binary as a PreToolUse hook and
    /// answers each request itself — the hook process needs this address to
    /// reach us. `None` disables the bridge, which means agy runs with its gate
    /// open and NO per-call approval; only acceptable in tests.
    pub antigravity_hook_base_url: Option<String>,
    /// Per-conversation tokens authenticating the permission hook's callback.
    /// Shared with the HTTP endpoint that answers those callbacks.
    pub antigravity_hook_tokens: Arc<crate::antigravity_hook::HookTokenRegistry>,
}

/// Build a production agent factory that dispatches to concrete agent types.
///
/// [`AgentFactory`] is async: the returned `BoxFuture` is driven by
/// [`crate::task_manager::IWorkerTaskManager::get_or_build_task`] on whatever
/// runtime is currently polling it. This lets us spawn CLI processes and
/// await ACP handshakes directly, without the scoped-thread + `block_on`
/// bridge the old sync-factory version needed.
pub fn build_agent_factory(deps: AgentFactoryDeps) -> AgentFactory {
    let deps = Arc::new(deps);

    Arc::new(move |options: BuildTaskOptions| {
        let deps = deps.clone();
        async move { build_agent(deps, options).await }.boxed()
    })
}

fn can_run_host_tools(restrict_member_host_tools: bool, site_role: SiteRole) -> bool {
    !restrict_member_host_tools || site_role == SiteRole::Admin
}

async fn build_agent(deps: Arc<AgentFactoryDeps>, options: BuildTaskOptions) -> Result<AgentInstance, AgentError> {
    let context = options.context;
    let ctx = FactoryContext::resolve(&context).await?;
    let model = context.model.clone();
    let host_tools_allowed = if deps.restrict_member_host_tools {
        let user = deps
            .user_repo
            .find_active_by_id(&ctx.user_id)
            .await
            .map_err(|error| AgentError::internal(format!("Failed to authorize agent runtime: {error}")))?
            .ok_or_else(|| AgentError::unauthorized("Active user required for agent runtime"))?;
        can_run_host_tools(true, user.site_role)
    } else {
        true
    };
    match context.kind {
        AgentSessionKind::Aionrs(aionrs_context) => {
            aionrs::build(deps, *aionrs_context, model, ctx, host_tools_allowed).await
        }
        AgentSessionKind::Acp(acp_context) if host_tools_allowed => acp::build(deps, *acp_context, ctx).await,
        AgentSessionKind::Antigravity(agy_context) if host_tools_allowed => {
            antigravity::build(deps, *agy_context, ctx).await
        }
        AgentSessionKind::Acp(_) | AgentSessionKind::Antigravity(_) => Err(AgentError::forbidden(
            "External agent runtimes are restricted to administrators in hosted multi-user mode",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_deps_can_be_constructed() {
        // Verify types compile — actual construction requires DB
        let _: fn() -> AgentFactoryDeps = || {
            panic!("compile-time check only");
        };
    }

    #[test]
    fn hosted_runtime_trusts_only_live_site_admins() {
        assert!(can_run_host_tools(true, SiteRole::Admin));
        assert!(!can_run_host_tools(true, SiteRole::Member));
        assert!(can_run_host_tools(false, SiteRole::Member));
    }
}
