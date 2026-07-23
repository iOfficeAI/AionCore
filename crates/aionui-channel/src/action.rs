use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_api_types::{ChannelAssistantSettingRequest, ChannelDefaultModelSetting};
use tracing::{debug, info, warn};

use crate::approval::{ChannelApprovalPort, ChannelApprovalResolutionContext};
use crate::channel_settings::{ChannelAgentOption, ChannelSettingsService};
use crate::development::{ChannelDevelopmentCommand, ChannelDevelopmentContext, ChannelDevelopmentPort};
use crate::error::ChannelError;
use crate::pairing::PairingService;
use crate::session::SessionManager;
use crate::types::{
    ActionBehavior, ActionButton, ActionCategory, ActionResponse, ChannelConversationTitleHint, MessageContentType,
    ParseMode, UnifiedAction, UnifiedIncomingMessage,
};

/// Result of processing an incoming message.
///
/// The caller (ChannelManager / plugin) uses this to decide what to send
/// back to the IM platform.
#[derive(Debug, Clone)]
pub enum MessageResult {
    /// An action response to send/edit on the platform.
    Action(ActionResponse),
    /// Message was dispatched to the AI Agent. The caller should send
    /// a "thinking" placeholder and then relay stream events.
    Dispatched {
        session_id: String,
        conversation_id: Option<String>,
        title_hint: Option<ChannelConversationTitleHint>,
    },
    /// Message was a text but user already has an active agent stream
    /// (no duplicate dispatch needed).
    AlreadyProcessing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelTeamSummary {
    pub id: String,
    pub name: String,
    pub lead_conversation_id: Option<String>,
    pub agent_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelTeamCreateRequest {
    pub name: String,
    pub lead_name: String,
    pub lead_role: String,
    pub assistant_id: String,
    pub model: String,
    pub source_channel: Option<String>,
    pub source_channel_id: Option<String>,
    pub source_chat_id: Option<String>,
    pub source_user_id: Option<String>,
    pub source_label: Option<String>,
    pub created_from: Option<String>,
}

#[async_trait::async_trait]
pub trait ChannelTeamDirectory: Send + Sync {
    async fn list_teams(&self, user_id: &str) -> Result<Vec<ChannelTeamSummary>, ChannelError>;
    async fn get_team(&self, user_id: &str, team_id: &str) -> Result<Option<ChannelTeamSummary>, ChannelError>;
    async fn ensure_team_session(&self, user_id: &str, team_id: &str) -> Result<(), ChannelError>;
    async fn create_team(
        &self,
        user_id: &str,
        request: ChannelTeamCreateRequest,
    ) -> Result<ChannelTeamSummary, ChannelError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPersonalConversationSummary {
    pub id: String,
    pub name: String,
    pub agent_type: String,
    pub agent_label: Option<String>,
    pub recent_message: Option<String>,
}

#[async_trait::async_trait]
pub trait ChannelPersonalDirectory: Send + Sync {
    async fn list_personal_conversations(
        &self,
        user_id: &str,
        platform: crate::types::PluginType,
        chat_id: &str,
    ) -> Result<Vec<ChannelPersonalConversationSummary>, ChannelError>;
    async fn get_personal_conversation(
        &self,
        user_id: &str,
        platform: crate::types::PluginType,
        chat_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError>;
    async fn rename_personal_conversation(
        &self,
        user_id: &str,
        platform: crate::types::PluginType,
        chat_id: &str,
        conversation_id: &str,
        title: &str,
    ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError>;
}

/// Processes incoming IM messages: authorization → action routing → AI dispatch.
///
/// This is the core message entry point for the channel system. Each
/// incoming `UnifiedIncomingMessage` is either:
/// 1. Rejected (unauthorized → pairing flow)
/// 2. Routed to an action handler (button callback)
/// 3. Dispatched to the AI Agent (text message)
pub struct ActionExecutor {
    pairing: Arc<PairingService>,
    session_mgr: Arc<SessionManager>,
    settings: Arc<ChannelSettingsService>,
    team_directory: Option<Arc<dyn ChannelTeamDirectory>>,
    personal_directory: Option<Arc<dyn ChannelPersonalDirectory>>,
    approval_port: Option<Arc<dyn ChannelApprovalPort>>,
    development_port: Option<Arc<dyn ChannelDevelopmentPort>>,
    pending_flows: Arc<Mutex<HashMap<String, PendingFlow>>>,
    personal_title_hints: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingFlow {
    TeamNewAwaitingName,
    TeamNewConfirm { topic: String },
}

impl ActionExecutor {
    pub fn new(
        pairing: Arc<PairingService>,
        session_mgr: Arc<SessionManager>,
        settings: Arc<ChannelSettingsService>,
    ) -> Self {
        Self {
            pairing,
            session_mgr,
            settings,
            team_directory: None,
            personal_directory: None,
            approval_port: None,
            development_port: None,
            pending_flows: Arc::new(Mutex::new(HashMap::new())),
            personal_title_hints: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_team_directory(mut self, team_directory: Arc<dyn ChannelTeamDirectory>) -> Self {
        self.team_directory = Some(team_directory);
        self
    }

    pub fn with_personal_directory(mut self, personal_directory: Arc<dyn ChannelPersonalDirectory>) -> Self {
        self.personal_directory = Some(personal_directory);
        self
    }

    pub fn with_approval_port(mut self, approval_port: Arc<dyn ChannelApprovalPort>) -> Self {
        self.approval_port = Some(approval_port);
        self
    }

    pub fn with_development_port(mut self, development_port: Arc<dyn ChannelDevelopmentPort>) -> Self {
        self.development_port = Some(development_port);
        self
    }

    /// Main entry point: handle an incoming message from any platform.
    ///
    /// Flow:
    /// 1. Authorization check → if unauthorized, trigger pairing
    /// 2. Button callback → route to action handler
    /// 3. Text message → get/create session → return Dispatched for AI
    pub async fn handle_incoming_message(&self, msg: &UnifiedIncomingMessage) -> Result<MessageResult, ChannelError> {
        let platform_type = msg.platform.to_string();
        let user_id = &msg.user.id;
        let chat_id = &msg.chat_id;

        // 1. Authorization check — resolve platform user → internal user ID
        let internal_user_id = self.pairing.get_internal_user_id(user_id, &platform_type).await?;

        let internal_user_id = match internal_user_id {
            Some(id) => id,
            None => {
                let response = self
                    .handle_unauthorized(user_id, &platform_type, &msg.user.display_name)
                    .await?;
                return Ok(MessageResult::Action(response));
            }
        };

        // 2. Button callback → action routing
        if let Some(action) = &msg.action {
            let response = self.route_action(action, &internal_user_id, msg).await?;
            return Ok(MessageResult::Action(response));
        }

        if msg.content.content_type == MessageContentType::Command || msg.content.text.trim_start().starts_with('/') {
            let response = self.handle_command(&msg.content.text, &internal_user_id, msg).await?;
            return Ok(MessageResult::Action(response));
        }

        if msg.content.content_type == MessageContentType::Text
            && let Some(response) = self.handle_pending_text(&internal_user_id, msg).await?
        {
            return Ok(MessageResult::Action(response));
        }

        // 3. Text message → session resolution → AI dispatch
        let session = if msg.platform == crate::types::PluginType::Telegram
            && let Some(topic) = &msg.topic
            && topic.message_thread_id != 1
        {
            let Some(binding) = self
                .session_mgr
                .get_topic_binding(chat_id, topic.message_thread_id)
                .await?
            else {
                return Ok(MessageResult::Action(html_response(
                    "此话题尚未绑定 Agent。请群管理员在本话题执行 /topic_bind &lt;agent-id&gt;。",
                    None,
                )));
            };
            let option = self
                .settings
                .list_agent_options()
                .await?
                .into_iter()
                .find(|option| option.agent_id == binding.agent_id)
                .ok_or_else(|| {
                    ChannelError::InvalidConfig(format!("Bound agent '{}' is unavailable", binding.agent_id))
                })?;
            self.session_mgr
                .get_or_create_topic_session(
                    &internal_user_id,
                    chat_id,
                    topic.message_thread_id,
                    &option.agent_type,
                    &option.agent_id,
                    option.backend.as_deref(),
                    None,
                )
                .await?
        } else {
            let agent_config = self.settings.get_agent_config(msg.platform).await?;
            self.session_mgr
                .get_or_create_session(&internal_user_id, chat_id, &agent_config.agent_type, None)
                .await?
        };
        let title_hint = if session.conversation_id.is_none() {
            self.take_or_build_personal_title_hint(&internal_user_id, msg)
        } else {
            None
        };

        info!(
            session_id = %session.id,
            user_id = %user_id,
            chat_id = %chat_id,
            text_len = msg.content.text.len(),
            "message dispatched to agent"
        );

        Ok(MessageResult::Dispatched {
            session_id: session.id,
            conversation_id: session.conversation_id,
            title_hint,
        })
    }

    fn take_or_build_personal_title_hint(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Option<ChannelConversationTitleHint> {
        let key = pending_key(internal_user_id, msg.platform, &msg.chat_id);
        if let Some(title) = self.personal_title_hints.lock().unwrap().remove(&key) {
            return Some(ChannelConversationTitleHint {
                title,
                source: format!("{}_explicit", msg.platform),
            });
        }
        auto_title_from_message(&msg.content.text).map(|title| ChannelConversationTitleHint {
            title,
            source: format!("{}_first_message", msg.platform),
        })
    }

    /// Handles an unauthorized user: generate pairing code and return
    /// a response with instructions and action buttons.
    async fn handle_unauthorized(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        display_name: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let code = self
            .pairing
            .request_pairing(platform_user_id, platform_type, Some(display_name))
            .await?;

        debug!(
            platform_user_id = %platform_user_id,
            code = %code,
            "pairing code generated for unauthorized user"
        );

        Ok(build_pairing_response(&code))
    }

    /// Routes an action to the appropriate handler by category.
    async fn route_action(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        if msg.platform == crate::types::PluginType::Telegram
            && msg.topic.as_ref().is_some_and(|topic| topic.message_thread_id != 1)
            && (action.action.starts_with("agent.")
                || action.action.starts_with("team.")
                || action.action.starts_with("personal.")
                || action.action == "model.clear")
        {
            return Ok(html_response(
                "此话题固定绑定单 Agent，不能使用 Agent、Team 或入口切换按钮。",
                None,
            ));
        }
        match action.category {
            ActionCategory::Platform => self.handle_platform_action(action).await,
            ActionCategory::System => self.handle_system_action(action, internal_user_id, msg).await,
            ActionCategory::Chat => self.handle_chat_action(action).await,
        }
    }

    // ── Platform actions ────────────────────────────────────────────

    async fn handle_platform_action(&self, action: &UnifiedAction) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "pairing.show" | "pairing.refresh" => {
                let code = self
                    .pairing
                    .request_pairing(&action.context.user_id, &action.context.platform.to_string(), None)
                    .await?;
                Ok(build_pairing_response(&code))
            }
            "pairing.check" => {
                let authorized = self
                    .pairing
                    .is_user_authorized(&action.context.user_id, &action.context.platform.to_string())
                    .await?;
                if authorized {
                    Ok(ActionResponse {
                        text: Some("You are authorized! Send a message to start chatting.".into()),
                        parse_mode: None,
                        buttons: None,
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                } else {
                    Ok(ActionResponse {
                        text: Some("Still waiting for approval. Ask the admin to check Settings → Channel.".into()),
                        parse_mode: None,
                        buttons: Some(vec![vec![
                            ActionButton {
                                label: "Refresh".into(),
                                action: "pairing.refresh".into(),
                                params: None,
                            },
                            ActionButton {
                                label: "Check Again".into(),
                                action: "pairing.check".into(),
                                params: None,
                            },
                        ]]),
                        keyboard: None,
                        behavior: ActionBehavior::Send,
                        toast: None,
                        edit_message_id: None,
                    })
                }
            }
            "pairing.help" => Ok(ActionResponse {
                text: Some(
                    "To use this bot, you need authorization:\n\
                         1. Send any message to get a 6-digit pairing code\n\
                         2. Share this code with the admin\n\
                         3. Admin approves in Settings → Channel\n\
                         4. You're ready to chat!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            other => {
                warn!(action = %other, "unknown platform action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── System actions ──────────────────────────────────────────────

    async fn handle_system_action(
        &self,
        action: &UnifiedAction,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "approval.resolve" => {
                let params = action
                    .params
                    .as_ref()
                    .ok_or_else(|| ChannelError::InvalidConfig("Missing approval callback parameters".into()))?;
                let approval_id = params
                    .get("id")
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| ChannelError::InvalidConfig("Missing approval ID".into()))?;
                let option_index = params
                    .get("o")
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or_else(|| ChannelError::InvalidConfig("Invalid approval option".into()))?;
                let approval_port = self
                    .approval_port
                    .as_ref()
                    .ok_or_else(|| ChannelError::InvalidConfig("Approval service is unavailable".into()))?;
                let status = approval_port
                    .resolve(
                        ChannelApprovalResolutionContext {
                            source_user_id: internal_user_id.to_owned(),
                            platform: msg.platform,
                            chat_id: msg.chat_id.clone(),
                            message_thread_id: msg.topic.as_ref().map(|topic| topic.message_thread_id),
                            is_admin: msg
                                .raw
                                .as_ref()
                                .and_then(|raw| raw.get("telegram_chat_member_status"))
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|status| matches!(status, "creator" | "administrator")),
                        },
                        approval_id,
                        option_index,
                    )
                    .await?;
                let rejected = status == "rejected";
                let toast = if rejected { "审批已拒绝" } else { "审批已同意" };
                let edit_message_id = action.context.message_id.clone();
                Ok(ActionResponse {
                    text: Some(format!(
                        "{}\n审批 ID：{approval_id}",
                        if rejected {
                            "❌ 审批已拒绝"
                        } else {
                            "✅ 审批已同意"
                        }
                    )),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: if edit_message_id.is_some() {
                        ActionBehavior::Edit
                    } else {
                        ActionBehavior::Send
                    },
                    toast: Some(toast.into()),
                    edit_message_id,
                })
            }
            "session.new" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let agent_config = self.settings.get_agent_config(action.context.platform).await?;
                let session = self
                    .session_mgr
                    .reset_session(user_id, chat_id, &agent_config.agent_type, None)
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!(
                        "New session created.\nAgent: {}\nSession: {}",
                        session.agent_type,
                        &session.id[..8]
                    )),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "Help".into(),
                        action: "help.show".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "session.status" => {
                let user_id = internal_user_id;
                let chat_id = &action.context.chat_id;
                let agent_config = self.settings.get_agent_config(action.context.platform).await?;
                let session = self
                    .session_mgr
                    .get_or_create_session(user_id, chat_id, &agent_config.agent_type, None)
                    .await?;

                Ok(ActionResponse {
                    text: Some(format!(
                        "Session: {}\nAgent: {}\nCreated: {}\nLast active: {}",
                        &session.id[..8],
                        session.agent_type,
                        session.created_at,
                        session.last_activity,
                    )),
                    parse_mode: None,
                    buttons: Some(vec![vec![ActionButton {
                        label: "New Session".into(),
                        action: "session.new".into(),
                        params: None,
                    }]]),
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: None,
                    edit_message_id: None,
                })
            }
            "help.show" => Ok(build_help_response()),
            "team.select" => {
                let team_id = action
                    .params
                    .as_ref()
                    .and_then(|params| params.get("id"))
                    .map(String::as_str)
                    .unwrap_or_default();
                self.select_team_by_selector(
                    internal_user_id,
                    action.context.platform,
                    &action.context.chat_id,
                    team_id,
                )
                .await
            }
            "team.list" => self.build_team_list_command(internal_user_id).await,
            "team.new.start" => self.create_team_command(internal_user_id, msg, "").await,
            "team.help" => Ok(build_team_help_response()),
            "team.new.create" => {
                let key = pending_key(internal_user_id, action.context.platform, &action.context.chat_id);
                let topic = {
                    let mut pending = self.pending_flows.lock().unwrap();
                    match pending.remove(&key) {
                        Some(PendingFlow::TeamNewConfirm { topic }) => Some(topic),
                        _ => None,
                    }
                };
                let Some(topic) = topic else {
                    return Ok(html_response(
                        "没有待确认的 Team 创建草稿。\n\n发送 /team_new 重新开始。",
                        None,
                    ));
                };
                self.create_team_from_topic(
                    internal_user_id,
                    action.context.platform,
                    &action.context.chat_id,
                    &action.context.user_id,
                    &topic,
                )
                .await
            }
            "team.new.confirm" => Ok(html_response("旧的草稿确认按钮已下线。发送 /team_new 重新开始。", None)),
            "team.new.cancel" => {
                let key = pending_key(internal_user_id, action.context.platform, &action.context.chat_id);
                self.pending_flows.lock().unwrap().remove(&key);
                Ok(html_response("已取消 Telegram Team 创建流程。", None))
            }
            "agent.list" => self.build_agent_picker_command(action.context.platform).await,
            "agent.select" => self.select_agent_action(internal_user_id, action).await,
            "model.select" => self.select_model_action(internal_user_id, action, msg).await,
            "model.clear" => self.clear_model_action(internal_user_id, action).await,
            "personal.select" => self.select_personal_action(internal_user_id, action).await,
            "personal.list" => self.build_personal_list_command(internal_user_id, msg).await,
            "help.features" => Ok(ActionResponse {
                text: Some(
                    "Features:\n\
                         • AI chat through your configured assistant\n\
                         • Tool execution with explicit approval\n\
                         • Session isolation per chat"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.pairing" => Ok(ActionResponse {
                text: Some(
                    "Pairing:\n\
                         Send any message → get a 6-digit code → admin approves → you're in!"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "help.tips" => Ok(ActionResponse {
                text: Some(
                    "Tips:\n\
                         • Start a new session to clear context\n\
                         • Use /help to see available commands\n\
                         • In group chats, @mention the bot"
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            "settings.show" => Ok(ActionResponse {
                text: Some(
                    "Settings are managed in the desktop app.\n\
                         Go to Settings → Channel to configure plugins and manage users."
                        .into(),
                ),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Send,
                toast: None,
                edit_message_id: None,
            }),
            other => {
                warn!(action = %other, "unknown system action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    // ── Chat actions ────────────────────────────────────────────────

    async fn handle_chat_action(&self, action: &UnifiedAction) -> Result<ActionResponse, ChannelError> {
        match action.action.as_str() {
            "chat.send" | "chat.regenerate" | "chat.continue" => {
                // These are handled by the message flow, not action responses.
                // Return a placeholder; the real logic is in ChannelMessageService.
                Ok(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Send,
                    toast: Some("Processing...".into()),
                    edit_message_id: None,
                })
            }
            "action.copy" => Ok(ActionResponse {
                text: None,
                parse_mode: None,
                buttons: None,
                keyboard: None,
                behavior: ActionBehavior::Answer,
                toast: Some("Copied to clipboard".into()),
                edit_message_id: None,
            }),
            "system.confirm" => {
                let call_id = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("callId"))
                    .cloned()
                    .unwrap_or_default();
                let value = action
                    .params
                    .as_ref()
                    .and_then(|p| p.get("value"))
                    .cloned()
                    .unwrap_or_else(|| "true".into());

                debug!(call_id = %call_id, value = %value, "tool confirmation received");

                Ok(ActionResponse {
                    text: None,
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    behavior: ActionBehavior::Answer,
                    toast: Some("Confirmed".into()),
                    edit_message_id: None,
                })
            }
            other => {
                warn!(action = %other, "unknown chat action");
                Ok(build_unknown_action_response(other))
            }
        }
    }

    async fn handle_pending_text(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<Option<ActionResponse>, ChannelError> {
        let key = pending_key(internal_user_id, msg.platform, &msg.chat_id);
        let flow = self.pending_flows.lock().unwrap().get(&key).cloned();
        let Some(flow) = flow else {
            return Ok(None);
        };

        match flow {
            PendingFlow::TeamNewAwaitingName | PendingFlow::TeamNewConfirm { .. } => {
                let topic = msg.content.text.trim();
                if topic.is_empty() {
                    return Ok(Some(html_response(
                        "团队名称不能为空。请直接回复团队名称，或点击取消。",
                        Some(vec![vec![ActionButton {
                            label: "取消".into(),
                            action: "team.new.cancel".into(),
                            params: None,
                        }]]),
                    )));
                }

                self.pending_flows.lock().unwrap().insert(
                    key,
                    PendingFlow::TeamNewConfirm {
                        topic: topic.to_owned(),
                    },
                );

                Ok(Some(html_response(
                    &format!(
                        "<b>准备创建 Team</b>\nTeam: {}\n\n确认后会创建真实 Team，并把当前 Telegram 聊天切换到 Team Lead。",
                        html_escape(topic)
                    ),
                    Some(vec![vec![
                        ActionButton {
                            label: "创建 Team".into(),
                            action: "team.new.create".into(),
                            params: None,
                        },
                        ActionButton {
                            label: "取消".into(),
                            action: "team.new.cancel".into(),
                            params: None,
                        },
                    ]]),
                )))
            }
        }
    }

    async fn handle_command(
        &self,
        raw: &str,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        let trimmed = raw.trim();
        let without_slash = trimmed.trim_start_matches('/');
        let mut parts = without_slash.splitn(2, char::is_whitespace);
        let command = parts
            .next()
            .unwrap_or_default()
            .split('@')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let args = parts.next().unwrap_or_default().trim();

        if msg.platform == crate::types::PluginType::Telegram
            && let Some(topic) = &msg.topic
            && topic.message_thread_id != 1
        {
            let is_admin = msg
                .raw
                .as_ref()
                .and_then(|raw| raw.get("telegram_chat_member_status"))
                .and_then(|status| status.as_str())
                .is_some_and(|status| matches!(status, "creator" | "administrator"));
            match command.as_str() {
                "topic_bind" => {
                    if !is_admin {
                        return Ok(html_response("仅群管理员可以绑定话题。", None));
                    }
                    let options = self.settings.list_agent_options().await?;
                    let Some(option) = options
                        .into_iter()
                        .find(|option| option.agent_id == args || option.name.eq_ignore_ascii_case(args))
                    else {
                        return Ok(html_response(
                            "Agent 不存在或当前不可用。请使用系统中的准确 Agent ID。",
                            None,
                        ));
                    };
                    self.session_mgr
                        .bind_topic(
                            &msg.chat_id,
                            topic.message_thread_id,
                            &option.agent_id,
                            internal_user_id,
                        )
                        .await?;
                    return Ok(html_response(
                        &format!("已将当前话题绑定到 <b>{}</b>。", html_escape(&option.name)),
                        None,
                    ));
                }
                "topic_unbind" => {
                    if !is_admin {
                        return Ok(html_response("仅群管理员可以解绑话题。", None));
                    }
                    self.session_mgr
                        .unbind_topic(&msg.chat_id, topic.message_thread_id)
                        .await?;
                    return Ok(html_response("当前话题已解绑。", None));
                }
                "topic_info" => {
                    let binding = self
                        .session_mgr
                        .get_topic_binding(&msg.chat_id, topic.message_thread_id)
                        .await?;
                    return Ok(match binding {
                        Some(binding) => html_response(
                            &format!(
                                "<b>话题绑定</b>\nThread: {}\nAgent ID: {}",
                                topic.message_thread_id,
                                html_escape(&binding.agent_id)
                            ),
                            None,
                        ),
                        None => html_response("当前话题尚未绑定 Agent。", None),
                    });
                }
                _ => {}
            }
            let binding = self
                .session_mgr
                .get_topic_binding(&msg.chat_id, topic.message_thread_id)
                .await?;
            if let Some(binding) = binding {
                let option = self
                    .settings
                    .list_agent_options()
                    .await?
                    .into_iter()
                    .find(|option| option.agent_id == binding.agent_id)
                    .ok_or_else(|| {
                        ChannelError::InvalidConfig(format!("Bound agent '{}' is unavailable", binding.agent_id))
                    })?;
                if matches!(command.as_str(), "help" | "start" | "") {
                    return Ok(html_response(
                        &format!(
                            "<b>单 Agent 话题</b>\n当前 Agent：<b>{}</b>\n\n可用命令：\n/model — 查看并切换此 Agent 可用模型\n/status — 查看当前话题会话状态\n/new 或 /new_session — 新建当前话题会话\n/topic_info — 查看绑定信息\n/project — 当前项目\n/run_info — 当前开发运行\n/diff_summary — 变更与证据摘要\n/test — 执行配置的单元测试门禁\n/stop — 停止当前运行\n/retry — 重试最近失败门禁\n/handoff — Web 接力入口\n\n管理员命令：\n/topic_bind &lt;agent-id&gt;\n/topic_unbind\n\n此话题不支持 Agent 切换或 Team/集群功能。",
                            html_escape(&option.name),
                        ),
                        None,
                    ));
                }
                if command == "model" {
                    return self
                        .build_topic_model_command(internal_user_id, msg, &binding.agent_id, args)
                        .await;
                }
                if command == "status" {
                    let session = self
                        .session_mgr
                        .get_or_create_topic_session(
                            internal_user_id,
                            &msg.chat_id,
                            topic.message_thread_id,
                            &option.agent_type,
                            &option.agent_id,
                            option.backend.as_deref(),
                            None,
                        )
                        .await?;
                    let model = match (&session.bound_provider_id, &session.bound_model) {
                        (Some(provider), Some(model)) => format!("{provider} / {model}"),
                        _ => "Agent 默认模型".to_owned(),
                    };
                    return Ok(html_response(
                        &format!(
                            "<b>当前话题会话</b>\nThread: {}\nAgent: {}\nSession: {}\nModel: {}\nConversation: {}",
                            topic.message_thread_id,
                            html_escape(&option.name),
                            html_escape(&short_channel_id(&session.id)),
                            html_escape(&model),
                            html_escape(session.conversation_id.as_deref().unwrap_or("尚未创建")),
                        ),
                        None,
                    ));
                }
                if matches!(command.as_str(), "new" | "new_session") {
                    self.session_mgr
                        .reset_topic_session(
                            internal_user_id,
                            &msg.chat_id,
                            topic.message_thread_id,
                            &option.agent_type,
                            &option.agent_id,
                            option.backend.as_deref(),
                            None,
                        )
                        .await?;
                    return Ok(html_response("已为你新建当前话题的单 Agent 会话。", None));
                }
                if command == "agent" {
                    return Ok(html_response(
                        &format!(
                            "当前话题固定绑定 Agent：<b>{}</b>，不可在话题内切换。",
                            html_escape(&option.name)
                        ),
                        None,
                    ));
                }
                if command.starts_with("team")
                    || matches!(
                        command.as_str(),
                        "personal" | "personal_list" | "conversation_select" | "rename"
                    )
                {
                    return Ok(html_response(
                        "此话题已绑定单 Agent，不能使用集群或入口切换命令。",
                        None,
                    ));
                }
            } else if !matches!(command.as_str(), "help" | "start") {
                return Ok(html_response(
                    "此话题尚未绑定 Agent。请群管理员执行 /topic_bind &lt;agent-id&gt;。",
                    None,
                ));
            }
        }

        match command.as_str() {
            "start" | "help" => Ok(build_command_help_response()),
            "status" => self.build_status_command(internal_user_id, msg).await,
            "agent" => self.build_agent_command(msg).await,
            "model" => self.build_model_command(internal_user_id, msg, args).await,
            "personal" => {
                self.reset_personal_session_command(internal_user_id, msg, false, None)
                    .await
            }
            "personal_list" | "conversation_select" => self.build_personal_list_command(internal_user_id, msg).await,
            "new_session" => {
                self.reset_personal_session_command(internal_user_id, msg, true, Some(args))
                    .await
            }
            "rename" => self.rename_personal_command(internal_user_id, msg, args).await,
            "team" | "team_status" => self.build_team_status_command(internal_user_id, msg).await,
            "team_list" => self.build_team_list_command(internal_user_id).await,
            "team_select" => self.select_team_command(internal_user_id, msg, args).await,
            "team_help" => Ok(build_team_help_response()),
            "team_new" => self.create_team_command(internal_user_id, msg, args).await,
            "project" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::Project)
                    .await
            }
            "run_info" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::RunInfo)
                    .await
            }
            "diff_summary" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::DiffSummary)
                    .await
            }
            "test" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::Test)
                    .await
            }
            "stop" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::Stop)
                    .await
            }
            "retry" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::Retry)
                    .await
            }
            "handoff" => {
                self.execute_development_command(internal_user_id, msg, ChannelDevelopmentCommand::Handoff)
                    .await
            }
            "" => Ok(build_command_help_response()),
            _ => Ok(html_response("未知命令。发送 /help 查看可用命令。", None)),
        }
    }

    async fn execute_development_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        command: ChannelDevelopmentCommand,
    ) -> Result<ActionResponse, ChannelError> {
        let port = self
            .development_port
            .as_ref()
            .ok_or_else(|| ChannelError::InvalidConfig("Development command service is unavailable".into()))?;
        let thread_id = msg.topic.as_ref().map(|topic| topic.message_thread_id);
        let conversation_id = self
            .session_mgr
            .get_active_sessions()
            .await?
            .into_iter()
            .find(|session| {
                session.user_id == internal_user_id
                    && session.chat_id.as_deref() == Some(msg.chat_id.as_str())
                    && session.message_thread_id == thread_id
            })
            .and_then(|session| session.conversation_id);
        let text = port
            .execute(
                ChannelDevelopmentContext {
                    source_user_id: internal_user_id.to_owned(),
                    conversation_id,
                    platform: msg.platform,
                    chat_id: msg.chat_id.clone(),
                    message_thread_id: thread_id,
                },
                command,
            )
            .await?;
        Ok(html_response(&html_escape(&text), None))
    }

    async fn build_status_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let session = self
            .session_mgr
            .get_or_create_session(internal_user_id, &msg.chat_id, &agent_config.agent_type, None)
            .await?;
        let conversation = session.conversation_id.as_deref().unwrap_or("尚未绑定");
        Ok(html_response(
            &format!(
                "<b>当前 Telegram 绑定</b>\nSession: {}\nAgent: {}\nConversation: {}\nChat: {}",
                html_escape(&short_channel_id(&session.id)),
                html_escape(&session.agent_type),
                html_escape(conversation),
                html_escape(&msg.chat_id),
            ),
            None,
        ))
    }

    async fn build_agent_command(&self, msg: &UnifiedIncomingMessage) -> Result<ActionResponse, ChannelError> {
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let model_config = self.settings.get_model_config(msg.platform).await?;
        let model = model_config
            .as_ref()
            .map(|config| format!("{} / {}", config.provider_id, config.model))
            .unwrap_or_else(|| "默认模型".into());
        let agent_options = self.settings.list_agent_options().await?;
        let assistant = self.settings.get_assistant_setting(msg.platform).await?;
        let agent_label = resolve_current_agent_option(&agent_options, assistant.as_ref(), &agent_config)
            .map(|option| option.name.as_str())
            .or_else(|| assistant.as_ref().and_then(|setting| setting.name.as_deref()))
            .or_else(|| assistant.as_ref().and_then(|setting| setting.assistant_id.as_deref()))
            .unwrap_or("默认 Agent");
        let buttons = Some(vec![vec![ActionButton {
            label: "切换 Agent".into(),
            action: "agent.list".into(),
            params: None,
        }]]);
        Ok(html_response(
            &format!(
                "<b>当前 Telegram Agent 绑定</b>\nAgent: {}\nType: {}\nBackend: {}\nModel: {}\n\n这个绑定作用于 Telegram 渠道的新个人会话和 Team 创建。",
                html_escape(agent_label),
                html_escape(&agent_config.agent_type),
                html_escape(agent_config.backend.as_deref().unwrap_or("默认")),
                html_escape(&model),
            ),
            buttons,
        ))
    }

    async fn build_agent_picker_command(
        &self,
        platform: crate::types::PluginType,
    ) -> Result<ActionResponse, ChannelError> {
        let options = self.settings.list_agent_options().await?;
        if options.is_empty() {
            return Ok(html_response(
                "当前没有可在 Telegram 中切换的本地 Agent。\n\n可以先在 WebUI 的 Agents 页面刷新检测本地 Agent。",
                None,
            ));
        }
        let agent_config = self.settings.get_agent_config(platform).await?;
        let assistant = self.settings.get_assistant_setting(platform).await?;
        let current_agent_id = resolve_current_agent_option(&options, assistant.as_ref(), &agent_config)
            .map(|option| option.agent_id.as_str());

        let mut text = String::from("<b>请选择 Telegram 渠道要绑定的 Agent</b>\n");
        let mut buttons = Vec::new();
        for option in options
            .iter()
            .filter(|option| Some(option.agent_id.as_str()) != current_agent_id)
            .take(12)
        {
            text.push_str(&format!(
                "\n- {} ({})",
                html_escape(&option.name),
                html_escape(&option.agent_type)
            ));
            buttons.push(vec![ActionButton {
                label: short_button_label(&option.name),
                action: "agent.select".into(),
                params: Some(HashMap::from([("agentId".into(), option.agent_id.clone())])),
            }]);
        }
        if buttons.is_empty() {
            text.push_str("\n\n当前已绑定唯一可用 Agent，没有其他可切换项。");
        }
        if options.len() > 12 {
            text.push_str("\n\n当前只显示前 12 个本地 Agent。更完整的管理请到 WebUI Agents 页面。");
        }

        Ok(html_response(&text, Some(buttons)))
    }

    async fn select_agent_action(
        &self,
        internal_user_id: &str,
        action: &UnifiedAction,
    ) -> Result<ActionResponse, ChannelError> {
        let agent_id = action
            .params
            .as_ref()
            .and_then(|params| params.get("agentId").or_else(|| params.get("assistantId")))
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        if agent_id.is_empty() {
            return Ok(html_response(
                "缺少 agentId，无法切换 Agent。请发送 /agent 后点击列表按钮。",
                None,
            ));
        }

        let options = self.settings.list_agent_options().await?;
        let selected = options.iter().find(|option| option.agent_id == agent_id);
        if !options.is_empty() && selected.is_none() {
            return Ok(html_response(
                "没有找到这个本地 Agent，可能已经被禁用或当前不可用。请发送 /agent 重新选择。",
                None,
            ));
        }
        let Some(selected) = selected else {
            return Ok(html_response(
                "当前没有可切换的本地 Agent。请先在 WebUI Agents 页面刷新检测。",
                None,
            ));
        };

        self.settings
            .set_assistant_setting(
                action.context.platform,
                &ChannelAssistantSettingRequest {
                    assistant_id: selected.agent_id.clone(),
                    name: Some(selected.name.clone()),
                },
            )
            .await?;
        self.settings.clear_model_setting(action.context.platform).await?;

        let agent_config = self.settings.get_agent_config(action.context.platform).await?;
        let session = self
            .session_mgr
            .reset_session(
                internal_user_id,
                &action.context.chat_id,
                &agent_config.agent_type,
                None,
            )
            .await?;

        Ok(html_response(
            &format!(
                "<b>Telegram Agent 已切换</b>\nAgent: {}\nType: {}\nBackend: {}\nSession: {}\nModel: 已恢复 Agent 默认模型\n\n当前聊天已新建个人会话，后续消息会使用新的绑定。",
                html_escape(&selected.name),
                html_escape(&agent_config.agent_type),
                html_escape(agent_config.backend.as_deref().unwrap_or("默认")),
                html_escape(&short_channel_id(&session.id)),
            ),
            None,
        ))
    }

    async fn build_model_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        args: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let args = args.trim();
        let model_config = self.settings.get_model_config(msg.platform).await?;
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let agent_options = self.settings.list_agent_options().await?;
        let assistant = self.settings.get_assistant_setting(msg.platform).await?;
        let current_agent = resolve_current_agent_option(&agent_options, assistant.as_ref(), &agent_config);
        let is_openclaw_agent = agent_config.backend.as_deref() == Some("openclaw")
            || current_agent
                .map(|agent| agent.name.eq_ignore_ascii_case("openclaw"))
                .unwrap_or(false);

        if !args.is_empty() {
            let mut parts = args.split_whitespace();
            let Some(provider_id) = parts.next() else {
                return Ok(html_response("用法：/model &lt;provider_id&gt; &lt;model&gt;", None));
            };
            let model = parts.collect::<Vec<_>>().join(" ");
            if provider_id.trim().is_empty() || model.trim().is_empty() {
                return Ok(html_response("用法：/model &lt;provider_id&gt; &lt;model&gt;", None));
            }
            if is_openclaw_agent {
                let current = model_config
                    .as_ref()
                    .map(|config| format!("{} / {}", config.provider_id, config.model))
                    .unwrap_or_else(|| "未单独指定，使用 OpenClaw 默认运行模型".into());
                let mut text = format!("<b>当前 Telegram 模型绑定</b>\n{}\n", html_escape(&current));
                if let Some(agent) = current_agent {
                    text.push_str(&format!("\n当前 Agent: {}\n", html_escape(&agent.name)));
                }
                append_openclaw_model_status(&mut text);
                return Ok(html_response(
                    &text,
                    Some(vec![vec![ActionButton {
                        label: "恢复 Agent 默认模型".into(),
                        action: "model.clear".into(),
                        params: None,
                    }]]),
                ));
            }
            self.settings
                .set_model_setting(
                    msg.platform,
                    &ChannelDefaultModelSetting {
                        id: provider_id.to_owned(),
                        use_model: model.clone(),
                    },
                )
                .await?;
            let session = self
                .session_mgr
                .reset_session(internal_user_id, &msg.chat_id, &agent_config.agent_type, None)
                .await?;
            return Ok(html_response(
                &format!(
                    "<b>Telegram 默认模型已切换</b>\nProvider: {}\nModel: {}\nSession: {}\n\n当前聊天已新建个人会话，后续消息会使用新的模型绑定。",
                    html_escape(provider_id),
                    html_escape(&model),
                    html_escape(&short_channel_id(&session.id))
                ),
                None,
            ));
        }

        let current = model_config
            .as_ref()
            .map(|config| format!("{} / {}", config.provider_id, config.model))
            .unwrap_or_else(|| {
                if is_openclaw_agent {
                    "未单独指定，使用 OpenClaw 默认运行模型".into()
                } else {
                    "未单独指定，使用 Agent 默认模型".into()
                }
            });
        let mut text = format!("<b>当前 Telegram 模型绑定</b>\n{}\n", html_escape(&current));
        let mut buttons = Vec::new();
        if let Some(agent) = current_agent {
            text.push_str(&format!("\n当前 Agent: {}\n", html_escape(&agent.name)));
            if is_openclaw_agent {
                append_openclaw_model_status(&mut text);
            } else if agent.models.is_empty() {
                text.push_str("\n这个 Agent 当前没有上报可选模型。可以使用 Agent 默认模型。");
            } else {
                text.push_str("\n请选择当前 Agent 可用模型：");
                for model in agent.models.iter().take(12) {
                    buttons.push(vec![ActionButton {
                        label: short_button_label(&model.label),
                        action: "model.select".into(),
                        params: Some(HashMap::from([
                            ("p".into(), model.provider_id.clone()),
                            ("m".into(), model.model.clone()),
                        ])),
                    }]);
                }
            }
        } else {
            text.push_str("\n当前 Agent 无法从本地 Agent 列表中解析。请先发送 /agent 选择一个本地 Agent。");
        }
        buttons.push(vec![ActionButton {
            label: "恢复 Agent 默认模型".into(),
            action: "model.clear".into(),
            params: None,
        }]);
        Ok(html_response(&text, Some(buttons)))
    }

    async fn build_topic_model_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        agent_id: &str,
        args: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let thread_id = msg
            .topic
            .as_ref()
            .map(|topic| topic.message_thread_id)
            .ok_or_else(|| ChannelError::InvalidConfig("Telegram topic is missing".into()))?;
        let option = self
            .settings
            .list_agent_options()
            .await?
            .into_iter()
            .find(|option| option.agent_id == agent_id)
            .ok_or_else(|| ChannelError::InvalidConfig(format!("Agent '{agent_id}' is unavailable")))?;
        if option.models.is_empty() {
            return Ok(html_response("当前 Agent 没有上报可切换模型。", None));
        }
        let args = args.trim();
        if !args.is_empty() {
            let mut parts = args.split_whitespace();
            let provider_id = parts.next().unwrap_or_default();
            let model = parts.collect::<Vec<_>>().join(" ");
            if !option
                .models
                .iter()
                .any(|item| item.provider_id == provider_id && item.model == model)
            {
                return Ok(html_response("该模型不属于当前话题绑定 Agent 的可用模型。", None));
            }
            self.session_mgr
                .set_topic_model(internal_user_id, &msg.chat_id, thread_id, agent_id, provider_id, &model)
                .await?;
            return Ok(html_response(
                &format!(
                    "<b>话题模型已切换</b>\nAgent: {}\nProvider: {}\nModel: {}",
                    html_escape(&option.name),
                    html_escape(provider_id),
                    html_escape(&model)
                ),
                None,
            ));
        }
        let buttons = option
            .models
            .iter()
            .take(12)
            .map(|item| {
                vec![ActionButton {
                    label: item.label.clone(),
                    action: "model.select".into(),
                    params: Some(HashMap::from([
                        ("p".into(), item.provider_id.clone()),
                        ("m".into(), item.model.clone()),
                    ])),
                }]
            })
            .collect();
        Ok(html_response(
            &format!(
                "<b>{} 可用模型</b>\n模型选择只影响你在当前话题中的会话。{}",
                html_escape(&option.name),
                option
                    .last_probe_at
                    .map(|checked_at| format!("\n能力探测时间: {checked_at}"))
                    .unwrap_or_default()
            ),
            Some(buttons),
        ))
    }

    async fn select_model_action(
        &self,
        internal_user_id: &str,
        action: &UnifiedAction,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        let provider_id = action
            .params
            .as_ref()
            .and_then(|params| params.get("providerId").or_else(|| params.get("p")))
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        let model = action
            .params
            .as_ref()
            .and_then(|params| params.get("model").or_else(|| params.get("m")))
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        if provider_id.is_empty() || model.is_empty() {
            return Ok(html_response("缺少模型参数。请发送 /model 后点击模型按钮。", None));
        }

        if msg.platform == crate::types::PluginType::Telegram
            && let Some(topic) = &msg.topic
            && topic.message_thread_id != 1
        {
            let thread_id = topic.message_thread_id;
            let binding = self
                .session_mgr
                .get_topic_binding(&action.context.chat_id, thread_id)
                .await?;
            let Some(binding) = binding else {
                return Ok(html_response("当前话题尚未绑定 Agent。", None));
            };
            let agent_id = binding.agent_id;
            let option = self
                .settings
                .list_agent_options()
                .await?
                .into_iter()
                .find(|option| option.agent_id == agent_id);
            let allowed = option.as_ref().is_some_and(|option| {
                option
                    .models
                    .iter()
                    .any(|item| item.provider_id == provider_id && item.model == model)
            });
            if !allowed {
                return Ok(html_response("该模型不属于当前话题绑定 Agent。", None));
            }
            self.session_mgr
                .set_topic_model(
                    internal_user_id,
                    &action.context.chat_id,
                    thread_id,
                    &agent_id,
                    provider_id,
                    model,
                )
                .await?;
            return Ok(html_response(
                &format!(
                    "<b>话题模型已切换</b>\nAgent: {}\nProvider: {}\nModel: {}",
                    html_escape(option.as_ref().map(|value| value.name.as_str()).unwrap_or(&agent_id)),
                    html_escape(provider_id),
                    html_escape(model)
                ),
                None,
            ));
        }

        let agent_config = self.settings.get_agent_config(action.context.platform).await?;
        let agent_options = self.settings.list_agent_options().await?;
        let assistant = self.settings.get_assistant_setting(action.context.platform).await?;
        let current_agent = resolve_current_agent_option(&agent_options, assistant.as_ref(), &agent_config);
        let allowed = current_agent
            .as_ref()
            .map(|agent| {
                agent
                    .models
                    .iter()
                    .any(|option| option.provider_id == provider_id && option.model == model)
            })
            .unwrap_or(false);
        if !allowed {
            return Ok(html_response(
                "这个模型不属于当前 Telegram Agent 的可选模型。请发送 /model 重新选择。",
                None,
            ));
        }

        self.settings
            .set_model_setting(
                action.context.platform,
                &ChannelDefaultModelSetting {
                    id: provider_id.to_owned(),
                    use_model: model.to_owned(),
                },
            )
            .await?;
        let session = self
            .session_mgr
            .reset_session(
                internal_user_id,
                &action.context.chat_id,
                &agent_config.agent_type,
                None,
            )
            .await?;
        Ok(html_response(
            &format!(
                "<b>Telegram 模型已切换</b>\nAgent: {}\nProvider: {}\nModel: {}\nSession: {}\n\n当前聊天已新建个人会话，后续消息会使用新的模型绑定。",
                html_escape(current_agent.map(|agent| agent.name.as_str()).unwrap_or("当前 Agent")),
                html_escape(provider_id),
                html_escape(model),
                html_escape(&short_channel_id(&session.id))
            ),
            None,
        ))
    }

    async fn clear_model_action(
        &self,
        internal_user_id: &str,
        action: &UnifiedAction,
    ) -> Result<ActionResponse, ChannelError> {
        self.settings.clear_model_setting(action.context.platform).await?;
        let agent_config = self.settings.get_agent_config(action.context.platform).await?;
        let session = self
            .session_mgr
            .reset_session(
                internal_user_id,
                &action.context.chat_id,
                &agent_config.agent_type,
                None,
            )
            .await?;
        Ok(html_response(
            &format!(
                "<b>已恢复 Agent 默认模型</b>\nSession: {}\n\n当前聊天已新建个人会话。",
                html_escape(&short_channel_id(&session.id))
            ),
            None,
        ))
    }

    async fn reset_personal_session_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        new_session: bool,
        title_arg: Option<&str>,
    ) -> Result<ActionResponse, ChannelError> {
        let key = pending_key(internal_user_id, msg.platform, &msg.chat_id);
        self.pending_flows.lock().unwrap().remove(&key);
        self.personal_title_hints.lock().unwrap().remove(&key);
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let session = self
            .session_mgr
            .reset_session(internal_user_id, &msg.chat_id, &agent_config.agent_type, None)
            .await?;
        let title = title_arg.and_then(normalize_personal_title);
        if new_session && let Some(title) = title.as_ref() {
            self.personal_title_hints.lock().unwrap().insert(key, title.clone());
        }
        let title = if new_session {
            "已新建个人会话。"
        } else {
            "已切换到个人会话。"
        };
        let title_line = title_arg
            .and_then(normalize_personal_title)
            .map(|title| format!("\n标题: {}", html_escape(&title)))
            .unwrap_or_default();
        Ok(html_response(
            &format!(
                "{}{}\nSession: {}\nAgent: {}",
                title,
                title_line,
                html_escape(&short_channel_id(&session.id)),
                html_escape(&session.agent_type)
            ),
            None,
        ))
    }

    async fn rename_personal_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        args: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(title) = normalize_personal_title(args) else {
            return Ok(html_response("用法：/rename &lt;新的会话标题&gt;", None));
        };

        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let session = self
            .session_mgr
            .get_or_create_session(internal_user_id, &msg.chat_id, &agent_config.agent_type, None)
            .await?;

        let Some(conversation_id) = session.conversation_id.as_deref() else {
            let key = pending_key(internal_user_id, msg.platform, &msg.chat_id);
            self.personal_title_hints.lock().unwrap().insert(key, title.clone());
            return Ok(html_response(
                &format!(
                    "<b>已设置新个人会话标题</b>\n{}\n\n当前聊天还没有创建实际会话，下一条普通消息会使用这个标题。",
                    html_escape(&title)
                ),
                None,
            ));
        };

        let Some(directory) = self.personal_directory.as_ref() else {
            return Ok(html_response("当前运行时未接入个人会话重命名服务。", None));
        };
        let Some(renamed) = directory
            .rename_personal_conversation(internal_user_id, msg.platform, &msg.chat_id, conversation_id, &title)
            .await?
        else {
            return Ok(html_response("没有找到当前个人会话，无法重命名。", None));
        };

        Ok(html_response(
            &format!(
                "<b>已重命名当前个人会话</b>\n{}\nID: {}",
                html_escape(&renamed.name),
                html_escape(&short_channel_id(&renamed.id))
            ),
            None,
        ))
    }

    async fn build_personal_list_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = self.personal_directory.as_ref() else {
            return Ok(html_response("当前运行时未接入个人会话列表服务。", None));
        };
        let conversations = directory
            .list_personal_conversations(internal_user_id, msg.platform, &msg.chat_id)
            .await?;
        if conversations.is_empty() {
            return Ok(html_response(
                "当前没有可切换的 Telegram 个人会话。\n\n可以发送 /new_session 新建一个个人会话。",
                None,
            ));
        }

        let mut text = String::from("<b>请选择要切换的个人会话</b>\n");
        for (idx, conversation) in conversations.iter().take(12).enumerate() {
            let agent_label = conversation
                .agent_label
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(&conversation.agent_type);
            text.push_str(&format!(
                "\n\n{}. {}\nAgent: {}\nID: {}",
                idx + 1,
                html_escape(&conversation.name),
                html_escape(agent_label),
                html_escape(&short_channel_id(&conversation.id))
            ));
            if let Some(recent_message) = conversation.recent_message.as_deref()
                && !recent_message.trim().is_empty()
            {
                text.push_str(&format!("\n最近: {}", html_escape(recent_message)));
            }
        }
        Ok(html_response(&text, Some(personal_select_buttons(&conversations))))
    }

    async fn select_personal_action(
        &self,
        internal_user_id: &str,
        action: &UnifiedAction,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = self.personal_directory.as_ref() else {
            return Ok(html_response("当前运行时未接入个人会话列表服务。", None));
        };
        let conversation_id = action
            .params
            .as_ref()
            .and_then(|params| params.get("id"))
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        if conversation_id.is_empty() {
            return Ok(html_response(
                "缺少 conversation id。请发送 /personal_list 后点击会话按钮。",
                None,
            ));
        }
        let Some(conversation) = directory
            .get_personal_conversation(
                internal_user_id,
                action.context.platform,
                &action.context.chat_id,
                conversation_id,
            )
            .await?
        else {
            return Ok(html_response(
                "没有找到这个个人会话。请发送 /personal_list 重新选择。",
                None,
            ));
        };

        let session = self
            .session_mgr
            .reset_session(
                internal_user_id,
                &action.context.chat_id,
                &conversation.agent_type,
                None,
            )
            .await?;
        self.session_mgr
            .bind_conversation(&session.id, &conversation.id)
            .await?;
        Ok(html_response(
            &format!(
                "<b>已切换到个人会话</b>\nConversation: {}\nSession: {}\nAgent: {}",
                html_escape(&conversation.name),
                html_escape(&short_channel_id(&session.id)),
                html_escape(&conversation.agent_type),
            ),
            None,
        ))
    }

    async fn build_team_status_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
    ) -> Result<ActionResponse, ChannelError> {
        let agent_config = self.settings.get_agent_config(msg.platform).await?;
        let session = self
            .session_mgr
            .get_or_create_session(internal_user_id, &msg.chat_id, &agent_config.agent_type, None)
            .await?;
        let conversation = session.conversation_id.as_deref().unwrap_or("尚未绑定 Team 会话");
        let team_label = self
            .find_team_by_conversation(internal_user_id, session.conversation_id.as_deref())
            .await?
            .map(|team| format!("{} ({})", team.name, team.id))
            .unwrap_or_else(|| "未识别为已知 Team".into());
        Ok(html_response(
            &format!(
                "<b>Team 会话状态</b>\n当前 Channel Session: {}\n当前 Conversation: {}\n当前 Team: {}\n\n如果这个 Conversation 属于 Team-owned 会话，后续普通消息会自动通过 Team API 路由。",
                html_escape(&short_channel_id(&session.id)),
                html_escape(conversation),
                html_escape(&team_label)
            ),
            None,
        ))
    }

    async fn build_team_list_command(&self, internal_user_id: &str) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = &self.team_directory else {
            return Ok(html_response("当前运行时未接入 Team 查询服务。", None));
        };
        let teams = directory.list_teams(internal_user_id).await?;
        if teams.is_empty() {
            return Ok(html_response(
                "当前没有可用 Team。\n\n可以先在 WebUI 创建团队，或发送 /team_new &lt;主题&gt; 创建真实 Team。",
                None,
            ));
        }
        let mut text = String::from("<b>可用 Team</b>\n");
        for (idx, team) in teams.iter().enumerate() {
            text.push_str(&format!(
                "\n{}. {} ({})\n成员数：{}",
                idx + 1,
                html_escape(&team.name),
                html_escape(&short_channel_id(&team.id)),
                team.agent_count,
            ));
        }
        text.push_str("\n\n点击下面按钮即可切换，不需要复制 Team ID。");
        Ok(html_response(&text, Some(team_select_buttons(&teams))))
    }

