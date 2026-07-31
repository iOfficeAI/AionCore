use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{AssistantConversationRequest, CreateConversationRequest, GetConfigOptionsResponse};
use aionui_conversation::{
    ConversationAgentTurnRequest, ConversationAgentTurnStarted, ConversationAgentTurnStatus, ConversationError,
    ConversationService,
};
use aionui_db::models::{AgentMetadataRow, MessageRow};
use aionui_db::{IAgentMetadataRepository, IConversationRepository};
use aionui_team::{
    AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
    AgentTurnStarted, AgentTurnStatus, NativeSlashCommandPort, SlashCatalogSource, SlashCommandRecognition,
    TeamConversationBindingLookup, TeamConversationCreateRequest, TeamConversationCreateResult,
    TeamConversationLookupPort, TeamConversationProvisioningPort, TeamError, TeamProjectionMessageStore,
};
use async_trait::async_trait;
use tracing::{debug, info, warn};

/// Vendor label of the codex builtin backend (its `agent_metadata.backend`), used
/// to decide when the static codex command catalog is the right fallback source.
const CODEX_BACKEND: &str = "codex";

pub struct TeamConversationAdapters {
    conversation_service: ConversationService,
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

impl TeamConversationAdapters {
    pub fn new(
        conversation_service: ConversationService,
        conversation_repo: Arc<dyn IConversationRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        task_manager: Arc<dyn IWorkerTaskManager>,
    ) -> Self {
        Self {
            conversation_service,
            conversation_repo,
            agent_metadata_repo,
            task_manager,
        }
    }

    async fn owner_user_id(&self, conversation_id: &str) -> Result<Option<String>, TeamError> {
        Ok(self.conversation_repo.owner_user_id(conversation_id).await?)
    }

