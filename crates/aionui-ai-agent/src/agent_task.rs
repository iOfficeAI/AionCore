//! Minimal public contract for a running agent task.
//!
//! `IAgentTask` captures **only** the operations that every agent type
//! implements identically and that the generic task_manager / idle_scanner /
//! message-flow code actually needs. Anything that is type-specific
//! (session modes, session keys, model switching, config options, pending
//! confirmation lists, approval memory, ACP usage, etc.) lives as
//! **inherent** methods on each concrete `XxxAgentManager`
//! and is reached through the `AgentInstance` enum — forcing every callsite
//! to say out loud which agent type it is addressing.
//!
//! Replaces the old bloated `IAgentManager` trait + `as_any()` downcast
//! pattern (deleted in PR #8c).
use std::sync::Arc;

use aionui_common::{AgentKillReason, AgentType, ConversationStatus, TimestampMs};
use tokio::sync::broadcast;

use crate::error::AgentError;
use crate::manager::acp::AcpAgentManager;
use crate::manager::aionrs::AionrsAgentManager;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;

use aionui_api_types::{
    GetConfigOptionsResponse, GetModelInfoResponse, ModelInfoEntry, ModelInfoPayload, SetConfigOptionResponse,
    SideQuestionRequest, SideQuestionResponse, SlashCommandItem,
};

#[cfg(any(test, feature = "test-support"))]
use aionui_common::Confirmation;

/// Ten-method public surface every agent type implements identically.
///
/// Object-safe by construction (no generic methods, no `Self` by value).
/// Used by generic lifecycle code (task_manager, idle_scanner, stream
/// fan-out) that genuinely does not care which agent type it is dealing
/// with. For type-specific operations, match on [`AgentInstance`] and
/// call the concrete manager's inherent methods.
#[async_trait::async_trait]
pub trait IAgentTask: Send + Sync {
    /// The type of agent this task controls.
    fn agent_type(&self) -> AgentType;

    /// Conversation ID this task is bound to.
    fn conversation_id(&self) -> &str;

    /// Working directory for this agent session.
    fn workspace(&self) -> &str;

    /// Current conversation status. `None` if the agent has not
    /// transitioned into a known status yet.
    fn status(&self) -> Option<ConversationStatus>;

    /// Timestamp (ms) of the last activity (message send, event received).
    fn last_activity_at(&self) -> TimestampMs;

    /// Subscribe to the agent's stream event channel.
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent>;

    /// Send a user message to the agent. Returns once the agent has
    /// accepted the turn; actual streaming proceeds on the broadcast
    /// channel returned by [`Self::subscribe`].
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError>;

    /// Stop the current streaming response without killing the agent.
    async fn cancel(&self) -> Result<(), AgentError>;

    /// Terminate the agent process.
    ///
    /// - `reason: Some(IdleTimeout)` — idle cleanup
    /// - `reason: None` — explicit user/system kill
    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError>;
}

/// Extended trait used exclusively by the `AgentInstance::Mock` variant so
/// tests can inject richer fake behaviour (pending confirmations, approval
/// memory, fake session keys, etc.) without polluting the production
/// `IAgentTask` contract with trait-level defaults that would be lies for
/// at least one concrete manager.
///
/// Every method has a sensible identity-style default so simple mocks only
/// need to implement the ten `IAgentTask` methods and pick up nothing for
/// free.
#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
pub trait IMockAgent: IAgentTask {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        Vec::new()
    }
    fn check_approval(&self, _action: &str, _command_type: Option<&str>) -> bool {
        false
    }
    fn confirm(
        &self,
        _msg_id: &str,
        _call_id: &str,
        _data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AgentError> {
        Ok(())
    }
    fn get_session_key(&self) -> Option<String> {
        None
    }
    async fn mode(&self) -> Result<aionui_api_types::AgentModeResponse, AgentError> {
        Ok(aionui_api_types::AgentModeResponse {
            mode: "default".into(),
            initialized: false,
        })
    }
    async fn get_model(&self) -> Result<GetModelInfoResponse, AgentError> {
        Ok(GetModelInfoResponse { model_info: None })
    }
    async fn get_config_options(&self) -> Result<GetConfigOptionsResponse, AgentError> {
        Ok(GetConfigOptionsResponse {
            config_options: Vec::new(),
        })
    }
    async fn set_config_option(&self, _option_id: &str, _value: &str) -> Result<SetConfigOptionResponse, AgentError> {
        Err(AgentError::bad_request(
            "Config option switching is not supported for this mock",
        ))
    }
    async fn get_usage(&self) -> Result<Option<serde_json::Value>, AgentError> {
        Ok(None)
    }
    async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AgentError> {
        Ok(Vec::new())
    }
    async fn handle_side_question(&self, _req: SideQuestionRequest) -> Result<SideQuestionResponse, AgentError> {
        Ok(SideQuestionResponse {
            status: "unsupported".into(),
            answer: None,
        })
    }
}

