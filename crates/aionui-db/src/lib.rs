#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod database;
mod error;
pub mod models;
mod repository;

pub use database::{
    Database, DatabaseInitError, init_database, init_database_memory, init_database_staged, maybe_copy_legacy_database,
};
pub use error::DbError;
pub use models::{
    AgentMetadataRow, AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow,
    AssistantRow, ConversationArtifactRow, ConversationAssistantSnapshotRow, CreateAssistantParams,
    UpdateAgentHandshakeParams, UpdateAssistantParams, UpsertAgentMetadataParams, UpsertAssistantDefinitionParams,
    UpsertAssistantOverlayParams, UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams,
    UpsertOverrideParams,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationRowUpdate, MessagePageCursor, MessagePageDirection, MessagePageParams,
    MessagePageResult, MessageRowUpdate, MessageSearchRow,
};
pub use repository::cron::UpdateCronJobParams;
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::{
    CreateAcpSessionParams, IAcpSessionRepository, IAgentMetadataRepository, IAssistantDefinitionRepository,
    IAssistantOverlayRepository, IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository,
    IChannelRepository, IClientPreferenceRepository, IConversationRepository, ICronRepository, IMcpServerRepository,
    IOAuthTokenRepository, IProviderRepository, IRemoteAgentRepository, ISettingsRepository, ITeamRepository,
    IUserRepository, PersistedSessionState, SaveRuntimeStateParams, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteConversationRepository, SqliteCronRepository,
    SqliteMcpServerRepository, SqliteOAuthTokenRepository, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteTeamRepository, SqliteUserRepository, rebuild_legacy_assistant_mirror,
};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
