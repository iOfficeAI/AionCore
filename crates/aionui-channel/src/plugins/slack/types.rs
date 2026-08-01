//! Slack Web API / Socket Mode types used by the channel plugin.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Web API envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct SlackApiResponse<T> {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AuthTestResult {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConnectionsOpenResult {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatPostResult {
    #[serde(default)]
    pub ts: Option<String>,
}

// ---------------------------------------------------------------------------
// Outbound request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ChatPostMessageRequest<'a> {
    pub channel: &'a str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mrkdwn: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatUpdateRequest<'a> {
    pub channel: &'a str,
    pub ts: &'a str,
    pub text: &'a str,
}

// ---------------------------------------------------------------------------
// Socket Mode envelopes
// ---------------------------------------------------------------------------

/// Top-level Socket Mode frame.
#[derive(Debug, Deserialize)]
pub(crate) struct SocketEnvelope {
    #[serde(default)]
    pub envelope_id: Option<String>,
    #[serde(rename = "type")]
    pub envelope_type: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// Present on `disconnect` frames.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventsApiPayload {
    #[serde(default)]
    pub event: Option<SlackEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlackEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// `im` | `mpim` | `channel` | `group` (on message events).
    #[serde(default)]
    pub channel_type: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Policy helpers
// ---------------------------------------------------------------------------

/// Parse a comma-separated allowlist of conversation IDs (`C…`/`G…`/`D…`).
pub(crate) fn parse_allowed_channels(raw: Option<&str>) -> std::collections::HashSet<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether this event is a 1:1 DM (`im`).
pub(crate) fn is_dm_event(event: &SlackEvent) -> bool {
    matches!(event.channel_type.as_deref(), Some("im"))
        || event
            .channel
            .as_deref()
            .is_some_and(|c| c.starts_with('D'))
}

/// Hermes-style session key: each Slack thread is its own conversation.
///
/// - Top-level message (no `thread_ts`): thread root = this message's `ts`
/// - Reply in an existing thread: thread root = `thread_ts`
///
/// Encoded as `{channel}:{thread_root}` so session isolation is
/// `(user_id, chat_id)` without changing the core session manager.
pub(crate) fn encode_session_chat_id(channel: &str, thread_root: &str) -> String {
    format!("{channel}:{thread_root}")
}

/// Split a session chat_id into `(channel, thread_ts)`.
///
/// Bare channel ids (legacy) decode as `(channel, None)`.
pub(crate) fn decode_session_chat_id(chat_id: &str) -> (&str, Option<&str>) {
    match chat_id.split_once(':') {
        Some((channel, thread_ts)) if !channel.is_empty() && !thread_ts.is_empty() => {
            (channel, Some(thread_ts))
        }
        _ => (chat_id, None),
    }
}

/// Thread root for an inbound event: existing thread, else this message ts.
pub(crate) fn thread_root_for_event(event: &SlackEvent) -> Option<String> {
    event
        .thread_ts
        .clone()
        .or_else(|| event.ts.clone())
        .filter(|s| !s.is_empty())
}

/// Whether the message text @mentions the bot user.
pub(crate) fn text_mentions_bot(text: &str, bot_user_id: &str) -> bool {
    if bot_user_id.is_empty() {
        return false;
    }
    text.contains(&format!("<@{bot_user_id}>"))
}

/// Strip Slack user mention tokens for cleaner agent input.
pub(crate) fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    let token = format!("<@{bot_user_id}>");
    text.replace(&token, "").trim().to_string()
}

/// Why an event was accepted or dropped (for diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptDecision {
    AcceptDm,
    AcceptMention,
    DropBotMessage,
    DropSubtype,
    DropEventType,
    DropNotAllowlisted,
    DropNoMention,
    DropNoChannel,
}

/// Decide whether an inbound event should be processed by the agent.
///
/// Policy:
/// - Always accept 1:1 DMs.
/// - Channels/groups: only if `channel` is in `allowed` **and** the bot is @mentioned
///   (or the event is `app_mention`). Empty allowlist → drop all non-DMs.
pub(crate) fn classify_event(
    event: &SlackEvent,
    bot_user_id: &str,
    allowed: &std::collections::HashSet<String>,
) -> AcceptDecision {
    // Ignore bot-authored / system noise
    if event.bot_id.is_some() {
        return AcceptDecision::DropBotMessage;
    }
    if let Some(sub) = event.subtype.as_deref() {
        // message_changed, message_deleted, bot_message, channel_join, etc.
        if sub != "file_share" && sub != "me_message" {
            return AcceptDecision::DropSubtype;
        }
    }

    let is_app_mention = event.event_type == "app_mention";
    let is_message = event.event_type == "message";
    if !is_app_mention && !is_message {
        return AcceptDecision::DropEventType;
    }

    if is_dm_event(event) {
        return AcceptDecision::AcceptDm;
    }

    let channel = match event.channel.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => return AcceptDecision::DropNoChannel,
    };

    if !allowed.contains(channel) {
        return AcceptDecision::DropNotAllowlisted;
    }

    if is_app_mention {
        return AcceptDecision::AcceptMention;
    }

    let text = event.text.as_deref().unwrap_or("");
    if text_mentions_bot(text, bot_user_id) {
        AcceptDecision::AcceptMention
    } else {
        AcceptDecision::DropNoMention
    }
}

