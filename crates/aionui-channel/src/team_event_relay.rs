use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_common::now_ms;
use aionui_db::models::{AssistantSessionRow, AssistantUserRow};
use aionui_db::{
    IApprovalRepository, IChannelRepository, IConversationRepository, IDevelopmentOperationsRepository,
    IDevelopmentRepository, IProjectRepository,
};
use tokio::sync::broadcast;
use tokio::time::{Duration, interval};
use tracing::{debug, warn};

use crate::formatter::format_outgoing_text_for_platform;
use crate::message_service::ChannelMessageService;
use crate::stream_relay::ChannelSender;
use crate::types::{ActionButton, ChannelTopicContext, OutgoingMessageType, PluginType, UnifiedOutgoingMessage};

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
            let mut outgoing = format_outgoing_for_plugin(outgoing.clone(), &plugin_id);
            outgoing.topic = session
                .message_thread_id
                .map(|message_thread_id| ChannelTopicContext { message_thread_id });
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

#[derive(Clone)]
pub struct ChannelDevelopmentNotifier {
    owner_user_id: String,
    channel_repo: Arc<dyn IChannelRepository>,
    project_repo: Arc<dyn IProjectRepository>,
    development_repo: Arc<dyn IDevelopmentRepository>,
    operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
    approval_repo: Arc<dyn IApprovalRepository>,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelDevelopmentNotifier {
    pub fn new(
        owner_user_id: String,
        channel_repo: Arc<dyn IChannelRepository>,
        project_repo: Arc<dyn IProjectRepository>,
        development_repo: Arc<dyn IDevelopmentRepository>,
        operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
        approval_repo: Arc<dyn IApprovalRepository>,
        sender: Arc<dyn ChannelSender>,
    ) -> Self {
        Self {
            owner_user_id,
            channel_repo,
            project_repo,
            development_repo,
            operations_repo,
            approval_repo,
            sender,
        }
    }

    pub async fn run(self) {
        let mut seen = HashSet::new();
        let mut ticker = interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            if let Err(error) = self.scan(&mut seen).await {
                warn!(error = %error, "development channel notification scan failed");
            }
        }
    }

