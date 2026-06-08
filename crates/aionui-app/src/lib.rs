#![warn(clippy::disallowed_types)]

//! Application crate: assembles all domain crates into an Axum server with DI and middleware.
//!
//! This file is a public façade — it only re-exports symbols defined in
//! submodules. All logic lives in the modules below.

mod config;
mod router;
mod services;

pub use config::{AppConfig, derive_encryption_key};
pub use router::{
    ChannelOrchestratorComponents, ModuleStates, RouterBuildError, build_assistant_state, build_conversation_state,
    build_extension_states, build_module_states, build_ws_state, create_router, create_router_with_all_state,
    create_router_with_states,
};
pub use services::AppServices;