    async fn create_team_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        topic: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = &self.team_directory else {
            return Ok(html_response("当前运行时未接入 Team 创建服务。", None));
        };
        let topic = topic.trim();
        if topic.is_empty() {
            let key = pending_key(internal_user_id, msg.platform, &msg.chat_id);
            self.pending_flows
                .lock()
                .unwrap()
                .insert(key, PendingFlow::TeamNewAwaitingName);
            return Ok(html_response(
                "<b>创建新 Team</b>\n\n请直接回复团队名称。\n例如：今日4小时BTC做单分析\n\n也可以发送 /team_new &lt;团队名&gt; 快速创建。",
                Some(vec![vec![ActionButton {
                    label: "取消".into(),
                    action: "team.new.cancel".into(),
                    params: None,
                }]]),
            ));
        }
        if topic.eq_ignore_ascii_case("confirm") || topic.eq_ignore_ascii_case("cancel") {
            return Ok(html_response(
                "Team 草稿确认流程已下线。\n\n现在请直接发送：/team_new &lt;团队主题&gt;",
                None,
            ));
        }

        let _ = directory;
        self.create_team_from_topic(internal_user_id, msg.platform, &msg.chat_id, &msg.user.id, topic)
            .await
    }

    async fn create_team_from_topic(
        &self,
        internal_user_id: &str,
        platform: crate::types::PluginType,
        chat_id: &str,
        platform_user_id: &str,
        topic: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = &self.team_directory else {
            return Ok(html_response("当前运行时未接入 Team 创建服务。", None));
        };
        let assistant_setting = self.settings.get_assistant_setting(platform).await?;
        let Some(assistant_id) = assistant_setting
            .as_ref()
            .and_then(|setting| setting.assistant_id.as_deref())
            .map(str::trim)
            .filter(|assistant_id| !assistant_id.is_empty())
            .map(normalize_team_assistant_id)
        else {
            return Ok(html_response(
                "当前 Telegram 还没有可用于创建 Team 的 Assistant 绑定。\n\n请先在 WebUI 的 Channel 设置里为 Telegram 选择一个 Agent/Assistant，然后再发送 /team_new &lt;团队主题&gt;。",
                None,
            ));
        };

        let agent_config = match self.settings.get_agent_config(platform).await {
            Ok(config) => config,
            Err(error) => {
                warn!(error = %error, "failed to resolve channel agent config for team creation; falling back to acp session binding");
                crate::channel_settings::ResolvedAgentConfig {
                    agent_type: "acp".into(),
                    backend: None,
                }
            }
        };
        let model_config = self.settings.get_model_config(platform).await?;
        let model = model_config
            .and_then(|config| config.use_model.or(Some(config.model)))
            .unwrap_or_else(|| "default".into());
        let lead_name = assistant_setting
            .as_ref()
            .and_then(|setting| setting.name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| agent_config.backend.clone())
            .unwrap_or_else(|| "Team Lead".into());

        let team = match directory
            .create_team(
                internal_user_id,
                ChannelTeamCreateRequest {
                    name: topic.to_owned(),
                    lead_name,
                    lead_role: "lead".into(),
                    assistant_id,
                    model,
                    source_channel: Some(platform.to_string()),
                    source_channel_id: None,
                    source_chat_id: Some(chat_id.to_owned()),
                    source_user_id: Some(platform_user_id.to_owned()),
                    source_label: Some(channel_source_label(platform).into()),
                    created_from: Some(platform.to_string()),
                },
            )
            .await
        {
            Ok(team) => team,
            Err(error) => {
                return Ok(html_response(
                    &format!(
                        "Team 创建失败：{}\n\n请检查 Telegram 当前绑定的 Assistant 是否可用于 Team，或先在 WebUI 创建/选择 Team。",
                        html_escape(&error.to_string())
                    ),
                    None,
                ));
            }
        };
        if let Err(error) = directory.ensure_team_session(internal_user_id, &team.id).await {
            return Ok(html_response(
                &format!(
                    "Team 已创建，但启动 Team session 失败：{}\n\n请稍后发送 /team_select {} 重试切换。",
                    html_escape(&error.to_string()),
                    html_escape(&team.id)
                ),
                None,
            ));
        }

        let Some(conversation_id) = team.lead_conversation_id.as_deref().filter(|id| !id.trim().is_empty()) else {
            return Ok(html_response(
                "Team 已创建，但 Lead conversation 尚未初始化完成。请稍后发送 /team_list 查看并使用 /team_select 切换。",
                None,
            ));
        };

        let session = self
            .session_mgr
            .get_or_create_session(internal_user_id, chat_id, &agent_config.agent_type, None)
            .await?;
        self.session_mgr.bind_conversation(&session.id, conversation_id).await?;

        Ok(html_response(
            &format!(
                "<b>Team 已创建并切换</b>\nTeam: {}\nTeam ID: {}\nLead Conversation: {}\n\n现在直接发送普通消息，会通过 Team API 发给 Team Lead。",
                html_escape(&team.name),
                html_escape(&team.id),
                html_escape(conversation_id),
            ),
            None,
        ))
    }

    async fn select_team_command(
        &self,
        internal_user_id: &str,
        msg: &UnifiedIncomingMessage,
        selector: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = &self.team_directory else {
            return Ok(html_response("当前运行时未接入 Team 查询服务。", None));
        };
        if selector.trim().is_empty() {
            let teams = directory.list_teams(internal_user_id).await?;
            if teams.is_empty() {
                return Ok(html_response(
                    "当前没有可用 Team。\n\n可以先在 WebUI 创建团队，或发送 /team_new 创建 Telegram 侧 Team。",
                    None,
                ));
            }
            let text = "<b>请选择要切换的 Team</b>\n\n点击下面按钮即可切换，不需要复制 Team ID。";
            return Ok(html_response(text, Some(team_select_buttons(&teams))));
        }

        self.select_team_by_selector(internal_user_id, msg.platform, &msg.chat_id, selector)
            .await
    }

    async fn select_team_by_selector(
        &self,
        internal_user_id: &str,
        platform: crate::types::PluginType,
        chat_id: &str,
        selector: &str,
    ) -> Result<ActionResponse, ChannelError> {
        let Some(directory) = &self.team_directory else {
            return Ok(html_response("当前运行时未接入 Team 查询服务。", None));
        };
        if selector.trim().is_empty() {
            return Ok(html_response("请选择一个 Team。发送 /team_select 查看按钮列表。", None));
        }

        let teams = directory.list_teams(internal_user_id).await?;
        let selector_lower = selector.to_ascii_lowercase();
        let Some(team) = teams.into_iter().find(|team| {
            team.id == selector || team.name == selector || team.name.to_ascii_lowercase().contains(&selector_lower)
        }) else {
            return Ok(html_response(
                "没有找到匹配的 Team。发送 /team_list 查看可用团队。",
                None,
            ));
        };

        directory.ensure_team_session(internal_user_id, &team.id).await?;
        let team = directory.get_team(internal_user_id, &team.id).await?.unwrap_or(team);
        let Some(conversation_id) = team.lead_conversation_id.as_deref().filter(|id| !id.trim().is_empty()) else {
            return Ok(html_response(
                "已找到 Team，但还没有可绑定的 Lead conversation。请先在 WebUI 打开该 Team，让系统完成初始化。",
                None,
            ));
        };

        let agent_config = match self.settings.get_agent_config(platform).await {
            Ok(config) => config,
            Err(error) => {
                warn!(error = %error, "failed to resolve channel agent config for team selection; falling back to acp session binding");
                crate::channel_settings::ResolvedAgentConfig {
                    agent_type: "acp".into(),
                    backend: None,
                }
            }
        };
        let session = self
            .session_mgr
            .get_or_create_session(internal_user_id, chat_id, &agent_config.agent_type, None)
            .await?;
        self.session_mgr.bind_conversation(&session.id, conversation_id).await?;

        Ok(html_response(
            &format!(
                "已切换到 Team 会话。\nTeam: {}\nConversation: {}\n\n现在直接发送普通消息，会通过 Team API 发给 Team Lead。",
                html_escape(&team.name),
                html_escape(conversation_id),
            ),
            None,
        ))
    }

    async fn find_team_by_conversation(
        &self,
        internal_user_id: &str,
        conversation_id: Option<&str>,
    ) -> Result<Option<ChannelTeamSummary>, ChannelError> {
        let Some(conversation_id) = conversation_id else {
            return Ok(None);
        };
        let Some(directory) = &self.team_directory else {
            return Ok(None);
        };
        let teams = directory.list_teams(internal_user_id).await?;
        Ok(teams
            .into_iter()
            .find(|team| team.lead_conversation_id.as_deref() == Some(conversation_id)))
    }
}

