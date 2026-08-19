#![warn(clippy::disallowed_types)]

//! Cross-session message delivery.
//!
//! Delivery delegates to `ConversationService::send_message` — the human send
//! path — so "cross-session delivery ≡ a human pressing send" is a structural
//! guarantee rather than two code paths kept in sync by hand.

pub mod error;

pub use error::SessionMessageError;
