//! Project registration and software-development preflight domain.

mod detection;
mod error;
mod knowledge;
mod repository_source;
mod routes;
mod service;
mod types;

pub use aionui_api_types::{
    DirtyWorktreeChoice, ProjectKnowledgeFact, ProjectKnowledgeStatus, ProjectRepositoryFacts,
    ProjectRepositoryOnboardingInput, ProjectTaskContext, ProjectTaskContextRequest, RepositorySource,
    RepositorySubmodule,
};
pub use error::ProjectError;
pub use knowledge::{
    CodebaseMemoryCliProvider, KnowledgeProviderError, ProjectKnowledgeProvider, ProjectKnowledgeProviderHealth,
    ProjectKnowledgeProviderRequest, ProjectKnowledgeProviderResult,
};
pub use repository_source::RepositoryOnboarder;
pub use routes::{ProjectRouterState, project_routes};
pub use service::{ProjectAgentCapabilityPort, ProjectService};
pub use types::{
    AgentCapabilitySnapshot, AgentPreflightResult, CreateProjectInput, OnboardProjectResult, PreflightCheck,
    ProjectCommandProfileInput, ProjectPreflightResult, ProjectRuntimeProfileInput, UpdateProjectInput,
};