/// Concrete, closed-set dispatcher for runnable agent variants.
///
/// Every generic path holds an `AgentInstance` (not `Arc<dyn IAgentTask>`):
/// this gives us the `IAgentTask` ten-method surface via [`Self::as_task`]
/// **and** lets type-specific routes recover the concrete manager with a
/// single `match` — no `as_any` / `downcast_ref` anywhere. Adding a new
/// agent type means adding a new variant here; every `match` in the
/// codebase then fails to compile until it explicitly handles the new
/// type, which is the compile-time pressure we want.
#[derive(Clone)]
pub enum AgentInstance {
    Acp(Arc<AcpAgentManager>),
    Aionrs(Arc<AionrsAgentManager>),
    /// Test-only trait-object escape hatch used by downstream crates
    /// (conversation/cron/team/app tests) to inject fake agents without
    /// spinning up a real CLI or WebSocket connection. Gated behind
    /// `#[cfg(any(test, feature = "test-support"))]`: production builds
    /// never see this variant, so every `match` in release code can
    /// rely on the runnable closed set. The trait object is
    /// [`IMockAgent`] (extends `IAgentTask`) so mocks can also override
    /// the enum-level helpers — `get_confirmations`, `check_approval`,
    /// `confirm`, `get_session_key`, `get_mode`, `set_mode`.
    #[cfg(any(test, feature = "test-support"))]
    Mock(Arc<dyn IMockAgent>),
}

impl AgentInstance {
    /// Common `IAgentTask` view, regardless of variant.
    pub fn as_task(&self) -> &dyn IAgentTask {
        match self {
            Self::Acp(m) => m.as_ref(),
            Self::Aionrs(m) => m.as_ref(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.as_ref(),
        }
    }

    // ── Convenience forwarders ───────────────────────────────────────
    //
    // These stay in the final API (not a migration crutch): they turn
    // `instance.agent_type()` into a direct vtable-free call on the
    // concrete `Arc<XxxManager>`, and they keep callsites terse.

    /// The type of agent this instance controls.
    pub fn agent_type(&self) -> AgentType {
        self.as_task().agent_type()
    }

    /// Conversation ID this task is bound to.
    pub fn conversation_id(&self) -> &str {
        self.as_task().conversation_id()
    }

    /// Working directory for this agent session.
    pub fn workspace(&self) -> &str {
        self.as_task().workspace()
    }

    /// Current conversation status.
    pub fn status(&self) -> Option<ConversationStatus> {
        self.as_task().status()
    }

    /// Timestamp (ms) of the last activity.
    pub fn last_activity_at(&self) -> TimestampMs {
        self.as_task().last_activity_at()
    }

    /// Subscribe to the stream event channel.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.as_task().subscribe()
    }

