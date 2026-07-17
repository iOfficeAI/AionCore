use std::sync::Arc;

use aionui_common::{TimestampMs, now_ms};
use aionui_db::models::ApprovalRequestRow;
use aionui_db::{DbError, IApprovalRepository};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const APPROVAL_TTL_MS: TimestampMs = 10 * 60 * 1_000;

#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("Invalid approval request: {0}")]
    BadRequest(String),
    #[error("Approval request not found")]
    NotFound,
    #[error("Approval access denied: {0}")]
    Forbidden(String),
    #[error("Approval conflicts with current state: {0}")]
    Conflict(String),
    #[error("Approval resolver failed: {0}")]
    Resolver(String),
    #[error("Approval persistence failed: {0}")]
    Internal(String),
}

impl From<DbError> for ApprovalError {
    fn from(value: DbError) -> Self {
        Self::Internal(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalOption {
    pub label: String,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSource {
    pub channel: String,
    pub user_id: String,
    pub chat_id: String,
    pub thread_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ApprovalRequestInput {
    pub requester_user_id: String,
    pub project_id: Option<String>,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub conversation_id: String,
    pub agent_id: Option<String>,
    pub call_id: String,
    pub action_type: String,
    pub command: Option<String>,
    pub working_directory: Option<String>,
    pub risk_level: String,
    pub options: Vec<ApprovalOption>,
    pub source: Option<ApprovalSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveApprovalContext {
    Web {
        user_id: String,
    },
    Channel {
        user_id: String,
        channel: String,
        source_user_id: String,
        chat_id: String,
        thread_id: Option<i64>,
    },
}

impl ResolveApprovalContext {
    fn user_id(&self) -> &str {
        match self {
            Self::Web { user_id } | Self::Channel { user_id, .. } => user_id,
        }
    }
}

#[async_trait]
pub trait ApprovalResolver: Send + Sync {
    async fn resolve(
        &self,
        conversation_id: &str,
        call_id: &str,
        value: Value,
        always_allow: bool,
    ) -> Result<(), String>;
}

#[derive(Clone)]
pub struct ApprovalService {
    repository: Arc<dyn IApprovalRepository>,
    resolver: Arc<dyn ApprovalResolver>,
}

impl ApprovalService {
    pub fn new(repository: Arc<dyn IApprovalRepository>, resolver: Arc<dyn ApprovalResolver>) -> Self {
        Self { repository, resolver }
    }

    pub async fn create(&self, input: ApprovalRequestInput) -> Result<ApprovalRequestRow, ApprovalError> {
        validate_input(&input)?;
        if let Some(existing) = self
            .repository
            .get_by_conversation_call(&input.conversation_id, &input.call_id)
            .await?
        {
            if existing.requester_user_id != input.requester_user_id {
                return Err(ApprovalError::Forbidden(
                    "idempotency key belongs to another user".into(),
                ));
            }
            return Ok(existing);
        }

        let now = now_ms();
        let source = input.source.as_ref();
        let row = ApprovalRequestRow {
            id: short_approval_id(),
            requester_user_id: input.requester_user_id,
            project_id: input.project_id,
            run_id: input.run_id,
            task_id: input.task_id,
            conversation_id: input.conversation_id,
            agent_id: input.agent_id,
            call_id: input.call_id,
            action_type: input.action_type,
            command: input.command.as_deref().map(redact_secrets),
            working_directory: input.working_directory,
            risk_level: input.risk_level,
            options: serde_json::to_string(&input.options)
                .map_err(|error| ApprovalError::Internal(error.to_string()))?,
            status: "pending".into(),
            approver_user_id: None,
            source_channel: source.map(|value| value.channel.clone()),
            source_user_id: source.map(|value| value.user_id.clone()),
            source_chat_id: source.map(|value| value.chat_id.clone()),
            source_thread_id: source.and_then(|value| value.thread_id),
            expires_at: now + APPROVAL_TTL_MS,
            consumed_at: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(error) = self.repository.create(&row).await {
            if let Some(existing) = self
                .repository
                .get_by_conversation_call(&row.conversation_id, &row.call_id)
                .await?
            {
                return Ok(existing);
            }
            return Err(error.into());
        }
        Ok(row)
    }

    pub async fn get(&self, user_id: &str, approval_id: &str) -> Result<ApprovalRequestRow, ApprovalError> {
        let row = self.repository.get(approval_id).await?.ok_or(ApprovalError::NotFound)?;
        ensure_owner(&row, user_id)?;
        Ok(row)
    }

    pub async fn list(&self, user_id: &str, run_id: Option<&str>) -> Result<Vec<ApprovalRequestRow>, ApprovalError> {
        self.repository.mark_expired(now_ms()).await?;
        Ok(self.repository.list_for_user(user_id, run_id).await?)
    }

    pub async fn resolve(
        &self,
        approval_id: &str,
        option_index: usize,
        context: ResolveApprovalContext,
    ) -> Result<ApprovalRequestRow, ApprovalError> {
        let now = now_ms();
        self.repository.mark_expired(now).await?;
        let row = self.repository.get(approval_id).await?.ok_or(ApprovalError::NotFound)?;
        ensure_owner(&row, context.user_id())?;
        ensure_source(&row, &context)?;
        if row.status != "pending" {
            return Err(ApprovalError::Conflict(format!("approval is {}", row.status)));
        }
        let options: Vec<ApprovalOption> =
            serde_json::from_str(&row.options).map_err(|error| ApprovalError::Internal(error.to_string()))?;
        let option = options
            .get(option_index)
            .cloned()
            .ok_or_else(|| ApprovalError::BadRequest("approval option does not exist".into()))?;
        let status = if is_rejection(&option) { "rejected" } else { "approved" };
        if !self
            .repository
            .consume(approval_id, context.user_id(), status, now)
            .await?
        {
            return Err(ApprovalError::Conflict(
                "approval was already consumed or expired".into(),
            ));
        }

        let always_allow = option
            .params
            .as_ref()
            .and_then(|params| params.get("always_allow"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Err(error) = self
            .resolver
            .resolve(&row.conversation_id, &row.call_id, option.value, always_allow)
            .await
        {
            let _ = self
                .repository
                .cancel_consumed(approval_id, context.user_id(), now_ms())
                .await;
            return Err(ApprovalError::Resolver(error));
        }
        self.get(context.user_id(), approval_id).await
    }
}

fn validate_input(input: &ApprovalRequestInput) -> Result<(), ApprovalError> {
    if input.requester_user_id.trim().is_empty()
        || input.conversation_id.trim().is_empty()
        || input.call_id.trim().is_empty()
        || input.action_type.trim().is_empty()
    {
        return Err(ApprovalError::BadRequest(
            "requester, conversation, call and action are required".into(),
        ));
    }
    if input.options.is_empty() || input.options.len() > 8 {
        return Err(ApprovalError::BadRequest(
            "approval must contain between one and eight options".into(),
        ));
    }
    if !matches!(input.risk_level.as_str(), "low" | "medium" | "high" | "critical") {
        return Err(ApprovalError::BadRequest("unsupported risk level".into()));
    }
    Ok(())
}

fn ensure_owner(row: &ApprovalRequestRow, user_id: &str) -> Result<(), ApprovalError> {
    if row.requester_user_id != user_id {
        return Err(ApprovalError::Forbidden("approval belongs to another user".into()));
    }
    Ok(())
}

fn ensure_source(row: &ApprovalRequestRow, context: &ResolveApprovalContext) -> Result<(), ApprovalError> {
    let ResolveApprovalContext::Channel {
        channel,
        source_user_id,
        chat_id,
        thread_id,
        ..
    } = context
    else {
        return Ok(());
    };
    if row.source_channel.as_deref() != Some(channel.as_str())
        || row.source_user_id.as_deref() != Some(source_user_id.as_str())
        || row.source_chat_id.as_deref() != Some(chat_id.as_str())
        || row.source_thread_id != *thread_id
    {
        return Err(ApprovalError::Forbidden(
            "approval source does not match this channel topic".into(),
        ));
    }
    Ok(())
}

fn is_rejection(option: &ApprovalOption) -> bool {
    if option
        .params
        .as_ref()
        .and_then(|params| params.get("decision"))
        .and_then(Value::as_str)
        == Some("reject")
    {
        return true;
    }
    let value = option.value.as_str().unwrap_or_default().to_ascii_lowercase();
    let label = option.label.to_ascii_lowercase();
    ["reject", "deny", "cancel"]
        .iter()
        .any(|word| value.contains(word) || label.contains(word))
}

fn short_approval_id() -> String {
    Uuid::now_v7().simple().to_string()[..16].to_owned()
}

fn redact_secrets(command: &str) -> String {
    let sensitive = [
        "token",
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "authorization",
    ];
    let mut redact_next = false;
    command
        .split_whitespace()
        .map(|part| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]".to_owned();
            }
            let lower = part.to_ascii_lowercase();
            if let Some((key, _)) = part.split_once('=')
                && sensitive.iter().any(|name| key.to_ascii_lowercase().contains(name))
            {
                return format!("{key}=[REDACTED]");
            }
            if sensitive
                .iter()
                .any(|name| lower == *name || lower == format!("--{name}"))
            {
                redact_next = true;
            }
            part.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
