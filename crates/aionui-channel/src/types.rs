use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// A. Plugin Type
// ---------------------------------------------------------------------------

/// Platform type identifier for channel plugins.
///
/// Includes the four supported IM platforms and reserved variants
/// for future platforms (`slack`/`discord` per the `assistant_plugins.type`
/// CHECK constraint in the DB schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    Telegram,
    Lark,
    Dingtalk,
    Weixin,
    /// Reserved variant for future Slack integration.
    Slack,
    /// Reserved variant for future Discord integration.
    Discord,
}

impl fmt::Display for PluginType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram => write!(f, "telegram"),
            Self::Lark => write!(f, "lark"),
            Self::Dingtalk => write!(f, "dingtalk"),
            Self::Weixin => write!(f, "weixin"),
            Self::Slack => write!(f, "slack"),
            Self::Discord => write!(f, "discord"),
        }
    }
}

impl PluginType {
    /// Parse from a string, returning `None` for unknown types.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "telegram" => Some(Self::Telegram),
            "lark" => Some(Self::Lark),
            "dingtalk" => Some(Self::Dingtalk),
            "weixin" => Some(Self::Weixin),
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// B. Plugin Status (lifecycle state machine)
// ---------------------------------------------------------------------------

/// Plugin lifecycle status.
///
/// State machine:
/// ```text
/// created → initializing → ready → starting → running → stopping → stopped
///                ↓                    ↓           ↓
///              error ←←←←←←←←←←←←←←←←←←←←←←←←←←←
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginStatus {
    Created,
    Initializing,
    Ready,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

impl fmt::Display for PluginStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Initializing => write!(f, "initializing"),
            Self::Ready => write!(f, "ready"),
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Error => write!(f, "error"),
        }
    }
}

impl PluginStatus {
    /// Parse from a string, returning `None` for unknown values.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "initializing" => Some(Self::Initializing),
            "ready" => Some(Self::Ready),
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// C. Pairing Status
// ---------------------------------------------------------------------------

/// Status of a pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl fmt::Display for PairingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
            Self::Expired => write!(f, "expired"),
        }
    }
}

impl PairingStatus {
    /// Parse from a string, returning `None` for unknown values.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// D. Plugin Credentials & Config
// ---------------------------------------------------------------------------

/// Platform-specific plugin credentials.
///
/// Each platform uses a subset of fields:
/// - Telegram: `token`
/// - Lark: `app_id` + `app_secret` + optional `encrypt_key`/`verification_token`
/// - DingTalk: `client_id` + `client_secret`
/// - WeChat: `account_id` + `bot_token`
///
/// Remaining fields are captured in `extra` for extensibility
/// (API Spec `[key: string]: unknown`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginCredentials {
    // Telegram
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    // Lark
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_token: Option<String>,

    // DingTalk
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    // WeChat (iLink Bot)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,

    // Extensibility
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl PluginCredentials {
    /// Returns true when no credential field carries a value.
    ///
    /// Used to detect "reuse stored credentials" enable requests: the Settings
    /// re-enable toggle sends an empty config, so the manager must fall back to
    /// the previously persisted credentials instead of failing.
    pub fn is_empty(&self) -> bool {
        self.token.is_none()
            && self.app_id.is_none()
            && self.app_secret.is_none()
            && self.encrypt_key.is_none()
            && self.verification_token.is_none()
            && self.client_id.is_none()
            && self.client_secret.is_none()
            && self.account_id.is_none()
            && self.bot_token.is_none()
            && self.extra.is_empty()
    }
}

/// Plugin connection options.
///
/// Configures the connection mode, webhook URL, rate limiting,
/// and group-chat mention requirement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginConfigOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ConnectionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_mention: Option<bool>,

    // Extensibility
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Connection mode for a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    Polling,
    Webhook,
    Websocket,
}

/// Combined plugin configuration: credentials + options.
///
/// Stored as JSON in the `assistant_plugins.config` column (encrypted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginConfig {
    pub credentials: PluginCredentials,
    #[serde(default)]
    pub config: Option<PluginConfigOptions>,
}