    /// Send a user message to the agent.
    pub async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.as_task().send_message(data).await
    }

    /// Cancel the current streaming response without killing the agent.
    pub async fn cancel(&self) -> Result<(), AgentError> {
        self.as_task().cancel().await
    }

    /// Terminate the agent process.
    pub fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.as_task().kill(reason)
    }

    /// Terminate the agent process and return a future that resolves when the
    /// underlying OS process has exited.
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        match self {
            Self::Acp(m) => m.kill_and_wait(reason),
            Self::Aionrs(m) => m.kill_and_wait(reason),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(_) => Box::pin(std::future::ready(())),
        }
    }

    // ── Cross-variant semi-specific helpers ──────────────────────────
    //
    // These fan out to inherent methods on concrete managers. Variants
    // that don't support the operation return a sensible zero-value
    // rather than an error: "no pending confirmations" and "no session
    // key" are honest statements about those variants.

    /// Pending confirmation items for this task.
    ///
    /// ACP surfaces pending permission prompts through its permission
    /// router. Aionrs maintains inline confirmation lists.
    pub fn get_confirmations(&self) -> Vec<aionui_common::Confirmation> {
        match self {
            Self::Acp(m) => m.get_confirmations(),
            Self::Aionrs(m) => m.get_confirmations(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_confirmations(),
        }
    }

    /// Submit a confirmation response for a pending tool call.
    pub fn confirm(
        &self,
        msg_id: &str,
        call_id: &str,
        data: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), AgentError> {
        match self {
            Self::Acp(m) => m.confirm(msg_id, call_id, data, always_allow),
            Self::Aionrs(m) => m.confirm(msg_id, call_id, data, always_allow),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.confirm(msg_id, call_id, data, always_allow),
        }
    }

    /// Check whether an action is auto-approved in this session.
    pub fn check_approval(&self, action: &str, command_type: Option<&str>) -> bool {
        match self {
            Self::Acp(_) => false,
            Self::Aionrs(m) => m.check_approval(action, command_type),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.check_approval(action, command_type),
        }
    }

    /// Session key for test doubles that expose one.
    pub fn get_session_key(&self) -> Option<String> {
        match self {
            Self::Acp(_) | Self::Aionrs(_) => None,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_session_key(),
        }
    }

    /// Get the current session mode.
    pub async fn get_mode(&self) -> Result<aionui_api_types::AgentModeResponse, AgentError> {
        match self {
            Self::Acp(m) => m.mode().await,
            Self::Aionrs(m) => m.mode().await,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.mode().await,
        }
    }

    /// Get the current session model info. Only ACP exposes a model
    /// catalog; other variants report `model_info = None` so the UI can
    /// hide the model picker without an error.
    pub async fn get_model(&self) -> Result<GetModelInfoResponse, AgentError> {
        match self {
            Self::Acp(m) => {
                let sdk_model = m.model().await;
                let sdk_info = sdk_model.map(map_sdk_model_to_payload);
                let cc_switch_info = if m.is_claude_backend() {
                    crate::cc_switch::read_claude_model_info()
                } else {
                    None
                };
                let model_info = merge_model_info(sdk_info, cc_switch_info);
                Ok(GetModelInfoResponse { model_info })
            }
            Self::Aionrs(_) => Ok(GetModelInfoResponse { model_info: None }),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_model().await,
        }
    }

    pub async fn get_config_options(&self) -> Result<GetConfigOptionsResponse, AgentError> {
        match self {
            Self::Acp(m) => m.config_options().await,
            Self::Aionrs(_) => Ok(GetConfigOptionsResponse {
                config_options: Vec::new(),
            }),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_config_options().await,
        }
    }

    pub async fn set_config_option(&self, option_id: &str, value: &str) -> Result<SetConfigOptionResponse, AgentError> {
        if option_id.trim().is_empty() {
            return Err(AgentError::bad_request("option_id must not be empty"));
        }
        if value.trim().is_empty() {
            return Err(AgentError::bad_request("value must not be empty"));
        }
        match self {
            Self::Acp(m) => m.set_config_option_confirmed(option_id, value).await,
            Self::Aionrs(_) => Err(AgentError::bad_request(
                "Config option switching is not supported for this agent type",
            )),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.set_config_option(option_id, value).await,
        }
    }

    /// Returns the cached session usage as a snake_case JSON object. The
    /// structure mirrors the ACP SDK `UsageUpdate` schema
    /// (`used` / `size` / `cost` / `_meta`), normalised via
    /// [`aionui_common::normalize_keys_to_snake_case`] so keys land as
    /// `used` / `size` / `cost` to match the AionUI wire convention —
    /// `_meta` passes through verbatim.
    ///
    /// Non-ACP agents return `None`.
    pub async fn get_usage(&self) -> Result<Option<serde_json::Value>, AgentError> {
        match self {
            Self::Acp(m) => {
                let Some(usage) = m.usage().await else { return Ok(None) };
                let mut value = serde_json::to_value(usage)
                    .map_err(|e| AgentError::internal(format!("Failed to serialize usage: {e}")))?;
                aionui_common::normalize_keys_to_snake_case(&mut value);
                Ok(Some(value))
            }
            Self::Aionrs(_) => Ok(None),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_usage().await,
        }
    }

    /// Slash commands available in the current session. Only ACP exposes
    /// a slash-command catalog; other variants report an empty list
    /// (the UI renders "no commands").
    pub async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AgentError> {
        match self {
            Self::Acp(m) => m.load_slash_commands().await,
            Self::Aionrs(m) => m.get_slash_commands().await,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.get_slash_commands().await,
        }
    }

    /// Dispatch a side-question to the agent. **Placeholder** — matches
    /// the current `AgentService::handle_side_question` behaviour: ACP
    /// agents whose behavior_policy enables side-questions return a stub
    /// "ok" response, everyone else returns `unsupported`.
    pub async fn handle_side_question(&self, req: SideQuestionRequest) -> Result<SideQuestionResponse, AgentError> {
        if req.question.trim().is_empty() {
            return Err(AgentError::bad_request("question must not be empty"));
        }
        match self {
            Self::Acp(m) => {
                if !m.supports_side_question() {
                    return Ok(SideQuestionResponse {
                        status: "unsupported".into(),
                        answer: None,
                    });
                }
                Ok(SideQuestionResponse {
                    status: "ok".into(),
                    answer: Some("Side question support will be fully wired in app integration phase.".into()),
                })
            }
            Self::Aionrs(_) => Ok(SideQuestionResponse {
                status: "unsupported".into(),
                answer: None,
            }),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(m) => m.handle_side_question(req).await,
        }
    }
}

