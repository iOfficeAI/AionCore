use std::sync::{Arc, Mutex};

use aionui_ai_agent::AgentStreamEvent;
use aionui_ai_agent::protocol::events::{
    AcpPermissionEventData, ErrorEventData, FinishEventData, TextEventData, ToolCallEventData, ToolCallStatus,
};
use aionui_channel::approval::{ChannelApprovalContext, ChannelApprovalPort};
use aionui_channel::error::ChannelError;
use aionui_channel::stream_relay::{ChannelStreamRelay, MessageRecorder, RelayConfig};
use aionui_channel::types::PluginType;
use aionui_common::{Confirmation, ConfirmationOption};
use async_trait::async_trait;
use tokio::sync::broadcast;

#[derive(Default)]
struct RecordingApprovalPort {
    created: Mutex<Vec<(ChannelApprovalContext, Confirmation)>>,
}

#[async_trait]
impl ChannelApprovalPort for RecordingApprovalPort {
    async fn create(
        &self,
        context: ChannelApprovalContext,
        confirmation: Confirmation,
    ) -> Result<String, ChannelError> {
        self.created.lock().unwrap().push((context, confirmation));
        Ok("approval1234567".into())
    }

    async fn resolve(
        &self,
        _source_user_id: &str,
        _platform: PluginType,
        _chat_id: &str,
        _message_thread_id: Option<i64>,
        _approval_id: &str,
        _option_index: usize,
    ) -> Result<String, ChannelError> {
        Ok("approved".into())
    }
}

// ── RelayConfig construction ─────────────────────────────────────

#[test]
fn relay_config_fields() {
    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "123".into(),
        throttle_ms: 500,
    };
    assert_eq!(config.throttle_ms, 500);
    assert_eq!(config.plugin_id, "telegram");
}

// ── Full relay run with mock ChannelSender ───────────────────────

#[tokio::test]
async fn relay_sends_thinking_then_final_message() {
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10,
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());

    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::Text(TextEventData {
            content: "Hello".into(),
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Text(TextEventData {
            content: " World".into(),
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }))
        .unwrap();

    relay.run(rx).await;

    let sends = recorder.take_sends();
    assert!(!sends.is_empty());
    assert!(sends[0].text.as_deref().unwrap().contains("Thinking"));

    let edits = recorder.take_edits();
    let last = edits.last().unwrap();
    assert!(last.text.as_deref().unwrap().contains("Hello World"));
    assert!(last.buttons.is_none());
}

#[tokio::test]
async fn relay_handles_error_event() {
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10,
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());
    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::Error(ErrorEventData::legacy("timeout", None)))
        .unwrap();

    relay.run(rx).await;

    let edits = recorder.take_edits();
    let last = edits.last().unwrap();
    assert!(last.text.as_deref().unwrap().contains("timeout"));
}

#[tokio::test]
async fn weixin_flushes_pending_text_before_tool_call() {
    // Port of AionUi TS fix `406a62665` to the backend relay layer. On
    // WeChat, in-place editing is not supported, so a tool-status update
    // would otherwise overwrite any assistant text the user hasn't yet
    // seen. The relay should flush buffered text as an independent
    // send_message before rendering the tool-call indicator, matching the
    // TS WeixinPlugin.sendTextNow draft-flush behaviour.
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Weixin,
        plugin_id: "weixin".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10_000, // large throttle so the mid-stream edit doesn't fire
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());
    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::Text(TextEventData {
            content: "Here is the plan:".into(),
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "read_file".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }))
        .unwrap();

    relay.run(rx).await;

    let sends = recorder.take_sends();
    // WeChat relay does NOT send a "Thinking..." placeholder. The first
    // send_message should be the flushed assistant text triggered by the
    // ToolCall event.
    assert!(!sends.is_empty(), "expected flush send_message, got {:?}", sends);
    let flushed = &sends[0];
    assert!(
        flushed.text.as_deref().unwrap().contains("Here is the plan"),
        "expected flushed text, got {:?}",
        flushed.text
    );
}