#[cfg(test)]
pub(crate) fn should_accept_event(
    event: &SlackEvent,
    bot_user_id: &str,
    allowed: &std::collections::HashSet<String>,
) -> bool {
    matches!(
        classify_event(event, bot_user_id, allowed),
        AcceptDecision::AcceptDm | AcceptDecision::AcceptMention
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(channel: &str, channel_type: &str, text: &str) -> SlackEvent {
        SlackEvent {
            event_type: "message".into(),
            user: Some("U_USER".into()),
            channel: Some(channel.into()),
            text: Some(text.into()),
            ts: Some("1.0".into()),
            thread_ts: None,
            channel_type: Some(channel_type.into()),
            subtype: None,
            bot_id: None,
        }
    }

    #[test]
    fn parse_allowed_channels_trims() {
        let set = parse_allowed_channels(Some(" C1 , G2,  ,D3 "));
        assert!(set.contains("C1"));
        assert!(set.contains("G2"));
        assert!(set.contains("D3"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn dm_always_accepted() {
        let allowed = std::collections::HashSet::new();
        let event = msg("D123", "im", "hello");
        assert!(should_accept_event(&event, "U_BOT", &allowed));
    }

    #[test]
    fn channel_dropped_when_not_allowlisted() {
        let allowed = parse_allowed_channels(Some("C_OTHER"));
        let event = msg("C_MAIN", "channel", "<@U_BOT> hi");
        assert_eq!(
            classify_event(&event, "U_BOT", &allowed),
            AcceptDecision::DropNotAllowlisted
        );
    }

    #[test]
    fn channel_needs_mention_even_when_allowlisted() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let plain = msg("C_MAIN", "channel", "hi everyone");
        assert_eq!(
            classify_event(&plain, "U_BOT", &allowed),
            AcceptDecision::DropNoMention
        );
        let mentioned = msg("C_MAIN", "channel", "<@U_BOT> hi");
        assert_eq!(
            classify_event(&mentioned, "U_BOT", &allowed),
            AcceptDecision::AcceptMention
        );
    }

    #[test]
    fn empty_allowlist_blocks_channels() {
        let allowed = parse_allowed_channels(None);
        let event = msg("C_MAIN", "channel", "<@U_BOT> hi");
        assert_eq!(
            classify_event(&event, "U_BOT", &allowed),
            AcceptDecision::DropNotAllowlisted
        );
    }

    #[test]
    fn app_mention_accepted_when_allowlisted() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let mut event = msg("C_MAIN", "channel", "hi");
        event.event_type = "app_mention".into();
        assert_eq!(
            classify_event(&event, "U_BOT", &allowed),
            AcceptDecision::AcceptMention
        );
    }

    #[test]
    fn bot_messages_dropped() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let mut event = msg("C_MAIN", "channel", "<@U_BOT> hi");
        event.bot_id = Some("B123".into());
        assert_eq!(
            classify_event(&event, "U_BOT", &allowed),
            AcceptDecision::DropBotMessage
        );
    }

    #[test]
    fn dm_without_channel_type_still_accepted() {
        let allowed = std::collections::HashSet::new();
        let mut event = msg("D999", "channel", "hello");
        event.channel_type = None;
        assert_eq!(
            classify_event(&event, "U_BOT", &allowed),
            AcceptDecision::AcceptDm
        );
    }

    #[test]
    fn strip_bot_mention_cleans_text() {
        assert_eq!(strip_bot_mention("<@U_BOT>  run tests", "U_BOT"), "run tests");
    }

    #[test]
    fn session_chat_id_roundtrip() {
        let encoded = encode_session_chat_id("D123", "1710000.000100");
        assert_eq!(encoded, "D123:1710000.000100");
        let (ch, thr) = decode_session_chat_id(&encoded);
        assert_eq!(ch, "D123");
        assert_eq!(thr, Some("1710000.000100"));
    }

    #[test]
    fn session_chat_id_legacy_bare_channel() {
        let (ch, thr) = decode_session_chat_id("D123");
        assert_eq!(ch, "D123");
        assert!(thr.is_none());
    }

    #[test]
    fn thread_root_top_level_uses_message_ts() {
        let event = msg("D1", "im", "hi");
        assert_eq!(thread_root_for_event(&event).as_deref(), Some("1.0"));
    }

    #[test]
    fn thread_root_in_thread_uses_thread_ts() {
        let mut event = msg("D1", "im", "follow up");
        event.thread_ts = Some("1.0".into());
        event.ts = Some("2.0".into());
        assert_eq!(thread_root_for_event(&event).as_deref(), Some("1.0"));
    }
}