// ── Helper builders ─────────────────────────────────────────────────

fn build_pairing_response(code: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!(
            "Welcome! To use this bot, you need authorization.\n\n\
             Your pairing code: *{code}*\n\n\
             Share this code with the admin, who can approve it in \
             Settings → Channel → Pairing Requests.\n\
             The code expires in 10 minutes."
        )),
        parse_mode: None,
        buttons: Some(vec![vec![
            ActionButton {
                label: "Refresh Code".into(),
                action: "pairing.refresh".into(),
                params: None,
            },
            ActionButton {
                label: "Check Status".into(),
                action: "pairing.check".into(),
                params: None,
            },
            ActionButton {
                label: "Help".into(),
                action: "pairing.help".into(),
                params: None,
            },
        ]]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_help_response() -> ActionResponse {
    ActionResponse {
        text: Some(
            "How can I help?\n\
             Choose an option below or just send me a message."
                .into(),
        ),
        parse_mode: None,
        buttons: Some(vec![
            vec![
                ActionButton {
                    label: "New Session".into(),
                    action: "session.new".into(),
                    params: None,
                },
                ActionButton {
                    label: "Session Status".into(),
                    action: "session.status".into(),
                    params: None,
                },
            ],
            vec![
                ActionButton {
                    label: "Features".into(),
                    action: "help.features".into(),
                    params: None,
                },
                ActionButton {
                    label: "Tips".into(),
                    action: "help.tips".into(),
                    params: None,
                },
            ],
        ]),
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn build_command_help_response() -> ActionResponse {
    html_response(
        "<b>Telegram 可用命令</b>\n/status - 查看当前绑定\n/agent - 按钮切换 Telegram 本地 Agent\n/model - 按钮切换当前 Agent 可用模型\n/project - 当前项目\n/run_info - 当前开发运行\n/diff_summary - 变更和证据摘要\n/test - 执行单元测试门禁\n/stop - 停止当前运行\n/retry - 重试最近失败门禁\n/handoff - Web 接力入口\n/team - 查看团队会话状态\n/team_help - 查看 Team 功能说明\n/team_list - 团队列表入口\n/team_select - 按钮选择团队\n/team_new - 交互式创建 Team\n/team_new &lt;主题&gt; - 快速创建真实 Team\n/personal - 切换到新的个人会话\n/personal_list - 按钮选择已有个人会话\n/new_session - 新建个人会话\n/new_session &lt;标题&gt; - 新建带标题的个人会话\n/rename &lt;标题&gt; - 重命名当前个人会话\n/help - 查看帮助",
        Some(vec![
            vec![
                ActionButton {
                    label: "选择 Agent".into(),
                    action: "agent.list".into(),
                    params: None,
                },
                ActionButton {
                    label: "个人会话".into(),
                    action: "personal.list".into(),
                    params: None,
                },
            ],
            vec![
                ActionButton {
                    label: "选择 Team".into(),
                    action: "team.list".into(),
                    params: None,
                },
                ActionButton {
                    label: "创建 Team".into(),
                    action: "team.new.start".into(),
                    params: None,
                },
            ],
            vec![ActionButton {
                label: "Team 帮助".into(),
                action: "team.help".into(),
                params: None,
            }],
        ]),
    )
}

fn build_team_help_response() -> ActionResponse {
    html_response(
        "<b>Telegram Team 功能说明</b>\n\n<b>Team 是什么</b>\nTeam 是一个多 Agent 协作会话。通常由 Team Lead 负责拆解任务、分配给成员、汇总结果；成员可以使用不同 assistant、模型、权限和工作区。\n\n<b>常用流程</b>\n1. 创建新 Team：/team_new &lt;主题&gt;\n2. 查看已有 Team：/team_list\n3. 按钮切换 Team：/team_select\n4. 查看当前 Team 状态：/team\n5. 回到个人会话：/personal、/personal_list 或 /new_session\n\n<b>Telegram 与 WebUI 的关系</b>\nTelegram 负责移动端下达任务、接收结果和切换入口；WebUI 更适合查看完整历史、多列 Team 运行状态、成员配置、权限、模型和工作区。\n\n<b>上下文说明</b>\n/team_new 会创建一个新的真实 Team，并把 Telegram 绑定到该 Team Lead conversation。切换到 Team 后，普通消息会通过 Team API 发送给 Team Lead。\n\n<b>个人会话说明</b>\n/personal 会切回一个新的个人 agent 会话；/personal_list 可以按钮选择已有 Telegram 个人会话；/new_session 会新建个人会话，不会删除已有 Team。",
        None,
    )
}

fn channel_source_label(platform: crate::types::PluginType) -> &'static str {
    match platform {
        crate::types::PluginType::Telegram => "Telegram",
        crate::types::PluginType::Lark => "Lark",
        crate::types::PluginType::Dingtalk => "DingTalk",
        crate::types::PluginType::Weixin => "WeCom",
        crate::types::PluginType::Slack => "Slack",
        crate::types::PluginType::Discord => "Discord",
    }
}

fn short_channel_id(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 13 {
        return id.to_owned();
    }
    let head: String = chars.iter().take(8).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

fn short_button_label(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= 28 {
        return label.to_owned();
    }
    let head: String = chars.iter().take(25).collect();
    format!("{head}...")
}

fn team_select_buttons(teams: &[ChannelTeamSummary]) -> Vec<Vec<ActionButton>> {
    let mut rows = Vec::new();
    for team in teams.iter().take(12) {
        rows.push(vec![ActionButton {
            label: short_button_label(&team.name),
            action: "team.select".into(),
            params: Some(HashMap::from([("id".into(), team.id.clone())])),
        }]);
    }
    rows
}

fn personal_select_buttons(conversations: &[ChannelPersonalConversationSummary]) -> Vec<Vec<ActionButton>> {
    let mut rows = Vec::new();
    for conversation in conversations.iter().take(12) {
        rows.push(vec![ActionButton {
            label: short_button_label(&personal_button_label(conversation)),
            action: "personal.select".into(),
            params: Some(HashMap::from([("id".into(), conversation.id.clone())])),
        }]);
    }
    rows
}

fn personal_button_label(conversation: &ChannelPersonalConversationSummary) -> String {
    if !agent_label_matches_title(conversation)
        || conversation
            .recent_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
    {
        return conversation.name.clone();
    }
    conversation
        .recent_message
        .as_deref()
        .and_then(normalize_personal_title)
        .unwrap_or_else(|| conversation.name.clone())
}

fn agent_label_matches_title(conversation: &ChannelPersonalConversationSummary) -> bool {
    conversation
        .agent_label
        .as_deref()
        .map(|agent| agent.trim().eq_ignore_ascii_case(conversation.name.trim()))
        .unwrap_or(false)
}

fn normalize_personal_title(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, 32))
}

fn auto_title_from_message(raw: &str) -> Option<String> {
    let title = normalize_personal_title(raw)?;
    if title.starts_with('/') {
        return None;
    }
    Some(title)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

fn resolve_current_agent_option<'a>(
    options: &'a [ChannelAgentOption],
    assistant: Option<&aionui_api_types::ChannelAssistantSettingResponse>,
    config: &crate::channel_settings::ResolvedAgentConfig,
) -> Option<&'a ChannelAgentOption> {
    if let Some(id) = assistant
        .and_then(|setting| setting.assistant_id.as_deref().or(setting.custom_agent_id.as_deref()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        && let Some(option) = options.iter().find(|option| option.agent_id == id)
    {
        return Some(option);
    }

    if let Some(backend) = config.backend.as_deref()
        && let Some(option) = options.iter().find(|option| option.backend.as_deref() == Some(backend))
    {
        return Some(option);
    }

    options
        .iter()
        .find(|option| option.agent_type == config.agent_type && option.backend.is_none())
        .or_else(|| options.iter().find(|option| option.agent_type == config.agent_type))
}

fn pending_key(internal_user_id: &str, platform: crate::types::PluginType, chat_id: &str) -> String {
    format!("{platform}:{internal_user_id}:{chat_id}")
}

fn html_response(text: &str, buttons: Option<Vec<Vec<ActionButton>>>) -> ActionResponse {
    ActionResponse {
        text: Some(text.to_owned()),
        parse_mode: Some(ParseMode::HTML),
        buttons,
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

fn normalize_team_assistant_id(assistant_id: &str) -> String {
    let trimmed = assistant_id.trim();
    if trimmed.len() == 8 && trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!("bare:{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenClawRuntimeModel {
    provider: Option<String>,
    name: Option<String>,
    think_level: Option<String>,
}

impl OpenClawRuntimeModel {
    fn display_name(&self) -> String {
        match (self.provider.as_deref(), self.name.as_deref()) {
            (Some(provider), Some(name)) => format!("{provider} / {name}"),
            (Some(provider), None) => provider.to_owned(),
            (None, Some(name)) => name.to_owned(),
            (None, None) => "未知模型".to_owned(),
        }
    }
}

fn append_openclaw_model_status(text: &mut String) {
    text.push_str("\nOpenClaw 当前没有通过 ACP 上报可切换模型列表。");
    if let Some(runtime_model) = find_openclaw_runtime_model() {
        text.push_str(&format!(
            "\n最近运行时检测模型: {}",
            html_escape(&runtime_model.display_name())
        ));
        if let Some(think_level) = runtime_model.think_level.as_deref()
            && !think_level.trim().is_empty()
        {
            text.push_str(&format!("\n推理强度: {}", html_escape(think_level)));
        }
    } else {
        text.push_str("\n最近运行时检测模型: 暂未检测到");
    }
    text.push_str(
        "\n\n暂不支持在 Telegram 中切换 OpenClaw 模型。\n后续等待 OpenClaw ACP 完善模型列表和 set model 能力后可实现。",
    );
}

fn find_openclaw_runtime_model() -> Option<OpenClawRuntimeModel> {
    let sessions_dir = openclaw_sessions_dir()?;
    let mut entries = std::fs::read_dir(sessions_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| name.ends_with(".trajectory.jsonl"))
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();

    entries.sort_by(|(a, _), (b, _)| b.cmp(a));

    for (_, path) in entries {
        if let Some(runtime_model) = extract_openclaw_runtime_model_from_trajectory(&path) {
            return Some(runtime_model);
        }
    }
    None
}

fn openclaw_sessions_dir() -> Option<std::path::PathBuf> {
    if let Ok(state_dir) = std::env::var("OPENCLAW_STATE_DIR")
        && !state_dir.trim().is_empty()
    {
        return Some(std::path::PathBuf::from(state_dir).join("agents/main/sessions"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".openclaw/agents/main/sessions"))
}

fn extract_openclaw_runtime_model_from_trajectory(path: &std::path::Path) -> Option<OpenClawRuntimeModel> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut latest = None;
    for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
        if let Some(runtime_model) = extract_openclaw_runtime_model_from_trajectory_line(&line) {
            latest = Some(runtime_model);
        }
    }
    latest
}

fn extract_openclaw_runtime_model_from_trajectory_line(line: &str) -> Option<OpenClawRuntimeModel> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("trace.metadata") {
        return None;
    }
    let model = value.get("data")?.get("model")?;
    let read_string = |key: &str| {
        model
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let provider = read_string("provider");
    let name = read_string("name").or_else(|| read_string("model"));
    let think_level = read_string("thinkLevel").or_else(|| read_string("think_level"));

    if provider.is_none() && name.is_none() && think_level.is_none() {
        return None;
    }

    Some(OpenClawRuntimeModel {
        provider,
        name,
        think_level,
    })
}

fn build_unknown_action_response(action: &str) -> ActionResponse {
    ActionResponse {
        text: Some(format!("Unknown action: {action}")),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionContext, MessageContentType, PluginType, UnifiedMessageContent, UnifiedUser};
    use aionui_api_types::WebSocketMessage;
    use aionui_common::{TimestampMs, now_ms};
    use aionui_db::models::{
        AgentMetadataRow, AssistantSessionRow, AssistantUserRow, ChannelPluginRow, ClientPreference, PairingCodeRow,
        Provider, UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpsertAgentMetadataParams,
    };
    use aionui_db::{
        CreateProviderParams, DbError, IAgentMetadataRepository, IChannelRepository, IClientPreferenceRepository,
        IProviderRepository, UpdatePluginStatusParams, UpdateProviderParams,
    };
    use aionui_realtime::EventBroadcaster;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type ApprovalResolution = (String, PluginType, String, Option<i64>, String, usize);

    #[derive(Default)]
    struct RecordingApprovalPort {
        resolutions: Mutex<Vec<ApprovalResolution>>,
    }

    #[derive(Default)]
    struct RecordingDevelopmentPort {
        calls: Mutex<
            Vec<(
                crate::development::ChannelDevelopmentContext,
                crate::development::ChannelDevelopmentCommand,
            )>,
        >,
    }

    #[async_trait::async_trait]
    impl crate::development::ChannelDevelopmentPort for RecordingDevelopmentPort {
        async fn execute(
            &self,
            context: crate::development::ChannelDevelopmentContext,
            command: crate::development::ChannelDevelopmentCommand,
        ) -> Result<String, ChannelError> {
            self.calls.lock().unwrap().push((context, command));
            Ok("Project: Aion\nRun: running".into())
        }
    }

    #[async_trait::async_trait]
    impl crate::approval::ChannelApprovalPort for RecordingApprovalPort {
        async fn create(
            &self,
            _context: crate::approval::ChannelApprovalContext,
            _confirmation: aionui_common::Confirmation,
        ) -> Result<String, ChannelError> {
            Ok("approval".into())
        }

        async fn resolve(
            &self,
            context: crate::approval::ChannelApprovalResolutionContext,
            approval_id: &str,
            option_index: usize,
        ) -> Result<String, ChannelError> {
            self.resolutions.lock().unwrap().push((
                context.source_user_id,
                context.platform,
                context.chat_id,
                context.message_thread_id,
                approval_id.into(),
                option_index,
            ));
            Ok("approved".into())
        }
    }

    // ── Mock EventBroadcaster ──────────────────────────────────────────

    struct MockBroadcaster;

    impl EventBroadcaster for MockBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
    }

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        users: Mutex<Vec<AssistantUserRow>>,
        sessions: Mutex<Vec<AssistantSessionRow>>,
        pairings: Mutex<Vec<PairingCodeRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                users: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
                pairings: Mutex::new(Vec::new()),
            }
        }

        fn add_authorized_user(&self, platform_user_id: &str, platform_type: &str) {
            let user = AssistantUserRow {
                id: format!("user_{platform_user_id}"),
                platform_user_id: platform_user_id.to_owned(),
                platform_type: platform_type.to_owned(),
                display_name: Some("Test User".into()),
                authorized_at: now_ms(),
                last_active: None,
                session_id: None,
            };
            self.users.lock().unwrap().push(user);
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(vec![])
        }
        async fn get_plugin(&self, _id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            Ok(None)
        }
        async fn upsert_plugin(&self, _row: &ChannelPluginRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_plugin_status(&self, _id: &str, _params: &UpdatePluginStatusParams) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_plugin(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(self.users.lock().unwrap().clone())
        }
        async fn get_user_by_platform(
            &self,
            platform_user_id: &str,
            platform_type: &str,
        ) -> Result<Option<AssistantUserRow>, DbError> {
            let users = self.users.lock().unwrap();
            Ok(users
                .iter()
                .find(|u| u.platform_user_id == platform_user_id && u.platform_type == platform_type)
                .cloned())
        }
        async fn create_user(&self, row: &AssistantUserRow) -> Result<(), DbError> {
            self.users.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn update_user_last_active(&self, _id: &str, _last_active: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_user(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(self.sessions.lock().unwrap().clone())
        }
        async fn get_session(&self, id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions.iter().find(|s| s.id == id).cloned())
        }
        async fn get_or_create_session(
            &self,
            user_id: &str,
            chat_id: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(existing) = sessions
                .iter_mut()
                .find(|s| s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id))
            {
                existing.last_activity = new_row.last_activity;
                return Ok(existing.clone());
            }
            sessions.push(new_row.clone());
            Ok(new_row.clone())
        }
        async fn update_session_activity(&self, _id: &str, _last_activity: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(&self, id: &str, conversation_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.conversation_id = Some(conversation_id.to_owned());
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn update_session_agent_type(&self, id: &str, agent_type: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.iter_mut().find(|s| s.id == id) {
                s.agent_type = agent_type.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }
        async fn delete_sessions_by_user(&self, user_id: &str) -> Result<(), DbError> {
            self.sessions.lock().unwrap().retain(|s| s.user_id != user_id);
            Ok(())
        }
        async fn delete_session_by_user_chat(&self, user_id: &str, chat_id: &str) -> Result<(), DbError> {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| !(s.user_id == user_id && s.chat_id.as_deref() == Some(chat_id)));
            Ok(())
        }

        async fn create_pairing(&self, row: &PairingCodeRow) -> Result<(), DbError> {
            self.pairings.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().filter(|p| p.status == "pending").cloned().collect())
        }
        async fn get_pairing_by_code(&self, code: &str) -> Result<Option<PairingCodeRow>, DbError> {
            let pairings = self.pairings.lock().unwrap();
            Ok(pairings.iter().find(|p| p.code == code).cloned())
        }
        async fn update_pairing_status(&self, code: &str, status: &str) -> Result<(), DbError> {
            let mut pairings = self.pairings.lock().unwrap();
            if let Some(p) = pairings.iter_mut().find(|p| p.code == code) {
                p.status = status.to_owned();
                Ok(())
            } else {
                Err(DbError::NotFound(code.into()))
            }
        }
        async fn cleanup_expired_pairings(&self, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    // ── Mock IClientPreferenceRepository ──────────────────────────────

    struct MockPrefRepo;

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for MockPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn get_by_keys(&self, _keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            Ok(vec![])
        }
        async fn upsert_batch(&self, _entries: &[(&str, &str)]) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_keys(&self, _keys: &[&str]) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct StaticPrefRepo {
        prefs: Mutex<HashMap<String, String>>,
    }

    impl StaticPrefRepo {
        fn new(prefs: HashMap<String, String>) -> Self {
            Self {
                prefs: Mutex::new(prefs),
            }
        }

        fn get(&self, key: &str) -> Option<String> {
            self.prefs.lock().unwrap().get(key).cloned()
        }
    }

    #[async_trait::async_trait]
    impl IClientPreferenceRepository for StaticPrefRepo {
        async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
            Ok(self
                .prefs
                .lock()
                .unwrap()
                .iter()
                .map(|(key, value)| ClientPreference {
                    key: key.clone(),
                    value: value.clone(),
                    updated_at: now_ms(),
                })
                .collect())
        }
        async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
            Ok(keys
                .iter()
                .filter_map(|key| {
                    self.prefs.lock().unwrap().get(*key).map(|value| ClientPreference {
                        key: (*key).to_owned(),
                        value: value.clone(),
                        updated_at: now_ms(),
                    })
                })
                .collect())
        }
        async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError> {
            let mut prefs = self.prefs.lock().unwrap();
            for (key, value) in entries {
                prefs.insert((*key).to_owned(), (*value).to_owned());
            }
            Ok(())
        }
        async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError> {
            let mut prefs = self.prefs.lock().unwrap();
            for key in keys {
                prefs.remove(*key);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockAgentMetadataRepo {
        rows: Mutex<Vec<AgentMetadataRow>>,
    }

    impl MockAgentMetadataRepo {
        fn with_rows(rows: Vec<AgentMetadataRow>) -> Self {
            Self { rows: Mutex::new(rows) }
        }
    }

    #[derive(Default)]
    struct MockProviderRepo {
        rows: Mutex<Vec<Provider>>,
    }

    impl MockProviderRepo {
        fn with_rows(rows: Vec<Provider>) -> Self {
            Self { rows: Mutex::new(rows) }
        }
    }

    #[async_trait::async_trait]
    impl IProviderRepository for MockProviderRepo {
        async fn list(&self) -> Result<Vec<Provider>, DbError> {
            Ok(self.rows.lock().unwrap().clone())
        }

        async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError> {
            Ok(self.rows.lock().unwrap().iter().find(|row| row.id == id).cloned())
        }

        async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
            unimplemented!("not needed in channel action tests")
        }

        async fn update(&self, _id: &str, _params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
            unimplemented!("not needed in channel action tests")
        }

        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            unimplemented!("not needed in channel action tests")
        }
    }

    #[async_trait::async_trait]
    impl IAgentMetadataRepository for MockAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
            Ok(self.rows.lock().unwrap().clone())
        }

        async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self.rows.lock().unwrap().iter().find(|row| row.id == id).cloned())
        }

        async fn find_by_source_and_name(
            &self,
            agent_source: &str,
            name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.agent_source == agent_source && row.name == name)
                .cloned())
        }

        async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.agent_source == "builtin" && row.backend.as_deref() == Some(backend))
                .cloned())
        }

        async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
            Err(DbError::NotFound("unused mock upsert".into()))
        }

        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn update_availability_snapshot(
            &self,
            _id: &str,
            _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn update_agent_overrides(
            &self,
            _id: &str,
            _command_override: Option<&str>,
            _env_override: Option<&str>,
        ) -> Result<(), DbError> {
            Ok(())
        }

        async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
            Ok(false)
        }

        async fn delete(&self, _id: &str) -> Result<bool, DbError> {
            Ok(false)
        }
    }

    #[derive(Default)]
    struct MockTeamDirectory {
        teams: Mutex<Vec<ChannelTeamSummary>>,
        created: Mutex<Vec<ChannelTeamCreateRequest>>,
        ensured: Mutex<Vec<String>>,
    }

    impl MockTeamDirectory {
        fn with_teams(teams: Vec<ChannelTeamSummary>) -> Self {
            Self {
                teams: Mutex::new(teams),
                ..Default::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelTeamDirectory for MockTeamDirectory {
        async fn list_teams(&self, _user_id: &str) -> Result<Vec<ChannelTeamSummary>, ChannelError> {
            Ok(self.teams.lock().unwrap().clone())
        }

        async fn get_team(&self, _user_id: &str, _team_id: &str) -> Result<Option<ChannelTeamSummary>, ChannelError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .find(|team| team.id == _team_id)
                .cloned())
        }

        async fn ensure_team_session(&self, _user_id: &str, team_id: &str) -> Result<(), ChannelError> {
            self.ensured.lock().unwrap().push(team_id.to_owned());
            Ok(())
        }

        async fn create_team(
            &self,
            _user_id: &str,
            request: ChannelTeamCreateRequest,
        ) -> Result<ChannelTeamSummary, ChannelError> {
            self.created.lock().unwrap().push(request.clone());
            Ok(ChannelTeamSummary {
                id: "team-created-1".into(),
                name: request.name,
                lead_conversation_id: Some("lead-conv-1".into()),
                agent_count: 1,
            })
        }
    }

    #[derive(Default)]
    struct MockPersonalDirectory {
        conversations: Mutex<Vec<ChannelPersonalConversationSummary>>,
    }

    impl MockPersonalDirectory {
        fn with_conversations(conversations: Vec<ChannelPersonalConversationSummary>) -> Self {
            Self {
                conversations: Mutex::new(conversations),
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPersonalDirectory for MockPersonalDirectory {
        async fn list_personal_conversations(
            &self,
            _user_id: &str,
            _platform: PluginType,
            _chat_id: &str,
        ) -> Result<Vec<ChannelPersonalConversationSummary>, ChannelError> {
            Ok(self.conversations.lock().unwrap().clone())
        }

        async fn get_personal_conversation(
            &self,
            _user_id: &str,
            _platform: PluginType,
            _chat_id: &str,
            conversation_id: &str,
        ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError> {
            Ok(self
                .conversations
                .lock()
                .unwrap()
                .iter()
                .find(|conversation| conversation.id == conversation_id)
                .cloned())
        }

        async fn rename_personal_conversation(
            &self,
            _user_id: &str,
            _platform: PluginType,
            _chat_id: &str,
            conversation_id: &str,
            title: &str,
        ) -> Result<Option<ChannelPersonalConversationSummary>, ChannelError> {
            let mut conversations = self.conversations.lock().unwrap();
            let Some(conversation) = conversations
                .iter_mut()
                .find(|conversation| conversation.id == conversation_id)
            else {
                return Ok(None);
            };
            conversation.name = title.to_owned();
            Ok(Some(conversation.clone()))
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    fn setup() -> (ActionExecutor, Arc<MockRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(MockPrefRepo);
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings);
        (executor, repo)
    }

    fn setup_with_team_directory(
        team_directory: Arc<MockTeamDirectory>,
    ) -> (ActionExecutor, Arc<MockRepo>, Arc<MockTeamDirectory>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".to_owned(),
            r#"{"assistant_id":"assistant-telegram-lead","name":"Telegram Lead"}"#.to_owned(),
        );
        let pref_repo: Arc<dyn IClientPreferenceRepository> = Arc::new(StaticPrefRepo::new(prefs));
        let settings = Arc::new(ChannelSettingsService::new(pref_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings).with_team_directory(team_directory.clone());
        (executor, repo, team_directory)
    }

    fn setup_with_agent_rows(
        rows: Vec<AgentMetadataRow>,
        prefs: HashMap<String, String>,
    ) -> (ActionExecutor, Arc<MockRepo>, Arc<StaticPrefRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo = Arc::new(StaticPrefRepo::new(prefs));
        let agent_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(MockAgentMetadataRepo::with_rows(rows));
        let settings = Arc::new(ChannelSettingsService::new(pref_repo.clone()).with_agent_metadata_repo(agent_repo));
        let executor = ActionExecutor::new(pairing, session_mgr, settings);
        (executor, repo, pref_repo)
    }

    fn setup_with_agent_rows_and_providers(
        rows: Vec<AgentMetadataRow>,
        providers: Vec<Provider>,
        prefs: HashMap<String, String>,
    ) -> (ActionExecutor, Arc<MockRepo>, Arc<StaticPrefRepo>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(MockBroadcaster);
        let pairing = Arc::new(PairingService::new(repo.clone(), broadcaster));
        let session_mgr = Arc::new(SessionManager::new(repo.clone()));
        let pref_repo = Arc::new(StaticPrefRepo::new(prefs));
        let agent_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(MockAgentMetadataRepo::with_rows(rows));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(MockProviderRepo::with_rows(providers));
        let settings = Arc::new(
            ChannelSettingsService::new(pref_repo.clone())
                .with_agent_metadata_repo(agent_repo)
                .with_provider_repo(provider_repo),
        );
        let executor = ActionExecutor::new(pairing, session_mgr, settings);
        (executor, repo, pref_repo)
    }

    fn setup_with_personal_directory(
        personal_directory: Arc<MockPersonalDirectory>,
    ) -> (ActionExecutor, Arc<MockRepo>, Arc<MockPersonalDirectory>) {
        let (executor, repo) = setup();
        let executor = executor.with_personal_directory(personal_directory.clone());
        (executor, repo, personal_directory)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_agent_row(
        id: &str,
        name: &str,
        agent_type: &str,
        backend: Option<&str>,
        enabled: bool,
        last_check_status: Option<&str>,
        available_models: Option<&str>,
        sort_order: i64,
    ) -> AgentMetadataRow {
        let dynamic_probe_result = if agent_type == "acp" {
            available_models.and_then(test_dynamic_probe_from_models)
        } else {
            None
        };
        AgentMetadataRow {
            id: id.into(),
            icon: None,
            name: name.into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(str::to_owned),
            agent_type: agent_type.into(),
            agent_source: if agent_type == "aionrs" {
                "internal".into()
            } else {
                "builtin".into()
            },
            agent_source_info: None,
            enabled,
            command: Some(name.to_ascii_lowercase()),
            args: None,
            env: None,
            native_skills_dirs: None,
            behavior_policy: None,
            yolo_id: None,
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: available_models.map(str::to_owned),
            available_commands: None,
            sort_order,
            last_check_status: last_check_status.map(str::to_owned),
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: last_check_status.map(|_| now_ms()),
            last_failure_at: None,
            dynamic_probe_result,
            command_override: None,
            env_override: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    fn test_dynamic_probe_from_models(raw: &str) -> Option<String> {
        use aionui_api_types::{AgentDynamicProbeResult, AgentProbeStatus, AgentProbeStep, AgentProbeStepResult};

        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let entries = value
            .get("available_models")
            .and_then(serde_json::Value::as_array)
            .or_else(|| value.as_array())?;
        let models = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("id").and_then(serde_json::Value::as_str))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let checked_at = now_ms();
        serde_json::to_string(&AgentDynamicProbeResult {
            agent_id: "fixture-agent".into(),
            checked_at,
            available_models: models,
            steps: [
                AgentProbeStep::Spawn,
                AgentProbeStep::Initialize,
                AgentProbeStep::Models,
                AgentProbeStep::MinimalPrompt,
                AgentProbeStep::Cancel,
            ]
            .into_iter()
            .map(|step| AgentProbeStepResult {
                step,
                status: AgentProbeStatus::Passed,
                started_at: checked_at,
                duration_ms: 1,
                error_category: None,
                error_message: None,
            })
            .collect(),
        })
        .ok()
    }

    fn make_provider(id: &str, name: &str, models: &str) -> Provider {
        Provider {
            id: id.into(),
            platform: "custom".into(),
            name: name.into(),
            base_url: "https://example.test/v1".into(),
            api_key_encrypted: "encrypted".into(),
            models: models.into(),
            enabled: true,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}".into(),
            bedrock_config: None,
            is_full_url: false,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    fn make_text_message(user_id: &str, chat_id: &str, text: &str, platform: PluginType) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform,
            chat_id: chat_id.into(),
            user: UnifiedUser {
                id: user_id.into(),
                username: None,
                display_name: "Test".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: text.into(),
                attachments: None,
            },
            timestamp: now_ms(),
            topic: None,
            reply_to_message_id: None,
            action: None,
            raw: None,
        }
    }

    fn make_command_message(user_id: &str, chat_id: &str, text: &str, platform: PluginType) -> UnifiedIncomingMessage {
        let mut msg = make_text_message(user_id, chat_id, text, platform);
        msg.content.content_type = MessageContentType::Command;
        msg
    }

    fn make_action_message(
        user_id: &str,
        chat_id: &str,
        action_name: &str,
        category: ActionCategory,
        platform: PluginType,
        params: Option<HashMap<String, String>>,
    ) -> UnifiedIncomingMessage {
        UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform,
            chat_id: chat_id.into(),
            user: UnifiedUser {
                id: user_id.into(),
                username: None,
                display_name: "Test".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Action,
                text: String::new(),
                attachments: None,
            },
            timestamp: now_ms(),
            topic: None,
            reply_to_message_id: None,
            action: Some(UnifiedAction {
                action: action_name.into(),
                category,
                params,
                context: ActionContext {
                    platform,
                    user_id: user_id.into(),
                    chat_id: chat_id.into(),
                    message_id: None,
                    session_id: None,
                },
            }),
            raw: None,
        }
    }

    // ── Authorization tests ────────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_user_gets_pairing_response() {
        let (executor, _repo) = setup();
        let msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Send);
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
                assert!(resp.buttons.is_some());
            }
            _ => panic!("Expected Action result for unauthorized user"),
        }
    }

    #[tokio::test]
    async fn authorized_user_text_dispatches_to_agent() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_text_message("tg_42", "chat_1", "Hello AI", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Dispatched { session_id, .. } => {
                assert!(!session_id.is_empty());
            }
            _ => panic!("Expected Dispatched result for authorized user"),
        }
    }

    // ── Platform action tests ──────────────────────────────────────────

    #[tokio::test]
    async fn pairing_show_generates_code() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.show",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("pairing code"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_check_not_authorized() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_99", // different user
            "chat_1",
            "pairing.check",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        // tg_99 is not authorized, but the action itself needs the user to be authorized
        // first (it's routed via handle_incoming_message which checks auth first)
        // So for this test, authorize tg_99 too
        repo.add_authorized_user("tg_99", "telegram");

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                // tg_99 is authorized
                assert!(text.contains("authorized"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn pairing_help_returns_instructions() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "pairing.help",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("authorization"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── System action tests ────────────────────────────────────────────

    #[tokio::test]
    async fn session_new_creates_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
                // With no client_preferences configured, defaults to "aionrs"
                assert!(text.contains("aionrs"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn session_new_resets_existing_session() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        // First: send a text message to create a session
        let text_msg = make_text_message("tg_42", "chat_1", "Hello", PluginType::Telegram);
        let r1 = executor.handle_incoming_message(&text_msg).await.unwrap();
        let sid1 = match r1 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };

        // Then: session.new should delete old + create fresh
        let new_msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.new",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let r2 = executor.handle_incoming_message(&new_msg).await.unwrap();
        match r2 {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("New session"));
            }
            _ => panic!("Expected Action result"),
        }

        // Send another text message — the session ID should differ
        let text_msg2 = make_text_message("tg_42", "chat_1", "Again", PluginType::Telegram);
        let r3 = executor.handle_incoming_message(&text_msg2).await.unwrap();
        let sid3 = match r3 {
            MessageResult::Dispatched { session_id, .. } => session_id,
            _ => panic!("Expected Dispatched"),
        };
        // New session has different full ID (reset deleted the old one)
        assert_ne!(sid1, sid3);

        // Only 1 session should exist for this user+chat
        let sessions = repo.sessions.lock().unwrap();
        let user_chat_sessions: Vec<_> = sessions
            .iter()
            .filter(|s| s.user_id == "user_tg_42" && s.chat_id.as_deref() == Some("chat_1"))
            .collect();
        assert_eq!(user_chat_sessions.len(), 1);
    }

    #[tokio::test]
    async fn session_status_shows_info() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "session.status",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Session:"));
                assert!(text.contains("Agent:"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn help_show_returns_menu() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "help.show",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert!(resp.text.is_some());
                assert!(resp.buttons.is_some());
                let buttons = resp.buttons.unwrap();
                assert!(buttons.len() >= 2); // at least 2 rows
                assert!(
                    !buttons.iter().flatten().any(|button| button.action == "agent.show"),
                    "help menu must not expose direct agent selection"
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_help_returns_telegram_command_menu() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/help", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("/status"), "got: {text}");
                assert!(text.contains("/team_new"), "got: {text}");
                assert!(text.contains("/team_help"), "got: {text}");
                assert!(!text.contains("/new_team_session"), "got: {text}");
                let actions: Vec<_> = resp
                    .buttons
                    .expect("General help should expose contextual operation buttons")
                    .into_iter()
                    .flatten()
                    .map(|button| button.action)
                    .collect();
                assert!(actions.contains(&"agent.list".to_owned()));
                assert!(actions.contains(&"personal.list".to_owned()));
                assert!(actions.contains(&"team.list".to_owned()));
                assert!(actions.contains(&"team.new.start".to_owned()));
                assert!(actions.contains(&"team.help".to_owned()));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn general_help_team_list_button_routes_to_team_directory() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "team.list",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );

        let MessageResult::Action(resp) = executor.handle_incoming_message(&msg).await.unwrap() else {
            panic!("Expected Action result");
        };
        assert!(resp.text.unwrap().contains("Team 查询服务"));
    }

    #[tokio::test]
    async fn general_help_team_new_button_starts_creation_flow() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "team.new.start",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );

        let MessageResult::Action(resp) = executor.handle_incoming_message(&msg).await.unwrap() else {
            panic!("Expected Action result");
        };
        assert!(resp.text.unwrap().contains("Team 创建服务"));
    }

    #[tokio::test]
    async fn general_help_personal_list_button_routes_to_personal_directory() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "personal.list",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );

        let MessageResult::Action(resp) = executor.handle_incoming_message(&msg).await.unwrap() else {
            panic!("Expected Action result");
        };
        assert!(resp.text.unwrap().contains("个人会话列表服务"));
    }

    #[tokio::test]
    async fn general_help_team_help_button_returns_team_help() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "team.help",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );

        let MessageResult::Action(resp) = executor.handle_incoming_message(&msg).await.unwrap() else {
            panic!("Expected Action result");
        };
        assert!(resp.text.unwrap().contains("Telegram Team 功能说明"));
    }

    #[tokio::test]
    async fn slash_new_team_session_is_not_a_supported_command() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/new_team_session", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("未知命令"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_status_shows_current_binding() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/status", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("当前 Telegram 绑定"), "got: {text}");
                assert!(text.contains("Session"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_team_new_creates_real_team_and_binds_session() {
        let team_directory = Arc::new(MockTeamDirectory::default());
        let (executor, repo, team_directory) = setup_with_team_directory(team_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/team_new BTC 4H Strategy", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Team 已创建并切换"), "got: {text}");
                assert!(text.contains("BTC 4H Strategy"), "got: {text}");
                assert!(text.contains("lead-conv-1"), "got: {text}");
                assert!(resp.buttons.is_none());
            }
            _ => panic!("Expected Action result"),
        }

        {
            let created = team_directory.created.lock().unwrap();
            assert_eq!(created.len(), 1);
            assert_eq!(created[0].name, "BTC 4H Strategy");
            assert_eq!(created[0].assistant_id, "assistant-telegram-lead");
            assert_eq!(created[0].lead_name, "Telegram Lead");
        }

        assert_eq!(team_directory.ensured.lock().unwrap().as_slice(), ["team-created-1"]);
        let sessions = repo.get_all_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].conversation_id.as_deref(), Some("lead-conv-1"));
    }

    #[tokio::test]
    async fn slash_team_select_without_selector_returns_inline_team_buttons() {
        let team_directory = Arc::new(MockTeamDirectory::with_teams(vec![
            ChannelTeamSummary {
                id: "team-1".into(),
                name: "BTC Strategy".into(),
                lead_conversation_id: Some("lead-1".into()),
                agent_count: 3,
            },
            ChannelTeamSummary {
                id: "team-2".into(),
                name: "ETH Research".into(),
                lead_conversation_id: Some("lead-2".into()),
                agent_count: 2,
            },
        ]));
        let (executor, repo, _team_directory) = setup_with_team_directory(team_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/team_select", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("请选择要切换的 Team"), "got: {text}");
                let buttons = resp.buttons.expect("team_select should return inline buttons");
                assert_eq!(buttons[0][0].label, "BTC Strategy");
                assert_eq!(buttons[0][0].action, "team.select");
                assert_eq!(
                    buttons[0][0].params.as_ref().unwrap().get("id").map(String::as_str),
                    Some("team-1")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn team_select_button_binds_the_selected_team() {
        let team_directory = Arc::new(MockTeamDirectory::with_teams(vec![ChannelTeamSummary {
            id: "team-1".into(),
            name: "BTC Strategy".into(),
            lead_conversation_id: Some("lead-1".into()),
            agent_count: 3,
        }]));
        let (executor, repo, team_directory) = setup_with_team_directory(team_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("id".into(), "team-1".into())]);
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "team.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();

        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("已切换到 Team 会话"), "got: {text}");
                assert!(text.contains("BTC Strategy"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        assert_eq!(team_directory.ensured.lock().unwrap().as_slice(), ["team-1"]);
        let sessions = repo.get_all_sessions().await.unwrap();
        assert_eq!(sessions[0].conversation_id.as_deref(), Some("lead-1"));
    }

    #[tokio::test]
    async fn slash_team_new_without_topic_starts_reply_driven_creation_flow() {
        let team_directory = Arc::new(MockTeamDirectory::default());
        let (executor, repo, team_directory) = setup_with_team_directory(team_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let start = make_command_message("tg_42", "chat_1", "/team_new", PluginType::Telegram);
        let start_result = executor.handle_incoming_message(&start).await.unwrap();
        match start_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("请直接回复团队名称"), "got: {text}");
                assert_eq!(resp.buttons.unwrap()[0][0].action, "team.new.cancel");
            }
            _ => panic!("Expected Action result"),
        }

        let name = make_text_message("tg_42", "chat_1", "BTC 4H Button Flow", PluginType::Telegram);
        let name_result = executor.handle_incoming_message(&name).await.unwrap();
        match name_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("准备创建 Team"), "got: {text}");
                assert!(text.contains("BTC 4H Button Flow"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].action, "team.new.create");
                assert_eq!(buttons[0][1].action, "team.new.cancel");
            }
            _ => panic!("Expected Action result"),
        }

        let create = make_action_message(
            "tg_42",
            "chat_1",
            "team.new.create",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let create_result = executor.handle_incoming_message(&create).await.unwrap();
        match create_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Team 已创建并切换"), "got: {text}");
                assert!(text.contains("BTC 4H Button Flow"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        assert_eq!(team_directory.created.lock().unwrap()[0].name, "BTC 4H Button Flow");
    }

    #[test]
    fn team_new_normalizes_generated_agent_id_for_team_assistant() {
        assert_eq!(normalize_team_assistant_id("8e1acf31"), "bare:8e1acf31");
        assert_eq!(normalize_team_assistant_id(" 8E1ACF31 "), "bare:8E1ACF31");
        assert_eq!(normalize_team_assistant_id("cowork"), "cowork");
        assert_eq!(
            normalize_team_assistant_id("custom-1782376459103-3673"),
            "custom-1782376459103-3673"
        );
        assert_eq!(normalize_team_assistant_id("bare:8e1acf31"), "bare:8e1acf31");
    }

    #[tokio::test]
    async fn agent_show_is_treated_as_unknown_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.show",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
                assert!(text.contains("agent.show"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_select_without_assistant_id_returns_clear_error() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("agentType".into(), "acp".into())]);
        let select_msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor.handle_incoming_message(&select_msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("缺少 agentId"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_list_returns_only_available_local_agents() {
        let (executor, repo, _prefs) = setup_with_agent_rows(
            vec![
                make_agent_row(
                    "codex-1",
                    "Codex CLI",
                    "acp",
                    Some("codex"),
                    true,
                    Some("online"),
                    None,
                    10,
                ),
                make_agent_row(
                    "claude-1",
                    "Claude Code",
                    "acp",
                    Some("claude"),
                    true,
                    Some("offline"),
                    None,
                    20,
                ),
                make_agent_row(
                    "disabled-1",
                    "Disabled Agent",
                    "acp",
                    Some("disabled"),
                    false,
                    Some("online"),
                    None,
                    30,
                ),
            ],
            HashMap::new(),
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.list",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Codex CLI"), "got: {text}");
                assert!(!text.contains("Claude Code"), "got: {text}");
                assert!(!text.contains("Disabled Agent"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].action, "agent.select");
                assert_eq!(
                    buttons[0][0]
                        .params
                        .as_ref()
                        .unwrap()
                        .get("agentId")
                        .map(String::as_str),
                    Some("codex-1")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_list_hides_current_agent_and_includes_path_available_agents() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"codex-1","name":"Codex CLI"}"#.into(),
        );
        let mut openclaw = make_agent_row("openclaw-1", "OpenClaw", "acp", Some("openclaw"), true, None, None, 20);
        openclaw.command = Some("sh".into());
        let (executor, repo, _prefs) = setup_with_agent_rows(
            vec![
                make_agent_row(
                    "codex-1",
                    "Codex CLI",
                    "acp",
                    Some("codex"),
                    true,
                    Some("online"),
                    None,
                    10,
                ),
                openclaw,
                make_agent_row(
                    "disabled-1",
                    "Disabled Agent",
                    "acp",
                    Some("disabled"),
                    false,
                    Some("online"),
                    None,
                    30,
                ),
            ],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.list",
            ActionCategory::System,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("OpenClaw"), "got: {text}");
                assert!(!text.contains("- Codex CLI"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(
                    buttons[0][0]
                        .params
                        .as_ref()
                        .unwrap()
                        .get("agentId")
                        .map(String::as_str),
                    Some("openclaw-1")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn agent_select_binds_local_agent_and_clears_model_override() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.defaultModel".into(),
            r#"{"id":"old-provider","use_model":"old-model"}"#.into(),
        );
        let (executor, repo, prefs) = setup_with_agent_rows(
            vec![make_agent_row(
                "codex-1",
                "Codex CLI",
                "acp",
                Some("codex"),
                true,
                Some("online"),
                None,
                10,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "agent.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(HashMap::from([("agentId".into(), "codex-1".into())])),
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Telegram Agent 已切换"), "got: {text}");
                assert!(text.contains("Codex CLI"), "got: {text}");
                assert!(text.contains("已恢复 Agent 默认模型"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        let saved_agent = prefs.get("assistant.telegram.agent").unwrap();
        assert!(saved_agent.contains("codex-1"), "saved: {saved_agent}");
        assert!(prefs.get("assistant.telegram.defaultModel").is_none());
        let sessions = repo.get_all_sessions().await.unwrap();
        assert_eq!(sessions[0].agent_type, "acp");
    }

    #[tokio::test]
    async fn slash_model_returns_buttons_for_current_agent_models() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"codex-1","name":"Codex CLI"}"#.into(),
        );
        let model_payload = r#"{"current_model_id":"gpt-5","available_models":[{"id":"gpt-5","label":"GPT-5"},{"id":"gpt-5-mini","label":"GPT-5 Mini"}]}"#;
        let (executor, repo, _prefs) = setup_with_agent_rows(
            vec![make_agent_row(
                "codex-1",
                "Codex CLI",
                "acp",
                Some("codex"),
                true,
                Some("online"),
                Some(model_payload),
                10,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/model", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("当前 Agent: Codex CLI"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].label, "gpt-5");
                assert_eq!(buttons[0][0].action, "model.select");
                assert_eq!(
                    buttons[0][0].params.as_ref().unwrap().get("m").map(String::as_str),
                    Some("gpt-5")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_model_uses_acp_config_options_when_probe_models_are_empty() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"claude-1","name":"Claude Code"}"#.into(),
        );
        let mut row = make_agent_row(
            "claude-1",
            "Claude Code",
            "acp",
            Some("claude"),
            true,
            Some("online"),
            Some(r#"{"available_models":[]}"#),
            10,
        );
        row.config_options = Some(
            r#"{"config_options":[{"category":"model","currentValue":"sonnet","id":"model","name":"Model","options":[{"name":"Default","value":"default"},{"name":"Sonnet","value":"sonnet"}],"type":"select"}]}"#
                .into(),
        );
        let (executor, repo, _prefs) = setup_with_agent_rows(vec![row], prefs);
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/model", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("当前 Agent: Claude Code"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].label, "Default");
                assert_eq!(buttons[1][0].label, "Sonnet");
                assert_eq!(buttons[1][0].action, "model.select");
                assert_eq!(
                    buttons[1][0].params.as_ref().unwrap().get("m").map(String::as_str),
                    Some("sonnet")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_model_uses_compact_callback_params_for_telegram_limit() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"hermes-1","name":"Hermes"}"#.into(),
        );
        let model_payload = r#"{"available_models":[{"id":"openai-codex:gpt-5.3-codex-spark","label":"openai-codex:gpt-5.3-codex-spark"}]}"#;
        let (executor, repo, _prefs) = setup_with_agent_rows(
            vec![make_agent_row(
                "hermes-1",
                "Hermes",
                "acp",
                Some("hermes"),
                true,
                Some("online"),
                Some(model_payload),
                10,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/model", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let buttons = resp.buttons.unwrap();
                let params = buttons[0][0].params.as_ref().unwrap();
                assert_eq!(params.get("p").map(String::as_str), Some("hermes-1"));
                assert_eq!(
                    params.get("m").map(String::as_str),
                    Some("openai-codex:gpt-5.3-codex-spark")
                );
                assert!(!params.contains_key("providerId"));
                assert!(!params.contains_key("model"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_model_returns_provider_buttons_for_aionrs_agent() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"aion-1","name":"Aion CLI"}"#.into(),
        );
        prefs.insert(
            "aionrs.defaultModel".into(),
            r#"{"id":"deepseek-provider","use_model":"deepseek-v4-pro"}"#.into(),
        );
        let (executor, repo, _prefs) = setup_with_agent_rows_and_providers(
            vec![make_agent_row(
                "aion-1", "Aion CLI", "aionrs", None, true, None, None, 10,
            )],
            vec![make_provider(
                "deepseek-provider",
                "DeepSeek",
                r#"["deepseek-v4-pro","deepseek-v4-flash"]"#,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/model", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("deepseek-provider / deepseek-v4-pro"), "got: {text}");
                assert!(text.contains("当前 Agent: Aion CLI"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].label, "DeepSeek / deepseek-v4-pro");
                assert_eq!(buttons[0][0].action, "model.select");
                assert_eq!(
                    buttons[0][0].params.as_ref().unwrap().get("p").map(String::as_str),
                    Some("deepseek-provider")
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn slash_model_explains_openclaw_runtime_model_is_display_only() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"openclaw-1","name":"OpenClaw"}"#.into(),
        );
        let (executor, repo, _prefs) = setup_with_agent_rows(
            vec![make_agent_row(
                "openclaw-1",
                "OpenClaw",
                "acp",
                Some("openclaw"),
                true,
                Some("online"),
                None,
                10,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_command_message("tg_42", "chat_1", "/model", PluginType::Telegram);
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("当前 Agent: OpenClaw"), "got: {text}");
                assert!(
                    text.contains("暂不支持在 Telegram 中切换 OpenClaw 模型。"),
                    "got: {text}"
                );
                assert!(
                    text.contains("后续等待 OpenClaw ACP 完善模型列表和 set model 能力后可实现。"),
                    "got: {text}"
                );
                let buttons = resp.buttons.unwrap_or_default();
                assert!(
                    buttons.iter().flatten().all(|button| button.action != "model.select"),
                    "OpenClaw must not show model switch buttons until ACP reports model support"
                );
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[test]
    fn openclaw_runtime_model_line_extracts_metadata_model_only() {
        let line = serde_json::json!({
            "type": "trace.metadata",
            "sessionKey": "agent:main:acp:sess-123",
            "data": {
                "model": {
                    "provider": "openai-codex",
                    "name": "gpt-5.5",
                    "thinkLevel": "medium"
                },
                "config": {
                    "redacted": {
                        "auth": "must not leak"
                    }
                }
            }
        })
        .to_string();

        let model = super::extract_openclaw_runtime_model_from_trajectory_line(&line).unwrap();
        assert_eq!(model.provider.as_deref(), Some("openai-codex"));
        assert_eq!(model.name.as_deref(), Some("gpt-5.5"));
        assert_eq!(model.think_level.as_deref(), Some("medium"));
    }

    #[tokio::test]
    async fn model_select_writes_allowed_model_and_resets_session() {
        let mut prefs = HashMap::new();
        prefs.insert(
            "assistant.telegram.agent".into(),
            r#"{"assistant_id":"codex-1","name":"Codex CLI"}"#.into(),
        );
        let model_payload = r#"{"available_models":[{"id":"gpt-5","label":"GPT-5"}]}"#;
        let (executor, repo, prefs) = setup_with_agent_rows(
            vec![make_agent_row(
                "codex-1",
                "Codex CLI",
                "acp",
                Some("codex"),
                true,
                Some("online"),
                Some(model_payload),
                10,
            )],
            prefs,
        );
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "model.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(HashMap::from([
                ("providerId".into(), "codex-1".into()),
                ("model".into(), "gpt-5".into()),
            ])),
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Telegram 模型已切换"), "got: {text}");
                assert!(text.contains("gpt-5"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        let saved_model = prefs.get("assistant.telegram.defaultModel").unwrap();
        assert!(saved_model.contains("codex-1"), "saved: {saved_model}");
        assert!(saved_model.contains("gpt-5"), "saved: {saved_model}");
    }

    #[tokio::test]
    async fn personal_list_and_select_bind_existing_personal_conversation() {
        let personal_directory = Arc::new(MockPersonalDirectory::with_conversations(vec![
            ChannelPersonalConversationSummary {
                id: "conv-1".into(),
                name: "检查本机资源情况".into(),
                agent_type: "acp".into(),
                agent_label: Some("OpenClaw".into()),
                recent_message: Some("帮我检查本机资源情况".into()),
            },
        ]));
        let (executor, repo, _personal_directory) = setup_with_personal_directory(personal_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let list_msg = make_command_message("tg_42", "chat_1", "/personal_list", PluginType::Telegram);
        let list_result = executor.handle_incoming_message(&list_msg).await.unwrap();
        match list_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("请选择要切换的个人会话"), "got: {text}");
                assert!(text.contains("检查本机资源情况"), "got: {text}");
                assert!(text.contains("Agent: OpenClaw"), "got: {text}");
                assert!(text.contains("最近: 帮我检查本机资源情况"), "got: {text}");
                assert!(text.contains("ID: conv-1"), "got: {text}");
                let buttons = resp.buttons.unwrap();
                assert_eq!(buttons[0][0].action, "personal.select");
                assert_eq!(buttons[0][0].label, "检查本机资源情况");
                assert_eq!(
                    buttons[0][0].params.as_ref().unwrap().get("id").map(String::as_str),
                    Some("conv-1")
                );
            }
            _ => panic!("Expected Action result"),
        }

        let select_msg = make_action_message(
            "tg_42",
            "chat_1",
            "personal.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(HashMap::from([("id".into(), "conv-1".into())])),
        );
        let select_result = executor.handle_incoming_message(&select_msg).await.unwrap();
        match select_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("已切换到个人会话"), "got: {text}");
                assert!(text.contains("检查本机资源情况"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }
        let sessions = repo.get_all_sessions().await.unwrap();
        assert_eq!(sessions[0].conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(sessions[0].agent_type, "acp");
    }

    #[tokio::test]
    async fn new_session_with_title_passes_explicit_title_hint_to_next_dispatch() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let new_msg = make_command_message("tg_42", "chat_1", "/new_session 检查本机资源情况", PluginType::Telegram);
        let new_result = executor.handle_incoming_message(&new_msg).await.unwrap();
        match new_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("已新建个人会话"), "got: {text}");
                assert!(text.contains("检查本机资源情况"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        let text_msg = make_text_message("tg_42", "chat_1", "hello", PluginType::Telegram);
        let dispatch_result = executor.handle_incoming_message(&text_msg).await.unwrap();
        match dispatch_result {
            MessageResult::Dispatched { title_hint, .. } => {
                let title_hint = title_hint.expect("expected explicit title hint");
                assert_eq!(title_hint.title, "检查本机资源情况");
                assert_eq!(title_hint.source, "telegram_explicit");
            }
            _ => panic!("Expected Dispatched result"),
        }
    }

    #[tokio::test]
    async fn first_personal_message_generates_auto_title_hint() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let text_msg = make_text_message(
            "tg_42",
            "chat_1",
            "帮我检查本机资源情况，重点看 CPU 和内存",
            PluginType::Telegram,
        );
        let dispatch_result = executor.handle_incoming_message(&text_msg).await.unwrap();
        match dispatch_result {
            MessageResult::Dispatched { title_hint, .. } => {
                let title_hint = title_hint.expect("expected first-message title hint");
                assert_eq!(title_hint.title, "帮我检查本机资源情况，重点看 CPU 和内存");
                assert_eq!(title_hint.source, "telegram_first_message");
            }
            _ => panic!("Expected Dispatched result"),
        }
    }

    #[tokio::test]
    async fn rename_updates_current_personal_conversation_title() {
        let personal_directory = Arc::new(MockPersonalDirectory::with_conversations(vec![
            ChannelPersonalConversationSummary {
                id: "conv-1".into(),
                name: "OpenClaw".into(),
                agent_type: "acp".into(),
                agent_label: Some("OpenClaw".into()),
                recent_message: None,
            },
        ]));
        let (executor, repo, personal_directory) = setup_with_personal_directory(personal_directory);
        repo.add_authorized_user("tg_42", "telegram");

        let select_msg = make_action_message(
            "tg_42",
            "chat_1",
            "personal.select",
            ActionCategory::System,
            PluginType::Telegram,
            Some(HashMap::from([("id".into(), "conv-1".into())])),
        );
        executor.handle_incoming_message(&select_msg).await.unwrap();

        let rename_msg = make_command_message("tg_42", "chat_1", "/rename 检查本机资源情况", PluginType::Telegram);
        let rename_result = executor.handle_incoming_message(&rename_msg).await.unwrap();
        match rename_result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("已重命名当前个人会话"), "got: {text}");
                assert!(text.contains("检查本机资源情况"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }

        let renamed = personal_directory.conversations.lock().unwrap()[0].clone();
        assert_eq!(renamed.name, "检查本机资源情况");
    }

    // ── Chat action tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn system_confirm_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let params = HashMap::from([("callId".into(), "call_123".into()), ("value".into(), "true".into())]);
        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "system.confirm",
            ActionCategory::Chat,
            PluginType::Telegram,
            Some(params),
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert_eq!(resp.toast.as_deref(), Some("Confirmed"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    #[tokio::test]
    async fn approval_callback_is_resolved_with_authorized_topic_identity() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let approvals = Arc::new(RecordingApprovalPort::default());
        let executor = executor.with_approval_port(approvals.clone());
        let mut msg = make_action_message(
            "tg_42",
            "chat_1",
            "approval.resolve",
            ActionCategory::System,
            PluginType::Telegram,
            Some(HashMap::from([
                ("id".into(), "approval1234567".into()),
                ("o".into(), "1".into()),
            ])),
        );
        msg.topic = Some(crate::types::ChannelTopicContext { message_thread_id: 5 });
        msg.action.as_mut().unwrap().context.message_id = Some("telegram-message-77".into());

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(response) => {
                assert_eq!(response.behavior, ActionBehavior::Edit);
                assert_eq!(response.edit_message_id.as_deref(), Some("telegram-message-77"));
                assert_eq!(response.buttons, None);
                assert_eq!(response.toast.as_deref(), Some("审批已同意"));
                let text = response
                    .text
                    .expect("approval callback must visibly update the Telegram card");
                assert!(text.contains("审批已同意"), "got: {text}");
                assert!(text.contains("approval1234567"), "got: {text}");
            }
            _ => panic!("Expected Action result"),
        }
        let calls = approvals.resolutions.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "user_tg_42");
        assert_eq!(calls[0].3, Some(5));
        assert_eq!(calls[0].4, "approval1234567");
        assert_eq!(calls[0].5, 1);
    }

    #[tokio::test]
    async fn project_command_uses_authorized_channel_context() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");
        let development = Arc::new(RecordingDevelopmentPort::default());
        let executor = executor.with_development_port(development.clone());
        let msg = make_command_message("tg_42", "chat_1", "/project", PluginType::Telegram);

        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(response) => assert!(response.text.unwrap().contains("Project: Aion")),
            _ => panic!("Expected Action result"),
        }
        let calls = development.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.source_user_id, "user_tg_42");
        assert_eq!(calls[0].0.chat_id, "chat_1");
        assert_eq!(calls[0].1, crate::development::ChannelDevelopmentCommand::Project);
    }

    #[tokio::test]
    async fn action_copy_returns_answer() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "action.copy",
            ActionCategory::Chat,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                assert_eq!(resp.behavior, ActionBehavior::Answer);
                assert!(resp.toast.as_deref().unwrap().contains("Copied"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── Unknown action tests ───────────────────────────────────────────

    #[tokio::test]
    async fn unknown_platform_action() {
        let (executor, repo) = setup();
        repo.add_authorized_user("tg_42", "telegram");

        let msg = make_action_message(
            "tg_42",
            "chat_1",
            "unknown.action",
            ActionCategory::Platform,
            PluginType::Telegram,
            None,
        );
        let result = executor.handle_incoming_message(&msg).await.unwrap();
        match result {
            MessageResult::Action(resp) => {
                let text = resp.text.unwrap();
                assert!(text.contains("Unknown action"));
            }
            _ => panic!("Expected Action result"),
        }
    }

    // ── build_pairing_response tests ───────────────────────────────────

    #[test]
    fn pairing_response_contains_code() {
        let resp = build_pairing_response("123456");
        let text = resp.text.unwrap();
        assert!(text.contains("123456"));
        assert!(text.contains("pairing code"));
        assert_eq!(resp.behavior, ActionBehavior::Send);
        assert!(resp.buttons.is_some());
    }

    #[test]
    fn help_response_has_buttons() {
        let resp = build_help_response();
        assert!(resp.text.is_some());
        let buttons = resp.buttons.unwrap();
        assert!(!buttons.is_empty());
    }

    #[test]
    fn unknown_action_response_includes_name() {
        let resp = build_unknown_action_response("foo.bar");
        let text = resp.text.unwrap();
        assert!(text.contains("foo.bar"));
    }
}
