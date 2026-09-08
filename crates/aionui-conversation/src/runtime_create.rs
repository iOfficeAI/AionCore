//! Service half of `POST /api/runtime/conversations/create`: resolve the helper
//! CLI's `{name, workspace?, assistant_id?}` into a full
//! `CreateConversationRequest` and hand it to `ConversationService::create`.
//!
//! Every check runs BEFORE `create` is called, so a rejection never leaves a
//! half-built conversation behind. The caller's identity comes from the
//! runtime token's bound conversation id — never from the request body.

use std::path::Path;

use aionui_api_types::{
    AssistantConversationRequest, ConversationCreateAssistant, ConversationCreateRequest, ConversationCreateResponse,
    ConversationToolErrorCode, CreateConversationRequest,
};
use aionui_common::{AgentType, ConversationSource, ProviderWithModel};
use aionui_db::models::ConversationRow;
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::convert::string_to_enum;
use crate::error::ConversationError;
use crate::service::ConversationService;
use crate::session_mentions::{team_id_from_extra_str, workspace_from_extra};
use crate::task_options::provider_model_from_conversation_row;

/// Crate-owned error, mapped to the CLI envelope only at the route boundary
/// (AGENTS.md: service code must not touch `ApiError`). `runtime_auth_failed`
/// has no variant here — the route owns token validation.
#[derive(Debug, thiserror::Error)]
pub enum ConversationCreateError {
    #[error("caller conversation is team-owned: {id}")]
    CallerIsTeam { id: String },

    #[error("workspace must be an absolute path: {path}")]
    WorkspaceNotAbsolute { path: String },

    #[error("workspace is not an existing directory: {path}")]
    WorkspaceUnavailable { path: String },

    #[error("assistant not found: {id}")]
    AssistantNotFound { id: String },

    #[error("assistant is disabled: {id}")]
    AssistantDisabled { id: String },

    /// `model_id` is `None` when the assistant resolves to no model at all
    /// (auto mode with no preference yet), `Some` when a model id exists but
    /// no provider of this user lists it.
    #[error("assistant {assistant_id} has no usable aionrs model{}", model_id.as_deref().map(|m| format!(": `{m}` is not offered by any enabled provider")).unwrap_or_default())]
    AssistantModelUnresolved {
        assistant_id: String,
        model_id: Option<String>,
    },

    #[error("request does not match the schema: {reason}")]
    SchemaValidation { reason: String },

    #[error("conversation service unavailable: {reason}")]
    TransportUnavailable { reason: String },
}

impl ConversationCreateError {
    pub fn code(&self) -> ConversationToolErrorCode {
        match self {
            Self::CallerIsTeam { .. } => ConversationToolErrorCode::CallerIsTeam,
            Self::WorkspaceNotAbsolute { .. } => ConversationToolErrorCode::WorkspaceNotAbsolute,
            Self::WorkspaceUnavailable { .. } => ConversationToolErrorCode::WorkspaceUnavailable,
            Self::AssistantNotFound { .. } => ConversationToolErrorCode::AssistantNotFound,
            Self::AssistantDisabled { .. } => ConversationToolErrorCode::AssistantDisabled,
            Self::AssistantModelUnresolved { .. } => ConversationToolErrorCode::AssistantModelUnresolved,
            Self::SchemaValidation { .. } => ConversationToolErrorCode::SchemaValidationFailed,
            Self::TransportUnavailable { .. } => ConversationToolErrorCode::TransportUnavailable,
        }
    }

    /// Pinned by a unit test below.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::CallerIsTeam { .. } => 403,
            Self::WorkspaceNotAbsolute { .. }
            | Self::WorkspaceUnavailable { .. }
            | Self::AssistantDisabled { .. }
            | Self::AssistantModelUnresolved { .. } => 422,
            Self::AssistantNotFound { .. } => 404,
            Self::SchemaValidation { .. } => 400,
            Self::TransportUnavailable { .. } => 503,
        }
    }

    fn transport(error: impl std::fmt::Display) -> Self {
        Self::TransportUnavailable {
            reason: error.to_string(),
        }
    }
}

