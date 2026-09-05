#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod agent_binding;
mod database;
mod error;
mod instance_lock;
mod legacy_handoff;
mod migrate_repair;
pub mod models;
mod repository;

pub use agent_binding::{
    AgentBindingResolution, binding_resolution_for_agent, resolve_agent_binding, resolve_agent_binding_for_user,
    resolve_agent_binding_from_rows, runtime_backend_for_agent,
};
pub use database::{
    DATABASE_NEWER_THAN_APP_STAGE, Database, DatabaseInitError, DatabaseInitOptions, init_database,
    init_database_memory, init_database_staged, init_database_staged_with_options, init_database_with_options,
    latest_known_migration_version, maybe_copy_legacy_database,
};
pub use error::{
    DbError, SQLITE_BUSY_MESSAGE_MARKERS, SQLITE_UNIQUE_VIOLATION_MARKER, message_indicates_busy,
    message_indicates_unique_violation,
};
pub use instance_lock::{DataDirInstanceGuard, instance_lock_path};
pub use models::{
    AgentMetadataRow, AssistantAgentCenterRow, AssistantDefinitionRevisionRow, AssistantDefinitionRow,
    AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow, AssistantRow, ConversationArtifactRow,
    ConversationAssistantSnapshotRow, ConversationCapabilitySnapshotRow, ConversationInputRow,
    CreateAssistantDefinitionRevisionParams, CreateAssistantParams, CreateExperienceArticleParams,
    CreateSkillEvolutionProposalParams, ExperienceArticleRow, ExternalUserProjection, FolderRow,
    JournalProjectionCheckpointRow, OrderItemType, OrderScene, ProjectExplorerRow, ProjectKind, ProjectRow, Role,
    SkillEvolutionProposalRow, SkillEvolutionSettingsRow, SkillImportRecordRow, SkillRegistryInstallRow, SkillRow,
    UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpdateAssistantParams,
    UpdateSkillEvolutionProposalParams, UpsertAgentMetadataParams, UpsertAssistantAgentCenterParams,
    UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams, UpsertAssistantPreferenceParams,
    UpsertConversationAssistantSnapshotParams, UpsertConversationCapabilitySnapshotParams,
    UpsertJournalProjectionCheckpointParams, UpsertOverrideParams, UpsertSkillEvolutionSettingsParams, UsageEventRow,
    UserOrderRow, UserStatus, UserType,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationInputInsert, ConversationInputUpdate, ConversationRowUpdate,
    MentionableCandidatesParams, MessagePageCursor, MessagePageDirection, MessagePageParams, MessagePageResult,
    MessageRowUpdate, MessageSearchRow, StaleRuntimeMessageRow,
};
pub use repository::cron::{
    ClaimCronRunParams, CronRunClaimResult, FinishCronRunParams, RecoverableCronRun, UpdateCronJobParams,
};
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::skill::{CreateSkillImportRecordParams, UpsertSkillParams, UpsertSkillRegistryInstallParams};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::{
    ActivityCursor, ArchiveScope, CreateAcpSessionParams, FeedbackDiagnosticsDbContext, FeedbackDiagnosticsProfile,
    FeedbackDiagnosticsProfileResult, FeedbackDiagnosticsRequest, FeedbackDiagnosticsResult, IAcpSessionRepository,
    IAgentMetadataRepository, IAssistantAgentCenterRepository, IAssistantDefinitionRepository,
    IAssistantDefinitionRevisionRepository, IAssistantOverlayRepository, IAssistantOverrideRepository,
    IAssistantPreferenceRepository, IAssistantRepository, IChannelRepository, IClientPreferenceRepository,
    IConversationRepository, ICronRepository, IExperienceArticleRepository, IFeedbackDiagnosticsRepository,
    IMcpServerRepository, IOAuthTokenRepository, IProjectStore, IProviderRepository, IRemoteAgentRepository,
    ISettingsRepository, ISidebarStore, ISkillEvolutionProposalRepository, ISkillEvolutionSettingsRepository,
    ISkillRepository, ITeamRepository, IUsageEventRepository, IUserOrderStore, IUserRepository, InsertUsageEventParams,
    MoveOutcome, OrderItemRef, PageDirection, PersistedSessionState, PinOutcome, PinnedCursor, SaveRuntimeStateParams,
    SidebarConversationThin, SidebarProjectMeta, SidebarTeamThin, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteAssistantAgentCenterRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantDefinitionRevisionRepository, SqliteAssistantOverlayRepository, SqliteAssistantOverrideRepository,
    SqliteAssistantPreferenceRepository, SqliteAssistantRepository, SqliteChannelRepository,
    SqliteClientPreferenceRepository, SqliteConversationRepository, SqliteCronRepository,
    SqliteExperienceArticleRepository, SqliteFeedbackDiagnosticsRepository, SqliteMcpServerRepository,
    SqliteOAuthTokenRepository, SqliteProjectStore, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteSidebarStore, SqliteSkillEvolutionProposalRepository,
    SqliteSkillEvolutionSettingsRepository, SqliteSkillRepository, SqliteTeamRepository, SqliteUsageEventRepository,
    SqliteUserOrderStore, SqliteUserRepository,
};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
