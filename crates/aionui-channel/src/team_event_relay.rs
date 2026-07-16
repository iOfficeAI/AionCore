use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_db::models::{AssistantSessionRow, AssistantUserRow};
use aionui_db::{IChannelRepository, IConversationRepository};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::formatter::format_outgoing_text_for_platform;
use crate::message_service::ChannelMessageService;
use crate::stream_relay::ChannelSender;
use crate::types::{OutgoingMessageType, PluginType, UnifiedOutgoingMessage};

const MESSAGE_STREAM_EVENT: &str = "message.stream";
const TEAMMATE_MESSAGE_EVENT: &str = "team.teammateMessage";

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayEmission {
    Final { conversation_id: String, text: String },
    Error { conversation_id: String, text: String },
    Immediate { conversation_id: String, text: String },
}

#[derive(Debug, Default)]
struct TeamRelayAccumulator {
    buffers: HashMap<String, String>,
}

impl TeamRelayAccumulator {
    fn accept(&mut self, event: &WebSocketMessage<serde_json::Value>) -> Option<RelayEmission> {
        match event.name.as_str() {
            MESSAGE_STREAM_EVENT => self.accept_message_stream(&event.data),
            TEAMMATE_MESSAGE_EVENT => self.accept_teammate_message(&event.data),
            _ => None,
        }
    }

    fn accept_message_stream(&mut self, data: &serde_json::Value) -> Option<RelayEmission> {
        let conversation_id = data.get("conversation_id")?.as_str()?.to_owned();
        let event_type = data.get("type")?.as_str()?;
        if data.get("hidden").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            return None;
        }

        match event_type {
            "text" | "content" => {
                let content = data
                    .get("data")
                    .and_then(|v| v.get("content"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if content.trim().is_empty() {
                    return None;
                }
                let buffer = self.buffers.entry(conversation_id).or_default();
                if data
                    .get("replace")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    buffer.clear();
                }
                buffer.push_str(content);
                None
            }
            "finish" => {
                let text = self.buffers.remove(&conversation_id).unwrap_or_default();
                if text.trim().is_empty() {
                    None
                } else {
                    Some(RelayEmission::Final { conversation_id, text })
                }
            }
            "error" => {
                let text = data
                    .get("data")
                    .and_then(|v| v.get("message").or_else(|| v.get("content")))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Team processing failed")
                    .to_owned();
                self.buffers.remove(&conversation_id);
                Some(RelayEmission::Error { conversation_id, text })
            }
            _ => None,
        }
    }

    fn accept_teammate_message(&mut self, data: &serde_json::Value) -> Option<RelayEmission> {
        let conversation_id = data.get("conversation_id")?.as_str()?.to_owned();
        let content = data.get("content")?.as_str()?.trim();
        if content.is_empty() {
            return None;
        }
        let from_name = data
            .get("from_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Teammate");
        Some(RelayEmission::Immediate {
            conversation_id,
            text: format!("{from_name}: {content}"),
        })
    }
}

