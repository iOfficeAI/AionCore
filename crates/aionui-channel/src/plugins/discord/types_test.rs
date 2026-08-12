use super::*;
use crate::types::MessageContentType;
use serde_json::json;

// -- intents / builders ------------------------------------------------------

#[test]
fn intents_bitfield_is_expected_sum() {
    // GUILDS(1) + GUILD_MESSAGES(512) + DIRECT_MESSAGES(4096) + MESSAGE_CONTENT(32768)
    assert_eq!(DISCORD_INTENTS, 1 + 512 + 4096 + 32768);
}

#[test]
fn identify_has_op_and_intents() {
    let v: serde_json::Value = serde_json::from_str(&build_identify("tok")).unwrap();
    assert_eq!(v["op"], op::IDENTIFY);
    assert_eq!(v["d"]["token"], "tok");
    assert_eq!(v["d"]["intents"], DISCORD_INTENTS);
    assert_eq!(v["d"]["properties"]["browser"], "aionui");
}

#[test]
fn heartbeat_carries_seq_or_null() {
    let with = serde_json::from_str::<serde_json::Value>(&build_heartbeat(Some(42))).unwrap();
    assert_eq!(with["op"], op::HEARTBEAT);
    assert_eq!(with["d"], 42);
    let without = serde_json::from_str::<serde_json::Value>(&build_heartbeat(None)).unwrap();
    assert!(without["d"].is_null());
}

#[test]
fn resume_has_op_and_fields() {
    let v: serde_json::Value = serde_json::from_str(&build_resume("tok", "sess", 7)).unwrap();
    assert_eq!(v["op"], op::RESUME);
    assert_eq!(v["d"]["session_id"], "sess");
    assert_eq!(v["d"]["seq"], 7);
}

// -- snowflake_to_ms ---------------------------------------------------------

#[test]
fn snowflake_matches_discord_documented_example() {
    // Discord docs example: snowflake 175928847299117063 → 1462015105796 ms.
    assert_eq!(snowflake_to_ms("175928847299117063"), 1_462_015_105_796);
}

#[test]
fn snowflake_invalid_returns_zero() {
    assert_eq!(snowflake_to_ms("not-a-snowflake"), 0);
}

// -- strip_bot_mention -------------------------------------------------------

#[test]
fn strip_plain_mention() {
    assert_eq!(strip_bot_mention("<@123> hello", "123"), "hello");
}

#[test]
fn strip_nick_mention() {
    assert_eq!(strip_bot_mention("<@!123> hi there", "123"), "hi there");
}

#[test]
fn strip_absent_is_noop() {
    assert_eq!(strip_bot_mention("plain", "123"), "plain");
}

// -- REST / gateway serde ----------------------------------------------------

#[test]
fn gateway_bot_response_parses() {
    let r: GatewayBotResponse = serde_json::from_value(json!({ "url": "wss://gateway.discord.gg" })).unwrap();
    assert_eq!(r.url, "wss://gateway.discord.gg");
}

#[test]
fn hello_parses() {
    let h: Hello = serde_json::from_value(json!({ "heartbeat_interval": 41250 })).unwrap();
    assert_eq!(h.heartbeat_interval, 41250);
}

#[test]
fn ready_parses() {
    let r: Ready = serde_json::from_value(json!({
        "session_id": "s1",
        "resume_gateway_url": "wss://resume.example",
        "user": { "id": "BOT1" }
    }))
    .unwrap();
    assert_eq!(r.session_id, "s1");
    assert_eq!(r.resume_gateway_url, "wss://resume.example");
    assert_eq!(r.user.id, "BOT1");
}

#[test]
fn rate_limit_body_parses_float_seconds() {
    let b: RateLimitBody =
        serde_json::from_value(json!({ "message": "...", "retry_after": 1.462, "global": false })).unwrap();
    assert_eq!(b.retry_after, Some(1.462));
    assert_eq!(b.global, Some(false));
}

#[test]
fn gateway_frame_dispatch_parses() {
    let f: GatewayFrame = serde_json::from_value(json!({
        "op": 0, "s": 5, "t": "MESSAGE_CREATE", "d": { "id": "1" }
    }))
    .unwrap();
    assert_eq!(f.op, op::DISPATCH);
    assert_eq!(f.s, Some(5));
    assert_eq!(f.t.as_deref(), Some("MESSAGE_CREATE"));
}

// -- message_to_unified (gate) -----------------------------------------------

fn dm_message() -> MessageCreate {
    serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "D1",
        "author": { "id": "U1", "username": "alice", "global_name": "Alice" },
        "content": "hi bot"
    }))
    .unwrap()
}

#[test]
fn dm_is_accepted_and_normalized() {
    let msg = message_to_unified(&dm_message(), "BOT1").unwrap();
    assert_eq!(msg.platform, PluginType::Discord);
    assert_eq!(msg.chat_id, "D1");
    assert_eq!(msg.user.id, "U1");
    assert_eq!(msg.user.display_name, "Alice");
    assert_eq!(msg.content.content_type, MessageContentType::Text);
    assert_eq!(msg.content.text, "hi bot");
    assert_eq!(msg.timestamp, 1_462_015_105_796);
}

#[test]
fn guild_mention_is_accepted_and_stripped() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "C1",
        "guild_id": "G1",
        "author": { "id": "U1", "username": "alice" },
        "content": "<@BOT1> summarize",
        "mentions": [ { "id": "BOT1" } ]
    }))
    .unwrap();
    let msg = message_to_unified(&m, "BOT1").unwrap();
    assert_eq!(msg.chat_id, "C1");
    assert_eq!(msg.content.text, "summarize");
}

#[test]
fn guild_without_mention_is_rejected() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "C1",
        "guild_id": "G1",
        "author": { "id": "U1" },
        "content": "hello channel",
        "mentions": []
    }))
    .unwrap();
    assert!(message_to_unified(&m, "BOT1").is_none());
}

#[test]
fn bot_authored_message_is_rejected() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "D1",
        "author": { "id": "U9", "bot": true },
        "content": "automated"
    }))
    .unwrap();
    assert!(message_to_unified(&m, "BOT1").is_none());
}

#[test]
fn own_message_is_rejected() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "D1",
        "author": { "id": "BOT1", "username": "self" },
        "content": "echo"
    }))
    .unwrap();
    assert!(message_to_unified(&m, "BOT1").is_none());
}

#[test]
fn reply_reference_maps_to_reply_to() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "D1",
        "author": { "id": "U1" },
        "content": "re",
        "message_reference": { "message_id": "999" }
    }))
    .unwrap();
    let msg = message_to_unified(&m, "BOT1").unwrap();
    assert_eq!(msg.reply_to_message_id.as_deref(), Some("999"));
}

#[test]
fn display_name_falls_back_to_username_then_id() {
    let m: MessageCreate = serde_json::from_value(json!({
        "id": "175928847299117063",
        "channel_id": "D1",
        "author": { "id": "U1", "username": "bob" },
        "content": "x"
    }))
    .unwrap();
    assert_eq!(message_to_unified(&m, "BOT1").unwrap().user.display_name, "bob");
}
