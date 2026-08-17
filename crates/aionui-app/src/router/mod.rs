//! HTTP router assembly for the application.

mod antigravity_hook;
mod clipboard_writer;
mod fs_monitor;
mod health;
mod item_revealer;
mod routes;
mod runtime_team_tools;
mod scm_monitor;
mod state;
mod system_file_opener;
mod team_capability_resolver;
mod team_conversation_adapters;
mod trace;

pub use routes::{
    RouterRuntime, create_router, create_router_with_all_state, create_router_with_runtime, create_router_with_states,
};
pub use state::{
    ChannelOrchestratorComponents, ModuleStates, RouterBuildError, build_assistant_state, build_conversation_state,
    build_extension_states, build_module_states, build_ws_state,
};
