#![warn(clippy::disallowed_types)]

//! User-authored assistant management.
//!
//! Owns the `assistants` and `assistant_overrides` tables, built-in
//! assistant loading from on-disk manifest, and merge logic for
//! `GET /api/assistants` across builtin + user + extension sources.

pub mod agent_catalog;
pub mod agent_center_routes;
pub mod agent_center_service;
pub mod builtin;
pub mod error;
pub mod routes;
pub mod service;
pub mod skill_evolution_routes;
pub mod skill_evolution_service;
pub mod state;

pub use agent_catalog::AssistantAgentCatalogPort;
pub use agent_center_routes::{AgentCenterRouterState, agent_center_routes};
pub use agent_center_service::AgentCenterService;
pub use builtin::{AvatarAsset, BuiltinAssistant, BuiltinAssistantRegistry};
pub use error::AssistantError;
pub use routes::{AssistantRouterState, assistant_routes};
pub use service::AssistantService;
pub use skill_evolution_routes::{SkillEvolutionRouterState, skill_evolution_routes};
pub use skill_evolution_service::SkillEvolutionService;