    async fn require_owner_user_id(&self, conversation_id: &str) -> Result<String, TeamError> {
        self.owner_user_id(conversation_id)
            .await?
            .ok_or_else(|| TeamError::InvalidRequest(format!("conversation {conversation_id} not found")))
    }
}

#[async_trait]
impl AgentTurnExecutionPort for TeamConversationAdapters {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
        let team_started = request.on_started.clone();
        let team_run_id = request.team_run_id.clone();
        let slot_id = request.slot_id.clone();
        let role = request.role.clone();
        let on_started = team_started.map(|callback| {
            Arc::new(move |started: ConversationAgentTurnStarted| {
                let callback = callback.clone();
                let team_run_id = team_run_id.clone();
                let slot_id = slot_id.clone();
                let role = role.clone();
                Box::pin(async move {
                    callback(AgentTurnStarted {
                        team_run_id,
                        slot_id,
                        role,
                        conversation_id: started.conversation_id,
                        turn_id: started.turn_id,
                    })
                    .await;
                }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            })
                as Arc<
                    dyn Fn(
                            ConversationAgentTurnStarted,
                        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                        + Send
                        + Sync,
                >
        });

        let conversation_id = request.conversation_id.clone();
        let outcome = loop {
            match self
                .conversation_service
                .run_agent_turn(ConversationAgentTurnRequest {
                    user_id: request.user_id.clone(),
                    conversation_id: conversation_id.clone(),
                    content: request.content.clone(),
                    files: request.files.clone(),
                    inject_skills: Vec::new(),
                    required_runtime_mode: None,
                    persist_user_message: false,
                    user_message_hidden: false,
                    on_started: on_started.clone(),
                })
                .await
            {
                Ok(outcome) => break outcome,
                Err(error) if is_retryable_conversation_busy(&error) => {
                    info!(
                        conversation_id = %conversation_id,
                        team_run_id = ?request.team_run_id,
                        slot_id = %request.slot_id,
                        "team conversation turn waiting for active conversation turn to release"
                    );
                    self.conversation_service
                        .runtime_state()
                        .wait_until_unclaimed(&conversation_id)
                        .await;
                    info!(
                        conversation_id = %conversation_id,
                        team_run_id = ?request.team_run_id,
                        slot_id = %request.slot_id,
                        "team conversation turn retrying after active conversation turn released"
                    );
                }
                Err(error) => return Err(map_conversation_turn_error(error)),
            }
        };

        Ok(AgentTurnOutcome {
            conversation_id: outcome.conversation_id,
            turn_id: outcome.turn_id,
            status: match outcome.status {
                ConversationAgentTurnStatus::Completed => AgentTurnStatus::Completed,
                ConversationAgentTurnStatus::Failed => AgentTurnStatus::Failed,
            },
            runtime: Some(outcome.runtime),
        })
    }
}

#[async_trait]
impl AgentTurnCancellationPort for TeamConversationAdapters {
    async fn cancel_agent_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<(), AgentTurnExecutionError> {
        self.conversation_service
            .cancel(user_id, conversation_id, turn_id, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(map_conversation_turn_error)
    }
}

#[async_trait]
impl NativeSlashCommandPort for TeamConversationAdapters {
    /// ELECTRON-3RN recognition predicate. Splits the leading command name with
    /// the SHARED grammar owner (`aionui_session::slash_command_name`, same
    /// function codex's `route_slash_command` uses) then tests membership against
    /// the backend's self-advertised catalog via a live→cached→static degradation
    /// chain. Never asserts anything about an external CLI's behaviour; a name the
    /// backend does not advertise (or an unresolvable catalog) falls back to the
    /// ordinary wrapped wake (zero regression).
    async fn recognize(&self, conversation_id: &str, content: &str) -> SlashCommandRecognition {
        let Some(name) = aionui_session::slash_command_name(content) else {
            return SlashCommandRecognition::NotCommand;
        };
        let name = name.to_owned();
        match self.resolve_slash_catalog(conversation_id).await {
            // A resolved-but-EMPTY catalog is an anomaly (e.g. a live backend
            // returned no commands): it silently disables every team command, so
            // surface it distinctly from a populated catalog that merely lacks
            // this name (NotInCatalog). The team layer logs it at `warn`.
            Some((catalog, _)) if catalog.is_empty() => SlashCommandRecognition::CatalogEmpty { name },
            Some((catalog, source)) => {
                if catalog.iter().any(|command| command == &name) {
                    SlashCommandRecognition::Recognized { command: name, source }
                } else {
                    SlashCommandRecognition::NotInCatalog { name }
                }
            }
            None => SlashCommandRecognition::CatalogUnavailable { name },
        }
    }
}

impl TeamConversationAdapters {
    /// Resolve the native slash-command NAME catalog for a conversation via the
    /// degradation chain (spec §8-1): (a) live backend session capabilities,
    /// (b) the persisted `agent_metadata.available_commands` snapshot, (c) the
    /// static codex builtin catalog. Returns `None` when none resolve (chain (d)).
    async fn resolve_slash_catalog(&self, conversation_id: &str) -> Option<(Vec<String>, SlashCatalogSource)> {
        // (a) live: a running backend session's current capabilities are the
        // freshest, authoritative source (even if the list happens to be empty).
        if let Some(task) = self.task_manager.get_task(conversation_id) {
            match task.get_slash_commands().await {
                Ok(items) => {
                    let names = items.into_iter().map(|item| item.command).collect();
                    return Some((names, SlashCatalogSource::Live));
                }
                // A live fetch fault is caught only here (composition layer); the
                // raw error exists nowhere else, so log it at `warn` before
                // falling through to cached/static. Only `conversation_id` is
                // available at this seam (no team_id/slot_id).
                Err(error) => warn!(
                    conversation_id = %conversation_id,
                    %error,
                    reason = "live_catalog_fetch_failed",
                    "live slash-command catalog fetch failed; falling through to cached/static"
                ),
            }
        }

        // (b)/(c) require the conversation's agent_metadata row.
        let row = self.resolve_agent_metadata(conversation_id).await;

        // (b) cached: a persisted discovery snapshot.
        if let Some(row) = &row
            && let Some(raw) = row.available_commands.as_deref()
        {
            match parse_available_command_names(raw) {
                // Non-empty snapshot → use it.
                Some(names) if !names.is_empty() => return Some((names, SlashCatalogSource::Cached)),
                // Genuinely empty snapshot (`[]`) → fall through quietly; an empty
                // array is a legitimate "discovered, no commands" state, not a fault.
                Some(_) => {}
                // Malformed / unexpected-shape snapshot → safely handled bad data;
                // log at `warn` (§14) before falling through.
                None => warn!(
                    conversation_id = %conversation_id,
                    reason = "available_commands_malformed",
                    "persisted available_commands snapshot is malformed; falling through to static/none"
                ),
            }
        }

        // (c) static: codex has a builtin catalog even before any discovery.
        if let Some(row) = &row
            && row.backend.as_deref() == Some(CODEX_BACKEND)
        {
            let names = aionui_session::codex_capabilities()
                .slash_commands
                .into_iter()
                .map(|command| command.name)
                .collect();
            return Some((names, SlashCatalogSource::Static));
        }

        // (d) nothing resolvable → caller falls back to the wrapped wake.
        None
    }

