#![warn(clippy::disallowed_types)]

//! Conversation and message CRUD with streaming relay and event emission.
mod acp_error_recovery;
mod agent_health_policy;
mod convert;
pub mod error;
mod message_persistence;
pub mod response_middleware;
pub mod routes;
pub mod routes_aux;
mod runtime_completion;
mod runtime_persistence;
pub mod runtime_state;
pub mod service;
mod service_ops;
pub(crate) mod session_context;
pub mod skill_resolver;
pub mod skill_snapshot;
mod startup_recovery;
pub mod state;
mod stream_persistence;
pub mod stream_relay;
pub mod task_options;
mod turn_continuation_policy;
mod turn_orchestrator;
mod turn_recovery_policy;

pub use error::ConversationError;
pub use response_middleware::{
    CronCommand, CronCommandResult, CronCreateParams, CronUpdateParams, ICronService, MessageMiddleware,
    MiddlewareResult, detect_cron_commands, has_cron_commands, strip_cron_commands, strip_think_tags,
};
pub use routes::conversation_routes;
pub use routes_aux::conversation_ops_routes;
pub use service::{
    ConversationAgentTurnOutcome, ConversationAgentTurnRequest, ConversationAgentTurnStarted,
    ConversationAgentTurnStartedCallback, ConversationAgentTurnStatus, ConversationService,
};
pub use state::ConversationRouterState;

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