// ---------------------------------------------------------------------------
// E. Bot Info
// ---------------------------------------------------------------------------

/// Information about the bot identity on a platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BotInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// F. Unified Incoming Message
// ---------------------------------------------------------------------------

/// Message received from an IM platform, normalized to a common format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedIncomingMessage {
    pub id: String,
    pub platform: PluginType,
    pub chat_id: String,
    pub user: UnifiedUser,
    pub content: UnifiedMessageContent,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<UnifiedAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Sender identity from an IM platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedUser {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

/// Content of an incoming message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedMessageContent {
    #[serde(rename = "type")]
    pub content_type: MessageContentType,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<UnifiedAttachment>>,
}

/// Type discriminant for message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageContentType {
    Text,
    Photo,
    Document,
    Voice,
    Audio,
    Video,
    Sticker,
    Action,
    Command,
}

/// File attachment in a unified message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedAttachment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// G. Unified Outgoing Message
// ---------------------------------------------------------------------------

/// Message to be sent to an IM platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedOutgoingMessage {
    #[serde(rename = "type")]
    pub message_type: OutgoingMessageType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    /// Inline action buttons (rows x columns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Vec<ActionButton>>>,
    /// Fixed keyboard buttons (rows x columns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Vec<Vec<ActionButton>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_actions: Option<Vec<ChannelMediaAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent: Option<bool>,
}

/// Outgoing message type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutgoingMessageType {
    Text,
    Image,
    File,
    Buttons,
}

/// Text formatting mode for platforms that support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParseMode {
    HTML,
    MarkdownV2,
    Markdown,
}

/// An interactive button in an outgoing message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionButton {
    pub label: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, String>>,
}

/// Media action attached to an outgoing message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMediaAction {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// H. Action System
// ---------------------------------------------------------------------------

/// A routable action parsed from a button callback or command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedAction {
    /// Action identifier, e.g. "session.new", "chat.send".
    pub action: String,
    pub category: ActionCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, String>>,
    pub context: ActionContext,
}

/// Action category (determines which handler group processes the action).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionCategory {
    Platform,
    System,
    Chat,
}

/// Context provided with every action for routing and state lookup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionContext {
    pub platform: PluginType,
    pub user_id: String,
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Response produced by an action handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Vec<ActionButton>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Vec<Vec<ActionButton>>>,
    pub behavior: ActionBehavior,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_message_id: Option<String>,
}

