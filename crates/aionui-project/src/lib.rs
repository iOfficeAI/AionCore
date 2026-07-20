//! Project registration and software-development preflight domain.

mod detection;
mod error;
mod repository_source;
mod routes;
mod service;
mod types;

pub use aionui_api_types::{
    DirtyWorktreeChoice, ProjectRepositoryFacts, ProjectRepositoryOnboardingInput, RepositorySource,
    RepositorySubmodule,
};
pub use error::ProjectError;
pub use repository_source::RepositoryOnboarder;
pub use routes::{ProjectRouterState, project_routes};
pub use service::{ProjectAgentCapabilityPort, ProjectService};
pub use types::{
    AgentCapabilitySnapshot, AgentPreflightResult, CreateProjectInput, OnboardProjectResult, PreflightCheck,
    ProjectCommandProfileInput, ProjectPreflightResult, ProjectRuntimeProfileInput, UpdateProjectInput,
};
