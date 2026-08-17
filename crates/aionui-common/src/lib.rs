#![warn(clippy::disallowed_types)]

//! Shared primitives: error types, enums, ID generation, crypto, timestamps, and pagination.
pub mod constants;
pub mod user_paths;

mod backend_capabilities;
mod case_convert;
mod crypto;
mod enums;
mod error;
pub mod error_extract;
mod hooks;
mod id;
mod pagination;
mod timestamp;
mod types;

pub use backend_capabilities::{CapabilityOrigin, McpTransportCapabilities, ResolvedBackendCapabilities};
pub use case_convert::{camel_to_snake, normalize_keys_to_snake_case};
pub use crypto::{CryptoError, decrypt_string, encrypt_string};
pub use enums::{
    AgentKillReason, AgentType, ConversationSource, ConversationStatus, FileChangeOperation, McpServerStatus,
    McpSource, MessagePosition, MessageStatus, MessageType, ProtocolType, RemoteAgentAuthType, RemoteAgentProtocol,
    RemoteAgentStatus,
};
#[allow(clippy::disallowed_types)]
pub use error::{
    ApiError, ApiErrorLogContext, ErrorChain, WorkspacePathValidationError, validate_workspace_path_availability,
};
pub use hooks::OnConversationDelete;
pub use id::{fnv1a_hex8, generate_id, generate_id_with_length, generate_prefixed_id, generate_short_id};
pub use pagination::PaginatedResult;
pub use timestamp::{TimestampMs, now_ms};
pub use types::{CommandSpec, Confirmation, ConfirmationOption, EnvVar, ProviderWithModel, UpdateType, VersionInfo};
pub use user_paths::{UserDirNameError, user_dir_name};
