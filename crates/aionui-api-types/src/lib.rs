#![warn(clippy::disallowed_types)]

//! All HTTP request/response DTOs shared across the API surface.
mod acp;
mod acp_prompt_hook;
mod agent_build_extra;
mod agent_discovery;
mod agent_error;
mod assistant;
mod auth;
mod channel;
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
mod provider;
mod remote_agent;
mod response;
mod runtime;
mod shell;
mod skill;
mod system;
mod team;
mod team_mcp;
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
    AuthStatusResponse, ChangePasswordRequest, LoginRequest, LoginResponse, PublicUser, QrLoginRequest,
    RefreshResponse, RefreshTokenRequest, UserInfoResponse, WebuiChangePasswordRequest, WebuiChangeUsernameRequest,
    WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse, WebuiResetPasswordResponse, WsTokenResponse,
};
pub use channel::{
    ApprovePairingRequest, BridgeResponse, ChannelAssistantSettingRequest, ChannelAssistantSettingResponse,
    ChannelDefaultModelSetting, ChannelPlatformSettingsResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PairingRequestedPayload,
    PluginStatusChangedPayload, PluginStatusResponse, RejectPairingRequest, RevokeUserRequest,
    SyncChannelSettingsRequest, TestPluginExtraConfig, TestPluginRequest, TestPluginResponse, UserAuthorizedPayload,
};
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
    BrowseDirectoryQuery, BrowseDirectoryResponse, BrowseEntry, CancelZipRequest, CopyFilesRequest, CopyFilesResponse,
    CreateTempFileRequest, DirOrFileResponse, FetchRemoteImageRequest, FileChangeInfoResponse, FileMetadataResponse,
    FileWatchRequest, GetFileMetadataRequest, GetFilesByDirRequest, GetImageBase64Request, ListWorkspaceFilesRequest,
    ReadFileBufferRequest, ReadFileRequest, RemoveEntryRequest, RenameRequest, RenameResponse, SnapshotBaselineRequest,
    SnapshotCompareResponse, SnapshotDiscardRequest, SnapshotInfoResponse, SnapshotMode, SnapshotStageRequest,
    SnapshotWorkspaceRequest, WorkspaceFlatFileResponse, WorkspaceOfficeWatchRequest, WriteFileRequest, ZipFileEntry,
    ZipRequest,
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
pub use provider::{
    BedrockAuthMethod, BedrockConfig, CreateProviderRequest, DetectProtocolRequest, DetectionSuggestion,
    FetchModelsAnonymousRequest, FetchModelsRequest, FetchModelsResponse, HealthStatus, KeyTestResult, ModelCapability,
    ModelHealthStatus, ModelInfo, ModelType, MultiKeyResult, ProtocolDetectionResponse, ProviderHealthCheckErrorKind,
    ProviderHealthCheckRequest, ProviderHealthCheckResponse, ProviderResponse, SuggestionType, UpdateProviderRequest,
};
pub use remote_agent::{
    CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
pub use response::{ApiResponse, ErrorResponse};
pub use runtime::{
    EnsureManagedAcpToolRequest, EnsureManagedAcpToolResponse, EnsureNodeRuntimeRequest, EnsureNodeRuntimeResponse,
    RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload, RuntimeStatusPhase, RuntimeStatusScope,
    RuntimeStatusScopeKind,
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
    TeamAgentRemovedPayload, TeamAgentRenamedPayload, TeamAgentResponse, TeamAgentSpawnedPayload,
    TeamAgentStatusPayload, TeamChildTurnPayload, TeamListResponse, TeamMcpPhase, TeamMcpRuntimeConfig,
    TeamMcpStatusPayload, TeamResponse, TeamRunAckResponse, TeamRunPayload, TeamRunSource, TeamRunStateResponse,
    TeamRunStatus, TeamRunTargetRole, TeamRuntimeSeed, TeamSendMessageDelivery, TeamSendMessageQueuedResponse,
    TeamSendMessageReason, TeamSendMessageStatus, TeamSendMessageTargetQueueState, TeamSessionBinding,
    TeamSlotRuntimeHealth, TeamSlotWorkPayload, TeammateMessagePayload,
};
pub use team_mcp::{TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
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
