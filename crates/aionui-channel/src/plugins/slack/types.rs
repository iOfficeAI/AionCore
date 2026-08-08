use serde::{Deserialize, Serialize};

use crate::types::{MessageContentType, PluginType, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedUser};

// ---------------------------------------------------------------------------
// Web API responses (https://api.slack.com/methods)
// ---------------------------------------------------------------------------

/// Response from `auth.test` — validates the bot token and returns identity.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct AuthTestResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Response from `apps.connections.open` — returns the Socket Mode WSS URL.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct OpenConnectionResponse {
    pub ok: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Request body for `chat.postMessage`.
#[derive(Debug, Clone, Serialize)]
pub(super) struct PostMessageRequest {
    pub channel: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
}

/// Response from `chat.postMessage`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct PostMessageResponse {
    pub ok: bool,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Request body for `chat.update`.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UpdateMessageRequest {
    pub channel: String,
    pub ts: String,
    pub text: String,
}

/// Response from `chat.update`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct UpdateMessageResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Socket Mode envelopes (over the WSS connection)
// ---------------------------------------------------------------------------

/// A Socket Mode envelope. The server sends `hello`, `events_api`,
/// `disconnect`, `slash_commands` and `interactive` envelopes; the client
/// must ack every non-`hello`/`disconnect` envelope by echoing `envelope_id`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct SocketEnvelope {
    #[serde(rename = "type")]
    pub envelope_type: String,
    #[serde(default)]
    pub envelope_id: Option<String>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The `payload` of an `events_api` envelope — an Events API `event_callback`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct EventCallback {
    #[serde(default)]
    pub event: Option<SlackEvent>,
    #[serde(default)]
    pub event_id: Option<String>,
}

/// A Slack event (`app_mention`, `message`, …). Only the fields we consume
/// are modeled; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct SlackEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub channel_type: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub client_msg_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Normalization: Slack event → UnifiedIncomingMessage
// ---------------------------------------------------------------------------

/// Build the ack frame the client must echo for an envelope.
pub(super) fn build_ack(envelope_id: &str) -> String {
    serde_json::json!({ "envelope_id": envelope_id }).to_string()
}

/// Parse a Slack `ts` (`"1700000000.123456"`, seconds.micro) into unix ms.
pub(super) fn parse_ts_to_ms(ts: &str) -> i64 {
    ts.parse::<f64>().map(|secs| (secs * 1000.0) as i64).unwrap_or(0)
}

/// Strip a leading bot mention (`<@U123>` / `<@U123|name>`) from `app_mention`
/// text, leaving the user's actual prompt.
pub(super) fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    let mut out = text.to_string();
    if !bot_user_id.is_empty() {
        out = out.replace(&format!("<@{bot_user_id}>"), "");
        out = out.replace(&format!("<@{bot_user_id}|"), "<@|");
    }
    out.trim().to_string()
}

/// Convert a Slack event into a `UnifiedIncomingMessage`, or `None` when the
/// event must be ignored (bot's own message, other bots, or an edit/delete
/// subtype). Only `app_mention` (channel @mention) and plain `message`
/// (direct message) events are accepted.
pub(super) fn event_to_unified(event: &SlackEvent, bot_user_id: &str) -> Option<UnifiedIncomingMessage> {
    // Ignore anything emitted by a bot (including our own echoes) to avoid loops.
    if event.bot_id.is_some() {
        return None;
    }
    if let Some(user) = event.user.as_deref()
        && !bot_user_id.is_empty()
        && user == bot_user_id
    {
        return None;
    }

    match event.event_type.as_str() {
        // Public-channel @mentions always route to the agent; the DM gate below
        // must NOT apply here (app_mention events are not `im`).
        "app_mention" => {}
        // Plain user messages only — reject message_changed/message_deleted/etc.
        "message" if event.subtype.is_none() => {
            // DM-only gate: accept `message` events solely from direct-message
            // channels. This prevents a future channel-level subscription
            // (message.channels/groups) from leaking whole-channel traffic to
            // the agent. Slack marks DMs with channel_type "im"; the `D` id
            // prefix is a fallback when channel_type is absent.
            let is_dm = event.channel_type.as_deref() == Some("im")
                || event.channel.as_deref().is_some_and(|c| c.starts_with('D'));
            if !is_dm {
                return None;
            }
        }
        _ => return None,
    }

    let channel = event.channel.clone()?;
    let user_id = event.user.clone().unwrap_or_default();
    let ts = event.ts.clone().unwrap_or_default();

    let raw_text = event.text.as_deref().unwrap_or("");
    let text = if event.event_type == "app_mention" {
        strip_bot_mention(raw_text, bot_user_id)
    } else {
        raw_text.to_string()
    };

    let id = event
        .client_msg_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ts.clone());

    Some(UnifiedIncomingMessage {
        owner_user_id: None,
        id,
        platform: PluginType::Slack,
        chat_id: channel,
        user: UnifiedUser {
            id: user_id,
            username: None,
            display_name: event.user.clone().unwrap_or_default(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text,
            attachments: None,
        },
        timestamp: parse_ts_to_ms(&ts),
        reply_to_message_id: event.thread_ts.clone(),
        action: None,
        raw: None,
    })
}

#[cfg(test)]
#[path = "types_test.rs"]
mod types_test;
