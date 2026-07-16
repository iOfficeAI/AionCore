use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

use crate::action::{ActionExecutor, MessageResult};
use crate::formatter::format_outgoing_text_for_platform;
use crate::message_service::ChannelMessageService;
use crate::session::SessionManager;
use crate::stream_relay::{ChannelSender, ChannelStreamRelay, RelayConfig};
use crate::types::{ActionBehavior, OutgoingMessageType, PluginType, UnifiedIncomingMessage, UnifiedOutgoingMessage};

type SequencedJob = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

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
    chat_sequencer: ChatMessageSequencer,
}

#[derive(Clone, Default)]
struct ChatMessageSequencer {
    queues: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<SequencedJob>>>>,
}

impl ChatMessageSequencer {
    async fn spawn<F>(&self, platform: PluginType, chat_id: &str, message_thread_id: Option<i64>, job: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = match message_thread_id {
            Some(thread_id) => format!("{platform}:{chat_id}:{thread_id}"),
            None => format!("{platform}:{chat_id}"),
        };
        let sender = {
            let mut queues = self.queues.lock().await;
            queues
                .entry(key.clone())
                .or_insert_with(|| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    spawn_chat_worker(key, rx);
                    tx
                })
                .clone()
        };

        if sender.send(Box::pin(job)).is_err() {
            warn!(platform = %platform, chat_id, "failed to enqueue channel message: chat worker stopped");
        }
    }
}