    async fn scan(&self, seen: &mut HashSet<String>) -> Result<(), String> {
        self.notify_pending_approvals(seen).await?;
        for project in self
            .project_repo
            .list_for_user(&self.owner_user_id)
            .await
            .map_err(|error| error.to_string())?
        {
            let conversations = self
                .project_repo
                .list_resource_links(&project.id, &self.owner_user_id)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|link| link.resource_type == "conversation")
                .map(|link| link.resource_id)
                .collect::<Vec<_>>();
            if conversations.is_empty() {
                continue;
            }
            let runs = self
                .development_repo
                .list_runs(&self.owner_user_id, Some(&project.id))
                .await
                .map_err(|error| error.to_string())?;
            for run in runs {
                let mut notices = Vec::new();
                if matches!(run.status.as_str(), "succeeded" | "failed") {
                    notices.push((
                        format!("run:{}:{}", run.id, run.status),
                        if run.status == "succeeded" {
                            "completion"
                        } else {
                            "crash"
                        },
                        format!("开发运行 {}：{}", run.id, run.status),
                    ));
                }
                for gate in self
                    .development_repo
                    .list_gates(&run.id, None)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|gate| matches!(gate.status.as_str(), "failed" | "timed_out" | "interrupted"))
                {
                    let kind = if gate.status == "timed_out" {
                        "timeout"
                    } else {
                        "test_failure"
                    };
                    notices.push((
                        format!("gate:{}:{}", gate.id, gate.status),
                        kind,
                        format!("质量门禁 {}：{}", gate.gate_type, gate.status),
                    ));
                }
                for task in self
                    .development_repo
                    .list_tasks(&run.id)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|task| task.status == "conflict")
                {
                    notices.push((
                        format!("task:{}:conflict", task.id),
                        "conflict",
                        format!("任务发生冲突：{}", task.subject),
                    ));
                }
                for alert in self
                    .operations_repo
                    .list_alerts(&self.owner_user_id, &project.id, Some(&run.id), true)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    notices.push((
                        format!("alert:{}:{}", alert.id, alert.updated_at),
                        if alert.alert_type == "budget" {
                            "budget"
                        } else {
                            "alert"
                        },
                        alert.message,
                    ));
                }
                for recovery in self
                    .operations_repo
                    .list_recovery(&self.owner_user_id, &project.id, Some(&run.id), 20)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    notices.push((
                        format!("recovery:{}:{}", recovery.id, recovery.decision),
                        "crash",
                        format!("运行恢复：{}（{}）", recovery.finding, recovery.decision),
                    ));
                }
                for (key, kind, detail) in notices {
                    if !seen.insert(key) {
                        continue;
                    }
                    let outgoing = notice_message(kind, &run.id, &detail);
                    for conversation_id in &conversations {
                        self.send_to_conversation(conversation_id, outgoing.clone()).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn notify_pending_approvals(&self, seen: &mut HashSet<String>) -> Result<(), String> {
        let now = now_ms();
        for approval in self
            .approval_repo
            .list_for_user(&self.owner_user_id, None)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|approval| approval.status == "pending" && approval.expires_at > now)
        {
            if !seen.insert(format!("approval:{}", approval.id)) {
                continue;
            }
            let (Some(plugin_id), Some(chat_id)) =
                (approval.source_channel.as_deref(), approval.source_chat_id.as_deref())
            else {
                continue;
            };
            let options = serde_json::from_str::<Vec<serde_json::Value>>(&approval.options).unwrap_or_default();
            let buttons = options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    vec![ActionButton {
                        label: option
                            .get("label")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("选择")
                            .to_owned(),
                        action: "approval.resolve".into(),
                        params: Some(HashMap::from([
                            ("id".into(), approval.id.clone()),
                            ("o".into(), index.to_string()),
                        ])),
                    }]
                })
                .collect::<Vec<_>>();
            let mut outgoing = notice_message(
                "approval",
                approval.run_id.as_deref().unwrap_or("unknown"),
                &format!("{} · 风险 {}", approval.action_type, approval.risk_level),
            );
            outgoing.message_type = OutgoingMessageType::Buttons;
            outgoing.buttons = Some(buttons);
            outgoing.topic = approval
                .source_thread_id
                .map(|message_thread_id| ChannelTopicContext { message_thread_id });
            self.sender
                .send_message(plugin_id, chat_id, format_outgoing_for_plugin(outgoing, plugin_id))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn send_to_conversation(
        &self,
        conversation_id: &str,
        outgoing: UnifiedOutgoingMessage,
    ) -> Result<(), String> {
        let sessions = self
            .channel_repo
            .get_all_sessions()
            .await
            .map_err(|error| error.to_string())?;
        let users = self
            .channel_repo
            .get_all_users()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|user| (user.id.clone(), user))
            .collect::<HashMap<_, _>>();
        for session in sessions
            .into_iter()
            .filter(|session| session.conversation_id.as_deref() == Some(conversation_id))
        {
            let Some(chat_id) = session.chat_id.as_deref() else {
                continue;
            };
            let Some(plugin_id) = plugin_id_for_session(&session, &users, None) else {
                continue;
            };
            let mut message = format_outgoing_for_plugin(outgoing.clone(), &plugin_id);
            message.topic = session
                .message_thread_id
                .map(|message_thread_id| ChannelTopicContext { message_thread_id });
            self.sender
                .send_message(&plugin_id, chat_id, message)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn notice_message(kind: &str, run_id: &str, detail: &str) -> UnifiedOutgoingMessage {
    let title = match kind {
        "approval" => "⏳ 需要审批",
        "test_failure" => "❌ 测试失败",
        "timeout" => "⏱️ 执行超时",
        "crash" => "💥 运行异常",
        "conflict" => "⚠️ 发现冲突",
        "budget" => "💰 预算提醒",
        "completion" => "✅ 运行完成",
        _ => "ℹ️ 开发运行通知",
    };
    UnifiedOutgoingMessage {
        topic: None,
        message_type: OutgoingMessageType::Text,
        text: Some(format!(
            "{title}\nRun: {run_id}\n{detail}\n使用 /run_info 查看状态，/handoff 转到 Web。"
        )),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        image_url: None,
        file_url: None,
        file_name: None,
        media_actions: None,
        reply_to_message_id: None,
        silent: None,
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

    #[test]
    fn development_notices_cover_all_proactive_categories_without_payload_details() {
        let cases = [
            ("approval", "需要审批"),
            ("test_failure", "测试失败"),
            ("timeout", "执行超时"),
            ("crash", "运行异常"),
            ("conflict", "发现冲突"),
            ("budget", "预算提醒"),
            ("completion", "运行完成"),
        ];
        for (kind, expected) in cases {
            let message = notice_message(kind, "run-1", "sanitized detail");
            let text = message.text.unwrap();
            assert!(text.contains(expected), "kind={kind}: {text}");
            assert!(text.contains("Run: run-1"));
            assert!(text.contains("/handoff"));
        }
    }

    #[test]
    fn team_relay_restores_the_session_topic_on_outgoing_messages() {
        let mut message = notice_message("completion", "run-1", "done");
        let session = AssistantSessionRow {
            id: "session-topic".into(),
            user_id: "user-1".into(),
            agent_type: "acp".into(),
            conversation_id: Some("conversation-1".into()),
            workspace: None,
            chat_id: Some("chat-1".into()),
            message_thread_id: Some(7),
            bound_agent_id: None,
            bound_backend: None,
            bound_provider_id: None,
            bound_model: None,
            created_at: 1,
            last_activity: 1,
        };
        message.topic = session
            .message_thread_id
            .map(|message_thread_id| ChannelTopicContext { message_thread_id });
        assert_eq!(message.topic.unwrap().message_thread_id, 7);
    }
}