pub struct ChannelTeamEventRelay {
    event_rx: broadcast::Receiver<WebSocketMessage<serde_json::Value>>,
    channel_repo: Arc<dyn IChannelRepository>,
    conversation_repo: Arc<dyn IConversationRepository>,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelTeamEventRelay {
    pub fn new(
        event_rx: broadcast::Receiver<WebSocketMessage<serde_json::Value>>,
        channel_repo: Arc<dyn IChannelRepository>,
        conversation_repo: Arc<dyn IConversationRepository>,
        sender: Arc<dyn ChannelSender>,
    ) -> Self {
        Self {
            event_rx,
            channel_repo,
            conversation_repo,
            sender,
        }
    }

    pub async fn run(mut self) {
        let mut accumulator = TeamRelayAccumulator::default();
        loop {
            match self.event_rx.recv().await {
                Ok(event) => {
                    if let Some(emission) = accumulator.accept(&event) {
                        self.deliver(emission).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    warn!(count, "channel team event relay lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    async fn deliver(&self, emission: RelayEmission) {
        let (conversation_id, outgoing) = match emission {
            RelayEmission::Final { conversation_id, text } => {
                (conversation_id, ChannelMessageService::build_final_message(&text))
            }
            RelayEmission::Error { conversation_id, text } => (
                conversation_id,
                UnifiedOutgoingMessage {
                    topic: None,
                    message_type: OutgoingMessageType::Text,
                    text: Some(format!("❌ {text}")),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    image_url: None,
                    file_url: None,
                    file_name: None,
                    media_actions: None,
                    reply_to_message_id: None,
                    silent: None,
                },
            ),
            RelayEmission::Immediate { conversation_id, text } => (
                conversation_id,
                UnifiedOutgoingMessage {
                    topic: None,
                    message_type: OutgoingMessageType::Text,
                    text: Some(text),
                    parse_mode: None,
                    buttons: None,
                    keyboard: None,
                    image_url: None,
                    file_url: None,
                    file_name: None,
                    media_actions: None,
                    reply_to_message_id: None,
                    silent: None,
                },
            ),
        };

        let sessions = match self.channel_repo.get_all_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                warn!(error = %error, "failed to load channel sessions for team relay");
                return;
            }
        };
        let users = match self.channel_repo.get_all_users().await {
            Ok(users) => users
                .into_iter()
                .map(|user| (user.id.clone(), user))
                .collect::<HashMap<_, _>>(),
            Err(error) => {
                warn!(error = %error, "failed to load channel users for team relay");
                HashMap::new()
            }
        };
        let conversation_row = match self.conversation_repo.get(&conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                debug!(conversation_id = %conversation_id, "skipping channel team relay for missing conversation");
                return;
            }
            Err(error) => {
                warn!(error = %error, conversation_id = %conversation_id, "failed to load conversation for team relay");
                return;
            }
        };
        if !is_team_owned_conversation_extra(&conversation_row.extra) {
            debug!(conversation_id = %conversation_id, "skipping channel team relay for non-team conversation");
            return;
        }
        let conversation_plugin_id = source_to_plugin_id(conversation_row.source.as_deref());

        for session in sessions {
            if session.conversation_id.as_deref() != Some(conversation_id.as_str()) {
                continue;
            }
            let Some(chat_id) = session.chat_id.as_deref() else {
                continue;
            };
            let Some(plugin_id) = plugin_id_for_session(&session, &users, conversation_plugin_id.as_deref()) else {
                warn!(
                    conversation_id = %conversation_id,
                    session_id = %session.id,
                    user_id = %session.user_id,
                    "failed to resolve channel plugin for team relay"
                );
                continue;
            };
            let outgoing = format_outgoing_for_plugin(outgoing.clone(), &plugin_id);
            match self.sender.send_message(&plugin_id, chat_id, outgoing.clone()).await {
                Ok(_) => {
                    debug!(conversation_id = %conversation_id, chat_id = %chat_id, "team event relayed to channel")
                }
                Err(error) => {
                    warn!(error = %error, conversation_id = %conversation_id, chat_id = %chat_id, "failed to relay team event to channel")
                }
            }
        }
    }
}

fn plugin_id_for_session(
    session: &AssistantSessionRow,
    users: &HashMap<String, AssistantUserRow>,
    conversation_plugin_id: Option<&str>,
) -> Option<String> {
    users
        .get(&session.user_id)
        .and_then(|user| source_to_plugin_id(Some(user.platform_type.as_str())))
        .or_else(|| conversation_plugin_id.map(ToOwned::to_owned))
}

fn format_outgoing_for_plugin(mut outgoing: UnifiedOutgoingMessage, plugin_id: &str) -> UnifiedOutgoingMessage {
    let Some(platform) = PluginType::from_str_opt(plugin_id) else {
        return outgoing;
    };
    let Some(text) = outgoing.text.as_deref() else {
        return outgoing;
    };
    let formatted = format_outgoing_text_for_platform(text, platform);
    outgoing.text = Some(formatted.text);
    outgoing.parse_mode = formatted.parse_mode;
    outgoing
}

fn source_to_plugin_id(source: Option<&str>) -> Option<String> {
    let plugin = match source? {
        "telegram" => PluginType::Telegram,
        "lark" => PluginType::Lark,
        "dingtalk" => PluginType::Dingtalk,
        "weixin" => PluginType::Weixin,
        "slack" => PluginType::Slack,
        "discord" => PluginType::Discord,
        _ => return None,
    };
    Some(plugin.to_string())
}

fn is_team_owned_conversation_extra(extra: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(extra)
        .ok()
        .and_then(|value| {
            value
                .get("teamId")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .map(str::to_owned)
        })
        .is_some_and(|team_id| !team_id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_common::now_ms;
    use serde_json::json;

    #[test]
    fn accumulator_buffers_stream_chunks_until_finish() {
        let mut acc = TeamRelayAccumulator::default();
        assert_eq!(
            acc.accept(&WebSocketMessage::new(
                MESSAGE_STREAM_EVENT,
                json!({"conversation_id":"c1","type":"content","data":{"content":"hello "},"hidden":false})
            )),
            None
        );
        assert_eq!(
            acc.accept(&WebSocketMessage::new(
                MESSAGE_STREAM_EVENT,
                json!({"conversation_id":"c1","type":"content","data":{"content":"team"},"hidden":false})
            )),
            None
        );
        assert_eq!(
            acc.accept(&WebSocketMessage::new(
                MESSAGE_STREAM_EVENT,
                json!({"conversation_id":"c1","type":"finish","data":{},"hidden":false})
            )),
            Some(RelayEmission::Final {
                conversation_id: "c1".into(),
                text: "hello team".into(),
            })
        );
    }

    #[test]
    fn accumulator_formats_teammate_message() {
        let mut acc = TeamRelayAccumulator::default();
        assert_eq!(
            acc.accept(&WebSocketMessage::new(
                TEAMMATE_MESSAGE_EVENT,
                json!({"conversation_id":"c1","from_name":"Risk","content":"watch leverage"})
            )),
            Some(RelayEmission::Immediate {
                conversation_id: "c1".into(),
                text: "Risk: watch leverage".into(),
            })
        );
    }

    #[test]
    fn relay_resolves_plugin_from_bound_channel_session_before_conversation_source() {
        let now = now_ms();
        let session = AssistantSessionRow {
            id: "session-1".into(),
            user_id: "user-1".into(),
            agent_type: "acp".into(),
            conversation_id: Some("team-lead-conversation".into()),
            workspace: None,
            chat_id: Some("chat-1".into()),
            message_thread_id: None,
            bound_agent_id: None,
            bound_backend: None,
            bound_provider_id: None,
            bound_model: None,
            created_at: now,
            last_activity: now,
        };
        let user = AssistantUserRow {
            id: "user-1".into(),
            platform_user_id: "telegram-user".into(),
            platform_type: "telegram".into(),
            display_name: None,
            authorized_at: now,
            last_active: None,
            session_id: None,
        };
        let users = HashMap::from([(user.id.clone(), user)]);

        assert_eq!(
            plugin_id_for_session(&session, &users, Some("lark")),
            Some("telegram".into()),
            "team conversations created by WebUI may have source=aionui, so channel relay must route by bound session"
        );
    }

    #[test]
    fn team_relay_only_treats_conversation_extra_with_team_id_as_team_owned() {
        assert!(is_team_owned_conversation_extra(r#"{"teamId":"team-1"}"#));
        assert!(!is_team_owned_conversation_extra("{}"));
        assert!(!is_team_owned_conversation_extra(r#"{"teamId":""}"#));
        assert!(!is_team_owned_conversation_extra("not-json"));
    }
}