    /// Best-effort resolution of a conversation's `agent_metadata` row, reusing
    /// the same identity sources the conversation layer uses: the persisted
    /// assistant snapshot's `agent_id`, then `extra.agent_id`, then the builtin
    /// row for `extra.backend`. Any failure yields `None` (→ catalog unavailable).
    async fn resolve_agent_metadata(&self, conversation_id: &str) -> Option<AgentMetadataRow> {
        // User-scoped repo reads need the conversation's owner; best-effort like
        // the rest of this path — an unresolvable owner yields `None`.
        let user_id = match self.owner_user_id(conversation_id).await {
            Ok(Some(user_id)) => user_id,
            Ok(None) => return None,
            Err(error) => {
                debug!(
                    conversation_id = %conversation_id, %error,
                    reason = "owner_lookup_failed",
                    "conversation owner lookup failed while resolving agent_metadata"
                );
                return None;
            }
        };
        // A genuine repo `Err` (not `Ok(None)` — "not found" is the normal case
        // for many conversations and must stay silent) is logged at `debug`: it is
        // a development-detail fault on a best-effort path, not a production signal.
        match self
            .conversation_repo
            .get_assistant_snapshot(&user_id, conversation_id)
            .await
        {
            Ok(Some(snapshot)) => {
                let agent_id = snapshot.agent_id.trim();
                if !agent_id.is_empty() {
                    match self.agent_metadata_repo.get(agent_id).await {
                        Ok(Some(row)) => return Some(row),
                        Ok(None) => {}
                        Err(error) => debug!(
                            conversation_id = %conversation_id, %error,
                            reason = "agent_metadata_lookup_failed",
                            "agent_metadata lookup by assistant agent_id failed"
                        ),
                    }
                }
            }
            Ok(None) => {}
            Err(error) => debug!(
                conversation_id = %conversation_id, %error,
                reason = "assistant_snapshot_lookup_failed",
                "assistant snapshot lookup failed while resolving agent_metadata"
            ),
        }

        let row = match self.conversation_repo.get(&user_id, conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(error) => {
                debug!(
                    conversation_id = %conversation_id, %error,
                    reason = "conversation_lookup_failed",
                    "conversation lookup failed while resolving agent_metadata"
                );
                return None;
            }
        };
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);

        if let Some(agent_id) = extra
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match self.agent_metadata_repo.get(agent_id).await {
                Ok(Some(row)) => return Some(row),
                Ok(None) => {}
                Err(error) => debug!(
                    conversation_id = %conversation_id, %error,
                    reason = "agent_metadata_lookup_failed",
                    "agent_metadata lookup by extra.agent_id failed"
                ),
            }
        }

        if let Some(backend) = extra
            .get("backend")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match self.agent_metadata_repo.find_builtin_by_backend(backend).await {
                Ok(Some(row)) => return Some(row),
                Ok(None) => {}
                Err(error) => debug!(
                    conversation_id = %conversation_id, %error,
                    reason = "agent_metadata_builtin_lookup_failed",
                    "builtin agent_metadata lookup by backend failed"
                ),
            }
        }

        None
    }
}

