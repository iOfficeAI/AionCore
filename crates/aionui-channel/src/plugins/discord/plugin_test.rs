use super::*;
use crate::types::PluginCredentials;
use serde_json::json;
use tokio::sync::mpsc;

fn make_config(token: Option<&str>) -> PluginConfig {
    let mut obj = serde_json::Map::new();
    if let Some(t) = token {
        obj.insert("token".into(), json!(t));
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
async fn initialize_missing_token_fails() {
    let mut plugin = DiscordPlugin::new();
    let result = plugin.initialize(make_config(None), make_callbacks()).await;
    assert!(matches!(result, Err(ChannelError::InvalidConfig(_))));
    assert_eq!(plugin.status(), PluginStatus::Error);
    assert!(plugin.last_error().unwrap().contains("token"));
}

#[tokio::test]
async fn initialize_empty_token_fails() {
    let mut plugin = DiscordPlugin::new();
    let result = plugin.initialize(make_config(Some("")), make_callbacks()).await;
    assert!(matches!(result, Err(ChannelError::InvalidConfig(_))));
    assert_eq!(plugin.status(), PluginStatus::Error);
}

// -- accessors / defaults ----------------------------------------------------

#[test]
fn new_plugin_is_created_state() {
    let plugin = DiscordPlugin::new();
    assert_eq!(plugin.status(), PluginStatus::Created);
    assert!(plugin.bot_info().is_none());
    assert_eq!(plugin.plugin_type(), PluginType::Discord);
    assert_eq!(plugin.active_user_count(), 0);
    assert!(plugin.last_error().is_none());
}

// -- truncate_message --------------------------------------------------------

#[test]
fn truncate_within_limit() {
    assert_eq!(truncate_message("hello", 100), "hello");
}

#[test]
fn truncate_exceeds_limit() {
    assert_eq!(truncate_message("Hello, world!", 10), "Hello, ...");
}

#[test]
fn truncate_unicode() {
    assert_eq!(truncate_message("你好世界测试文本", 5), "你好...");
}
