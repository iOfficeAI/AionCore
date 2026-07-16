use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{AssistantConversationRequest, CreateConversationRequest};
use aionui_conversation::{
    ConversationAgentTurnRequest, ConversationAgentTurnStarted, ConversationAgentTurnStatus, ConversationError,
    ConversationService,
};
use aionui_db::IConversationRepository;
use aionui_db::models::MessageRow;
use aionui_team::{
    AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
    AgentTurnStarted, AgentTurnStatus, TeamConversationBindingLookup, TeamConversationCreateRequest,
    TeamConversationCreateResult, TeamConversationLookupPort, TeamConversationProvisioningPort, TeamError,
    TeamProjectionMessageStore, TeamSourceMetadata,
};
use async_trait::async_trait;
use tracing::info;

pub struct TeamConversationAdapters {
    conversation_service: ConversationService,
    conversation_repo: Arc<dyn IConversationRepository>,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

impl TeamConversationAdapters {
    pub fn new(
        conversation_service: ConversationService,
        conversation_repo: Arc<dyn IConversationRepository>,
        task_manager: Arc<dyn IWorkerTaskManager>,
    ) -> Self {
        Self {
            conversation_service,
            conversation_repo,
            task_manager,
        }
    }
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn source_label(source_channel: &str) -> &'static str {
    match source_channel {
        "telegram" => "Telegram",
        "lark" => "Lark",
        "dingtalk" => "DingTalk",
        "weixin" | "wecom" => "WeCom",
        "discord" => "Discord",
        "desktop" => "Desktop",
        "webui" | "aionui" => "WebUI",
        _ => "本地历史",
    }
}

#[async_trait]
impl AgentTurnExecutionPort for TeamConversationAdapters {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
        let team_started = request.on_started.clone();
        let team_run_id = request.team_run_id.clone();
        let slot_id = request.slot_id.clone();
        let role = request.role.clone();
        let on_started = team_started.zip(team_run_id).map(|(callback, team_run_id)| {
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
            .get_message_by_msg_id(conversation_id, msg_id, msg_type)
            .await?)
    }

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), TeamError> {
        self.conversation_service
            .insert_raw_message(row)
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
                    source: request.source,
                    channel_chat_id: request.channel_chat_id,
                    source_channel: request.source_channel,
                    source_channel_id: request.source_channel_id,
                    source_chat_id: request.source_chat_id,
                    source_user_id: request.source_user_id,
                    source_label: request.source_label,
                    created_from: request.created_from,
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
        let Some(row) = self.conversation_repo.get(conversation_id).await? else {
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

    async fn conversation_source_metadata(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamSourceMetadata>, TeamError> {
        let Some(row) = self.conversation_repo.get(conversation_id).await? else {
            return Ok(None);
        };
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        let mut metadata = TeamSourceMetadata {
            source_channel: row
                .source_channel
                .clone()
                .or_else(|| json_string(&extra, "source_channel"))
                .or_else(|| {
                    row.source
                        .clone()
                        .map(|source| if source == "aionui" { "webui".into() } else { source })
                }),
            source_channel_id: row
                .source_channel_id
                .clone()
                .or_else(|| json_string(&extra, "source_channel_id")),
            source_chat_id: row
                .source_chat_id
                .clone()
                .or_else(|| json_string(&extra, "source_chat_id"))
                .or_else(|| row.channel_chat_id.clone()),
            source_user_id: row
                .source_user_id
                .clone()
                .or_else(|| json_string(&extra, "source_user_id")),
            source_label: row.source_label.clone().or_else(|| json_string(&extra, "source_label")),
            created_from: row.created_from.clone().or_else(|| json_string(&extra, "created_from")),
        };
        if metadata.source_label.is_none() {
            metadata.source_label = metadata.source_channel.as_deref().map(source_label).map(str::to_owned);
        }
        if metadata.created_from.is_none() {
            metadata.created_from = metadata.source_channel.clone();
        }
        if metadata.source_channel.is_none() {
            return Ok(None);
        }
        Ok(Some(metadata))
    }

    async fn conversation_assistant_id(&self, conversation_id: &str) -> Result<Option<String>, TeamError> {
        if let Some(snapshot) = self.conversation_repo.get_assistant_snapshot(conversation_id).await? {
            let assistant_id = snapshot.assistant_id.trim();
            if !assistant_id.is_empty() {
                return Ok(Some(assistant_id.to_owned()));
            }
        }

        let Some(row) = self.conversation_repo.get(conversation_id).await? else {
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

    async fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, TeamError> {
        self.conversation_service
            .create_team_temp_workspace(team_id)
            .map_err(map_conversation_update_error)
    }

    async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError> {
        self.conversation_service
            .update_extra(conversation_id, patch)
            .await
            .map_err(map_conversation_update_error)
    }

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError> {
        self.conversation_service
            .save_acp_runtime_mode(conversation_id, mode)
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
}

#[async_trait]
impl TeamConversationLookupPort for TeamConversationAdapters {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, TeamError> {
        let Some(row) = self.conversation_repo.get(conversation_id).await? else {
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
