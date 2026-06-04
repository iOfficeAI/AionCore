//! AI agent lifecycle, worker task dispatch, and skill management.
pub(crate) mod agent_runtime;
pub mod agent_task;
pub mod capability;
pub mod cc_switch;
pub mod factory;
pub(crate) mod idle_scanner;
pub mod manager;
pub(crate) mod persistence;
pub mod protocol;
pub mod registry;
pub mod routes;
pub(crate) mod services;
pub mod shared_kernel;
pub mod task_manager;
pub mod types;

pub use agent_runtime::AgentRuntime;
#[cfg(any(test, feature = "test-support"))]
pub use agent_task::IMockAgent;
pub use agent_task::{AgentInstance, IAgentTask};
pub use aionui_api_types::{
    AcpBuildExtra, AcpModelInfo, AionrsBuildExtra, OpenClawBuildExtra, OpenClawGatewayConfig, RemoteBuildExtra,
    SlashCommandItem,
};
pub use capability::skill_manager::{
    AcpSkillManager, SkillDefinition, SkillIndex, build_skills_index_text, build_system_instructions,
    build_system_instructions_with_skills_index, detect_skill_load_request, prepare_first_message,
    prepare_first_message_with_skills_index,
};
pub use factory::{AgentFactoryDeps, build_agent_factory};
pub use idle_scanner::start_idle_scanner;
pub use persistence::AcpSessionSyncService;
pub use protocol::error::AcpError;
pub use protocol::events::AgentStreamEvent;
pub use protocol::send_error::AgentSendError;
pub use registry::{AgentRegistry, UnavailableReason};
pub use routes::{AgentRouterState, RemoteAgentRouterState, agent_routes, remote_agent_routes};
pub use services::AgentService;
pub use services::RemoteAgentService;
pub use task_manager::{IWorkerTaskManager, WorkerTaskManagerImpl};
