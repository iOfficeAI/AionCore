#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod agent_binding;
mod database;
mod error;
mod instance_lock;
mod legacy_handoff;
pub mod models;
mod repository;

pub use agent_binding::{
    AgentBindingResolution, binding_resolution_for_agent, resolve_agent_binding, resolve_agent_binding_from_rows,
    runtime_backend_for_agent,
};
pub use database::{
    Database, DatabaseInitError, DatabaseInitOptions, init_database, init_database_memory, init_database_staged,
    init_database_staged_with_options, init_database_with_options, maybe_copy_legacy_database,
};
pub use error::{
    DbError, SQLITE_BUSY_MESSAGE_MARKERS, SQLITE_UNIQUE_VIOLATION_MARKER, message_indicates_busy,
    message_indicates_unique_violation,
};
pub use instance_lock::{DataDirInstanceGuard, instance_lock_path};
pub use models::{
    AgentMetadataRow, AppOperationsModelSettingRow, AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow,
    AssistantPreferenceRow, AssistantRow, ConversationArtifactRow, ConversationAssistantSnapshotRow,
    CreateAssistantParams, FolderRow, ProjectExplorerRow, ProjectKind, ProjectRow, Role, SkillImportRecordRow,
    SkillRow, UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpdateAssistantParams,
    UpsertAgentMetadataParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams, UpsertOverrideParams,
};
pub use models::{
    ConversationMemoryPolicyRow, ConversationMemoryRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow, MemoryEntryRow,
    MemoryImportStateRow, MemoryJobHealthRow, MemoryJobRow, MemoryJobTurnRow, MemoryRetrievalRow, MemorySettingsRow,
    MemorySourceRow,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationRowUpdate, LegacyConversationCursor, LegacyConversationImportBoundary,
    LegacyConversationImportPage, MessagePageCursor, MessagePageDirection, MessagePageParams, MessagePageResult,
    MessageRowUpdate, MessageSearchRow,
};
pub use repository::cron::{
    ClaimCronRunParams, CronRunClaimResult, FinishCronRunParams, RecoverableCronRun, UpdateCronJobParams,
};
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::memory::{
    BoundedMemoryTurnMessagesRow, ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition,
    CommitMemorySourceRow, CommitMemoryUpdateResult, CommitMemoryUpdateRow, ConsumeMemoryRetrievalSnapshotRow,
    CreateMemoryRetrievalSnapshotRow, EnqueueMemoryTurnRow, ExpectedMemoryEntryRow, FinalizeMemoryJobSnapshotResult,
    FinalizeMemoryJobSnapshotRow, ImportLegacyMemoryPageRow, LegacyMemorySummaryRow, MEMORY_EVIDENCE_MAX_BYTES,
    MEMORY_EVIDENCE_MAX_MESSAGES, MemoryCandidateQueryRow, MemoryChangeSetQueryRow, MemoryEntryQueryRow,
    MemoryEvidenceMessageKind, MemoryReconciliationSnapshotRow, MemoryRetrievalItemRow, MemoryRetrievalSnapshotRow,
    MemoryTurnSnapshotExpectationRow, ReleaseMemoryLeaseRow, RenewMemoryLeaseRow, ResolveMemoryConflictActionRow,
    ResolveMemoryConflictRow, SplitMemoryJobRow, TransitionMemoryJobRow, UpdateConversationMemoryLifecycleRow,
    UpdateConversationMemoryPolicyRow, UpdateMemoryEntryRow, UpdateMemoryLifecycleRow, UpdateMemorySettingsRow,
    derive_memory_fingerprint, memory_entry_content_hash, memory_evidence_content, memory_summary_conversation_id,
    memory_summary_selection_id,
};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::skill::{CreateSkillImportRecordParams, UpsertSkillParams};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::{
    CreateAcpSessionParams, FeedbackDiagnosticsDbContext, FeedbackDiagnosticsProfile, FeedbackDiagnosticsProfileResult,
    FeedbackDiagnosticsRequest, FeedbackDiagnosticsResult, IAcpSessionRepository, IAgentMetadataRepository,
    IAssistantDefinitionRepository, IAssistantOverlayRepository, IAssistantOverrideRepository,
    IAssistantPreferenceRepository, IAssistantRepository, IChannelRepository, IClientPreferenceRepository,
    IConversationRepository, ICronRepository, IFeedbackDiagnosticsRepository, IMcpServerRepository, IMemoryRepository,
    IOAuthTokenRepository, IProjectStore, IProviderRepository, IRemoteAgentRepository, ISettingsRepository,
    ISkillRepository, ITeamRepository, IUserRepository, PersistedSessionState, SaveRuntimeStateParams,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantOverlayRepository, SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository,
    SqliteAssistantRepository, SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteCronRepository, SqliteFeedbackDiagnosticsRepository, SqliteMcpServerRepository, SqliteMemoryRepository,
    SqliteOAuthTokenRepository, SqliteProjectStore, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteSkillRepository, SqliteTeamRepository, SqliteUserRepository,
};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
