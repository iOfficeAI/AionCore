#![warn(clippy::disallowed_types)]

//! Multi-agent team sessions with role-based prompts, task board, mailbox, and scheduling.
pub mod crash_detection;
pub mod error;
pub mod event_loop;
pub mod events;
pub mod guide;
pub mod mailbox;
pub mod mcp;
pub mod message_projection;
pub mod ports;
pub mod prompts;
pub mod provisioning;
pub mod routes;
pub mod scheduler;
pub mod service;
pub mod session;
mod slot_wake_gate;
pub mod task_board;
pub mod team_run;
#[cfg(test)]
pub(crate) mod test_utils;
pub mod types;
pub mod visibility;
mod wake;

pub use crash_detection::{CrashReason, detect_crash, is_rate_limited};
pub use error::TeamError;
pub use events::TeamEventEmitter;
pub use guide::{GuideMcpServer, handle_aion_list_models};
pub use mailbox::Mailbox;
pub use mcp::{TEAM_MCP_SERVER_NAME, TeamMcpServer, TeamMcpStdioConfig, TeamMcpStdioServerSpec};
pub use message_projection::{
    ProjectedTeamMessage, TeamMessageProjection, TeamProjectionMessageStore, TeamProjectionRequest,
    TeamProjectionSource,
};
pub use ports::{
    AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
    AgentTurnSource, AgentTurnStarted, AgentTurnStartedCallback, AgentTurnStatus, TeamConversationBindingLookup,
    TeamConversationLookupPort,
};

pub use prompts::{build_lead_prompt, build_teammate_prompt, build_wake_payload};
pub use provisioning::{
    TeamAgentProvisioner, TeamConversationAdoptRequest, TeamConversationCreateRequest, TeamConversationProvisioningPort,
};
pub use routes::{TeamRouterState, team_routes};
pub use scheduler::{
    SchedulerAction, TeammateManager, WAKE_TIMEOUT_MS, WakePayload, format_crash_testament, normalize_name,
};
pub use service::TeamSessionService;
pub use session::{TeamSession, WakeInput};
pub use task_board::{TaskBoard, TaskUpdate};
pub use team_run::{
    ActiveChildTurn, ChildCancelTarget, ChildStartDecision, StartingChildReservation, StartingReservationState,
    TeamRunManager, target_role_for,
};
pub use types::{
    MailboxMessage, MailboxMessageType, TaskStatus, Team, TeamAgent, TeamTask, TeammateRole, TeammateStatus,
};
pub use visibility::TeamVisibilityPolicy;
