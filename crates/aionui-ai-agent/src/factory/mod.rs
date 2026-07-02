pub mod acp_assembler;

mod acp;
mod acp_launch_policy;
pub(crate) mod aionrs;
mod context;

use std::path::PathBuf;
use std::sync::Arc;

use aionui_db::{IMcpServerRepository, IProviderRepository};
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

async fn build_agent(deps: Arc<AgentFactoryDeps>, options: BuildTaskOptions) -> Result<AgentInstance, AgentError> {
    let context = options.context;
    let ctx = FactoryContext::resolve(&context).await?;
    let model = context.model.clone();
    match context.kind {
        AgentSessionKind::Acp(acp_context) => acp::build(deps, *acp_context, ctx).await,
        AgentSessionKind::Aionrs(aionrs_context) => aionrs::build(deps, *aionrs_context, model, ctx).await,
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
}
