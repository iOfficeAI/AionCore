use super::*;
use crate::types::PluginCredentials;
use serde_json::json;
use tokio::sync::mpsc;

fn make_config(bot_token: Option<&str>, app_token: Option<&str>) -> PluginConfig {
    let mut obj = serde_json::Map::new();
    if let Some(t) = bot_token {
        obj.insert("token".into(), json!(t));
    }
    if let Some(a) = app_token {
        obj.insert("app_token".into(), json!(a));
    }
    let credentials: PluginCredentials = serde_json::from_value(serde_json::Value::Object(obj)).unwrap();
    PluginConfig {
        credentials,
        config: None,
    }
}

fn make_callbacks() -> PluginCallbacks {
    let (message_tx, _mr) = mpsc::channel(4);
    let (confirm_tx, _cr) = mpsc::channel(4);
    PluginCallbacks { message_tx, confirm_tx }
}

// -- initialize bad paths (validated before any network call) ----------------

#[tokio::test]
async fn initialize_missing_bot_token_fails() {
    let mut plugin = SlackPlugin::new();
    let result = plugin
        .initialize(make_config(None, Some("xapp-1")), make_callbacks())
        .await;
    assert!(matches!(result, Err(ChannelError::InvalidConfig(_))));
    assert_eq!(plugin.status(), PluginStatus::Error);
    assert!(plugin.last_error().unwrap().contains("bot token"));
}

#[tokio::test]
async fn initialize_empty_bot_token_fails() {
    let mut plugin = SlackPlugin::new();
    let result = plugin
        .initialize(make_config(Some(""), Some("xapp-1")), make_callbacks())
        .await;
    assert!(matches!(result, Err(ChannelError::InvalidConfig(_))));
}

// -- require_bot_token: bot token required, app token optional ---------------
// (app token is NOT required at config validation; it is only needed to open
// Socket Mode in start(). This is the credential-test regression guard.)

#[test]
fn require_bot_token_accepts_bot_token_only() {
    // No app_token supplied — the credential-test path — must still validate.
    let cfg = make_config(Some("xoxb-1"), None);
    assert_eq!(require_bot_token(&cfg).unwrap(), "xoxb-1");
}

#[test]
fn require_bot_token_missing_is_err() {
    let cfg = make_config(None, Some("xapp-1"));
    assert!(matches!(require_bot_token(&cfg), Err(ChannelError::InvalidConfig(_))));
}

#[test]
fn require_bot_token_empty_is_err() {
    let cfg = make_config(Some(""), Some("xapp-1"));
    assert!(matches!(require_bot_token(&cfg), Err(ChannelError::InvalidConfig(_))));
}

// -- start() requires the app-level token ------------------------------------

#[tokio::test]
async fn start_without_app_token_fails() {
    // A fresh plugin has app_token_present == false (as it would after an
    // initialize that received only the bot token). start() must reject it
    // before opening any connection.
    let mut plugin = SlackPlugin::new();
    let result = plugin.start().await;
    assert!(matches!(result, Err(ChannelError::InvalidConfig(_))));
    assert_eq!(plugin.status(), PluginStatus::Error);
    assert!(plugin.last_error().unwrap().contains("app-level token"));
}

// -- accessors / defaults ----------------------------------------------------

#[test]
fn new_plugin_is_created_state() {
    let plugin = SlackPlugin::new();
    assert_eq!(plugin.status(), PluginStatus::Created);
    assert!(plugin.bot_info().is_none());
    assert_eq!(plugin.plugin_type(), PluginType::Slack);
    assert_eq!(plugin.active_user_count(), 0);
    assert!(plugin.last_error().is_none());
}

// -- truncate_message --------------------------------------------------------

#[test]
fn truncate_within_limit() {
    assert_eq!(truncate_message("hello", 100), "hello");
}

#[test]
fn truncate_at_limit() {
    assert_eq!(truncate_message("abc", 3), "abc");
}

#[test]
fn truncate_exceeds_limit() {
    assert_eq!(truncate_message("Hello, world!", 10), "Hello, ...");
}

#[test]
fn truncate_unicode() {
    assert_eq!(truncate_message("你好世界测试文本", 5), "你好...");
}
