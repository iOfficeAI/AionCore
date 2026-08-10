use super::*;
use crate::types::MessageContentType;
use serde_json::json;

// -- Web API response parsing ------------------------------------------------

#[test]
fn auth_test_response_ok() {
    let raw = json!({ "ok": true, "user": "aion_bot", "user_id": "U0BOT", "bot_id": "B0BOT" });
    let resp: AuthTestResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.ok);
    assert_eq!(resp.user_id.as_deref(), Some("U0BOT"));
    assert_eq!(resp.user.as_deref(), Some("aion_bot"));
}

#[test]
fn auth_test_response_error() {
    let raw = json!({ "ok": false, "error": "invalid_auth" });
    let resp: AuthTestResponse = serde_json::from_value(raw).unwrap();
    assert!(!resp.ok);
    assert_eq!(resp.error.as_deref(), Some("invalid_auth"));
    assert!(resp.user_id.is_none());
}

#[test]
fn open_connection_response_ok() {
    let raw = json!({ "ok": true, "url": "wss://wss-primary.slack.com/link/?ticket=abc" });
    let resp: OpenConnectionResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.ok);
    assert_eq!(
        resp.url.as_deref(),
        Some("wss://wss-primary.slack.com/link/?ticket=abc")
    );
}

#[test]
fn open_connection_response_error() {
    let raw = json!({ "ok": false, "error": "not_allowed_token_type" });
    let resp: OpenConnectionResponse = serde_json::from_value(raw).unwrap();
    assert!(!resp.ok);
    assert!(resp.url.is_none());
}

#[test]
fn post_message_response_ok() {
    let raw = json!({ "ok": true, "ts": "1700000000.000100", "channel": "C0123" });
    let resp: PostMessageResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.ok);
    assert_eq!(resp.ts.as_deref(), Some("1700000000.000100"));
}

#[test]
fn post_message_request_omits_none_thread() {
    let req = PostMessageRequest {
        channel: "C1".into(),
        text: "hi".into(),
        thread_ts: None,
    };
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["channel"], "C1");
    assert!(v.get("thread_ts").is_none());
}

#[test]
fn post_message_request_includes_thread() {
    let req = PostMessageRequest {
        channel: "C1".into(),
        text: "hi".into(),
        thread_ts: Some("1700000000.000001".into()),
    };
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["thread_ts"], "1700000000.000001");
}

#[test]
fn update_message_response_error() {
    let raw = json!({ "ok": false, "error": "message_not_found" });
    let resp: UpdateMessageResponse = serde_json::from_value(raw).unwrap();
    assert!(!resp.ok);
    assert_eq!(resp.error.as_deref(), Some("message_not_found"));
}

// -- Socket Mode envelope parsing --------------------------------------------

#[test]
fn envelope_hello_parses() {
    let raw = json!({ "type": "hello", "num_connections": 1 });
    let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
    assert_eq!(env.envelope_type, "hello");
    assert!(env.envelope_id.is_none());
}

#[test]
fn envelope_disconnect_parses() {
    let raw = json!({ "type": "disconnect", "reason": "refresh_requested" });
    let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
    assert_eq!(env.envelope_type, "disconnect");
    assert_eq!(env.reason.as_deref(), Some("refresh_requested"));
}

#[test]
fn envelope_events_api_parses_with_payload() {
    let raw = json!({
        "type": "events_api",
        "envelope_id": "env-123",
        "payload": {
            "type": "event_callback",
            "event_id": "Ev0001",
            "event": { "type": "app_mention", "user": "U1", "text": "<@U0BOT> hi", "ts": "1700000000.000100", "channel": "C1" }
        }
    });
    let env: SocketEnvelope = serde_json::from_value(raw).unwrap();
    assert_eq!(env.envelope_type, "events_api");
    assert_eq!(env.envelope_id.as_deref(), Some("env-123"));
    let cb: EventCallback = serde_json::from_value(env.payload.unwrap()).unwrap();
    assert_eq!(cb.event_id.as_deref(), Some("Ev0001"));
    assert_eq!(cb.event.unwrap().event_type, "app_mention");
}

// -- build_ack ---------------------------------------------------------------

#[test]
fn build_ack_echoes_envelope_id() {
    let ack = build_ack("env-xyz");
    let v: serde_json::Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(v["envelope_id"], "env-xyz");
}

// -- parse_ts_to_ms ----------------------------------------------------------

#[test]
fn parse_ts_seconds_micros() {
    assert_eq!(parse_ts_to_ms("1700000000.000100"), 1_700_000_000_000);
}

#[test]
fn parse_ts_invalid_returns_zero() {
    assert_eq!(parse_ts_to_ms("not-a-ts"), 0);
}

// -- strip_bot_mention -------------------------------------------------------

#[test]
fn strip_mention_leading() {
    assert_eq!(strip_bot_mention("<@U0BOT> hello there", "U0BOT"), "hello there");
}

#[test]
fn strip_mention_absent_is_noop() {
    assert_eq!(strip_bot_mention("plain text", "U0BOT"), "plain text");
}

// -- event_to_unified --------------------------------------------------------

