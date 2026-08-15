use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::constants::DISCORD_MESSAGE_LIMIT;
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedOutgoingMessage};

use super::api::DiscordApi;
use super::gateway::{DedupCache, cleanup_expired_events, gateway_loop};

/// Interval between dedup-cache cleanup sweeps.
const DEDUP_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Discord platform plugin (Gateway).
///
/// Connects to the Discord Gateway over a WebSocket, receives MESSAGE_CREATE
/// events, and replies via the REST API (create/edit message). Editing is
/// supported, so streaming responses use the standard editable relay path.
pub struct DiscordPlugin {
    // Lifecycle state
    status: PluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    /// Bot user id (snowflake), used to filter self-echoes and strip mentions.
    bot_user_id: String,
    /// Bot token, retained for the Gateway IDENTIFY/RESUME handshake.
    token: String,

    // Dependencies (set during `initialize`)
    api: Option<Arc<DiscordApi>>,
    callbacks: Option<PluginCallbacks>,

    // Background tasks
    gateway_handle: Option<JoinHandle<()>>,
    cleanup_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,

    /// Shared message-deduplication cache.
    dedup_cache: DedupCache,
}

impl Default for DiscordPlugin {
    fn default() -> Self {
        Self {
            status: PluginStatus::Created,
            bot_info: None,
            last_error: None,
            bot_user_id: String::new(),
            token: String::new(),
            api: None,
            callbacks: None,
            gateway_handle: None,
            cleanup_handle: None,
            shutdown_tx: None,
            dedup_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl DiscordPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for DiscordPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status = PluginStatus::Initializing;

        let token = config
            .credentials
            .token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing Discord bot token".into());
                ChannelError::InvalidConfig("Missing Discord bot token".into())
            })?
            .to_string();

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status = PluginStatus::Error;
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(DiscordApi::new(http_client, &token));

        // Validate the token and capture the bot identity.
        let identity = api.get_current_user().await.map_err(|e| {
            self.status = PluginStatus::Error;
            self.last_error = Some(format!("Credential validation failed: {e}"));
            e
        })?;

        self.bot_user_id = identity.id.clone();
        self.bot_info = Some(BotInfo {
            id: identity.id,
            username: identity.display_name.clone(),
            display_name: identity.display_name.unwrap_or_else(|| "Discord Bot".into()),
        });

        info!(bot_user_id = %self.bot_user_id, "Discord bot initialized");

        self.token = token;
        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status = PluginStatus::Ready;
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Starting;

        let callbacks = self.callbacks.take().ok_or_else(|| {
            self.status = PluginStatus::Error;
            ChannelError::ConnectionFailed("Plugin not initialized".into())
        })?;
        // Discord MVP does not surface interactive tool confirmations; drop the
        // confirm sender clone.
        let PluginCallbacks {
            message_tx,
            confirm_tx: _,
        } = callbacks;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let api = Arc::clone(self.api.as_ref().expect("api set in initialize"));
        let token = self.token.clone();
        let bot_user_id = self.bot_user_id.clone();
        let dedup_cache = Arc::clone(&self.dedup_cache);

        self.gateway_handle = Some(tokio::spawn(gateway_loop(
            api,
            token,
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
        info!("Discord plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Stopping;

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.gateway_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        if let Some(handle) = self.cleanup_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        self.api = None;
        self.status = PluginStatus::Stopped;
        info!("Discord plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), DISCORD_MESSAGE_LIMIT);
        api.create_message(chat_id, &text, message.reply_to_message_id.as_deref())
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

        let text = truncate_message(message.text.as_deref().unwrap_or(""), DISCORD_MESSAGE_LIMIT);
        api.edit_message(chat_id, message_id, &text).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Discord
    }

    fn status(&self) -> PluginStatus {
        self.status
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
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
