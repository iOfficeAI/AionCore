use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::constants::SLACK_MESSAGE_LIMIT;
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedOutgoingMessage};

use super::api::SlackApi;
use super::socket::{DedupCache, cleanup_expired_events, socket_loop};

/// Interval between dedup-cache cleanup sweeps.
const DEDUP_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Slack platform plugin (Socket Mode).
///
/// Connects to Slack over a Socket Mode WebSocket, receives `app_mention` and
/// direct `message` events, and replies via the Web API (`chat.postMessage` /
/// `chat.update`). Editing is supported, so streaming responses use the
/// standard editable relay path.
pub struct SlackPlugin {
    // Lifecycle state
    status: PluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    /// Bot user id (`Uxxxx`) used to filter self-echoes and strip mentions.
    bot_user_id: String,
    /// Whether an app-level token (`xapp-`) was supplied. Required only to open
    /// the Socket Mode connection in `start()`, not for the credential test.
    app_token_present: bool,

    // Dependencies (set during `initialize`)
    api: Option<Arc<SlackApi>>,
    callbacks: Option<PluginCallbacks>,

    // Background tasks
    ws_handle: Option<JoinHandle<()>>,
    cleanup_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,

    /// Shared event-deduplication cache.
    dedup_cache: DedupCache,
}

impl Default for SlackPlugin {
    fn default() -> Self {
        Self {
            status: PluginStatus::Created,
            bot_info: None,
            last_error: None,
            bot_user_id: String::new(),
            app_token_present: false,
            api: None,
            callbacks: None,
            ws_handle: None,
            cleanup_handle: None,
            shutdown_tx: None,
            dedup_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SlackPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for SlackPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status = PluginStatus::Initializing;

        let bot_token = match require_bot_token(&config) {
            Ok(token) => token,
            Err(e) => {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing Slack bot token".into());
                return Err(e);
            }
        };

        // App-level token (xapp-) is optional here: it is only needed to open
        // the Socket Mode connection in `start()`. The credential-test path
        // sends the bot token alone, so requiring it here would break "Test".
        let app_token = config
            .credentials
            .app_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        self.app_token_present = !app_token.is_empty();

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status = PluginStatus::Error;
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(SlackApi::new(http_client, &bot_token, app_token));

        // Validate the bot token and capture the bot identity.
        let identity = api.auth_test().await.map_err(|e| {
            self.status = PluginStatus::Error;
            self.last_error = Some(format!("Credential validation failed: {e}"));
            e
        })?;

        self.bot_user_id = identity.user_id.clone();
        self.bot_info = Some(BotInfo {
            id: identity.user_id,
            username: identity.user.clone(),
            display_name: identity.user.unwrap_or_else(|| "Slack Bot".into()),
        });

        info!(bot_user_id = %self.bot_user_id, "Slack bot initialized");

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status = PluginStatus::Ready;
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Starting;

        // The app-level token is mandatory to open the Socket Mode connection.
        // Validated here (not in `initialize`) so the credential-test path can
        // succeed with the bot token alone.
        if !self.app_token_present {
            self.status = PluginStatus::Error;
            self.last_error = Some("Missing Slack app-level token".into());
            return Err(ChannelError::InvalidConfig(
                "Missing Slack app-level token (xapp-), required for Socket Mode".into(),
            ));
        }

        let callbacks = self.callbacks.take().ok_or_else(|| {
            self.status = PluginStatus::Error;
            ChannelError::ConnectionFailed("Plugin not initialized".into())
        })?;
        // Slack MVP does not surface interactive tool confirmations; drop the
        // confirm sender clone.
        let PluginCallbacks {
            message_tx,
            confirm_tx: _,
        } = callbacks;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let api = Arc::clone(self.api.as_ref().expect("api set in initialize"));
        let bot_user_id = self.bot_user_id.clone();
        let dedup_cache = Arc::clone(&self.dedup_cache);

        self.ws_handle = Some(tokio::spawn(socket_loop(
            api,
            bot_user_id,
            message_tx,
            Arc::clone(&dedup_cache),
            shutdown_rx,
        )));

        let mut cleanup_shutdown = self.shutdown_tx.as_ref().expect("shutdown_tx just set").subscribe();
        self.cleanup_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(DEDUP_CLEANUP_INTERVAL) => {
                        cleanup_expired_events(&dedup_cache).await;
                    }
                    _ = cleanup_shutdown.changed() => break,
                }
            }
        }));

        self.status = PluginStatus::Running;
        info!("Slack plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Stopping;

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.ws_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        if let Some(handle) = self.cleanup_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        self.api = None;
        self.status = PluginStatus::Stopped;
        info!("Slack plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), SLACK_MESSAGE_LIMIT);
        api.post_message(chat_id, &text, message.reply_to_message_id.as_deref())
            .await
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), SLACK_MESSAGE_LIMIT);
        api.update_message(chat_id, message_id, &text).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Slack
    }

    fn status(&self) -> PluginStatus {
        self.status
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// Validate that the Slack bot token (`xoxb-`) is present.
///
/// The app-level token (`xapp-`) is intentionally NOT required here: it is only
/// needed to open the Socket Mode connection in `start()`. Requiring it during
/// `initialize` would break the credential-test path, which sends the bot token
/// alone.
fn require_bot_token(config: &PluginConfig) -> Result<String, ChannelError> {
    config
        .credentials
        .token
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ChannelError::InvalidConfig("Missing Slack bot token (xoxb-)".into()))
}

/// Truncate a message to the platform limit, appending "..." if truncated.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
#[path = "plugin_test.rs"]
mod plugin_test;
