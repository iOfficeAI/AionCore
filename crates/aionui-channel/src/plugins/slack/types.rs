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

/// Decide whether an inbound event should be processed by the agent.
///
/// Policy:
/// - Always accept 1:1 DMs.
/// - Channels/groups: only if `channel` is in `allowed` **and** the bot is @mentioned
///   (or the event is `app_mention`). Empty allowlist → drop all non-DMs.
pub(crate) fn should_accept_event(
    event: &SlackEvent,
    bot_user_id: &str,
    allowed: &std::collections::HashSet<String>,
) -> bool {
    // Ignore bot-authored / system noise
    if event.bot_id.is_some() {
        return false;
    }
    if let Some(sub) = event.subtype.as_deref() {
        // message_changed, message_deleted, bot_message, channel_join, etc.
        if sub != "file_share" && sub != "me_message" {
            return false;
        }
    }

    let is_app_mention = event.event_type == "app_mention";
    let is_message = event.event_type == "message";
    if !is_app_mention && !is_message {
        return false;
    }

    if is_dm_event(event) {
        return true;
    }

    let channel = match event.channel.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => return false,
    };

    if !allowed.contains(channel) {
        return false;
    }

    if is_app_mention {
        return true;
    }

    let text = event.text.as_deref().unwrap_or("");
    text_mentions_bot(text, bot_user_id)
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
        assert!(!should_accept_event(&event, "U_BOT", &allowed));
    }

    #[test]
    fn channel_needs_mention_even_when_allowlisted() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let plain = msg("C_MAIN", "channel", "hi everyone");
        assert!(!should_accept_event(&plain, "U_BOT", &allowed));
        let mentioned = msg("C_MAIN", "channel", "<@U_BOT> hi");
        assert!(should_accept_event(&mentioned, "U_BOT", &allowed));
    }

    #[test]
    fn empty_allowlist_blocks_channels() {
        let allowed = parse_allowed_channels(None);
        let event = msg("C_MAIN", "channel", "<@U_BOT> hi");
        assert!(!should_accept_event(&event, "U_BOT", &allowed));
    }

    #[test]
    fn app_mention_accepted_when_allowlisted() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let mut event = msg("C_MAIN", "channel", "hi");
        event.event_type = "app_mention".into();
        assert!(should_accept_event(&event, "U_BOT", &allowed));
    }

    #[test]
    fn bot_messages_dropped() {
        let allowed = parse_allowed_channels(Some("C_MAIN"));
        let mut event = msg("C_MAIN", "channel", "<@U_BOT> hi");
        event.bot_id = Some("B123".into());
        assert!(!should_accept_event(&event, "U_BOT", &allowed));
    }

    #[test]
    fn strip_bot_mention_cleans_text() {
        assert_eq!(strip_bot_mention("<@U_BOT>  run tests", "U_BOT"), "run tests");
    }
}