/// Extract the command NAMEs from a persisted `available_commands` snapshot
/// (a JSON array of `{ "name", "description" }` objects).
///
/// - `Some(names)` — the snapshot parsed as a JSON array (`names` may be empty
///   for `[]`, a legitimate "discovered, no commands" state).
/// - `None` — the snapshot is not valid JSON, or is valid JSON of an unexpected
///   shape (not an array). The caller treats this as malformed data, logs a
///   `warn`, and falls through to the next catalog source.
fn parse_available_command_names(raw: &str) -> Option<Vec<String>> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let entries = value.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str).map(str::to_owned))
            .collect(),
    )
}

#[async_trait]
impl TeamProjectionMessageStore for TeamConversationAdapters {
    fn mint_message_id(&self) -> String {
        ConversationService::mint_msg_id()
    }

    async fn find_projected_message(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, TeamError> {
        Ok(self
            .conversation_repo
            .get_message_by_msg_id(
                &self.require_owner_user_id(conversation_id).await?,
                conversation_id,
                msg_id,
                msg_type,
            )
            .await?)
    }

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), TeamError> {
        self.conversation_service
            .insert_raw_message(&self.require_owner_user_id(&row.conversation_id).await?, row)
            .await
            .map_err(map_conversation_update_error)
    }
}

