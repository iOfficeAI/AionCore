#![warn(clippy::disallowed_types)]

//! Cross-session message delivery.
//!
//! Delivery delegates to `ConversationService::send_message` — the human send
//! path — so "cross-session delivery ≡ a human pressing send" is a structural
//! guarantee rather than two code paths kept in sync by hand.

pub mod drainer;
pub mod error;
pub mod hook;
pub mod queue;
pub mod rate_limit;
pub mod routes;
pub mod service;
pub mod state;
pub mod targets;

pub use error::SessionMessageError;
pub use hook::QueueClearingCancelHook;
pub use routes::{session_message_routes, session_message_user_routes};
pub use service::{SessionMessageDeps, SessionMessageService};
pub use state::SessionMessageRouterState;
pub use targets::MentionableTargets;
