use crate::error::ChannelError;
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, UnifiedOutgoingMessage};

/// Callback channels for a channel plugin.
///
/// Instead of closures (which are hard to make object-safe), plugins
/// receive an `mpsc::Sender` for incoming messages and tool-confirmation
/// events. The `ChannelManager` holds the receiving ends.
///
/// This addresses M-63 — the API Spec `BasePlugin.onMessage/onConfirm`
/// callbacks are mapped to channel-based injection.
#[derive(Clone)]
pub struct PluginCallbacks {
    /// Sender for incoming messages from the platform.
    pub message_tx: tokio::sync::mpsc::Sender<UnifiedIncomingMessage>,
    /// Sender for tool confirmation callbacks (callId, value).
    pub confirm_tx: tokio::sync::mpsc::Sender<(String, String)>,
}

/// Abstraction over a platform-specific channel plugin.
///
/// Each IM platform (Telegram, Lark, DingTalk, WeChat) implements this
/// trait behind a feature flag. The `ChannelManager` holds plugins as
/// `Box<dyn ChannelPlugin>` for runtime polymorphism.
///
/// ## Lifecycle
///
/// ```text
/// created → initialize(config, callbacks) → ready → start() → running
///   → stop() → stopped
/// ```
///
/// Any method may transition to `Error` on failure.
#[async_trait::async_trait]
pub trait ChannelPlugin: Send + Sync {
    /// Initialize the plugin with configuration and callback channels.
    ///
    /// Should validate credentials format (but not test the connection).
    /// Transitions status: `Created → Initializing → Ready` (or `Error`).
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError>;

    /// Start the platform connection (long-polling, WebSocket, etc.).
    ///
    /// Transitions status: `Ready → Starting → Running` (or `Error`).
    async fn start(&mut self) -> Result<(), ChannelError>;

    /// Gracefully stop the platform connection.
    ///
    /// Transitions status: `Running → Stopping → Stopped`.
    async fn stop(&mut self) -> Result<(), ChannelError>;

    /// Send a message to a specific chat. Returns the platform message ID.
    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError>;

    /// Edit an existing message on the platform.
    ///
    /// Platforms that don't support editing (e.g., WeChat) may implement
    /// a degraded strategy (send a new reply instead).
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError>;

    /// Number of currently active (chatting) users on this plugin.
    fn active_user_count(&self) -> usize;

    /// Bot identity on the platform, available after initialization.
    fn bot_info(&self) -> Option<&BotInfo>;

    /// The platform type this plugin handles.
    fn plugin_type(&self) -> PluginType;

    /// Current lifecycle status.
    fn status(&self) -> PluginStatus;

    /// The most recent error message, if status is `Error`.
    fn last_error(&self) -> Option<&str>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OutgoingMessageType, PluginCredentials, PluginStatus, PluginType};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Minimal mock plugin for testing the trait interface.
    struct MockPlugin {
        status: PluginStatus,
        plugin_type: PluginType,
        bot_info: Option<BotInfo>,
        last_error: Option<String>,
    }

    impl MockPlugin {
        fn new(plugin_type: PluginType) -> Self {
            Self {
                status: PluginStatus::Created,
                plugin_type,
                bot_info: None,
                last_error: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPlugin for MockPlugin {
        async fn initialize(&mut self, config: PluginConfig, _callbacks: PluginCallbacks) -> Result<(), ChannelError> {
            self.status = PluginStatus::Initializing;
            if config.credentials.token.is_none() {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing token".into());
                return Err(ChannelError::InvalidConfig("Missing token".into()));
            }
            self.bot_info = Some(BotInfo {
                id: "mock_bot".into(),
                username: Some("mock_bot_user".into()),
                display_name: "Mock Bot".into(),
            });
            self.status = PluginStatus::Ready;
            Ok(())
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
            self.status = PluginStatus::Starting;
            self.status = PluginStatus::Running;
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), ChannelError> {
            self.status = PluginStatus::Stopping;
            self.status = PluginStatus::Stopped;
            Ok(())
        }

        async fn send_message(&self, _chat_id: &str, _message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
            Ok("mock_msg_id".into())
        }

        async fn edit_message(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _message: UnifiedOutgoingMessage,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        fn active_user_count(&self) -> usize {
            0
        }

        fn bot_info(&self) -> Option<&BotInfo> {
            self.bot_info.as_ref()
        }

        fn plugin_type(&self) -> PluginType {
            self.plugin_type
        }

        fn status(&self) -> PluginStatus {
            self.status
        }

        fn last_error(&self) -> Option<&str> {
            self.last_error.as_deref()
        }
    }

    fn make_test_config(token: Option<&str>) -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                token: token.map(String::from),
                app_id: None,
                app_secret: None,
                encrypt_key: None,
                verification_token: None,
                client_id: None,
                client_secret: None,
                account_id: None,
                bot_token: None,
                app_token: None,
                extra: HashMap::new(),
            },
            config: None,
        }
    }

    fn make_test_callbacks() -> PluginCallbacks {
        let (message_tx, _message_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        PluginCallbacks { message_tx, confirm_tx }
    }

    fn make_test_outgoing() -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("test".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    #[tokio::test]
    async fn lifecycle_happy_path() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());

        let config = make_test_config(Some("bot:123"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Ready);
        assert!(plugin.bot_info().is_some());

        plugin.start().await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Running);

        plugin.stop().await.unwrap();
        assert_eq!(plugin.status(), PluginStatus::Stopped);
    }

    #[tokio::test]
    async fn initialize_with_missing_token_fails() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(None);
        let result = plugin.initialize(config, make_test_callbacks()).await;
        assert!(result.is_err());
        assert_eq!(plugin.status(), PluginStatus::Error);
        assert_eq!(plugin.last_error(), Some("Missing token"));
    }

    #[tokio::test]
    async fn send_message_returns_id() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(Some("bot:abc"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        plugin.start().await.unwrap();

        let msg_id = plugin.send_message("chat_1", make_test_outgoing()).await.unwrap();
        assert_eq!(msg_id, "mock_msg_id");
    }

    #[tokio::test]
    async fn edit_message_ok() {
        let mut plugin = MockPlugin::new(PluginType::Lark);
        let config = make_test_config(Some("token:xyz"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();
        plugin.start().await.unwrap();

        let result = plugin.edit_message("chat_1", "msg_1", make_test_outgoing()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn plugin_type_accessor() {
        let plugin = MockPlugin::new(PluginType::Dingtalk);
        assert_eq!(plugin.plugin_type(), PluginType::Dingtalk);
    }

    #[test]
    fn active_user_count_default() {
        let plugin = MockPlugin::new(PluginType::Weixin);
        assert_eq!(plugin.active_user_count(), 0);
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        let mut plugin = MockPlugin::new(PluginType::Telegram);
        let config = make_test_config(Some("bot:obj"));
        plugin.initialize(config, make_test_callbacks()).await.unwrap();

        // Verify the plugin can be used as a trait object
        let plugin_ref: &dyn ChannelPlugin = &plugin;
        assert_eq!(plugin_ref.plugin_type(), PluginType::Telegram);
        assert_eq!(plugin_ref.status(), PluginStatus::Ready);
        assert!(plugin_ref.bot_info().is_some());
    }
}
