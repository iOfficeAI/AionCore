use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::action::{ActionExecutor, MessageResult};
use crate::message_service::ChannelMessageService;
use crate::session::SessionManager;
use crate::stream_relay::{ChannelSender, ChannelStreamRelay, RelayConfig, throttle_ms_for_platform};
use crate::types::{ActionBehavior, OutgoingMessageType, UnifiedIncomingMessage, UnifiedOutgoingMessage};

/// Orchestrates the full channel message lifecycle.
///
/// Consumes incoming IM messages from `message_rx` and tool confirmation
/// callbacks from `confirm_rx`, driving the pipeline:
/// 1. ActionExecutor routing (auth → action/AI dispatch)
/// 2. For Dispatched: send_to_agent + spawn ChannelStreamRelay
/// 3. For Action: reply via plugin
/// 4. Forward tool confirmations to the agent
pub struct ChannelOrchestrator {
    action_executor: Arc<ActionExecutor>,
    message_service: Arc<ChannelMessageService>,
    session_manager: Arc<SessionManager>,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelOrchestrator {
    pub fn new(
        action_executor: Arc<ActionExecutor>,
        message_service: Arc<ChannelMessageService>,
        session_manager: Arc<SessionManager>,
        sender: Arc<dyn ChannelSender>,
    ) -> Self {
        Self {
            action_executor,
            message_service,
            session_manager,
            sender,
        }
    }

    /// Start the message loop. Runs until both channels close.
    pub async fn run(
        self,
        mut message_rx: mpsc::Receiver<UnifiedIncomingMessage>,
        mut confirm_rx: mpsc::Receiver<(String, String)>,
    ) {
        info!("ChannelOrchestrator started");

        loop {
            tokio::select! {
                Some(msg) = message_rx.recv() => {
                    self.handle_message(msg).await;
                }
                Some((call_id, value)) = confirm_rx.recv() => {
                    handle_confirm(&call_id, &value);
                }
                else => break,
            }
        }

        info!("ChannelOrchestrator stopped (channels closed)");
    }

    async fn handle_message(&self, msg: UnifiedIncomingMessage) {
        let platform = msg.platform;
        let chat_id = msg.chat_id.clone();
        let plugin_id = platform.to_string();
        let text = msg.content.text.clone();
        let message_owner_user_id = msg
            .owner_user_id
            .clone()
            .or_else(|| self.action_executor.owner_user_id().map(ToOwned::to_owned));

        let executor = Arc::clone(&self.action_executor);
        let msg_svc = Arc::clone(&self.message_service);
        let session_mgr = Arc::clone(&self.session_manager);
        let sender = Arc::clone(&self.sender);

        tokio::spawn(async move {
            match executor.handle_incoming_message(&msg).await {
                Ok(MessageResult::Action(response)) => {
                    if let Some(owner_user_id) = message_owner_user_id.as_deref() {
                        send_action_response(&sender, owner_user_id, &plugin_id, &chat_id, &response).await;
                    } else {
                        warn!("dropping channel action response without owner user");
                    }
                }
                Ok(MessageResult::Dispatched {
                    owner_user_id,
                    session_id,
                    conversation_id,
                }) => {
                    handle_dispatched(
                        &msg_svc,
                        &session_mgr,
                        &sender,
                        &owner_user_id,
                        &session_id,
                        conversation_id.as_deref(),
                        &text,
                        platform,
                        &plugin_id,
                        &chat_id,
                    )
                    .await;
                }
                Ok(MessageResult::AlreadyProcessing) => {
                    info!(chat_id = %chat_id, "message ignored: already processing");
                }
                Err(e) => {
                    error!(error = %e, "failed to handle incoming message");
                }
            }
        });
    }
}

async fn send_action_response(
    sender: &Arc<dyn ChannelSender>,
    owner_user_id: &str,
    plugin_id: &str,
    chat_id: &str,
    response: &crate::types::ActionResponse,
) {
    if let Some(text) = &response.text {
        let outgoing = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.clone()),
            parse_mode: response.parse_mode,
            buttons: response.buttons.clone(),
            keyboard: response.keyboard.clone(),
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };

        match response.behavior {
            ActionBehavior::Edit => {
                if let Some(ref edit_id) = response.edit_message_id {
                    let _ = sender
                        .edit_message(owner_user_id, plugin_id, chat_id, edit_id, outgoing)
                        .await;
                }
            }
            _ => {
                let _ = sender.send_message(owner_user_id, plugin_id, chat_id, outgoing).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_dispatched(
    msg_svc: &Arc<ChannelMessageService>,
    session_mgr: &Arc<SessionManager>,
    sender: &Arc<dyn ChannelSender>,
    owner_user_id: &str,
    session_id: &str,
    conversation_id: Option<&str>,
    text: &str,
    platform: crate::types::PluginType,
    plugin_id: &str,
    chat_id: &str,
) {
    let session = match session_mgr.get_session_by_id(owner_user_id, session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!(session_id = %session_id, "session not found after dispatch");
            return;
        }
        Err(e) => {
            error!(error = %e, "failed to get session");
            return;
        }
    };

    let send_result = match msg_svc.send_to_agent(owner_user_id, &session, text, platform).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to send to agent");
            let err_msg = UnifiedOutgoingMessage {
                message_type: OutgoingMessageType::Text,
                text: Some(format!("\u{274c} Failed to process: {e}")),
                parse_mode: None,
                buttons: None,
                keyboard: None,
                image_url: None,
                file_url: None,
                file_name: None,
                media_actions: None,
                reply_to_message_id: None,
                silent: None,
            };
            let _ = sender.send_message(owner_user_id, plugin_id, chat_id, err_msg).await;
            return;
        }
    };

    // Bind conversation to session if newly created
    if conversation_id.is_none()
        && let Err(e) = session_mgr
            .bind_conversation(owner_user_id, session_id, &send_result.conversation_id)
            .await
    {
        warn!(error = %e, "failed to bind conversation to session");
    }

    // Spawn stream relay if we got a subscription
    if let Some(rx) = send_result.stream_rx {
        let relay_config = RelayConfig {
            owner_user_id: owner_user_id.to_owned(),
            platform,
            plugin_id: plugin_id.to_owned(),
            chat_id: chat_id.to_owned(),
            throttle_ms: throttle_ms_for_platform(platform),
        };
        let relay = ChannelStreamRelay::new(relay_config, Arc::clone(sender));
        tokio::spawn(relay.run(rx));
    } else {
        warn!(
            conversation_id = %send_result.conversation_id,
            "no agent task for stream subscription"
        );
    }
}

/// Forward a tool confirmation callback to the active agent.
fn handle_confirm(call_id: &str, value: &str) {
    // Channel conversations use yoloMode which auto-approves everything,
    // so this path is rarely hit. When needed, we can add a
    // call_id→conversation_id lookup via IWorkerTaskManager.
    info!(call_id = %call_id, value = %value, "forwarding tool confirmation");
}
