pub mod acp_assembler;

mod acp;
pub(crate) mod aionrs;
mod context;
mod nanobot;
mod openclaw;
mod remote;

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::GuideMcpConfig;
use aionui_common::{AgentType, AppError};
use aionui_db::{IMcpServerRepository, IProviderRepository, IRemoteAgentRepository};
use aionui_realtime::EventBroadcaster;
use futures_util::FutureExt;

use crate::agent_task::AgentInstance;
use crate::capability::skill_manager::AcpSkillManager;
use crate::factory::context::FactoryContext;
use crate::persistence::AcpSessionSyncService;
use crate::registry::AgentRegistry;
use crate::task_manager::AgentFactory;
use crate::types::BuildTaskOptions;

/// Dependencies needed by the agent factory to construct agents.
pub struct AgentFactoryDeps {
    pub skill_manager: Arc<AcpSkillManager>,
    pub remote_agent_repo: Arc<dyn IRemoteAgentRepository>,
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub agent_registry: Arc<AgentRegistry>,
    pub acp_agent_service: Arc<AcpSessionSyncService>,
    pub data_dir: PathBuf,
    pub broadcaster: Arc<dyn EventBroadcaster>,
    /// Absolute path to the backend binary, reused as the `command` of the
    /// stdio MCP bridge injected into ACP `session/new` for team sessions.
    /// Captured once at app startup (`std::env::current_exe()`).
    pub backend_binary_path: Arc<PathBuf>,
    /// Guide MCP server config. When `Some`, injected into solo (non-team)
    /// ACP agent sessions so the agent gets the `aion_create_team` tool.
    /// `None` when the Guide server failed to start (graceful degradation).
    pub guide_mcp_config: Option<GuideMcpConfig>,
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

async fn build_agent(deps: Arc<AgentFactoryDeps>, options: BuildTaskOptions) -> Result<AgentInstance, AppError> {
    let ctx = FactoryContext::resolve(&deps, &options).await?;
    match options.agent_type {
        AgentType::Gemini => Err(AppError::ConversationArchived(
            "This conversation was created with the legacy Gemini runtime, which has been \
             removed. Please start a new conversation with the Gemini ACP backend to continue."
                .into(),
        )),
        AgentType::Acp => acp::build(deps, options, ctx).await,
        AgentType::OpenclawGateway => openclaw::build(deps, options, ctx).await,
        AgentType::Nanobot => nanobot::build(deps, options, ctx).await,
        AgentType::Remote => remote::build(deps, options, ctx).await,
        AgentType::Aionrs => aionrs::build(deps, options, ctx).await,
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
