use std::sync::Arc;

use aionui_ai_agent::AgentStreamEvent;
use aionui_ai_agent::protocol::events::{
    ErrorEventData, FinishEventData, TextEventData, ToolCallEventData, ToolCallStatus,
};
use aionui_channel::stream_relay::{ChannelStreamRelay, MessageRecorder, RelayConfig, throttle_ms_for_platform};
use aionui_channel::types::PluginType;
use tokio::sync::broadcast;

// ── RelayConfig construction ─────────────────────────────────────

#[test]
fn slack_uses_larger_throttle_than_others() {
    // Slack's chat.update is rate-limited harder, so it gets a wider interval;
    // the other editable platforms must stay at the original 500 ms.
    assert_eq!(throttle_ms_for_platform(PluginType::Slack), 1200);
    assert_eq!(throttle_ms_for_platform(PluginType::Discord), 1000);
    assert_eq!(throttle_ms_for_platform(PluginType::Telegram), 500);
    assert_eq!(throttle_ms_for_platform(PluginType::Lark), 500);
    assert_eq!(throttle_ms_for_platform(PluginType::Dingtalk), 500);
    assert_eq!(throttle_ms_for_platform(PluginType::Weixin), 500);
}

#[test]
fn relay_config_fields() {
    let config = RelayConfig {
        owner_user_id: "system_default_user".to_owned(),
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
        owner_user_id: "system_default_user".to_owned(),
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
    assert!(last.buttons.is_some());
}

#[tokio::test]
async fn relay_handles_error_event() {
    let (event_tx, _) = broadcast::channel::<AgentStreamEvent>(64);
    let recorder = Arc::new(MessageRecorder::new());

    let config = RelayConfig {
        owner_user_id: "system_default_user".to_owned(),
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
        owner_user_id: "system_default_user".to_owned(),
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
        owner_user_id: "system_default_user".to_owned(),
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
        owner_user_id: "system_default_user".to_owned(),
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
        owner_user_id: "system_default_user".to_owned(),
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