/// Which branch produced the request. Logged verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inheritance {
    Snapshot,
    LegacyTriple,
    // Constructed by the assistant-override branch (next task).
    #[allow(dead_code)]
    AssistantOverride,
}

impl Inheritance {
    fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::LegacyTriple => "legacy_triple",
            Self::AssistantOverride => "assistant_override",
        }
    }
}

/// Where the aionrs `model` came from. Logged verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelResolution {
    Inherited,
    // Constructed by the assistant-override branch (next task).
    #[allow(dead_code)]
    ProviderMatch,
    NotRequired,
}

impl ModelResolution {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inherited => "inherited",
            Self::ProviderMatch => "provider_match",
            Self::NotRequired => "not_required",
        }
    }
}

/// Everything the assistant/type resolution decided, before `extra` is assembled.
pub(crate) struct CreatePlan {
    pub(crate) r#type: Option<AgentType>,
    pub(crate) assistant_id: Option<String>,
    pub(crate) model: Option<ProviderWithModel>,
    /// `backend / agent_id / agent_source` copied from the caller (legacy
    /// triple only). Empty otherwise.
    pub(crate) legacy_triple: Map<String, Value>,
    pub(crate) inheritance: Inheritance,
    pub(crate) model_resolution: ModelResolution,
}

