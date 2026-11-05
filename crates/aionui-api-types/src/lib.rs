#![warn(clippy::disallowed_types)]

//! All HTTP request/response DTOs shared across the API surface.
mod acp;
mod acp_prompt_hook;
mod agent_build_extra;
mod agent_center;
mod agent_discovery;
mod agent_error;
mod antigravity_hook;
mod ask;
mod assistant;
mod auth;
mod batch;
mod channel;
mod chat_file;
mod codex_account;
mod confirmation;
mod connection_test;
mod context;
mod conversation;
mod conversation_tools;
mod cron;
mod custom_agent;
mod extension;
mod file;
mod inbox;
mod lifecycle;
mod mcp;
mod office;
mod project;
mod provider;
mod remote_agent;
mod response;
mod runtime;
mod session_tools;
mod shell;
mod sidebar;
mod skill;
mod skill_delivery;
mod skill_evolution;
mod skill_governance;
mod skill_runtime;
mod squad;
mod system;
mod task_board;
mod task_session;
mod team;
mod team_mcp;
mod team_tools;
mod usage;
mod websocket;

pub use acp::{
    AcpConfigOptionDto, AcpConfigSelectOptionDto, AcpEnvResponse, AgentModeResponse, CanonicalReplayProjectionResponse,
    ConfigOptionConfirmation, ConversationTrajectoryChangedEvent, DetectCliRequest, DetectCliResponse,
    GetConfigOptionsResponse, GetModelInfoResponse, HostPolicyResponse, JournalTranscriptItem,
    JournalTranscriptResponse, JournalTranscriptTokenNode, JournalTranscriptTokens, ModelInfoEntry, ModelInfoPayload,
    ProbeModelRequest, RawTrajectoryEventV1, RawTrajectoryProjectionV1, RetainedOutputResponse, SetConfigOptionRequest,
    SetConfigOptionResponse, SetHostPolicyRequest, SetModeRequest, SetModelRequest, SideQuestionRequest,
    SideQuestionResponse, TrajectoryOverviewV1, TrajectoryProjectionV1, TrajectoryQuery, TrajectoryRecordV1,
    TrajectoryTokenUsage, TryConnectCustomAgentRequest, TryConnectCustomAgentResponse, WorkspaceBrowseQuery,
    WorkspaceEntry,
};
pub use acp_prompt_hook::AcpPromptHookWarningPayload;
pub use agent_build_extra::{
    AcpBuildExtra, AcpModelInfo, AionrsBuildExtra, ForkSpec, SessionMcpServer, SessionMcpTransport,
    SlashCommandCompletionBehavior, SlashCommandItem,
};
pub use agent_center::{
    AdvanceAgentWorkflowRunRequest, AgentAclRole, AgentCenterDetailResponse, AgentCenterListItem, AgentCenterListQuery,
    AgentCenterMeta, AgentCenterMetaPatch, AgentCenterPreviewMode, AgentCenterRevisionResponse,
    AgentCenterRunPlanResponse, AgentMcpPolicy, AgentPublishStatus, AgentRoleBinding, AgentSkillRef, AgentVisibility,
    AgentWorkflowAgentPlan, AgentWorkflowApprovalDecision, AgentWorkflowCancellationStatus, AgentWorkflowDefinition,
    AgentWorkflowEdgeDefinition, AgentWorkflowInputDefinition, AgentWorkflowNextAction, AgentWorkflowNodeDefinition,
    AgentWorkflowNodeRun, AgentWorkflowNodeRunAttempt, AgentWorkflowNodeRunStatus, AgentWorkflowOutputDefinition,
    AgentWorkflowOutputFieldDefinition, AgentWorkflowOutputFieldType, AgentWorkflowOutputFormat,
    AgentWorkflowRunResponse, AgentWorkflowRunStatus, CreateAgentCenterRequest, CreateConversationRequestWire,
    DecideAgentWorkflowApprovalRequest, KnowledgeScopeRef, PublishAgentCenterRequest, SkillVersionPolicy,
    StartAgentWorkflowRunRequest, UpdateAgentCenterRequest,
};
pub use agent_discovery::{
    AgentEnvEntry, AgentHandshake, AgentLogoEntry, AgentManagementRow, AgentManagementStatus, AgentMetadata,
    AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource, AgentSourceInfo, BehaviorPolicy,
    ManagedRuntimeSource, ManagedRuntimeState, ManagedRuntimeStatus, RETIRED_DEEPSEEK_HARNESS_BACKEND, SessionLifetime,
};
pub use agent_error::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget,
    AgentStreamErrorData,
};
pub use antigravity_hook::{
    AntigravityHookConfig, AntigravityHookDecision, AntigravityHookInput, AntigravityHookOutput,
    AntigravityHookToolCall,
};
pub use ask::{AskAnswerRequest, AskQuestionAnswer};
pub use assistant::{
    ASSISTANT_MCP_BINDING_CHANGED_EVENT, AssistantAgentResponse, AssistantCapabilitiesResponse,
    AssistantDefaultListRequest, AssistantDefaultListResponse, AssistantDefaultScalarRequest,
    AssistantDefaultScalarResponse, AssistantDefaultsRequest, AssistantDefaultsResponse, AssistantDetailResponse,
    AssistantEngineResponse, AssistantMcpBindingChanged, AssistantPreferencesResponse, AssistantProfileResponse,
    AssistantPromptsResponse, AssistantResponse, AssistantRulesResponse, AssistantSource, AssistantStateResponse,
    CreateAssistantRequest, ImportAssistantsRequest, ImportAssistantsResult, ImportError, SetAssistantStateRequest,
    UpdateAssistantRequest, assistant_avatar_response_value, assistant_avatar_response_value_with_version,
    is_local_avatar_value,
};
pub use auth::{
    AuthStatusResponse, ChangePasswordRequest, EnsureExternalSessionRequest, EnsureExternalSessionResponse,
    EnsureExternalUserRequest, EnsureExternalUserResponse, ExternalUserType, InternalAuthErrorCode, LoginRequest,
    LoginResponse, PublicUser, QrLoginRequest, RefreshResponse, RefreshTokenRequest, RevokeExternalSessionRequest,
    RevokeExternalSessionResponse, UserInfoResponse, WebuiChangePasswordRequest, WebuiChangeUsernameRequest,
    WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse, WebuiResetPasswordResponse, WsTokenResponse,
};
pub use batch::{
    BATCH_DEFAULT_CONCURRENCY, BATCH_DEFAULT_ITEM_TIMEOUT_SEC, BATCH_DEFAULT_TRIAL_SIZE, BATCH_MAX_CHECKS,
    BATCH_MAX_CONCURRENCY, BATCH_MAX_ITEM_TIMEOUT_SEC, BATCH_MAX_TARGETS, BATCH_MAX_TRIAL_SIZE,
    BATCH_MIN_ITEM_TIMEOUT_SEC, BATCH_MODE_AGENT, BatchCheckResult, BatchCheckSpec, BatchCounts, BatchItemResponse,
    BatchJobResponse, BatchListQuery, BatchRetryRequest, BatchTemplate, CreateBatchJobRequest,
    EVENT_BATCH_ITEM_UPDATED, EVENT_BATCH_JOB_UPDATED, batch_item_status, batch_job_status,
};
pub use channel::{
    ApprovePairingRequest, BridgeResponse, ChannelAssistantSettingRequest, ChannelAssistantSettingResponse,
    ChannelDefaultModelSetting, ChannelPlatformSettingsResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PairingRequestedPayload,
    PluginStatusChangedPayload, PluginStatusResponse, RejectPairingRequest, RevokeUserRequest,
    SyncChannelSettingsRequest, TestPluginExtraConfig, TestPluginRequest, TestPluginResponse, UserAuthorizedPayload,
};
pub use chat_file::{
    ChatFileRef, PromptAttachmentDelivery, PromptAttachmentMediaType, PromptAttachmentSource, PromptAttachmentV1,
};
pub use codex_account::{
    CodexAccountErrorCode, CodexAccountSnapshot, CodexAccountUsageSummary, CodexAccountUsageView, CodexAccountView,
    CodexAccountWarning, CodexAuthState, CodexCreditsSnapshot, CodexDailyUsageBucket, CodexDiagnosticCheck,
    CodexDiagnosticStatus, CodexDiagnosticsView, CodexLoginAttempt, CodexLoginAttemptState, CodexLoginStartResponse,
    CodexRateLimitBucket, CodexRateLimitWindow, CodexRateLimitsView, CodexResetCredit, CodexResetCreditSummary,
    CodexSnapshotFreshness, CodexSnapshotSource, CodexSpendControlSnapshot, CodexUsageAvailability,
};
pub use confirmation::{ApprovalCheckQuery, ApprovalCheckResponse, ConfirmRequest, ConfirmationListResponse};
pub use connection_test::TestBedrockConnectionRequest;
pub use context::{
    ContextAuthorization, ContextAuthorizationRequest, ContextBudget, ContextDiscoveryRequest, ContextDocument,
    ContextFetchRequest, ContextHit, ContextProvenance, ContextProviderCapabilities, ContextProviderIdentity,
    ContextProviderSelection, ContextPurpose, ContextQuery, ContextResolutionOutcome, ContextResolutionRequest,
    ContextResolutionWarning, ContextResourceRef, ContextScope, ContextSnapshot, ContextSource,
    ContextSourceRecommendation,
};
pub use conversation::{
    ActiveCountResponse, AssistantConversationOverridesRequest, AssistantConversationRequest,
    CancelConversationRequest, CancelConversationResponse, CancellationChangedEvent, CancellationState,
    CapabilitiesChangedEvent, CloneConversationRequest, ConversationArtifactKind, ConversationArtifactListResponse,
    ConversationArtifactResponse, ConversationArtifactStatus, ConversationAssistantIdentityResponse,
    ConversationCapabilities, ConversationInputListResponse, ConversationInputMode, ConversationInputReceipt,
    ConversationInputResponse, ConversationInputStatus, ConversationListResponse, ConversationMcpStatus,
    ConversationMcpStatusKind, ConversationNameUpdatedPayload, ConversationResponse, ConversationRuntimeStateKind,
    ConversationRuntimeSummary, ConversationToolSurface, CreateConversationRequest, EnsureConversationRuntimeResponse,
    ForkCapabilityView, ForkConversationRequest, InputChangedEvent, ListConversationInputsQuery,
    ListConversationsQuery, ListMessagesQuery, McpDiscovery, McpIsolation, McpRuntimeSnapshot, MessageListResponse,
    MessageResponse, MessageSearchItem, MessageSearchResponse, MessageStatusChangedPayload, PromptCapabilityView,
    SearchMessagesQuery, SendMessageRequest, SendMessageResponse, SessionRef, SubmitConversationInputRequest,
    ToolEnforcementLevel, ToolSurfaceEntry, ToolSurfaceKind, ToolSurfaceMcpServer, ToolSurfaceSource,
    UpdateConversationArtifactRequest, UpdateConversationRequest, canonical_mcp_tool_name, normalize_aionrs_tools,
};
pub use conversation_tools::{
    CONVERSATION_TOOLS_SCHEMA_VERSION, ConversationCliEnvelope, ConversationCliMeta, ConversationCreateAssistant,
    ConversationCreateRequest, ConversationCreateResponse, ConversationToolDescriptor, ConversationToolErrorCode,
    ConversationToolErrorPayload, ConversationToolName, conversation_tool_descriptor, conversation_tool_descriptors,
    tool_name_for_conversation_cli_path,
};
pub use cron::{
    CreateConversationCronRequest, CreateConversationCronResponse, CreateCronJobRequest, CronAgentConfigReadDto,
    CronAgentConfigWriteDto, CronJobExecutedEvent, CronJobMetadataDto, CronJobPayloadDto, CronJobRemovedPayload,
    CronJobResponse, CronJobStateDto, CronJobTargetDto, CronScheduleDto, HasSkillResponse, ListCronJobsQuery,
    RunNowResponse, SaveCronSkillRequest, UpdateConversationCronRequest, UpdateCronJobRequest,
};
pub use custom_agent::{
    AgentOverridesResponse, CustomAgentAdvancedOverrides, CustomAgentUpsertRequest, DeleteCustomAgentResponse,
    SetAgentOverridesRequest, SetEnabledRequest,
};
pub use extension::{
    DisableExtensionRequest, EnableExtensionRequest, ExtensionSummaryResponse, GetI18nRequest, GetPermissionsRequest,
    GetRiskLevelRequest, HubExtensionListItem, HubExtensionListResponse, HubOperationResponse, HubUpdateInfo,
    InstallExtensionRequest, PermissionDetailResponse, PermissionSummaryResponse,
};
pub use file::{
    ContentEncoding, ContentMetadataRequest, CopyFailure, CopyFilesRequest, CopyFilesResponse, CopyTarget,
    DirOrFileResponse, FetchRemoteImageRequest, FileChangeInfoResponse, FileMetadataResponse, GetFileMetadataRequest,
    GetFilesByDirRequest, GetImageBase64Request, ListWorkspaceFilesRequest, OpenSystemFileRequest, ReadContentRequest,
    ReadFileRequest, RevealItemRequest, SnapshotBaselineRequest, SnapshotCompareResponse, SnapshotDiscardRequest,
    SnapshotInfoResponse, SnapshotMode, SnapshotStageRequest, SnapshotWorkspaceRequest, StreamQuery,
    WorkspaceFlatFileResponse, WriteContentRequest, WriteFileRequest,
};
pub use inbox::{
    AUTOPILOT_INBOX_TITLE_PREFIX, INBOX_ORIGIN_AUTOPILOT, INBOX_ORIGIN_BATCH, InboxItemDto, InboxListQuery,
    InboxListResponse, InboxMarkReadResponse, TASK_APPROVAL_INBOX_KEY_PREFIX, inbox_origin, inbox_task_approval_id,
};
pub use lifecycle::{GitHubReleaseAsset, SystemInfoResponse, UpdateCheckRequest, UpdateCheckResult, UpdateReleaseInfo};
pub use mcp::{
    BatchImportMcpServersRequest, CreateMcpServerRequest, DetectedMcpServerEntry, DetectedMcpServerResponse,
    ImportMcpServerRequest, McpAuthMethod, McpConnectionTestErrorCode, McpConnectionTestResult, McpServerResponse,
    McpToolResponse, McpTransport, OAuthCheckStatusRequest, OAuthLoginRequest, OAuthLoginResponse, OAuthLogoutRequest,
    OAuthStatusResponse, TestMcpConnectionRequest, UpdateMcpServerRequest,
};
pub use office::{
    CellCoord, CellRange, ConversionResultDto, ConversionTarget, DocumentConversionRequest, DocumentConversionResponse,
    ExcelSheetData, ExcelSheetImage, ExcelWorkbookData, PptJsonData, PptSlideData, PresentationAssetImportRequest,
    PresentationAssetImportResponse, PresentationCatalogControl, PresentationCatalogLayout,
    PresentationCatalogResponse, PresentationCatalogSlot, PresentationCatalogTheme, PresentationDiagnostic,
    PresentationFileRequest, PresentationRenderJob, PresentationRenderRequest, PresentationRenderStatus,
    PresentationValidationResponse, PreviewState, PreviewStatusEvent, PreviewUrlResponse, RefreshPreviewRequest,
    RefreshPreviewResponse, StartPreviewRequest, StopPreviewRequest,
};
pub use project::{
    AttachFolderRequest, ProjectDetailResponse, ProjectEntry, ProjectExplorer, ResolveRefRequest, ResolveRefResponse,
};
pub use provider::{
    BedrockAuthMethod, BedrockConfig, CreateProviderRequest, DetectProtocolRequest, DetectionSuggestion,
    FetchModelsAnonymousRequest, FetchModelsRequest, FetchModelsResponse, HealthStatus, KeyTestResult, ModelCapability,
    ModelHealthStatus, ModelImageInputCapability, ModelInfo, ModelOpenAiApiMode, ModelSettings, ModelType,
    MultiKeyResult, ProtocolDetectionResponse, ProviderHealthCheckErrorKind, ProviderHealthCheckRequest,
    ProviderHealthCheckResponse, ProviderResponse, SuggestionType, UpdateProviderRequest,
};
pub use remote_agent::{
    CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
pub use response::{ApiResponse, ErrorResponse};
pub use runtime::{
    EnsureNodeRuntimeRequest, EnsureNodeRuntimeResponse, RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload,
    RuntimeStatusPhase, RuntimeStatusScope, RuntimeStatusScopeKind,
};
pub use session_tools::{
    SESSION_TOOLS_SCHEMA_VERSION, SessionCliEnvelope, SessionCliMeta, SessionDeliveryStatus, SessionMentionTarget,
    SessionMentionableQuery, SessionMentionableResponse, SessionMessageRateLimitedPayload, SessionRateGate,
    SessionSendMessageRequest, SessionSendMessageResponse, SessionToolDescriptor, SessionToolErrorCode,
    SessionToolErrorPayload, SessionToolName, session_tool_descriptor, session_tool_descriptors,
    tool_name_for_session_cli_path,
};
pub use shell::{
    CheckToolInstalledRequest, CheckToolInstalledResponse, DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig,
    OpenExternalRequest, OpenFileRequest, OpenFolderWithRequest, ShowItemInFolderRequest, SpeechToTextConfig,
    SpeechToTextProvider, SpeechToTextResult, SttStreamClientMessage, SttStreamServerMessage, ToolType,
};
pub use sidebar::{
    ArchiveDeleteResult, MoveOrderRequest, OrderItemRefDto, RemoveProjectItem, RemoveProjectItemKind,
    RemoveProjectResult, SidebarGroup, SidebarItem, SidebarItemsResponse, SidebarResponse, SidebarScope,
    SidebarTeamItem,
};
pub use skill::{
    AddExternalPathRequest, DeleteSkillRequest, ExportSkillRequest, ExternalSkillSourceResponse,
    ImportSkillFailureResponse, ImportSkillRequest, ImportSkillResponse, InstallOfficialSkillRequest,
    MaterializeSkillsRequest, MaterializeSkillsResponse, MaterializedSkillRef, NamedPathResponse, OfficialSkillDetail,
    OfficialSkillFile, OfficialSkillInstallStatus, OfficialSkillInstallationResponse, OfficialSkillSearchQuery,
    OfficialSkillSearchResponse, OfficialSkillSummary, OfficialSkillVersionResponse, ReadAssistantRuleRequest,
    ReadBuiltinResourceRequest, ReadSkillInfoRequest, ReadSkillInfoResponse, RemoveExternalPathRequest,
    ScanForSkillsRequest, ScanForSkillsResponse, ScannedSkillResponse, SkillImportLimitsResponse,
    SkillImportRecordResponse, SkillListItemResponse, SkillPathsResponse, SkillRegistryOriginResponse,
    SkillSourceResponse, UpdateOfficialSkillRequest, WriteAssistantRuleRequest,
};
pub use skill_delivery::{SkillDelivery, SkillDeliveryMode, SkillDeliveryParse, parse_skill_delivery};
pub use skill_evolution::{
    ApplySkillEvolutionRequest, ApplySkillEvolutionResponse, ApproveSkillEvolutionResponse,
    CreateExperienceArticleRequest, CreateSkillEvolutionProposalRequest, CrossModelTransferNoteResponse,
    EvolveSkillEvolutionRequest, EvolveSkillEvolutionResponse, ExperienceArticleResponse, ExperienceListQuery,
    ExperienceVisibility, ReviewSkillEvolutionRequest, SkillEvolutionAction, SkillEvolutionExportPayload,
    SkillEvolutionGateMode, SkillEvolutionGateRecommendation, SkillEvolutionGateSignal, SkillEvolutionListQuery,
    SkillEvolutionProposalResponse, SkillEvolutionSettingsResponse, SkillEvolutionSkillRefPayload,
    SkillEvolutionStatus, SkillEvolutionTrajectoryOverview, UpdateSkillEvolutionSettingsRequest,
};
pub use skill_governance::{
    ImportFromSourceRequest, ImportFromSourceResponse, ImportSourceKind, ReviewDecision, ReviewSkillRequest,
    SetVisibilityRequest, SkillFileDiff, SkillGovernanceInfo, SkillRestoreResponse, SkillSourceDetailResponse,
    SkillSourceInfo, SkillSourceSyncResult, SkillVersionDiffResponse, SkillVersionInfo, TeamSkillInfo,
    TeamSkillLibraryResponse,
};
pub use skill_runtime::{
    RuntimeSkillFileQuery, RuntimeSkillFileResponse, RuntimeSkillListItem, RuntimeSkillListResponse,
    RuntimeSkillShowResponse, SKILL_RUNTIME_SCHEMA_VERSION, SkillRuntimeEnvelope, SkillRuntimeErrorCode,
    SkillRuntimeErrorPayload, SkillRuntimeMeta,
};
pub use squad::{
    AIONRS_ROUTE_ONLY_LEADER_BUILTINS, AssistantBrief, CLAUDE_SUBAGENT_TOOLS, CLAUDE_WRITE_EXEC_TOOLS,
    DeleteSquadResponse, ListSquadsQuery, NativeToolEnforcement, SQUAD_LEADER_BACKEND_UNENFORCEABLE,
    SQUAD_MAX_PARTICIPANTS, SQUAD_ROUTE_ONLY_LEADER_TOOLS, SQUAD_SUBAGENT_SPAWN_FORBIDDEN, SquadInstantiateInput,
    SquadInstantiateResponse, SquadLeaderMode, SquadMember, SquadPreview, SquadPreviewBlocker, SquadPreviewParticipant,
    SquadRunPayload, SquadRunState, SquadTemplate, SquadTemplateInput, SquadTemplatePatch, SquadToolProfile,
    SquadVisibility, SquadWorkItemWorktree, aionrs_route_only_leader_allowed_tools, claude_disallowed_tools,
    squad_native_tool_enforcement,
};
pub use system::{
    ClientPreferencesResponse, CurrentUserResponse, FeedbackDiagnosticsContextResponse,
    FeedbackDiagnosticsPrivacyResponse, FeedbackDiagnosticsProfileResponse, FeedbackDiagnosticsQuery,
    FeedbackDiagnosticsResponse, SystemSettingsResponse, UpdateClientPreferencesRequest, UpdateSettingsRequest,
};
pub use task_board::{
    CreateTaskCommentRequest, EVENT_TASK_BOARD_UPDATED, EVENT_TASK_COMMENT_CREATED, EVENT_TASK_COMMENT_UPDATED,
    EditTaskCommentRequest, ImportGithubIssuesRequest, ImportGithubIssuesResponse, ImportedIssueItem,
    ImportedIssueOutcome, TASK_BOARD_COLUMNS, TaskAssignee, TaskAssigneeKind, TaskBoardChange, TaskBoardColumnResponse,
    TaskBoardItemResponse, TaskBoardQuery, TaskBoardResponse, TaskBoardReviewDecision, TaskBoardReviewRequest,
    TaskBoardReviewResponse, TaskBoardStatus, TaskBoardUpdatedEvent, TaskCommentEvent, TaskCommentKind,
    TaskCommentResponse, UpdateTaskBoardRequest,
};
pub use task_session::{
    AcceptanceCriterionResponse, AcceptanceCriterionStatus, AcceptanceEvidence, AcceptanceEvidenceKind,
    AgentIntegrationMode, CreateTaskSessionRequest, DecideTaskApprovalRequest, ExecuteApprovedPlanRequest,
    ListTaskSessionsQuery, PlanningIsolationLevel, PlanningIsolationResponse, PolicyDecision, PolicyDecisionKind,
    RuntimeBindingResponse, RuntimeBindingState, StartAutomaticPlanningRequest, SubmitTaskArtifactRequest,
    SubmitTaskArtifactResponse, TASK_RETRY_MAX_ATTEMPTS_LIMIT, TASK_RUN_TIMEOUT_MAX_MS, TASK_RUN_TIMEOUT_MIN_MS,
    TaskApprovalDecision, TaskApprovalResponse, TaskApprovalStatus, TaskArtifactKind, TaskArtifactResponse,
    TaskArtifactStatus, TaskCheckpointResponse, TaskEvidenceResponse, TaskReviewResponse, TaskReviewSummary,
    TaskRunReplayQuery, TaskRunReplayResponse, TaskRunResponse, TaskRunStatus, TaskSessionMode, TaskSessionResponse,
    TaskSessionStatus, TaskTraceEventResponse, ToolCapability, UpdateTaskRetryPolicyRequest, UpdateTaskSessionRequest,
    VerifyAcceptanceCriterionRequest,
};
pub use team::{
    AddAgentRequest, CancelTeamChildTurnRequest, CancelTeamRunRequest, CreateTeamRequest, InterruptTeamAgentRequest,
    PauseTeamSlotRequest, RenameAgentRequest, RenameTeamRequest, SendAgentMessageRequest, SendTeamMessageRequest,
    TeamActivityCursor, TeamActivityItemResponse, TeamActivityKind, TeamActivityPageResponse, TeamAgentInput,
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentResponse, TeamAgentRuntimeStatus,
    TeamAgentRuntimeStatusPayload, TeamAgentSpawnedPayload, TeamAgentStatusPayload, TeamChildTurnPayload,
    TeamConfigPersistenceStatus, TeamContextResetAvailability, TeamContextResetCapability, TeamContextResetNotice,
    TeamContextResetResponse, TeamContextResetRuntimeStatus, TeamContextResetStatus, TeamInterruptAgentResponse,
    TeamInterruptOutcome, TeamListResponse, TeamMailboxChange, TeamMailboxChangedPayload, TeamMailboxMessageResponse,
    TeamMcpRuntimeConfig, TeamMcpSelection, TeamMessageEnqueueStatus, TeamQueuedPolicy, TeamResponse,
    TeamRunAckResponse, TeamRunPayload, TeamRunSource, TeamRunStateResponse, TeamRunStatus, TeamRunTargetRole,
    TeamRuntimeSeed, TeamSendMessageQueuedResponse, TeamSessionBinding, TeamSessionPhase, TeamSessionStatus,
    TeamSessionStatusPayload, TeamSetConfigOptionResponse, TeamSlotBlockedReason, TeamSlotWorkChangedPayload,
    TeamSlotWorkPayload, TeamSlotWorkState, TeamTaskChange, TeamTaskChangedPayload, TeamTaskResponse,
    TeammateMessagePayload, assistant_mcp_binding_fingerprint,
};
pub use team_mcp::{TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
pub use team_tools::{
    TEAM_DESCRIBE_ASSISTANT_DESCRIPTION, TEAM_LIST_ASSISTANTS_DESCRIPTION, TEAM_SPAWN_AGENT_DESCRIPTION,
    TEAM_TOOLS_SCHEMA_VERSION, TeamToolCall, TeamToolCliEnvelope, TeamToolCliMeta, TeamToolContextResponse,
    TeamToolDescriptor, TeamToolErrorCode, TeamToolErrorPayload, TeamToolName, TeamToolPermission, TeamToolRole,
    TeamToolRuntimeCallRequest, TeamToolRuntimeCallResponse, TeamToolTransport, cli_command_for_tool,
    team_tool_descriptor, team_tool_descriptors, team_tool_descriptors_for_role, tool_name_for_cli_path,
};
pub use usage::{
    ContextBreakdownResponse, ContextBreakdownSegment, ContextSegmentSource, UsageCostAmount, UsageEventDto,
    UsageGroupBy, UsageListQuery, UsageListResponse, UsageSummaryGroup, UsageSummaryQuery, UsageSummaryResponse,
};
pub use websocket::WebSocketMessage;

#[cfg(test)]
mod public_contract_tests {
    use super::{AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget};

    #[test]
    fn error_resolution_types_are_exported_from_crate_root() {
        let resolution = AgentErrorResolution::new(
            AgentErrorResolutionKind::Retry,
            Some(AgentErrorResolutionTarget::Feedback),
        );

        assert_eq!(resolution.kind, AgentErrorResolutionKind::Retry);
        assert_eq!(resolution.target, Some(AgentErrorResolutionTarget::Feedback));
    }
}
