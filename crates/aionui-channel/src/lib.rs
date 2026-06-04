#![warn(clippy::disallowed_types)]

//! External channel integration: plugin system, pairing handshake, and per-session messaging.
pub mod action;
pub mod channel_settings;
pub mod constants;
pub mod error;
pub mod formatter;
pub mod manager;
pub mod message_service;
pub mod orchestrator;
pub mod pairing;
pub mod plugin;
pub mod plugins;
pub mod routes;
pub mod session;
pub mod stream_relay;
pub mod types;

#[cfg(feature = "weixin")]
pub use routes::weixin_login_route;
pub use routes::{ChannelRouterState, channel_routes};
