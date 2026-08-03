#![warn(clippy::disallowed_types)]

//! All HTTP request/response DTOs shared across the API surface.
mod acp;
mod acp_prompt_hook;
mod agent_build_extra;
mod agent_discovery;
mod agent_error;
mod antigravity_hook;
mod assistant;
mod auth;
mod channel;
mod chat_file;
mod confirmation;
mod connection_test;
mod conversation;
mod cron;
mod custom_agent;
mod extension;
mod file;
mod lifecycle;
mod mcp;
mod office;
mod project;
mod provider;
mod remote_agent;
mod response;
mod runtime;
mod shell;
mod skill;
mod system;
mod team;
mod team_mcp;
mod team_tools;
mod websocket;

pub use acp::{
    AcpConfigOptionDto, AcpConfigSelectOptionDto, AcpEnvResponse, AgentModeResponse, ConfigOptionConfirmation,
    DetectCliRequest, DetectCliResponse, GetConfigOptionsResponse, GetModelInfoResponse, ModelInfoEntry,
    ModelInfoPayload, ProbeModelRequest, SetConfigOptionRequest, SetConfigOptionResponse, SetModeRequest,
    SetModelRequest, SideQuestionRequest, SideQuestionResponse, TryConnectCustomAgentRequest,
    TryConnectCustomAgentResponse, WorkspaceBrowseQuery, WorkspaceEntry,
};
pub use acp_prompt_hook::AcpPromptHookWarningPayload;
pub use agent_build_extra::{
    AcpBuildExtra, AcpModelInfo, AionrsBuildExtra, SessionMcpServer, SessionMcpTransport,
    SlashCommandCompletionBehavior, SlashCommandItem,
};
pub use agent_discovery::{
    AgentEnvEntry, AgentHandshake, AgentLogoEntry, AgentManagementRow, AgentManagementStatus, AgentMetadata,
    AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource, AgentSourceInfo, BehaviorPolicy,
};
pub use agent_error::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget,
    AgentStreamErrorData,
};
pub use antigravity_hook::{
    AntigravityHookConfig, AntigravityHookDecision, AntigravityHookInput, AntigravityHookOutput,
    AntigravityHookToolCall,
};
pub use assistant::{
    AssistantAgentResponse, AssistantCapabilitiesResponse, AssistantDefaultListRequest, AssistantDefaultListResponse,
    AssistantDefaultScalarRequest, AssistantDefaultScalarResponse, AssistantDefaultsRequest, AssistantDefaultsResponse,
    AssistantDetailResponse, AssistantEngineResponse, AssistantPreferencesResponse, AssistantProfileResponse,
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
pub use channel::{
    ApprovePairingRequest, BridgeResponse, ChannelAssistantSettingRequest, ChannelAssistantSettingResponse,
    ChannelDefaultModelSetting, ChannelPlatformSettingsResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PairingRequestedPayload,
    PluginStatusChangedPayload, PluginStatusResponse, RejectPairingRequest, RevokeUserRequest,
    SyncChannelSettingsRequest, TestPluginExtraConfig, TestPluginRequest, TestPluginResponse, UserAuthorizedPayload,
};
pub use chat_file::ChatFileRef;
pub use confirmation::{ApprovalCheckQuery, ApprovalCheckResponse, ConfirmRequest, ConfirmationListResponse};
pub use connection_test::TestBedrockConnectionRequest;
pub use conversation::{
    ActiveCountResponse, AssistantConversationOverridesRequest, AssistantConversationRequest,
    CancelConversationRequest, CancelConversationResponse, CloneConversationRequest, ConversationArtifactKind,
    ConversationArtifactListResponse, ConversationArtifactResponse, ConversationArtifactStatus,
    ConversationAssistantIdentityResponse, ConversationListResponse, ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeStateKind, ConversationRuntimeSummary, CreateConversationRequest,
    EnsureConversationRuntimeResponse, ListConversationsQuery, ListMessagesQuery, MessageListResponse, MessageResponse,
    MessageSearchItem, MessageSearchResponse, SearchMessagesQuery, SendMessageRequest, SendMessageResponse,
    UpdateConversationArtifactRequest, UpdateConversationRequest,
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
    CopyFailure, CopyFilesRequest, CopyFilesResponse, CopyTarget, DirOrFileResponse, FetchRemoteImageRequest,
    FileChangeInfoResponse, FileMetadataResponse, FileWatchRequest, GetFileMetadataRequest, GetFilesByDirRequest,
    GetImageBase64Request, ListWorkspaceFilesRequest, ReadFileRequest, RevealItemRequest, SnapshotBaselineRequest,
    SnapshotCompareResponse, SnapshotDiscardRequest, SnapshotInfoResponse, SnapshotMode, SnapshotStageRequest,
    SnapshotWorkspaceRequest, WorkspaceFlatFileResponse, WorkspaceOfficeWatchRequest, WriteFileRequest,
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
    ExcelSheetData, ExcelSheetImage, ExcelWorkbookData, GetSnapshotContentRequest, ListSnapshotsRequest, PptJsonData,
    PptSlideData, PreviewHistoryTargetDto, PreviewSnapshotInfoDto, PreviewState, PreviewStatusEvent,
    PreviewUrlResponse, SaveSnapshotRequest, SnapshotContentResponse, StartPreviewRequest, StopPreviewRequest,
};
pub use project::{AttachFolderRequest, ProjectDetailResponse, ProjectEntry, ProjectExplorer};
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
pub use shell::{
    CheckToolInstalledRequest, CheckToolInstalledResponse, DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig,
    OpenExternalRequest, OpenFileRequest, OpenFolderWithRequest, ShowItemInFolderRequest, SpeechToTextConfig,
    SpeechToTextProvider, SpeechToTextResult, SttStreamClientMessage, SttStreamServerMessage, ToolType,
};
pub use skill::{
    AddExternalPathRequest, DeleteSkillRequest, ExportSkillRequest, ExternalSkillSourceResponse,
    ImportSkillFailureResponse, ImportSkillRequest, ImportSkillResponse, MaterializeSkillsRequest,
    MaterializeSkillsResponse, MaterializedSkillRef, NamedPathResponse, ReadAssistantRuleRequest,
    ReadBuiltinResourceRequest, ReadSkillInfoRequest, ReadSkillInfoResponse, RemoveExternalPathRequest,
    ScanForSkillsRequest, ScanForSkillsResponse, ScannedSkillResponse, SkillImportLimitsResponse,
    SkillImportRecordResponse, SkillListItemResponse, SkillPathsResponse, SkillSourceResponse,
    WriteAssistantRuleRequest,
};
pub use system::{
    ClientPreferencesResponse, FeedbackDiagnosticsContextResponse, FeedbackDiagnosticsPrivacyResponse,
    FeedbackDiagnosticsProfileResponse, FeedbackDiagnosticsQuery, FeedbackDiagnosticsResponse, SystemSettingsResponse,
    UpdateClientPreferencesRequest, UpdateSettingsRequest,
};
pub use team::{
    AddAgentRequest, CancelTeamChildTurnRequest, CancelTeamRunRequest, CreateTeamRequest, PauseTeamSlotRequest,
    RenameAgentRequest, RenameTeamRequest, SendAgentMessageRequest, SendTeamMessageRequest, TeamAgentInput,
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentResponse, TeamAgentRuntimeStatus,
    TeamAgentRuntimeStatusPayload, TeamAgentSpawnedPayload, TeamAgentStatusPayload, TeamChildTurnPayload,
    TeamListResponse, TeamMcpRuntimeConfig, TeamMessageEnqueueStatus, TeamResponse, TeamRunAckResponse, TeamRunPayload,
    TeamRunSource, TeamRunStateResponse, TeamRunStatus, TeamRunTargetRole, TeamRuntimeSeed,
    TeamSendMessageQueuedResponse, TeamSessionBinding, TeamSessionPhase, TeamSessionStatus, TeamSessionStatusPayload,
    TeamSlotBlockedReason, TeamSlotWorkChangedPayload, TeamSlotWorkPayload, TeamSlotWorkState, TeammateMessagePayload,
};
pub use team_mcp::{TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
pub use team_tools::{
    TEAM_DESCRIBE_ASSISTANT_DESCRIPTION, TEAM_LIST_ASSISTANTS_DESCRIPTION, TEAM_SPAWN_AGENT_DESCRIPTION,
    TEAM_TOOLS_SCHEMA_VERSION, TeamToolCall, TeamToolCliEnvelope, TeamToolCliMeta, TeamToolContextResponse,
    TeamToolDescriptor, TeamToolErrorCode, TeamToolErrorPayload, TeamToolName, TeamToolPermission, TeamToolRole,
    TeamToolRuntimeCallRequest, TeamToolRuntimeCallResponse, TeamToolTransport, cli_command_for_tool,
    team_tool_descriptor, team_tool_descriptors, team_tool_descriptors_for_role, tool_name_for_cli_path,
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