fn app_mention_event() -> SlackEvent {
    SlackEvent {
        event_type: "app_mention".into(),
        user: Some("U1".into()),
        text: Some("<@U0BOT> summarize this".into()),
        ts: Some("1700000000.000100".into()),
        channel: Some("C1".into()),
        channel_type: None,
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: Some("cmid-1".into()),
    }
}

#[test]
fn app_mention_normalizes_and_strips_mention() {
    let msg = event_to_unified(&app_mention_event(), "U0BOT").unwrap();
    assert_eq!(msg.platform, PluginType::Slack);
    assert_eq!(msg.chat_id, "C1");
    assert_eq!(msg.user.id, "U1");
    assert_eq!(msg.content.content_type, MessageContentType::Text);
    assert_eq!(msg.content.text, "summarize this");
    assert_eq!(msg.id, "cmid-1");
    assert_eq!(msg.timestamp, 1_700_000_000_000);
}

#[test]
fn direct_message_normalizes() {
    let evt = SlackEvent {
        event_type: "message".into(),
        user: Some("U9".into()),
        text: Some("hi bot".into()),
        ts: Some("1700000001.000200".into()),
        channel: Some("D1".into()),
        channel_type: Some("im".into()),
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: None,
    };
    let msg = event_to_unified(&evt, "U0BOT").unwrap();
    assert_eq!(msg.chat_id, "D1");
    assert_eq!(msg.content.text, "hi bot");
    // No client_msg_id → id falls back to ts.
    assert_eq!(msg.id, "1700000001.000200");
}

#[test]
fn thread_ts_maps_to_reply_to() {
    let mut evt = app_mention_event();
    evt.thread_ts = Some("1700000000.000001".into());
    let msg = event_to_unified(&evt, "U0BOT").unwrap();
    assert_eq!(msg.reply_to_message_id.as_deref(), Some("1700000000.000001"));
}

#[test]
fn bot_own_message_is_ignored() {
    let mut evt = app_mention_event();
    evt.user = Some("U0BOT".into());
    assert!(event_to_unified(&evt, "U0BOT").is_none());
}

#[test]
fn other_bot_message_is_ignored() {
    let evt = SlackEvent {
        event_type: "message".into(),
        user: Some("U2".into()),
        text: Some("automated".into()),
        ts: Some("1700000002.0".into()),
        channel: Some("C1".into()),
        channel_type: None,
        thread_ts: None,
        bot_id: Some("B999".into()),
        subtype: None,
        client_msg_id: None,
    };
    assert!(event_to_unified(&evt, "U0BOT").is_none());
}

#[test]
fn message_changed_subtype_is_ignored() {
    let evt = SlackEvent {
        event_type: "message".into(),
        user: Some("U1".into()),
        text: Some("edited".into()),
        ts: Some("1700000003.0".into()),
        channel: Some("C1".into()),
        channel_type: Some("channel".into()),
        thread_ts: None,
        bot_id: None,
        subtype: Some("message_changed".into()),
        client_msg_id: None,
    };
    assert!(event_to_unified(&evt, "U0BOT").is_none());
}

#[test]
fn unhandled_event_type_is_ignored() {
    let evt = SlackEvent {
        event_type: "reaction_added".into(),
        user: Some("U1".into()),
        text: None,
        ts: Some("1700000004.0".into()),
        channel: Some("C1".into()),
        channel_type: None,
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: None,
    };
    assert!(event_to_unified(&evt, "U0BOT").is_none());
}

// -- M1 DM gate --------------------------------------------------------------

/// A `message` event from a public channel (channel_type "channel") must be
/// dropped — the DM gate blocks a future channel-level subscription from
/// leaking whole-channel traffic to the agent.
#[test]
fn public_channel_message_is_rejected() {
    let evt = SlackEvent {
        event_type: "message".into(),
        user: Some("U1".into()),
        text: Some("hello channel".into()),
        ts: Some("1700000005.000100".into()),
        channel: Some("C1".into()),
        channel_type: Some("channel".into()),
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: None,
    };
    assert!(event_to_unified(&evt, "U0BOT").is_none());
}

/// A `message` event with no channel_type but a `D`-prefixed channel id is
/// still treated as a DM (fallback path).
#[test]
fn dm_via_channel_prefix_is_accepted() {
    let evt = SlackEvent {
        event_type: "message".into(),
        user: Some("U1".into()),
        text: Some("hi".into()),
        ts: Some("1700000006.000100".into()),
        channel: Some("D9".into()),
        channel_type: None,
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: None,
    };
    assert!(event_to_unified(&evt, "U0BOT").is_some());
}

/// Regression guard: the DM gate must NOT touch `app_mention`. A public-channel
/// @mention (channel_type "channel") must still route to the agent.
#[test]
fn app_mention_in_public_channel_is_not_gated() {
    let evt = SlackEvent {
        event_type: "app_mention".into(),
        user: Some("U1".into()),
        text: Some("<@U0BOT> help".into()),
        ts: Some("1700000007.000100".into()),
        channel: Some("C1".into()),
        channel_type: Some("channel".into()),
        thread_ts: None,
        bot_id: None,
        subtype: None,
        client_msg_id: None,
    };
    let msg = event_to_unified(&evt, "U0BOT").unwrap();
    assert_eq!(msg.chat_id, "C1");
    assert_eq!(msg.content.text, "help");
}