#[async_trait]
impl TeamConversationProvisioningPort for TeamConversationAdapters {
    async fn create_team_conversation(
        &self,
        request: TeamConversationCreateRequest,
    ) -> Result<TeamConversationCreateResult, TeamError> {
        let response = self
            .conversation_service
            .create(
                &request.user_id,
                CreateConversationRequest {
                    r#type: request.agent_type,
                    name: Some(request.name),
                    model: request.top_level_model,
                    assistant: request.assistant_id.map(|assistant_id| AssistantConversationRequest {
                        id: assistant_id,
                        locale: None,
                        conversation_overrides: None,
                    }),
                    source: None,
                    channel_chat_id: None,
                    extra: request.extra,
                },
            )
            .await
            .map_err(map_conversation_create_error)?;
        let workspace = response
            .extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| TeamError::InvalidRequest("created team conversation did not resolve a workspace".into()))?
            .to_owned();
        Ok(TeamConversationCreateResult {
            conversation_id: response.id,
            workspace,
        })
    }

    async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, TeamError> {
        let Some(user_id) = self.owner_user_id(conversation_id).await? else {
            return Ok(None);
        };
        let Some(row) = self.conversation_repo.get(&user_id, conversation_id).await? else {
            return Ok(None);
        };
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok(extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned))
    }

    async fn conversation_assistant_id(&self, conversation_id: &str) -> Result<Option<String>, TeamError> {
        let Some(user_id) = self.owner_user_id(conversation_id).await? else {
            return Ok(None);
        };
        if let Some(snapshot) = self
            .conversation_repo
            .get_assistant_snapshot(&user_id, conversation_id)
            .await?
        {
            let assistant_id = snapshot.assistant_id.trim();
            if !assistant_id.is_empty() {
                return Ok(Some(assistant_id.to_owned()));
            }
        }

        let Some(row) = self.conversation_repo.get(&user_id, conversation_id).await? else {
            return Ok(None);
        };

        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok(extra
            .get("assistant_id")
            .or_else(|| extra.get("preset_assistant_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned))
    }

    async fn create_team_temp_workspace(&self, user_id: &str, team_id: &str) -> Result<String, TeamError> {
        self.conversation_service
            .create_team_temp_workspace(user_id, team_id)
            .map_err(map_conversation_update_error)
    }

    async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError> {
        self.conversation_service
            .update_extra(
                &self.require_owner_user_id(conversation_id).await?,
                conversation_id,
                patch,
            )
            .await
            .map_err(map_conversation_update_error)
    }

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError> {
        let user_id = self.require_owner_user_id(conversation_id).await?;
        self.conversation_service
            .save_acp_runtime_mode(&user_id, conversation_id, mode)
            .await
            .map_err(map_conversation_update_error)
    }

    async fn get_config_options(&self, conversation_id: &str) -> Result<GetConfigOptionsResponse, TeamError> {
        let user_id = self.require_owner_user_id(conversation_id).await?;
        self.conversation_service
            .get_config_options(&user_id, conversation_id)
            .await
            .map_err(map_conversation_update_error)
    }

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError> {
        self.conversation_service
            .warmup(user_id, conversation_id, task_manager)
            .await
            .map_err(map_conversation_update_error)
    }

    async fn delete_team_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), TeamError> {
        self.conversation_service
            .delete(user_id, conversation_id)
            .await
            .map_err(map_conversation_update_error)
    }

    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError> {
        let Some(user_id) = self.owner_user_id(conversation_id).await? else {
            return Ok(None);
        };
        let Some(row) = self.conversation_repo.get(&user_id, conversation_id).await? else {
            return Ok(None);
        };
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok(Some(TeamConversationBindingLookup {
            conversation_id: row.id,
            user_id: row.user_id,
            team_id: extra
                .get("teamId")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            slot_id: extra
                .get("slot_id")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            role: extra
                .get("role")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }))
    }
}

#[async_trait]
impl TeamConversationLookupPort for TeamConversationAdapters {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError> {
        <Self as TeamConversationProvisioningPort>::lookup_team_binding_by_conversation(self, conversation_id).await
    }
}

fn is_retryable_conversation_busy(error: &ConversationError) -> bool {
    matches!(error, ConversationError::Busy { reason } if reason.contains("already running"))
}

fn map_conversation_create_error(error: ConversationError) -> TeamError {
    match error {
        ConversationError::WorkspacePathUnavailable { path } => TeamError::WorkspacePathUnavailable(path),
        ConversationError::WorkspacePathRuntimeUnavailable { path } => TeamError::WorkspacePathRuntimeUnavailable(path),
        other => TeamError::InvalidRequest(format!("failed to create conversation: {other}")),
    }
}

fn map_conversation_update_error(error: ConversationError) -> TeamError {
    match error {
        ConversationError::WorkspacePathUnavailable { path } => TeamError::WorkspacePathUnavailable(path),
        ConversationError::WorkspacePathRuntimeUnavailable { path } => TeamError::WorkspacePathRuntimeUnavailable(path),
        ConversationError::Forbidden { reason } => TeamError::Forbidden(reason),
        ConversationError::ActiveAgentNotFound { conversation_id } => TeamError::RuntimeNotReady { conversation_id },
        ConversationError::NotFound { id } => TeamError::InvalidRequest(format!("conversation not found: {id}")),
        ConversationError::NotFoundReason { reason } => TeamError::InvalidRequest(reason),
        other => TeamError::InvalidRequest(other.to_string()),
    }
}

fn map_conversation_turn_error(error: ConversationError) -> AgentTurnExecutionError {
    match error {
        ConversationError::Busy { reason } => AgentTurnExecutionError::Skipped { reason },
        other => AgentTurnExecutionError::Failed {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_available_command_names_extracts_names() {
        // The persisted snapshot shape written by the registry:
        // a JSON array of `{ "name", "description" }` objects.
        let raw = r#"[{"name":"compact","description":"summarize"},{"name":"init","description":"agents.md"}]"#;
        assert_eq!(
            parse_available_command_names(raw),
            Some(vec!["compact".to_owned(), "init".to_owned()])
        );
        // Empty array → parsed, empty list (a legitimate "no commands" snapshot;
        // caller falls through QUIETLY, no warn).
        assert_eq!(parse_available_command_names("[]"), Some(Vec::<String>::new()));
        // Malformed JSON, or valid JSON of an unexpected (non-array) shape → `None`
        // (caller logs a `warn` and falls through to the next catalog source).
        assert_eq!(parse_available_command_names("not json"), None);
        assert_eq!(parse_available_command_names("{}"), None);
    }

    #[test]
    fn active_agent_missing_maps_to_team_runtime_not_ready() {
        let err = map_conversation_update_error(ConversationError::ActiveAgentNotFound {
            conversation_id: "conv-1".into(),
        });

        assert!(matches!(
            err,
            TeamError::RuntimeNotReady {
                conversation_id: ref id
            } if id == "conv-1"
        ));
    }
}
