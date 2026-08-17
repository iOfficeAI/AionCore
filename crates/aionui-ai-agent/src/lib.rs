#![warn(clippy::disallowed_types)]

//! AI agent lifecycle, worker task dispatch, and skill management.
pub mod active_lease;
pub(crate) mod agent_runtime;
pub mod agent_task;
pub mod antigravity_hook;
pub mod capability;
pub mod cc_switch;
mod claude_flags;
pub(crate) mod cli_probe;
pub(crate) mod dev_prompt_dump;
pub mod error;
pub mod factory;
pub(crate) mod idle_scanner;
pub mod manager;
/// Neutral MCP resolution for the session-model port (claude/codex). Ported from
/// clean-slate `aionui-agent-context::mcp_resolve` — the SSOT that turns a
/// conversation's configured MCP servers into the SDK-free `SessionMcpServer`
/// shape the `SessionBackend` stack carries in `SessionConfig.init.mcp_servers`.
pub mod mcp_resolve;
pub mod media;
pub(crate) mod persistence;
pub mod protocol;
pub mod registry;
pub mod routes;
pub(crate) mod runtime_status;
pub mod runtime_token;
pub(crate) mod services;
pub mod session_agent;
pub mod session_context;
pub mod shared_kernel;
pub mod task_manager;
pub mod terminal;
pub mod types;
mod workflow_progress;

pub use active_lease::{ACTIVE_LEASE_TTL_MS, ActiveLeaseRegistry};
pub use agent_runtime::AgentRuntime;
#[cfg(any(test, feature = "test-support"))]
pub use agent_task::IMockAgent;
pub use agent_task::{AgentInstance, IAgentTask};
pub use aionui_api_types::{AcpBuildExtra, AcpModelInfo, AionrsBuildExtra, SlashCommandItem};
// Backend-static capability table (session layer's single source of truth) —
// re-exported so the conversation layer can read the mid-turn bit for a
// conversation whose agent task is not currently live.
pub use aionui_session::backend_supports_midturn_delivery;
pub use aionui_session::effective_agent_capabilities;
pub use capability::skill_manager::{
    AcpSkillManager, SkillDefinition, SkillIndex, build_skills_index_text, build_system_instructions,
    build_system_instructions_with_skills_index, detect_skill_load_request, prepare_first_message,
    prepare_first_message_with_skills_index,
};
pub use error::AgentError;
pub use factory::{AgentFactoryDeps, build_agent_factory};
pub use idle_scanner::{
    IdleCleanupCoordinator, resolve_idle_config_from_env, start_idle_scanner, start_idle_scanner_with_coordinator,
};
pub use manager::acp::RequiredFullAutoApplication;
pub use persistence::AcpSessionSyncService;
pub use protocol::error::AcpError;
pub use protocol::events::AgentStreamEvent;
pub use protocol::send_error::AgentSendError;
pub use registry::{AgentRegistry, UnavailableReason};
pub use routes::{AgentRouterState, RemoteAgentRouterState, agent_routes, remote_agent_routes};
pub use runtime_token::{
    RuntimeTokenClaims, RuntimeTokenError, RuntimeTokenIssue, RuntimeTokenScope, RuntimeTokenService,
    TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
};
pub use services::AgentAvailabilityFeedbackPort;
pub use services::AgentService;
pub use services::RemoteAgentService;
pub use session_context::{
    AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, AionrsSessionBuildContext, ConversationContext,
    WorkspaceContext,
};
pub use task_manager::{IWorkerTaskManager, WorkerTaskManagerImpl};