/// Map the raw ACP SDK model state into the public API payload.
///
/// Kept private to this module: the only caller is
/// [`AgentInstance::get_model`]. Mirrors the helper formerly living in
/// `services/agent.rs`; do not duplicate — if the shape of
/// `ModelInfoPayload` changes, update it here.
fn map_sdk_model_to_payload(m: agent_client_protocol::schema::SessionModelState) -> ModelInfoPayload {
    let available: Vec<ModelInfoEntry> = m
        .available_models
        .iter()
        .map(|am| ModelInfoEntry {
            id: am.model_id.to_string(),
            label: am.name.clone(),
        })
        .collect();
    let current_id = m.current_model_id.to_string();
    let current_label = available
        .iter()
        .find(|e| e.id == current_id)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| current_id.clone());
    ModelInfoPayload {
        current_model_id: Some(current_id),
        current_model_label: Some(current_label),
        available_models: available,
    }
}

fn merge_model_info(
    sdk_info: Option<ModelInfoPayload>,
    cc_switch_info: Option<ModelInfoPayload>,
) -> Option<ModelInfoPayload> {
    sdk_info.or(cc_switch_info)
}

#[cfg(test)]
mod cc_switch_model_merge_tests {
    use super::*;

    #[test]
    fn merge_prefers_sdk_model_over_cc_switch() {
        let sdk_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("Claude Sonnet 4.6".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "Claude Sonnet 4.6".into(),
            }],
        };
        let cc_switch_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("DeepSeek V4".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "DeepSeek V4".into(),
            }],
        };

        let result = merge_model_info(Some(sdk_payload), Some(cc_switch_payload));
        assert_eq!(
            result.unwrap().current_model_label.as_deref(),
            Some("Claude Sonnet 4.6")
        );
    }

    #[test]
    fn merge_falls_back_to_cc_switch_when_sdk_none() {
        let cc_switch_payload = ModelInfoPayload {
            current_model_id: Some("default".into()),
            current_model_label: Some("DeepSeek V4".into()),
            available_models: vec![ModelInfoEntry {
                id: "default".into(),
                label: "DeepSeek V4".into(),
            }],
        };

        let result = merge_model_info(None, Some(cc_switch_payload));
        assert_eq!(result.unwrap().current_model_label.as_deref(), Some("DeepSeek V4"));
    }

    #[test]
    fn merge_returns_none_when_both_none() {
        let result = merge_model_info(None, None);
        assert!(result.is_none());
    }
}