fn spawn_chat_worker(key: String, mut rx: mpsc::UnboundedReceiver<SequencedJob>) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            job.await;
        }
        info!(chat_key = %key, "channel chat sequencer worker stopped");
    });
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
            chat_sequencer: ChatMessageSequencer::default(),
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
        let topic = msg.topic.clone();

        let executor = Arc::clone(&self.action_executor);
        let msg_svc = Arc::clone(&self.message_service);
        let session_mgr = Arc::clone(&self.session_manager);
        let sender = Arc::clone(&self.sender);
        let chat_sequencer = self.chat_sequencer.clone();

        chat_sequencer
            .spawn(
                platform,
                &chat_id.clone(),
                topic.as_ref().map(|value| value.message_thread_id),
                async move {
                    match executor.handle_incoming_message(&msg).await {
                        Ok(MessageResult::Action(response)) => {
                            send_action_response(&sender, &plugin_id, &chat_id, topic.clone(), &response).await;
                        }
                        Ok(MessageResult::Dispatched {
                            session_id,
                            conversation_id,
                            title_hint,
                        }) => {
                            handle_dispatched(
                                &msg_svc,
                                &session_mgr,
                                &sender,
                                &session_id,
                                conversation_id.as_deref(),
                                title_hint,
                                &text,
                                platform,
                                &plugin_id,
                                &chat_id,
                                topic.clone(),
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
                },
            )
            .await;
    }
}

async fn send_action_response(
    sender: &Arc<dyn ChannelSender>,
    plugin_id: &str,
    chat_id: &str,
    topic: Option<crate::types::ChannelTopicContext>,
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
            topic,
        };

        match response.behavior {
            ActionBehavior::Edit => {
                if let Some(ref edit_id) = response.edit_message_id {
                    if let Err(error) = sender.edit_message(plugin_id, chat_id, edit_id, outgoing).await {
                        warn!(error = %error, plugin_id, chat_id, message_id = %edit_id, "failed to edit action response");
                    }
                }
            }
            _ => {
                if let Err(error) = sender.send_message(plugin_id, chat_id, outgoing).await {
                    warn!(error = %error, plugin_id, chat_id, "failed to send action response");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_dispatched(
    msg_svc: &Arc<ChannelMessageService>,
    session_mgr: &Arc<SessionManager>,
    sender: &Arc<dyn ChannelSender>,
    session_id: &str,
    conversation_id: Option<&str>,
    title_hint: Option<crate::types::ChannelConversationTitleHint>,
    text: &str,
    platform: crate::types::PluginType,
    plugin_id: &str,
    chat_id: &str,
    topic: Option<crate::types::ChannelTopicContext>,
) {
    let session = match session_mgr.get_session_by_id(session_id).await {
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

    if platform == PluginType::Telegram {
        let _ = sender
            .send_chat_action_in_topic(
                plugin_id,
                chat_id,
                "typing",
                topic.as_ref().map(|value| value.message_thread_id),
            )
            .await;
    }

    let send_result = match msg_svc
        .send_to_agent_with_title_hint(&session, text, platform, title_hint)
        .await
    {
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
                topic: topic.clone(),
            };
            let _ = sender.send_message(plugin_id, chat_id, err_msg).await;
            return;
        }
    };

    // Bind conversation to session if newly created
    if conversation_id.is_none()
        && let Err(e) = session_mgr
            .bind_conversation(session_id, &send_result.conversation_id)
            .await
    {
        warn!(error = %e, "failed to bind conversation to session");
    }

    // Spawn stream relay if we got a subscription
    if let Some(rx) = send_result.stream_rx {
        let relay_config = RelayConfig {
            platform,
            plugin_id: plugin_id.to_owned(),
            chat_id: chat_id.to_owned(),
            throttle_ms: 500,
        };
        let relay = ChannelStreamRelay::new(relay_config, Arc::clone(sender)).with_topic(topic.clone());
        tokio::spawn(relay.run(rx));
    } else {
        if send_result.team_routed {
            let formatted = format_outgoing_text_for_platform("⏳ 已收到，Team 正在处理...", platform);
            let processing_msg = UnifiedOutgoingMessage {
                message_type: OutgoingMessageType::Text,
                text: Some(formatted.text),
                parse_mode: formatted.parse_mode,
                buttons: None,
                keyboard: None,
                image_url: None,
                file_url: None,
                file_name: None,
                media_actions: None,
                reply_to_message_id: None,
                silent: None,
                topic: topic.clone(),
            };
            let processing_msg_id = sender.send_message(plugin_id, chat_id, processing_msg).await.ok();

            if platform == PluginType::Telegram
                && let Some(processing_msg_id) = processing_msg_id
            {
                let sender = Arc::clone(sender);
                let plugin_id = plugin_id.to_owned();
                let chat_id = chat_id.to_owned();
                let topic = topic.clone();
                tokio::spawn(async move {
                    for _ in 0..15 {
                        let _ = sender
                            .send_chat_action_in_topic(
                                &plugin_id,
                                &chat_id,
                                "typing",
                                topic.as_ref().map(|value| value.message_thread_id),
                            )
                            .await;
                        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    }
                    let done_msg = UnifiedOutgoingMessage {
                        message_type: OutgoingMessageType::Text,
                        text: Some("✅ Team 已开始处理，稍后会发送结果。".into()),
                        parse_mode: None,
                        buttons: None,
                        keyboard: None,
                        image_url: None,
                        file_url: None,
                        file_name: None,
                        media_actions: None,
                        reply_to_message_id: None,
                        silent: None,
                        topic,
                    };
                    let _ = sender
                        .edit_message(&plugin_id, &chat_id, &processing_msg_id, done_msg)
                        .await;
                });
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, sleep, timeout};

    #[tokio::test]
    async fn chat_message_sequencer_runs_same_chat_messages_in_enqueue_order() {
        let sequencer = ChatMessageSequencer::default();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, release_rx) = oneshot::channel::<()>();

        {
            let order = Arc::clone(&order);
            sequencer
                .spawn(PluginType::Telegram, "chat-1", None, async move {
                    order.lock().await.push("first-start");
                    let _ = release_rx.await;
                    order.lock().await.push("first-end");
                })
                .await;
        }

        {
            let order = Arc::clone(&order);
            let sequencer = sequencer.clone();
            sequencer
                .spawn(PluginType::Telegram, "chat-1", None, async move {
                    order.lock().await.push("second");
                })
                .await;
        }

        sleep(Duration::from_millis(25)).await;
        assert_eq!(*order.lock().await, vec!["first-start"]);

        let _ = release_tx.send(());
        timeout(Duration::from_secs(1), async {
            loop {
                if order.lock().await.len() == 3 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("queued same-chat task should run after the first task finishes");

        assert_eq!(*order.lock().await, vec!["first-start", "first-end", "second"]);
    }

    #[tokio::test]
    async fn chat_message_sequencer_does_not_block_other_topics_in_same_group() {
        let sequencer = ChatMessageSequencer::default();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (other_topic_tx, other_topic_rx) = oneshot::channel::<()>();

        sequencer
            .spawn(PluginType::Telegram, "group", Some(3), async move {
                let _ = release_rx.await;
            })
            .await;
        sequencer
            .spawn(PluginType::Telegram, "group", Some(5), async move {
                let _ = other_topic_tx.send(());
            })
            .await;

        timeout(Duration::from_secs(1), other_topic_rx)
            .await
            .expect("a different topic should use an independent queue")
            .expect("topic worker should signal completion");
        let _ = release_tx.send(());
    }
}
