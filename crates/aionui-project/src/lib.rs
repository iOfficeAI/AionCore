//! Project registration and software-development preflight domain.

mod error;
mod routes;
mod service;
mod types;

pub use error::ProjectError;
pub use routes::{ProjectRouterState, project_routes};
pub use service::{ProjectAgentCapabilityPort, ProjectService};
pub use types::{
    AgentCapabilitySnapshot, AgentPreflightResult, CreateProjectInput, PreflightCheck, ProjectCommandProfileInput,
    ProjectPreflightResult, ProjectRuntimeProfileInput, UpdateProjectInput,
};