/// How the platform should deliver the action response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionBehavior {
    /// Send as a new message.
    Send,
    /// Edit an existing message (identified by `edit_message_id`).
    Edit,
    /// Answer the callback query (inline toast).
    Answer,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. PluginType -------------------------------------------------------

    #[test]
    fn plugin_type_serde_roundtrip() {
        let cases = [
            (PluginType::Telegram, "\"telegram\""),
            (PluginType::Lark, "\"lark\""),
            (PluginType::Dingtalk, "\"dingtalk\""),
            (PluginType::Weixin, "\"weixin\""),
            (PluginType::Slack, "\"slack\""),
            (PluginType::Discord, "\"discord\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PluginType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn plugin_type_display() {
        assert_eq!(PluginType::Telegram.to_string(), "telegram");
        assert_eq!(PluginType::Lark.to_string(), "lark");
        assert_eq!(PluginType::Dingtalk.to_string(), "dingtalk");
        assert_eq!(PluginType::Weixin.to_string(), "weixin");
        assert_eq!(PluginType::Slack.to_string(), "slack");
        assert_eq!(PluginType::Discord.to_string(), "discord");
    }

    #[test]
    fn plugin_type_from_str_opt() {
        assert_eq!(PluginType::from_str_opt("telegram"), Some(PluginType::Telegram));
        assert_eq!(PluginType::from_str_opt("lark"), Some(PluginType::Lark));
        assert_eq!(PluginType::from_str_opt("unknown"), None);
    }

    #[test]
    fn plugin_type_unknown_deserialization_fails() {
        let result = serde_json::from_str::<PluginType>("\"whatsapp\"");
        assert!(result.is_err());
    }

    // -- B. PluginStatus -----------------------------------------------------

    #[test]
    fn plugin_status_serde_roundtrip() {
        let cases = [
            (PluginStatus::Created, "\"created\""),
            (PluginStatus::Initializing, "\"initializing\""),
            (PluginStatus::Ready, "\"ready\""),
            (PluginStatus::Starting, "\"starting\""),
            (PluginStatus::Running, "\"running\""),
            (PluginStatus::Stopping, "\"stopping\""),
            (PluginStatus::Stopped, "\"stopped\""),
            (PluginStatus::Error, "\"error\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PluginStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn plugin_status_display() {
        assert_eq!(PluginStatus::Created.to_string(), "created");
        assert_eq!(PluginStatus::Running.to_string(), "running");
        assert_eq!(PluginStatus::Error.to_string(), "error");
    }

    #[test]
    fn plugin_status_from_str_opt() {
        assert_eq!(PluginStatus::from_str_opt("running"), Some(PluginStatus::Running));
        assert_eq!(PluginStatus::from_str_opt("unknown"), None);
    }

    // -- C. PairingStatus ----------------------------------------------------

    #[test]
    fn pairing_status_serde_roundtrip() {
        let cases = [
            (PairingStatus::Pending, "\"pending\""),
            (PairingStatus::Approved, "\"approved\""),
            (PairingStatus::Rejected, "\"rejected\""),
            (PairingStatus::Expired, "\"expired\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: PairingStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn pairing_status_display() {
        assert_eq!(PairingStatus::Pending.to_string(), "pending");
        assert_eq!(PairingStatus::Approved.to_string(), "approved");
    }

    #[test]
    fn pairing_status_from_str_opt() {
        assert_eq!(PairingStatus::from_str_opt("pending"), Some(PairingStatus::Pending));
        assert_eq!(PairingStatus::from_str_opt("nope"), None);
    }

    // -- D. Credentials & Config ---------------------------------------------

    #[test]
    fn plugin_credentials_telegram() {
        let creds = PluginCredentials {
            token: Some("bot123:ABC".into()),
            app_id: None,
            app_secret: None,
            encrypt_key: None,
            verification_token: None,
            client_id: None,
            client_secret: None,
            account_id: None,
            bot_token: None,
            extra: HashMap::new(),
        };
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["token"], "bot123:ABC");
        // Optional fields should be absent
        assert!(json.get("app_id").is_none());
    }

    #[test]
    fn plugin_credentials_lark() {
        let creds = PluginCredentials {
            token: None,
            app_id: Some("cli_abc".into()),
            app_secret: Some("secret".into()),
            encrypt_key: Some("ek".into()),
            verification_token: Some("vt".into()),
            client_id: None,
            client_secret: None,
            account_id: None,
            bot_token: None,
            extra: HashMap::new(),
        };
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["app_id"], "cli_abc");
        assert_eq!(json["app_secret"], "secret");
        assert_eq!(json["encrypt_key"], "ek");
        assert_eq!(json["verification_token"], "vt");
    }

    #[test]
    fn plugin_credentials_extensible() {
        let raw = json!({
            "token": "xxx",
            "customField": "hello"
        });
        let creds: PluginCredentials = serde_json::from_value(raw).unwrap();
        assert_eq!(creds.token.as_deref(), Some("xxx"));
        assert_eq!(creds.extra.get("customField").unwrap(), "hello");
    }

    #[test]
    fn plugin_config_full() {
        let raw = json!({
            "credentials": { "token": "bot:123" },
            "config": {
                "mode": "polling",
                "rate_limit": 10,
                "require_mention": true
            }
        });
        let cfg: PluginConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.credentials.token.as_deref(), Some("bot:123"));
        let opts = cfg.config.unwrap();
        assert_eq!(opts.mode, Some(ConnectionMode::Polling));
        assert_eq!(opts.rate_limit, Some(10));
        assert_eq!(opts.require_mention, Some(true));
    }

    #[test]
    fn plugin_config_minimal() {
        let raw = json!({
            "credentials": { "token": "bot:123" }
        });
        let cfg: PluginConfig = serde_json::from_value(raw).unwrap();
        assert!(cfg.config.is_none());
    }

    #[test]
    fn connection_mode_serde() {
        let cases = [
            (ConnectionMode::Polling, "\"polling\""),
            (ConnectionMode::Webhook, "\"webhook\""),
            (ConnectionMode::Websocket, "\"websocket\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ConnectionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // -- E. BotInfo ----------------------------------------------------------

    #[test]
    fn bot_info_serde() {
        let info = BotInfo {
            id: "bot_1".into(),
            username: Some("my_bot".into()),
            display_name: "My Bot".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "bot_1");
        assert_eq!(json["username"], "my_bot");
        assert_eq!(json["display_name"], "My Bot");
    }

    #[test]
    fn bot_info_without_username() {
        let info = BotInfo {
            id: "bot_2".into(),
            username: None,
            display_name: "Bot 2".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("username").is_none());
    }

    // -- F. Incoming Message -------------------------------------------------

    #[test]
    fn unified_incoming_message_text() {
        let msg = UnifiedIncomingMessage {
            id: "msg_1".into(),
            platform: PluginType::Telegram,
            chat_id: "chat_42".into(),
            user: UnifiedUser {
                id: "user_1".into(),
                username: Some("alice".into()),
                display_name: "Alice".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "Hello".into(),
                attachments: None,
            },
            timestamp: 1700000000,
            reply_to_message_id: None,
            action: None,
            raw: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["id"], "msg_1");
        assert_eq!(json["platform"], "telegram");
        assert_eq!(json["content"]["type"], "text");
        assert_eq!(json["content"]["text"], "Hello");
        assert_eq!(json["user"]["display_name"], "Alice");
    }

    #[test]
    fn message_content_type_serde() {
        let cases = [
            (MessageContentType::Text, "\"text\""),
            (MessageContentType::Photo, "\"photo\""),
            (MessageContentType::Document, "\"document\""),
            (MessageContentType::Voice, "\"voice\""),
            (MessageContentType::Audio, "\"audio\""),
            (MessageContentType::Video, "\"video\""),
            (MessageContentType::Sticker, "\"sticker\""),
            (MessageContentType::Action, "\"action\""),
            (MessageContentType::Command, "\"command\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "serialize {variant:?}");
        }
    }

    #[test]
    fn unified_attachment_serde() {
        let att = UnifiedAttachment {
            file_id: Some("file_1".into()),
            file_name: Some("photo.jpg".into()),
            mime_type: Some("image/jpeg".into()),
            file_size: Some(12345),
            url: None,
        };
        let json = serde_json::to_value(&att).unwrap();
        assert_eq!(json["file_id"], "file_1");
        assert_eq!(json["file_name"], "photo.jpg");
        assert_eq!(json["mime_type"], "image/jpeg");
        assert_eq!(json["file_size"], 12345);
        assert!(json.get("url").is_none());
    }

    // -- G. Outgoing Message -------------------------------------------------

    #[test]
    fn outgoing_message_text() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("Hello back!".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello back!");
    }

    #[test]
    fn outgoing_message_with_buttons() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Buttons,
            text: Some("Choose:".into()),
            parse_mode: None,
            buttons: Some(vec![vec![
                ActionButton {
                    label: "Yes".into(),
                    action: "confirm.yes".into(),
                    params: None,
                },
                ActionButton {
                    label: "No".into(),
                    action: "confirm.no".into(),
                    params: None,
                },
            ]]),
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "buttons");
        assert_eq!(json["buttons"][0][0]["label"], "Yes");
        assert_eq!(json["buttons"][0][1]["action"], "confirm.no");
    }

    #[test]
    fn outgoing_message_type_serde() {
        let cases = [
            (OutgoingMessageType::Text, "\"text\""),
            (OutgoingMessageType::Image, "\"image\""),
            (OutgoingMessageType::File, "\"file\""),
            (OutgoingMessageType::Buttons, "\"buttons\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn parse_mode_serde() {
        let cases = [
            (ParseMode::HTML, "\"HTML\""),
            (ParseMode::MarkdownV2, "\"MarkdownV2\""),
            (ParseMode::Markdown, "\"Markdown\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ParseMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // -- H. Action System ----------------------------------------------------

    #[test]
    fn action_category_serde() {
        let cases = [
            (ActionCategory::Platform, "\"platform\""),
            (ActionCategory::System, "\"system\""),
            (ActionCategory::Chat, "\"chat\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: ActionCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn unified_action_serde() {
        let action = UnifiedAction {
            action: "session.new".into(),
            category: ActionCategory::System,
            params: None,
            context: ActionContext {
                platform: PluginType::Telegram,
                user_id: "tg_42".into(),
                chat_id: "chat_1".into(),
                message_id: Some("msg_99".into()),
                session_id: None,
            },
        };
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["action"], "session.new");
        assert_eq!(json["category"], "system");
        assert_eq!(json["context"]["platform"], "telegram");
        assert_eq!(json["context"]["user_id"], "tg_42");
    }

    #[test]
    fn action_behavior_serde() {
        let cases = [
            (ActionBehavior::Send, "\"send\""),
            (ActionBehavior::Edit, "\"edit\""),
            (ActionBehavior::Answer, "\"answer\""),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn action_response_full() {
        let resp = ActionResponse {
            text: Some("Session created".into()),
            parse_mode: Some(ParseMode::HTML),
            buttons: Some(vec![vec![ActionButton {
                label: "Help".into(),
                action: "help.show".into(),
                params: None,
            }]]),
            keyboard: None,
            behavior: ActionBehavior::Send,
            toast: None,
            edit_message_id: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["text"], "Session created");
        assert_eq!(json["parse_mode"], "HTML");
        assert_eq!(json["behavior"], "send");
        assert_eq!(json["buttons"][0][0]["label"], "Help");
    }

    #[test]
    fn action_response_edit() {
        let resp = ActionResponse {
            text: Some("Updated".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            behavior: ActionBehavior::Edit,
            toast: None,
            edit_message_id: Some("msg_42".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["behavior"], "edit");
        assert_eq!(json["edit_message_id"], "msg_42");
    }

    #[test]
    fn action_button_with_params() {
        let mut params = HashMap::new();
        params.insert("agentType".into(), "gemini".into());
        let btn = ActionButton {
            label: "Switch to Gemini".into(),
            action: "agent.select".into(),
            params: Some(params),
        };
        let json = serde_json::to_value(&btn).unwrap();
        assert_eq!(json["params"]["agentType"], "gemini");
    }

    // -- Roundtrip tests -----------------------------------------------------

    #[test]
    fn incoming_message_roundtrip() {
        let msg = UnifiedIncomingMessage {
            id: "m1".into(),
            platform: PluginType::Lark,
            chat_id: "c1".into(),
            user: UnifiedUser {
                id: "u1".into(),
                username: None,
                display_name: "Bob".into(),
                avatar_url: None,
            },
            content: UnifiedMessageContent {
                content_type: MessageContentType::Text,
                text: "test".into(),
                attachments: None,
            },
            timestamp: 1000,
            reply_to_message_id: None,
            action: None,
            raw: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: UnifiedIncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn outgoing_message_roundtrip() {
        let msg = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("hello".into()),
            parse_mode: Some(ParseMode::Markdown),
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: Some(true),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: UnifiedOutgoingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn plugin_config_roundtrip() {
        let cfg = PluginConfig {
            credentials: PluginCredentials {
                token: Some("bot:abc".into()),
                app_id: None,
                app_secret: None,
                encrypt_key: None,
                verification_token: None,
                client_id: None,
                client_secret: None,
                account_id: None,
                bot_token: None,
                extra: HashMap::new(),
            },
            config: Some(PluginConfigOptions {
                mode: Some(ConnectionMode::Polling),
                webhook_url: None,
                rate_limit: Some(5),
                require_mention: None,
                extra: HashMap::new(),
            }),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cfg);
    }
}
