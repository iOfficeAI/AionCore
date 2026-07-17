#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod agent_binding;
mod database;
mod error;
mod legacy_handoff;
pub mod models;
mod repository;

pub use agent_binding::{
    AgentBindingResolution, binding_resolution_for_agent, resolve_agent_binding, resolve_agent_binding_from_rows,
    runtime_backend_for_agent,
};
pub use database::{
    Database, DatabaseInitError, init_database, init_database_memory, init_database_staged, maybe_copy_legacy_database,
};
pub use error::DbError;
pub use models::{
    AgentMetadataRow, AgentWorkspaceLeaseRow, ApprovalRequestRow, AssistantDefinitionRow, AssistantOverlayRow,
    AssistantOverrideRow, AssistantPreferenceRow, AssistantRow, ConversationArtifactRow,
    ConversationAssistantSnapshotRow, CreateAssistantParams, DevelopmentAlertRow, DevelopmentAuditEventRow,
    DevelopmentPolicyRow, DevelopmentRecoveryRecordRow, DevelopmentRunRoleRow, DevelopmentRunRow, DevelopmentTaskRow,
    DevelopmentUsageEventRow, DevelopmentUsageSummary, ProjectCommandProfileRow, ProjectResourceLinkRow, ProjectRow,
    ProjectRuntimeProfileRow, QualityGateRunRow, ReviewFindingRow, SkillImportRecordRow, SkillRow, TaskArtifactRow,
    UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpdateAssistantParams,
    UpsertAgentMetadataParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams, UpsertOverrideParams,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationRowUpdate, MessagePageCursor, MessagePageDirection, MessagePageParams,
    MessagePageResult, MessageRowUpdate, MessageSearchRow,
};
pub use repository::cron::UpdateCronJobParams;
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::project::UpdateProjectParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::skill::{CreateSkillImportRecordParams, UpsertSkillParams};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::{
    AgentWorkspaceLeaseUpdate, CreateAcpSessionParams, IAcpSessionRepository, IAgentMetadataRepository,
    IAgentWorkspaceLeaseRepository, IApprovalRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository, IChannelRepository,
    IClientPreferenceRepository, IConversationRepository, ICronRepository, IDevelopmentOperationsRepository,
    IDevelopmentRepository, IMcpServerRepository, IOAuthTokenRepository, IProjectRepository, IProviderRepository,
    IRemoteAgentRepository, ISettingsRepository, ISkillRepository, ITeamRepository, IUserRepository,
    PersistedSessionState, SaveRuntimeStateParams, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    SqliteAgentWorkspaceLeaseRepository, SqliteApprovalRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantOverlayRepository, SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository,
    SqliteAssistantRepository, SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteCronRepository, SqliteDevelopmentOperationsRepository, SqliteDevelopmentRepository,
    SqliteMcpServerRepository, SqliteOAuthTokenRepository, SqliteProjectRepository, SqliteProviderRepository,
    SqliteRemoteAgentRepository, SqliteSettingsRepository, SqliteSkillRepository, SqliteTeamRepository,
    SqliteUserRepository, rebuild_legacy_assistant_mirror,
};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
