#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod agent_binding;
pub mod audit;
mod database;
mod error;
pub mod governance;
mod instance_lock;
mod legacy_handoff;
mod migrate_repair;
pub mod models;
mod repository;

pub use agent_binding::{
    AgentBindingResolution, binding_resolution_for_agent, resolve_agent_binding, resolve_agent_binding_for_user,
    resolve_agent_binding_from_rows, runtime_backend_for_agent,
};
pub use audit::AuditSink;
pub use database::{
    DATABASE_NEWER_THAN_APP_STAGE, Database, DatabaseInitError, DatabaseInitOptions, init_database,
    init_database_memory, init_database_staged, init_database_staged_with_options, init_database_with_options,
    latest_known_migration_version, maybe_copy_legacy_database,
};
pub use error::{
    DbError, SQLITE_BUSY_MESSAGE_MARKERS, SQLITE_UNIQUE_VIOLATION_MARKER, message_indicates_busy,
    message_indicates_unique_violation,
};
pub use governance::{
    GovernanceError, GovernanceService, LaunchFacts, LaunchSurface, PolicyDenial, PolicyKey, PolicySet, UserRole,
};
pub use instance_lock::{DataDirInstanceGuard, instance_lock_path};
pub use models::AuditEventRow;
pub use models::UserDeviceRow;
pub use models::{
    AgentMetadataRow, AgentWorkflowRunRow, AssistantAgentCenterRow, AssistantDefinitionRevisionRow,
    AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow, AssistantRow,
    BatchItemRow, BatchJobRow, ContextSnapshotArtifactRow, ContextSnapshotRow, ConversationArtifactRow,
    ConversationAssistantSnapshotRow, ConversationCapabilitySnapshotRow, ConversationInputRow,
    CreateAgentWorkflowRunParams, CreateAssistantDefinitionRevisionParams, CreateAssistantParams,
    CreateExperienceArticleParams, CreateLocalKbDocumentParams, CreateLocalKbSourceParams,
    CreateSkillEvolutionProposalParams, ExperienceArticleRow, ExternalUserProjection, FolderRow, InboxItemRow,
    JournalProjectionCheckpointRow, LocalKbDocumentRow, LocalKbSettingsRow, LocalKbSourceRow, OrderItemType,
    OrderScene, ProjectExplorerRow, ProjectKind, ProjectRow, Role, SkillEvolutionProposalRow,
    SkillEvolutionSettingsRow, SkillGovernanceRow, SkillImportRecordRow, SkillRegistryInstallRow, SkillRow,
    SkillSourceRow, SkillVersionRow, TaskAcceptanceCriterionRow, TaskApprovalRow, TaskArtifactRow, TaskCheckpointRow,
    TaskEvidenceRow, TaskRunRow, TaskSessionRow, TaskTraceEventRow, UpdateAgentAvailabilitySnapshotParams,
    UpdateAgentHandshakeParams, UpdateAssistantParams, UpdateSkillEvolutionProposalParams, UpsertAgentMetadataParams,
    UpsertAssistantAgentCenterParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams,
    UpsertConversationCapabilitySnapshotParams, UpsertJournalProjectionCheckpointParams, UpsertLocalKbSettingsParams,
    UpsertOverrideParams, UpsertSkillEvolutionSettingsParams, UsageEventRow, UserOrderRow, UserStatus, UserType,
    evolution_auto_apply_allowed, review_status_is_live,
};
pub use repository::ILocalKbRepository;
pub use repository::SqliteAuditEventRepository;
pub use repository::SqliteLocalKbRepository;
pub use repository::SqliteUserDeviceRepository;
pub use repository::audit_event::{
    AUDIT_LIST_DEFAULT_LIMIT, AUDIT_LIST_MAX_LIMIT, AuditActorKind, AuditChainVerification, AuditEventFilter,
    AuditOrder, AuditVia, IAuditEventRepository, NewAuditEvent,
};
pub use repository::batch::{BatchJobListFilter, CreateBatchItemParams, CreateBatchJobParams, FinishBatchItemParams};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationInputInsert, ConversationInputUpdate, ConversationRowUpdate,
    MentionableCandidatesParams, MessagePageCursor, MessagePageDirection, MessagePageParams, MessagePageResult,
    MessageRowUpdate, MessageSearchRow, StaleRuntimeMessageRow,
};
pub use repository::cron::{
    ClaimCronRunParams, CronRunClaimResult, FinishCronRunParams, RecoverableCronRun, UpdateCronJobParams,
};
pub use repository::governance::{
    AgentRunPermissionRow, IGovernanceRepository, PolicyRow, SqliteGovernanceRepository, UserRoleRow,
};
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::skill::{CreateSkillImportRecordParams, UpsertSkillParams, UpsertSkillRegistryInstallParams};
pub use repository::skill_source::{
    AppendSkillVersionOutcome, AppendSkillVersionParams, ISkillSourceRepository, UpsertSkillSourceParams,
};
pub use repository::squad::{
    CreateSquadTemplateParams, ISquadTemplateRepository, SQUAD_NAME_CONFLICT_PREFIX, SQUAD_STALE_CONFLICT_PREFIX,
    UpdateSquadTemplateParams, WorkItemAssignment,
};
pub use repository::task_board::{
    AppendTaskCommentParams, BOARD_STALE_CONFLICT_PREFIX, BoardAssignee, TaskBoardFilter, UpdateTaskBoardParams,
};
pub use repository::task_session::{
    AppendTaskTraceEventParams, CreateContextSnapshotParams, CreatePlanningTaskRunParams, CreateRetryTaskRunParams,
    CreateTaskArtifactParams, CreateTaskCheckpointParams, CreateTaskEvidenceParams, CreateTaskRunParams,
    CreateTaskSessionParams, FinishTaskRunParams, LinkContextSnapshotArtifactParams, ResolveTaskApprovalParams,
    UpdateAcceptanceCriterionParams, UpdateTaskSessionParams,
};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::user_device::{CreateUserDeviceParams, IUserDeviceRepository};
pub use repository::{
    ActivityCursor, ArchiveScope, CreateAcpSessionParams, EVOLUTION_EVIDENCE_TAG, EvolutionEvidence, EvolutionSignal,
    FeedbackDiagnosticsDbContext, FeedbackDiagnosticsProfile, FeedbackDiagnosticsProfileResult,
    FeedbackDiagnosticsRequest, FeedbackDiagnosticsResult, IAcpSessionRepository, IAgentMetadataRepository,
    IAgentWorkflowRunRepository, IAssistantAgentCenterRepository, IAssistantDefinitionRepository,
    IAssistantDefinitionRevisionRepository, IAssistantOverlayRepository, IAssistantOverrideRepository,
    IAssistantPreferenceRepository, IAssistantRepository, IBatchRepository, IChannelRepository,
    IClientPreferenceRepository, IConversationRepository, ICronRepository, IExperienceArticleRepository,
    IFeedbackDiagnosticsRepository, IInboxRepository, IMcpServerRepository, IOAuthTokenRepository, IProjectStore,
    IProviderRepository, IRemoteAgentRepository, ISettingsRepository, ISidebarStore, ISkillEvolutionProposalRepository,
    ISkillEvolutionSettingsRepository, ISkillRepository, ITaskBoardRepository, ITaskSessionRepository, ITeamRepository,
    IUsageEventRepository, IUserOrderStore, IUserRepository, InboxFilter, InsertUsageEventParams, MoveOutcome,
    OrderItemRef, PageDirection, PersistedSessionState, PinOutcome, PinnedCursor, SaveRuntimeStateParams,
    SidebarConversationThin, SidebarProjectMeta, SidebarTeamThin, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteAgentWorkflowRunRepository, SqliteAssistantAgentCenterRepository,
    SqliteAssistantDefinitionRepository, SqliteAssistantDefinitionRevisionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteBatchRepository, SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteCronRepository, SqliteExperienceArticleRepository, SqliteFeedbackDiagnosticsRepository,
    SqliteInboxRepository, SqliteMcpServerRepository, SqliteOAuthTokenRepository, SqliteProjectStore,
    SqliteProviderRepository, SqliteRemoteAgentRepository, SqliteSettingsRepository, SqliteSidebarStore,
    SqliteSkillEvolutionProposalRepository, SqliteSkillEvolutionSettingsRepository, SqliteSkillRepository,
    SqliteSkillSourceRepository, SqliteSquadTemplateRepository, SqliteTaskBoardRepository, SqliteTaskSessionRepository,
    SqliteTeamRepository, SqliteUsageEventRepository, SqliteUserOrderStore, SqliteUserRepository,
    UpsertInboxItemParams, UsageEventFilter,
};
pub use repository::{evolution_evidence_tags, record_evolution_evidence};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
