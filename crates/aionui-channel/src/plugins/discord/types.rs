use serde::{Deserialize, Serialize};

use crate::types::{MessageContentType, PluginType, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedUser};

// ---------------------------------------------------------------------------
// Gateway intents (verified: docs.discord.com/developers/topics/gateway#gateway-intents)
// ---------------------------------------------------------------------------

/// Bitwise-combined intents the bot requests in IDENTIFY.
///
/// GUILDS (1<<0) + GUILD_MESSAGES (1<<9) + DIRECT_MESSAGES (1<<12) +
/// MESSAGE_CONTENT (1<<15, privileged — must be enabled in the Developer
/// Portal, otherwise `content` arrives empty).
pub(super) const DISCORD_INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

/// Discord snowflake epoch in unix ms (2015-01-01T00:00:00Z).
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

// ---------------------------------------------------------------------------
// Gateway opcodes (verified: docs.discord.com/developers/topics/opcodes-and-status-codes)
// ---------------------------------------------------------------------------

pub(super) mod op {
    pub const DISPATCH: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const IDENTIFY: u8 = 2;
    pub const RESUME: u8 = 6;
    pub const RECONNECT: u8 = 7;
    pub const INVALID_SESSION: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const HEARTBEAT_ACK: u8 = 11;
}

// ---------------------------------------------------------------------------
// REST responses (https://discord.com/api/v10)
// ---------------------------------------------------------------------------

/// Response from `GET /gateway/bot` — the WSS URL to connect to.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct GatewayBotResponse {
    pub url: String,
}

/// Response from `GET /users/@me` — the bot's own identity.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct CurrentUser {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
}

/// Request body for `POST /channels/{id}/messages` and `PATCH .../{msg_id}`.
#[derive(Debug, Clone, Serialize)]
pub(super) struct SendMessageRequest {
    pub content: String,
    /// Suppress accidental @everyone/@here/user pings coming from agent text.
    pub allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_reference: Option<MessageReference>,
}

/// `allowed_mentions` with an empty `parse` list disables all mentions.
#[derive(Debug, Clone, Serialize)]
pub(super) struct AllowedMentions {
    pub parse: Vec<String>,
}

impl AllowedMentions {
    pub fn none() -> Self {
        Self { parse: Vec::new() }
    }
}

/// Reference to a message being replied to.
#[derive(Debug, Clone, Serialize)]
pub(super) struct MessageReference {
    pub message_id: String,
}

/// Response from create/edit message — we only need the message id.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct MessageResponse {
    pub id: String,
}

/// Body of a 429 response (`retry_after` is float **seconds**).
#[derive(Debug, Clone, Deserialize)]
pub(super) struct RateLimitBody {
    #[serde(default)]
    pub retry_after: Option<f64>,
    #[serde(default)]
    pub global: Option<bool>,
}

// ---------------------------------------------------------------------------
// Gateway payloads
// ---------------------------------------------------------------------------

/// A generic inbound Gateway frame: `{ op, d, s, t }`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct GatewayFrame {
    pub op: u8,
    #[serde(default)]
    pub d: Option<serde_json::Value>,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(default)]
    pub t: Option<String>,
}

/// `d` of a Hello (op 10) frame.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Hello {
    pub heartbeat_interval: u64,
}

/// `d` of a READY dispatch — session bookkeeping + bot identity.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Ready {
    pub session_id: String,
    pub resume_gateway_url: String,
    pub user: ReadyUser,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ReadyUser {
    pub id: String,
}

