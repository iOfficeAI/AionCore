#![warn(clippy::disallowed_types)]

//! Multi-agent team sessions with role-based prompts, task board, mailbox, and scheduling.
pub mod capability;
pub mod crash_detection;
pub mod error;
pub mod event_loop;
pub mod events;
pub mod mailbox;
pub mod mcp;
mod member_runtime;
pub mod message_projection;
pub mod ports;
pub mod prompt_dump;
pub mod prompts;
pub mod provisioning;
pub mod routes;
pub mod scheduler;
pub mod service;
pub mod session;
pub mod task_board;
pub mod team_run;
#[cfg(test)]
pub(crate) mod test_utils;
pub mod types;
pub mod visibility;
mod work_coordinator;
mod work_source;
mod workspace;

pub use crash_detection::{CrashReason, detect_crash, is_rate_limited};
pub use error::TeamError;
pub use events::TeamEventEmitter;
pub use mailbox::Mailbox;
pub use mcp::{TEAM_MCP_SERVER_NAME, TeamMcpServer, TeamMcpStdioConfig, TeamMcpStdioServerSpec};
pub use message_projection::{
    ProjectedTeamMessage, TeamMessageProjection, TeamProjectionMessageStore, TeamProjectionRequest,
    TeamProjectionSource,
};
pub use ports::{
    AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
    AgentTurnSource, AgentTurnStarted, AgentTurnStartedCallback, AgentTurnStatus, TeamAssistantCatalogEntry,
    TeamAssistantCatalogPort, TeamConversationBindingLookup, TeamConversationLookupPort,
};

pub use prompt_dump::TeamPromptDumpConfig;
pub use prompts::{build_lead_prompt, build_teammate_prompt, build_wake_payload};
pub use provisioning::{
    TeamAgentProvisioner, TeamConversationCreateRequest, TeamConversationCreateResult, TeamConversationProvisioningPort,
};
pub use routes::{TeamRouterState, team_routes};
pub use scheduler::{
    SchedulerAction, TeammateManager, WAKE_TIMEOUT_MS, WakePayload, format_crash_testament, normalize_name,
};
pub use service::{TeamIdleCleanupCoordinator, TeamSessionService};
pub use session::{TeamSession, WakeInput};
pub use task_board::{TaskBoard, TaskUpdate};
pub use team_run::{TeamRunManager, target_role_for};
pub use types::{
    MailboxMessage, MailboxMessageType, TaskStatus, Team, TeamAgent, TeamTask, TeammateRole, TeammateStatus,
};
pub use visibility::TeamVisibilityPolicy;