#[tokio::test]
async fn telegram_does_not_flush_text_before_tool_call() {
    // Non-WeChat platforms support edit_message, so the TS flush rule does
    // not apply — the relay should continue to edit the placeholder in
    // place without issuing a new send_message for the buffered text.
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10_000,
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());
    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::Text(TextEventData {
            content: "Here is the plan:".into(),
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "read_file".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }))
        .unwrap();

    relay.run(rx).await;

    let sends = recorder.take_sends();
    // Only the "Thinking..." placeholder is sent — no flush on non-WeChat.
    assert_eq!(sends.len(), 1, "unexpected extra sends: {:?}", sends);
}

#[tokio::test]
async fn weixin_skips_flush_when_buffer_is_empty() {
    // Tool call before any assistant text should not trigger a blank flush.
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Weixin,
        plugin_id: "weixin".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10_000,
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());
    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "read_file".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }))
        .unwrap();

    relay.run(rx).await;

    let sends = recorder.take_sends();
    // WeChat relay does NOT send Thinking placeholder, and with no buffered
    // text there should be zero sends (no flush needed).
    assert_eq!(sends.len(), 0, "no sends expected for empty buffer: {:?}", sends);
}

#[tokio::test]
async fn relay_handles_channel_closed() {
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10,
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone());
    let rx = event_tx.subscribe();

    event_tx
        .send(AgentStreamEvent::Text(TextEventData {
            content: "partial".into(),
        }))
        .unwrap();
    drop(event_tx);

    relay.run(rx).await;

    let edits = recorder.take_edits();
    assert!(!edits.is_empty());
    assert!(edits.last().unwrap().text.as_deref().unwrap().contains("partial"));
}

#[tokio::test]
async fn telegram_permission_event_creates_topic_scoped_approval_buttons() {
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());
    let approvals = Arc::new(RecordingApprovalPort::default());
    let config = RelayConfig {
        platform: PluginType::Telegram,
        plugin_id: "telegram".into(),
        chat_id: "chat_1".into(),
        throttle_ms: 10,
    };
    let context = ChannelApprovalContext {
        source_user_id: "assistant-user-1".into(),
        conversation_id: "conversation-1".into(),
        agent_id: Some("claude-code".into()),
        platform: PluginType::Telegram,
        chat_id: "chat_1".into(),
        message_thread_id: Some(5),
    };
    let relay = ChannelStreamRelay::new(config, recorder.clone())
        .with_approval_port(approvals.clone(), context)
        .with_topic(Some(aionui_channel::types::ChannelTopicContext {
            message_thread_id: 5,
        }));
    let rx = event_tx.subscribe();
    event_tx
        .send(AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(
            Confirmation {
                id: "confirmation-1".into(),
                call_id: "call-1".into(),
                title: Some("Run tests?".into()),
                action: None,
                description: "cargo test".into(),
                command_type: Some("execute".into()),
                options: vec![
                    ConfirmationOption {
                        label: "Allow once".into(),
                        value: serde_json::json!("allow_once"),
                        params: None,
                    },
                    ConfirmationOption {
                        label: "Reject".into(),
                        value: serde_json::json!("reject"),
                        params: None,
                    },
                ],
            },
        )))
        .unwrap();
    event_tx
        .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }))
        .unwrap();

    relay.run(rx).await;

    assert_eq!(approvals.created.lock().unwrap().len(), 1);
    let sends = recorder.take_sends();
    let prompt = sends
        .iter()
        .find(|message| message.buttons.is_some())
        .expect("approval prompt");
    assert_eq!(prompt.topic.as_ref().unwrap().message_thread_id, 5);
    let buttons = prompt.buttons.as_ref().unwrap();
    assert_eq!(buttons[0][0].action, "approval.resolve");
    assert_eq!(buttons[0][0].params.as_ref().unwrap()["id"], "approval1234567");
}