/// `d` of a MESSAGE_CREATE dispatch (only the fields we consume). MESSAGE_CREATE
/// adds `guild_id` (absent for DMs) on top of the base message object.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct MessageCreate {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    pub author: Author,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<MentionUser>,
    #[serde(default)]
    pub message_reference: Option<IncomingMessageReference>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Author {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct MentionUser {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct IncomingMessageReference {
    #[serde(default)]
    pub message_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Outbound Gateway frame builders
// ---------------------------------------------------------------------------

/// Build a heartbeat frame (op 1, `d` = last sequence or null).
pub(super) fn build_heartbeat(last_seq: Option<u64>) -> String {
    serde_json::json!({ "op": op::HEARTBEAT, "d": last_seq }).to_string()
}

/// Build an IDENTIFY frame (op 2).
pub(super) fn build_identify(token: &str) -> String {
    serde_json::json!({
        "op": op::IDENTIFY,
        "d": {
            "token": token,
            "intents": DISCORD_INTENTS,
            "properties": { "os": "linux", "browser": "aionui", "device": "aionui" }
        }
    })
    .to_string()
}

/// Build a RESUME frame (op 6).
pub(super) fn build_resume(token: &str, session_id: &str, seq: u64) -> String {
    serde_json::json!({
        "op": op::RESUME,
        "d": { "token": token, "session_id": session_id, "seq": seq }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Normalization: MESSAGE_CREATE → UnifiedIncomingMessage
// ---------------------------------------------------------------------------

/// Extract a unix-ms timestamp from a Discord snowflake id
/// (`(id >> 22) + DISCORD_EPOCH`). Avoids ISO-8601 parsing.
pub(super) fn snowflake_to_ms(id: &str) -> i64 {
    id.parse::<u64>()
        .map(|n| ((n >> 22) as i64) + DISCORD_EPOCH_MS)
        .unwrap_or(0)
}

/// Strip a leading bot mention (`<@id>` or `<@!id>`) from `@mention` text.
pub(super) fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    let mut out = text.to_string();
    if !bot_user_id.is_empty() {
        out = out.replace(&format!("<@{bot_user_id}>"), "");
        out = out.replace(&format!("<@!{bot_user_id}>"), "");
    }
    out.trim().to_string()
}

/// Convert a MESSAGE_CREATE into a `UnifiedIncomingMessage`, or `None` when it
/// must be ignored. Accepts only DMs (no `guild_id`) or guild messages that
/// @mention the bot — mirrors the Slack M1 DM/@mention gate — and never
/// accepts messages authored by a bot (including the bot itself).
pub(super) fn message_to_unified(msg: &MessageCreate, bot_user_id: &str) -> Option<UnifiedIncomingMessage> {
    // Drop anything authored by a bot (including our own echoes) to avoid loops.
    if msg.author.bot == Some(true) {
        return None;
    }
    if !bot_user_id.is_empty() && msg.author.id == bot_user_id {
        return None;
    }

    let is_dm = msg.guild_id.is_none();
    let is_mention = !bot_user_id.is_empty() && msg.mentions.iter().any(|u| u.id == bot_user_id);
    // DM/@mention gate: guild messages that do not mention the bot are ignored,
    // so a broad GUILD_MESSAGES subscription cannot flood the agent.
    if !is_dm && !is_mention {
        return None;
    }

    let text = if is_mention {
        strip_bot_mention(&msg.content, bot_user_id)
    } else {
        msg.content.clone()
    };

    let display_name = msg
        .author
        .global_name
        .clone()
        .or_else(|| msg.author.username.clone())
        .unwrap_or_else(|| msg.author.id.clone());

    Some(UnifiedIncomingMessage {
        owner_user_id: None,
        id: msg.id.clone(),
        platform: PluginType::Discord,
        chat_id: msg.channel_id.clone(),
        user: UnifiedUser {
            id: msg.author.id.clone(),
            username: msg.author.username.clone(),
            display_name,
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text,
            attachments: None,
        },
        timestamp: snowflake_to_ms(&msg.id),
        reply_to_message_id: msg.message_reference.as_ref().and_then(|r| r.message_id.clone()),
        action: None,
        raw: None,
    })
}

#[cfg(test)]
#[path = "types_test.rs"]
mod types_test;