impl ConversationService {
    /// `{name, workspace?, assistant_id?}` → `create`. Logs one line per
    /// outcome and never records the name, the workspace path, or a model id.
    pub async fn create_for_conversation_helper(
        &self,
        user_id: &str,
        caller_conversation_id: &str,
        req: &ConversationCreateRequest,
    ) -> Result<ConversationCreateResponse, ConversationCreateError> {
        match self
            .create_for_conversation_helper_inner(user_id, caller_conversation_id, req)
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => {
                warn!(
                    from_conversation_id = caller_conversation_id,
                    outcome = "rejected",
                    error_code = error.code().as_str(),
                    "agent conversation create refused"
                );
                Err(error)
            }
        }
    }

    async fn create_for_conversation_helper_inner(
        &self,
        user_id: &str,
        caller_conversation_id: &str,
        req: &ConversationCreateRequest,
    ) -> Result<ConversationCreateResponse, ConversationCreateError> {
        // 1. The caller's own row. Missing means the token names a conversation
        //    that no longer exists — a transport problem, not a user error.
        let caller = self
            .conversation_repo()
            .get(user_id, caller_conversation_id)
            .await
            .map_err(ConversationCreateError::transport)?
            .ok_or_else(|| ConversationCreateError::TransportUnavailable {
                reason: format!("caller conversation missing: {caller_conversation_id}"),
            })?;

        // 2. Team callers get no ordinary-conversation surface.
        if team_id_from_extra_str(&caller.extra).is_some() {
            return Err(ConversationCreateError::CallerIsTeam {
                id: caller_conversation_id.to_owned(),
            });
        }

        // 3. name
        let name = req.name.trim();
        if name.is_empty() {
            return Err(ConversationCreateError::SchemaValidation {
                reason: "`name` must not be blank".to_owned(),
            });
        }

        // 4. workspace — absolute-ness here, existence inside `create`.
        let (workspace, workspace_inherited) = match req
            .workspace
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(explicit) => {
                if !Path::new(explicit).is_absolute() {
                    return Err(ConversationCreateError::WorkspaceNotAbsolute {
                        path: explicit.to_owned(),
                    });
                }
                (explicit.to_owned(), false)
            }
            None => (
                // `create` always persists `extra.workspace`, so a caller
                // without one is a broken row, not a user error.
                workspace_from_extra(&caller.extra).ok_or_else(|| ConversationCreateError::TransportUnavailable {
                    reason: "caller conversation has no workspace".to_owned(),
                })?,
                true,
            ),
        };

        // 5. assistant
        let plan = match req
            .assistant_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            None => self.inherit_plan(user_id, &caller).await?,
            Some(assistant_id) => self.override_plan(user_id, assistant_id).await?,
        };

        // 6. Assemble and delegate. `custom_workspace` is a request-only toggle
        //    that `create` strips; the non-empty `extra.workspace` is what
        //    actually suppresses the temp-dir provisioning.
        let mut extra = Map::new();
        extra.insert("workspace".to_owned(), Value::String(workspace.clone()));
        extra.insert("custom_workspace".to_owned(), Value::Bool(true));
        for (key, value) in plan.legacy_triple.iter() {
            extra.insert(key.clone(), value.clone());
        }
        let request = CreateConversationRequest {
            r#type: plan.r#type,
            name: Some(name.to_owned()),
            model: plan.model,
            assistant: plan.assistant_id.map(|id| AssistantConversationRequest {
                id,
                locale: None,
                conversation_overrides: None,
            }),
            source: Some(ConversationSource::Aionui),
            channel_chat_id: None,
            extra: Value::Object(extra),
        };

        let created = self
            .create(user_id, request)
            .await
            .map_err(|error| map_create_error(error, &workspace))?;

        let backend = created
            .assistant
            .as_ref()
            .map(|assistant| assistant.backend.clone())
            .or_else(|| created.extra.get("backend").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| created.r#type.serde_name().to_owned());
        info!(
            from_conversation_id = caller_conversation_id,
            conversation_id = %created.id,
            inheritance = plan.inheritance.as_str(),
            workspace_inherited,
            model_resolution = plan.model_resolution.as_str(),
            backend = %backend,
            "conversation created by agent"
        );

        let persisted_workspace = created
            .extra
            .get("workspace")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or(workspace);
        Ok(ConversationCreateResponse {
            id: created.id,
            name: created.name,
            workspace: persisted_workspace,
            assistant: created.assistant.map(|assistant| ConversationCreateAssistant {
                id: assistant.id,
                name: assistant.name,
                backend: assistant.backend,
            }),
        })
    }

    /// "Omitted" branch: same assistant definition (via the caller's snapshot)
    /// or, for snapshot-less rows, the caller's `type` plus the legacy
    /// `extra.{backend, agent_id, agent_source}` triple that `create` still
    /// accepts. aionrs copies `row.model` as-is.
    async fn inherit_plan(
        &self,
        user_id: &str,
        caller: &ConversationRow,
    ) -> Result<CreatePlan, ConversationCreateError> {
        let caller_type: AgentType = string_to_enum(&caller.r#type).map_err(ConversationCreateError::transport)?;
        let (model, model_resolution) = if caller_type == AgentType::Aionrs {
            let model = provider_model_from_conversation_row(caller);
            (
                Some(model).filter(|m| !m.provider_id.is_empty()),
                ModelResolution::Inherited,
            )
        } else {
            (None, ModelResolution::NotRequired)
        };

        let snapshot = self
            .conversation_repo()
            .get_assistant_snapshot(user_id, &caller.id)
            .await
            .map_err(ConversationCreateError::transport)?;
        // A snapshot whose definition has since been deleted would make `create`
        // fail with "Either `type` or `assistant.id` is required"; fall back to
        // the legacy triple so the new conversation still mirrors the caller.
        let snapshot_assistant_id = match snapshot {
            Some(snapshot) => match self.assistant_definition_repo() {
                Some(definition_repo) => definition_repo
                    .get_by_assistant_id_for_user(user_id, &snapshot.assistant_id)
                    .await
                    .map_err(ConversationCreateError::transport)?
                    .map(|_| snapshot.assistant_id),
                None => None,
            },
            None => None,
        };

        if let Some(assistant_id) = snapshot_assistant_id {
            return Ok(CreatePlan {
                r#type: None,
                assistant_id: Some(assistant_id),
                model,
                legacy_triple: Map::new(),
                inheritance: Inheritance::Snapshot,
                model_resolution,
            });
        }

        let caller_extra: Value = serde_json::from_str(&caller.extra).unwrap_or(Value::Null);
        let mut legacy_triple = Map::new();
        for key in ["backend", "agent_id", "agent_source"] {
            if let Some(value) = caller_extra.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                legacy_triple.insert(key.to_owned(), Value::String(value.to_owned()));
            }
        }
        Ok(CreatePlan {
            r#type: Some(caller_type),
            assistant_id: None,
            model,
            legacy_triple,
            inheritance: Inheritance::LegacyTriple,
            model_resolution,
        })
    }

    /// "Explicit" branch. Filled in by the next task.
    async fn override_plan(&self, _user_id: &str, _assistant_id: &str) -> Result<CreatePlan, ConversationCreateError> {
        Err(ConversationCreateError::TransportUnavailable {
            reason: "assistant override not implemented".to_owned(),
        })
    }
}

/// `create`'s own workspace check is the only error we translate by kind; every
/// other failure inside `create` is an infrastructure problem from the agent's
/// point of view, so it surfaces as `transport_unavailable` with the message.
fn map_create_error(error: ConversationError, workspace: &str) -> ConversationCreateError {
    match error {
        ConversationError::WorkspacePathUnavailable { path } => ConversationCreateError::WorkspaceUnavailable { path },
        ConversationError::WorkspacePathRuntimeUnavailable { .. } => ConversationCreateError::WorkspaceUnavailable {
            path: workspace.to_owned(),
        },
        other => ConversationCreateError::transport(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status/code pairing is a wire contract the CLI and every bad-path
    /// test assert on, so it is pinned here rather than re-derived per route.
    #[test]
    fn every_error_maps_to_the_status_and_code_the_spec_pins() {
        let cases: Vec<(ConversationCreateError, ConversationToolErrorCode, u16)> = vec![
            (
                ConversationCreateError::CallerIsTeam { id: "c".into() },
                ConversationToolErrorCode::CallerIsTeam,
                403,
            ),
            (
                ConversationCreateError::WorkspaceNotAbsolute { path: "p".into() },
                ConversationToolErrorCode::WorkspaceNotAbsolute,
                422,
            ),
            (
                ConversationCreateError::WorkspaceUnavailable { path: "p".into() },
                ConversationToolErrorCode::WorkspaceUnavailable,
                422,
            ),
            (
                ConversationCreateError::AssistantNotFound { id: "a".into() },
                ConversationToolErrorCode::AssistantNotFound,
                404,
            ),
            (
                ConversationCreateError::AssistantDisabled { id: "a".into() },
                ConversationToolErrorCode::AssistantDisabled,
                422,
            ),
            (
                ConversationCreateError::AssistantModelUnresolved {
                    assistant_id: "a".into(),
                    model_id: Some("m".into()),
                },
                ConversationToolErrorCode::AssistantModelUnresolved,
                422,
            ),
            (
                ConversationCreateError::SchemaValidation { reason: "r".into() },
                ConversationToolErrorCode::SchemaValidationFailed,
                400,
            ),
            (
                ConversationCreateError::TransportUnavailable { reason: "r".into() },
                ConversationToolErrorCode::TransportUnavailable,
                503,
            ),
        ];
        for (error, code, status) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert_eq!(error.http_status(), status, "{error}");
        }
    }

    #[test]
    fn model_unresolved_message_names_the_model_only_when_there_is_one() {
        let with = ConversationCreateError::AssistantModelUnresolved {
            assistant_id: "a".into(),
            model_id: Some("gpt-x".into()),
        };
        assert!(with.to_string().contains("`gpt-x`"), "{with}");
        let without = ConversationCreateError::AssistantModelUnresolved {
            assistant_id: "a".into(),
            model_id: None,
        };
        assert!(!without.to_string().contains('`'), "{without}");
    }
}
