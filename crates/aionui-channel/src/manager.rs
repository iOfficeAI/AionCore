use std::sync::Arc;

use aionui_api_types::{PluginStatusChangedPayload, PluginStatusResponse, WebSocketMessage};
use aionui_common::{decrypt_string, encrypt_string, now_ms};
use aionui_db::models::ChannelPluginRow;
use aionui_db::{IChannelRepository, UpdatePluginStatusParams};
use aionui_realtime::EventBroadcaster;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage};

/// Manages the lifecycle of channel plugins.
///
/// Responsibilities:
/// - Loading enabled plugins from DB on startup (`restore_plugins`)
/// - Enabling/disabling plugins (with credential encryption)
/// - Testing plugin credentials without persisting
/// - Broadcasting status change events via WebSocket
/// - Holding active plugin instances in a concurrent map
///
/// Plugin instances are stored as `Box<dyn ChannelPlugin>` behind a
/// `DashMap` for lock-free concurrent access.
pub struct ChannelManager {
    repo: Arc<dyn IChannelRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    encryption_key: [u8; 32],
    /// Active plugin instances keyed by plugin ID.
    plugins: DashMap<String, Box<dyn ChannelPlugin>>,
    /// Sender for incoming messages from all plugins.
    /// The `ActionExecutor` holds the receiving end.
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    /// Sender for tool confirmation callbacks from all plugins.
    confirm_tx: mpsc::Sender<(String, String)>,
}

/// Factory function type for creating plugin instances.
///
/// Platform-specific implementations register their factory via
/// `ChannelManager::enable_plugin`. The factory is called with a
/// `PluginType` and returns a boxed trait object.
///
/// This keeps the manager decoupled from concrete plugin types —
/// platform implementations are behind feature flags.
pub type PluginFactory = Box<dyn Fn(PluginType) -> Option<Box<dyn ChannelPlugin>> + Send + Sync>;

impl ChannelManager {
    /// Creates a new `ChannelManager`.
    ///
    /// # Arguments
    ///
    /// - `repo`: Data access for plugin configuration persistence
    /// - `broadcaster`: WebSocket event broadcaster for status updates
    /// - `encryption_key`: 32-byte AES-256-GCM key for credential encryption
    /// - `message_tx`: Channel sender for routing incoming messages
    /// - `confirm_tx`: Channel sender for tool confirmation callbacks
    pub fn new(
        repo: Arc<dyn IChannelRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        encryption_key: [u8; 32],
        message_tx: mpsc::Sender<UnifiedIncomingMessage>,
        confirm_tx: mpsc::Sender<(String, String)>,
    ) -> Self {
        Self {
            repo,
            broadcaster,
            encryption_key,
            plugins: DashMap::new(),
            message_tx,
            confirm_tx,
        }
    }

    /// Returns the status of all registered plugins from the database.
    ///
    /// Merges DB state with live runtime status for active plugins.
    pub async fn get_plugin_status(&self) -> Result<Vec<PluginStatusResponse>, ChannelError> {
        let rows = self.repo.get_all_plugins().await?;
        let statuses: Vec<PluginStatusResponse> = rows
            .into_iter()
            .map(|row| {
                let live_status = self.plugins.get(&row.id).map(|p| p.status().to_string());
                self.row_to_status_response(&row, live_status)
            })
            .collect();
        Ok(statuses)
    }

    /// Enables a plugin: validates config, encrypts credentials, persists
    /// to DB, and starts the plugin connection.
    ///
    /// If the plugin is already running, it will be stopped first and
    /// restarted with the new configuration.
    ///
    /// # Arguments
    ///
    /// - `plugin_id`: Platform identifier (e.g., "telegram")
    /// - `config_value`: Raw JSON config containing credentials and options
    /// - `factory`: Function to create the platform-specific plugin instance
    pub async fn enable_plugin(
        &self,
        plugin_id: &str,
        config_value: &serde_json::Value,
        factory: &PluginFactory,
    ) -> Result<(), ChannelError> {
        let plugin_type =
            PluginType::from_str_opt(plugin_id).ok_or_else(|| ChannelError::InvalidPluginType(plugin_id.to_owned()))?;

        // Resolve the effective config. The Settings re-enable toggle sends an
        // empty config and expects the previously stored credentials to be
        // reused, so fall back to the persisted config when no new credentials
        // are supplied.
        let config: PluginConfig = match Self::config_with_credentials(config_value)? {
            Some(config) => config,
            None => self.load_stored_config(plugin_id).await?,
        };

        // Stop existing plugin if running
        if self.plugins.contains_key(plugin_id) {
            self.stop_plugin(plugin_id).await;
        }

        // Encrypt config for storage
        let config_json = serde_json::to_string(&config)?;
        let encrypted_config = encrypt_string(&config_json, &self.encryption_key)
            .map_err(|e| ChannelError::EncryptionFailed(e.to_string()))?;

        // Persist to DB
        let now = now_ms();
        let row = ChannelPluginRow {
            id: plugin_id.to_owned(),
            r#type: plugin_type.to_string(),
            name: self.default_plugin_name(plugin_type),
            enabled: true,
            config: encrypted_config,
            status: Some(PluginStatus::Created.to_string()),
            last_connected: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.upsert_plugin(&row).await?;

        // Create and start plugin instance
        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        let callbacks = PluginCallbacks {
            message_tx: self.message_tx.clone(),
            confirm_tx: self.confirm_tx.clone(),
        };

        if let Err(e) = plugin.initialize(config, callbacks).await {
            self.update_plugin_error(plugin_id, &e.to_string()).await;
            self.broadcast_status_change(plugin_id).await;
            return Err(e);
        }

        if let Err(e) = plugin.start().await {
            self.update_plugin_error(plugin_id, &e.to_string()).await;
            self.broadcast_status_change(plugin_id).await;
            return Err(e);
        }

        // Update DB with running status
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Running.to_string()),
            last_connected: Some(now_ms()),
            enabled: None,
        };
        self.repo.update_plugin_status(plugin_id, &params).await?;

        // Store active instance
        self.plugins.insert(plugin_id.to_owned(), plugin);

        info!(plugin_id = %plugin_id, "plugin enabled and started");
        self.broadcast_status_change(plugin_id).await;
        Ok(())
    }

    /// Enables an extension-contributed plugin in metadata-only mode.
    ///
    /// The backend does not yet execute extension channel runtime JS, but we
    /// still persist the plugin configuration and enabled flag so Settings UI
    /// can behave consistently and survive restarts.
    pub async fn enable_extension_plugin(
        &self,
        plugin_id: &str,
        plugin_name: &str,
        config: &PluginConfig,
    ) -> Result<(), ChannelError> {
        if self.plugins.contains_key(plugin_id) {
            self.stop_plugin(plugin_id).await;
        }

        let config_json = serde_json::to_string(config)?;
        let encrypted_config = encrypt_string(&config_json, &self.encryption_key)
            .map_err(|e| ChannelError::EncryptionFailed(e.to_string()))?;

        let now = now_ms();
        let existing = self.repo.get_plugin(plugin_id).await?;
        let row = ChannelPluginRow {
            id: plugin_id.to_owned(),
            r#type: plugin_id.to_owned(),
            name: plugin_name.to_owned(),
            enabled: true,
            config: encrypted_config,
            status: Some(PluginStatus::Stopped.to_string()),
            last_connected: existing.as_ref().and_then(|row| row.last_connected),
            created_at: existing.as_ref().map(|row| row.created_at).unwrap_or(now),
            updated_at: now,
        };
        self.repo.upsert_plugin(&row).await?;

        info!(plugin_id = %plugin_id, "extension plugin enabled (metadata-only mode)");
        self.broadcast_status_change(plugin_id).await;
        Ok(())
    }

    /// Disables a plugin: stops the connection, updates DB, and removes
    /// the active instance.
    ///
    /// Idempotent — disabling an already-disabled plugin is a no-op.
    pub async fn disable_plugin(&self, plugin_id: &str) -> Result<(), ChannelError> {
        // Stop running instance if any
        self.stop_plugin(plugin_id).await;

        // Update DB
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Stopped.to_string()),
            last_connected: None,
            enabled: Some(false),
        };
        self.repo.update_plugin_status(plugin_id, &params).await?;

        info!(plugin_id = %plugin_id, "plugin disabled");
        self.broadcast_status_change(plugin_id).await;
        Ok(())
    }

    /// Tests plugin credentials without persisting.
    ///
    /// Creates a temporary plugin instance, initializes it with the
    /// provided credentials, and checks if the connection succeeds.
    /// Returns the bot username on success.
    pub async fn test_plugin(
        &self,
        plugin_id: &str,
        config: PluginConfig,
        factory: &PluginFactory,
    ) -> Result<Option<String>, ChannelError> {
        let plugin_type =
            PluginType::from_str_opt(plugin_id).ok_or_else(|| ChannelError::InvalidPluginType(plugin_id.to_owned()))?;

        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        // Create throwaway channels for the test
        let (msg_tx, _msg_rx) = mpsc::channel(1);
        let (confirm_tx, _confirm_rx) = mpsc::channel(1);
        let callbacks = PluginCallbacks {
            message_tx: msg_tx,
            confirm_tx,
        };

        plugin.initialize(config, callbacks).await?;

        let bot_username = plugin.bot_info().and_then(|b| b.username.clone());

        // Clean up — don't leave a started connection
        debug!(plugin_id = %plugin_id, "plugin credential test successful");
        Ok(bot_username)
    }

    /// Restores previously enabled plugins on startup.
    ///
    /// Reads all enabled plugins from DB, decrypts their config, and
    /// starts them. Errors on individual plugins are logged but don't
    /// prevent other plugins from starting.
    pub async fn restore_plugins(&self, factory: &PluginFactory) -> Result<(), ChannelError> {
        let rows = self.repo.get_all_plugins().await?;
        let enabled: Vec<ChannelPluginRow> = rows.into_iter().filter(|r| r.enabled).collect();

        if enabled.is_empty() {
            debug!("no enabled plugins to restore");
            return Ok(());
        }

        info!(count = enabled.len(), "restoring enabled plugins");

        for row in enabled {
            if PluginType::from_str_opt(&row.r#type).is_none() {
                info!(
                    plugin_id = %row.id,
                    plugin_type = %row.r#type,
                    "skipping extension plugin runtime restore; metadata-only mode"
                );
                self.broadcast_status_change(&row.id).await;
                continue;
            }
            if let Err(e) = self.restore_single_plugin(&row, factory).await {
                warn!(
                    plugin_id = %row.id,
                    error = %e,
                    "failed to restore plugin, marking as error"
                );
                self.update_plugin_error(&row.id, &e.to_string()).await;
                self.broadcast_status_change(&row.id).await;
            }
        }

        Ok(())
    }

    /// Gracefully stops all active plugin connections.
    ///
    /// Called during application shutdown.
    pub async fn shutdown(&self) {
        let keys: Vec<String> = self.plugins.iter().map(|entry| entry.key().clone()).collect();

        for key in keys {
            self.stop_plugin(&key).await;
        }
        info!("all plugins shut down");
    }

    /// Returns the number of currently active (in-memory) plugins.
    pub fn active_plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// Checks whether a specific plugin is currently running.
    pub fn is_plugin_running(&self, plugin_id: &str) -> bool {
        self.plugins
            .get(plugin_id)
            .map(|p| p.status() == PluginStatus::Running)
            .unwrap_or(false)
    }

    /// Sends a message through a specific plugin.
    ///
    /// Used by the `ChannelMessageService` to route outgoing messages
    /// to the correct platform plugin.
    pub async fn send_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;
        plugin.send_message(chat_id, message).await
    }

    /// Edits an existing message through a specific plugin.
    pub async fn edit_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;
        plugin.edit_message(chat_id, message_id, message).await
    }

    // ── Private helpers ──────────────────────────────────────────────

    /// Parses a freshly supplied plugin config, returning it only when it
    /// carries usable credentials.
    ///
    /// Returns `Ok(None)` when the caller supplied no credentials (an empty
    /// `{}` config or one whose `credentials` field is absent or blank),
    /// signalling that the stored configuration should be reused. A config that
    /// does carry credentials but fails to parse is reported as `InvalidConfig`.
    fn config_with_credentials(config_value: &serde_json::Value) -> Result<Option<PluginConfig>, ChannelError> {
        let credentials_supplied = config_value
            .get("credentials")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|creds| !creds.is_empty());
        if !credentials_supplied {
            return Ok(None);
        }

        let config: PluginConfig = serde_json::from_value(config_value.clone())
            .map_err(|e| ChannelError::InvalidConfig(format!("Invalid config: {e}")))?;
        if config.credentials.is_empty() {
            Ok(None)
        } else {
            Ok(Some(config))
        }
    }

    /// Loads and decrypts the persisted config for a plugin.
    ///
    /// Used when an enable request omits credentials and the stored
    /// configuration should be reused (Settings re-enable toggle). Returns
    /// `InvalidConfig` when there is no stored config to fall back to.
    async fn load_stored_config(&self, plugin_id: &str) -> Result<PluginConfig, ChannelError> {
        let row = self
            .repo
            .get_plugin(plugin_id)
            .await?
            .filter(|row| !row.config.is_empty())
            .ok_or_else(|| {
                ChannelError::InvalidConfig(format!(
                    "No credentials provided and no stored configuration for plugin '{plugin_id}'"
                ))
            })?;

        let config_json = decrypt_string(&row.config, &self.encryption_key)
            .map_err(|e| ChannelError::DecryptionFailed(e.to_string()))?;
        let config: PluginConfig = serde_json::from_str(&config_json)?;
        Ok(config)
    }

    /// Stops and removes an active plugin instance.
    async fn stop_plugin(&self, plugin_id: &str) {
        if let Some((_, mut plugin)) = self.plugins.remove(plugin_id) {
            if let Err(e) = plugin.stop().await {
                warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "error stopping plugin"
                );
            }
            debug!(plugin_id = %plugin_id, "plugin stopped");
        }
    }

    /// Restores a single plugin from its DB row.
    async fn restore_single_plugin(&self, row: &ChannelPluginRow, factory: &PluginFactory) -> Result<(), ChannelError> {
        let plugin_type =
            PluginType::from_str_opt(&row.r#type).ok_or_else(|| ChannelError::InvalidPluginType(row.r#type.clone()))?;

        // Decrypt config
        let config_json = decrypt_string(&row.config, &self.encryption_key)
            .map_err(|e| ChannelError::DecryptionFailed(e.to_string()))?;
        let config: PluginConfig = serde_json::from_str(&config_json)?;

        let mut plugin = factory(plugin_type)
            .ok_or_else(|| ChannelError::InvalidPluginType(format!("No implementation for {plugin_type}")))?;

        let callbacks = PluginCallbacks {
            message_tx: self.message_tx.clone(),
            confirm_tx: self.confirm_tx.clone(),
        };

        plugin.initialize(config, callbacks).await?;
        plugin.start().await?;

        // Update DB with running status
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Running.to_string()),
            last_connected: Some(now_ms()),
            enabled: None,
        };
        self.repo.update_plugin_status(&row.id, &params).await?;

        self.plugins.insert(row.id.clone(), plugin);
        info!(plugin_id = %row.id, "plugin restored");
        self.broadcast_status_change(&row.id).await;
        Ok(())
    }

    /// Updates a plugin to error status in the DB.
    async fn update_plugin_error(&self, plugin_id: &str, error_msg: &str) {
        let params = UpdatePluginStatusParams {
            status: Some(PluginStatus::Error.to_string()),
            last_connected: None,
            enabled: None,
        };
        if let Err(e) = self.repo.update_plugin_status(plugin_id, &params).await {
            error!(
                plugin_id = %plugin_id,
                db_error = %e,
                original_error = %error_msg,
                "failed to update plugin error status in DB"
            );
        }
    }

    /// Broadcasts a `channel.plugin-status-changed` event.
    async fn broadcast_status_change(&self, plugin_id: &str) {
        let row = match self.repo.get_plugin(plugin_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(plugin_id = %plugin_id, "plugin not found for status broadcast");
                return;
            }
            Err(e) => {
                warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "failed to read plugin for status broadcast"
                );
                return;
            }
        };

        let live_status = self.plugins.get(plugin_id).map(|p| p.status().to_string());
        let status_response = self.row_to_status_response(&row, live_status);

        let payload = PluginStatusChangedPayload {
            plugin_id: plugin_id.to_owned(),
            status: status_response,
        };
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "failed to serialize status change payload");
                return;
            }
        };
        self.broadcaster
            .broadcast(WebSocketMessage::new("channel.plugin-status-changed", value));
    }

    /// Converts a DB row + optional live status to a `PluginStatusResponse`.
    fn row_to_status_response(&self, row: &ChannelPluginRow, live_status: Option<String>) -> PluginStatusResponse {
        let is_running = self.plugins.contains_key(&row.id);
        let has_token = !row.config.is_empty();
        PluginStatusResponse {
            plugin_id: row.id.clone(),
            plugin_type: row.r#type.clone(),
            name: row.name.clone(),
            enabled: row.enabled,
            status: live_status.or_else(|| row.status.clone()),
            last_connected: row.last_connected,
            created_at: row.created_at,
            updated_at: row.updated_at,
            connected: is_running,
            has_token,
            bot_username: None,
            active_users: 0,
        }
    }

    /// Returns a default display name for a plugin type.
    fn default_plugin_name(&self, plugin_type: PluginType) -> String {
        match plugin_type {
            PluginType::Telegram => "Telegram Bot".into(),
            PluginType::Lark => "Lark Bot".into(),
            PluginType::Dingtalk => "DingTalk Bot".into(),
            PluginType::Weixin => "WeChat Bot".into(),
            PluginType::Slack => "Slack Bot".into(),
            PluginType::Discord => "Discord Bot".into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::stream_relay::ChannelSender for ChannelManager {
    async fn send_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<String, crate::error::ChannelError> {
        self.send_message(plugin_id, chat_id, message).await
    }

    async fn edit_message(
        &self,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: crate::types::UnifiedOutgoingMessage,
    ) -> Result<(), crate::error::ChannelError> {
        self.edit_message(plugin_id, chat_id, message_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BotInfo, OutgoingMessageType, PluginCredentials, PluginStatus, PluginType, UnifiedOutgoingMessage,
    };
    use aionui_common::TimestampMs;
    use aionui_db::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};
    use aionui_db::{DbError, IChannelRepository, UpdatePluginStatusParams};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Mock EventBroadcaster ──────────────────────────────────────────

    struct MockBroadcaster {
        events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl MockBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            let mut guard = self.events.lock().unwrap();
            std::mem::take(&mut *guard)
        }
    }

    impl EventBroadcaster for MockBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    // ── Mock IChannelRepository ────────────────────────────────────────

    struct MockRepo {
        plugins: Mutex<Vec<ChannelPluginRow>>,
    }

    impl MockRepo {
        fn new() -> Self {
            Self {
                plugins: Mutex::new(Vec::new()),
            }
        }

        fn get_plugins(&self) -> Vec<ChannelPluginRow> {
            self.plugins.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IChannelRepository for MockRepo {
        async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError> {
            Ok(self.plugins.lock().unwrap().clone())
        }

        async fn get_plugin(&self, id: &str) -> Result<Option<ChannelPluginRow>, DbError> {
            let plugins = self.plugins.lock().unwrap();
            Ok(plugins.iter().find(|p| p.id == id).cloned())
        }

        async fn upsert_plugin(&self, row: &ChannelPluginRow) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(existing) = plugins.iter_mut().find(|p| p.id == row.id) {
                *existing = row.clone();
            } else {
                plugins.push(row.clone());
            }
            Ok(())
        }

        async fn update_plugin_status(&self, id: &str, params: &UpdatePluginStatusParams) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == id) {
                if let Some(ref s) = params.status {
                    p.status = Some(s.clone());
                }
                if let Some(lc) = params.last_connected {
                    p.last_connected = Some(lc);
                }
                if let Some(e) = params.enabled {
                    p.enabled = e;
                }
                p.updated_at = now_ms();
                Ok(())
            } else {
                Err(DbError::NotFound(id.into()))
            }
        }

        async fn delete_plugin(&self, id: &str) -> Result<(), DbError> {
            let mut plugins = self.plugins.lock().unwrap();
            let len_before = plugins.len();
            plugins.retain(|p| p.id != id);
            if plugins.len() == len_before {
                Err(DbError::NotFound(id.into()))
            } else {
                Ok(())
            }
        }

        // -- User CRUD (unused stubs) --
        async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError> {
            Ok(vec![])
        }
        async fn get_user_by_platform(&self, _pid: &str, _pt: &str) -> Result<Option<AssistantUserRow>, DbError> {
            Ok(None)
        }
        async fn create_user(&self, _row: &AssistantUserRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_user_last_active(&self, _id: &str, _la: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_user(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Session CRUD (unused stubs) --
        async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError> {
            Ok(vec![])
        }
        async fn get_session(&self, _id: &str) -> Result<Option<AssistantSessionRow>, DbError> {
            Ok(None)
        }
        async fn get_or_create_session(
            &self,
            _uid: &str,
            _cid: &str,
            new_row: &AssistantSessionRow,
        ) -> Result<AssistantSessionRow, DbError> {
            Ok(new_row.clone())
        }
        async fn update_session_activity(&self, _id: &str, _la: TimestampMs) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_conversation(&self, _id: &str, _cid: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn update_session_agent_type(&self, _id: &str, _at: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_sessions_by_user(&self, _uid: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn delete_session_by_user_chat(&self, _uid: &str, _cid: &str) -> Result<(), DbError> {
            Ok(())
        }

        // -- Pairing codes (unused stubs) --
        async fn create_pairing(&self, _row: &PairingCodeRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError> {
            Ok(vec![])
        }
        async fn get_pairing_by_code(&self, _code: &str) -> Result<Option<PairingCodeRow>, DbError> {
            Ok(None)
        }
        async fn update_pairing_status(&self, _code: &str, _status: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn cleanup_expired_pairings(&self, _now: TimestampMs) -> Result<u64, DbError> {
            Ok(0)
        }
    }

    // ── Mock ChannelPlugin ─────────────────────────────────────────────

    struct MockPlugin {
        status: PluginStatus,
        plugin_type: PluginType,
        bot_info: Option<BotInfo>,
        last_error: Option<String>,
        should_fail_init: bool,
        should_fail_start: bool,
    }

    impl MockPlugin {
        fn new(plugin_type: PluginType) -> Self {
            Self {
                status: PluginStatus::Created,
                plugin_type,
                bot_info: None,
                last_error: None,
                should_fail_init: false,
                should_fail_start: false,
            }
        }

        fn failing_init(plugin_type: PluginType) -> Self {
            Self {
                should_fail_init: true,
                ..Self::new(plugin_type)
            }
        }

        fn failing_start(plugin_type: PluginType) -> Self {
            Self {
                should_fail_start: true,
                ..Self::new(plugin_type)
            }
        }
    }

    #[async_trait::async_trait]
    impl ChannelPlugin for MockPlugin {
        async fn initialize(&mut self, _config: PluginConfig, _callbacks: PluginCallbacks) -> Result<(), ChannelError> {
            if self.should_fail_init {
                self.status = PluginStatus::Error;
                self.last_error = Some("Init failed".into());
                return Err(ChannelError::ConnectionFailed("Init failed".into()));
            }
            self.status = PluginStatus::Initializing;
            self.bot_info = Some(BotInfo {
                id: "mock_bot".into(),
                username: Some("mock_bot_user".into()),
                display_name: "Mock Bot".into(),
            });
            self.status = PluginStatus::Ready;
            Ok(())
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
            if self.should_fail_start {
                self.status = PluginStatus::Error;
                self.last_error = Some("Start failed".into());
                return Err(ChannelError::ConnectionFailed("Start failed".into()));
            }
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

    // ── Test helpers ───────────────────────────────────────────────────

    fn test_key() -> [u8; 32] {
        [0x42; 32]
    }

    fn make_manager() -> (ChannelManager, Arc<MockRepo>, Arc<MockBroadcaster>) {
        let repo = Arc::new(MockRepo::new());
        let broadcaster = Arc::new(MockBroadcaster::new());
        let (msg_tx, _msg_rx) = mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = mpsc::channel(16);
        let mgr = ChannelManager::new(repo.clone(), broadcaster.clone(), test_key(), msg_tx, confirm_tx);
        (mgr, repo, broadcaster)
    }

    fn make_test_config() -> serde_json::Value {
        serde_json::json!({
            "credentials": { "token": "bot:test123" },
            "config": { "mode": "polling" }
        })
    }

    fn make_plugin_config() -> PluginConfig {
        PluginConfig {
            credentials: PluginCredentials {
                token: Some("bot:test123".into()),
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
            config: None,
        }
    }

    fn make_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::new(pt))))
    }

    fn make_failing_init_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::failing_init(pt))))
    }

    fn make_failing_start_factory() -> PluginFactory {
        Box::new(|pt| Some(Box::new(MockPlugin::failing_start(pt))))
    }

    fn make_no_impl_factory() -> PluginFactory {
        Box::new(|_pt| None)
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

    // ── get_plugin_status ──────────────────────────────────────────────

    #[tokio::test]
    async fn get_status_empty() {
        let (mgr, _repo, _bc) = make_manager();
        let statuses = mgr.get_plugin_status().await.unwrap();
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn get_status_returns_db_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let now = now_ms();
        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            config: "encrypted".into(),
            status: Some("running".into()),
            last_connected: Some(now),
            created_at: now,
            updated_at: now,
        });

        let statuses = mgr.get_plugin_status().await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].plugin_id, "telegram");
        assert_eq!(statuses[0].plugin_type, "telegram");
        assert_eq!(statuses[0].name, "Telegram Bot");
        assert!(statuses[0].enabled);
    }

    #[tokio::test]
    async fn get_status_uses_live_status_over_db() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        // Enable the plugin (will set live status to Running)
        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        // Manually set DB status to something different
        {
            let mut plugins = repo.plugins.lock().unwrap();
            if let Some(p) = plugins.iter_mut().find(|p| p.id == "telegram") {
                p.status = Some("stopped".into());
            }
        }

        let statuses = mgr.get_plugin_status().await.unwrap();
        assert_eq!(statuses.len(), 1);
        // Live status (running) should override DB status (stopped)
        assert_eq!(statuses[0].status.as_deref(), Some("running"));
    }

    // ── enable_plugin ──────────────────────────────────────────────────

    #[tokio::test]
    async fn enable_plugin_persists_encrypted_config() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        let plugins = repo.get_plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "telegram");
        assert!(plugins[0].enabled);
        // Config should be encrypted (base64), not plaintext
        assert_ne!(plugins[0].config, serde_json::to_string(&make_test_config()).unwrap());
        // Verify it can be decrypted back
        let decrypted = decrypt_string(&plugins[0].config, &test_key()).unwrap();
        let parsed: PluginConfig = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.credentials.token.as_deref(), Some("bot:test123"));
    }

    #[tokio::test]
    async fn enable_plugin_stores_running_instance() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
    }

    #[tokio::test]
    async fn enable_plugin_broadcasts_status_change() {
        let (mgr, _repo, bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        let events = bc.take_events();
        assert!(!events.is_empty());
        let last = events.last().unwrap();
        assert_eq!(last.name, "channel.plugin-status-changed");
        assert_eq!(last.data["plugin_id"], "telegram");
    }

    #[tokio::test]
    async fn enable_replaces_existing_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);

        // Re-enable should replace (stop old, start new)
        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);
    }

    #[tokio::test]
    async fn enable_invalid_plugin_type_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let err = mgr
            .enable_plugin("whatsapp", &make_test_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn enable_invalid_config_json_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let bad_config = serde_json::json!({ "wrong": "shape" });
        let err = mgr.enable_plugin("telegram", &bad_config, &factory).await.unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn enable_no_implementation_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_no_impl_factory();

        let err = mgr
            .enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn enable_init_failure_sets_error_status() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_failing_init_factory();

        let err = mgr.enable_plugin("telegram", &make_test_config(), &factory).await;
        assert!(err.is_err());

        // Plugin should not be in active map
        assert_eq!(mgr.active_plugin_count(), 0);

        // DB should have error status
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn enable_start_failure_sets_error_status() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_failing_start_factory();

        let err = mgr.enable_plugin("telegram", &make_test_config(), &factory).await;
        assert!(err.is_err());

        assert_eq!(mgr.active_plugin_count(), 0);
        let plugins = repo.get_plugins();
        assert_eq!(plugins[0].status.as_deref(), Some("error"));
    }

    // ── disable_plugin ─────────────────────────────────────────────────

    #[tokio::test]
    async fn disable_stops_and_updates_db() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();
        assert!(mgr.is_plugin_running("telegram"));

        mgr.disable_plugin("telegram").await.unwrap();

        assert_eq!(mgr.active_plugin_count(), 0);
        let plugins = repo.get_plugins();
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[0].status.as_deref(), Some("stopped"));
    }

    #[tokio::test]
    async fn disable_broadcasts_status_change() {
        let (mgr, _repo, bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();
        bc.take_events(); // clear enable events

        mgr.disable_plugin("telegram").await.unwrap();

        let events = bc.take_events();
        assert!(!events.is_empty());
        assert_eq!(events.last().unwrap().name, "channel.plugin-status-changed");
    }

    #[tokio::test]
    async fn disable_idempotent_for_not_running() {
        let (mgr, repo, _bc) = make_manager();
        // Manually insert a disabled plugin in DB
        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: false,
            config: "encrypted".into(),
            status: Some("stopped".into()),
            last_connected: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        // Should not error
        mgr.disable_plugin("telegram").await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── test_plugin ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_plugin_returns_bot_username() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let result = mgr
            .test_plugin("telegram", make_plugin_config(), &factory)
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some("mock_bot_user"));
    }

    #[tokio::test]
    async fn test_plugin_does_not_persist() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.test_plugin("telegram", make_plugin_config(), &factory)
            .await
            .unwrap();

        // Nothing should be stored in DB
        assert!(repo.get_plugins().is_empty());
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn test_plugin_invalid_type_fails() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        let err = mgr
            .test_plugin("whatsapp", make_plugin_config(), &factory)
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::InvalidPluginType(_)));
    }

    #[tokio::test]
    async fn test_plugin_init_failure_propagates() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_failing_init_factory();

        let err = mgr.test_plugin("telegram", make_plugin_config(), &factory).await;
        assert!(err.is_err());
    }

    // ── restore_plugins ────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_skips_disabled_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: false,
            config: encrypted,
            status: Some("stopped".into()),
            last_connected: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn restore_starts_enabled_plugins() {
        let (mgr, repo, _bc) = make_manager();
        let factory = make_factory();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        repo.plugins.lock().unwrap().push(ChannelPluginRow {
            id: "telegram".into(),
            r#type: "telegram".into(),
            name: "Telegram Bot".into(),
            enabled: true,
            config: encrypted,
            status: Some("stopped".into()),
            last_connected: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        });

        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
    }

    #[tokio::test]
    async fn restore_continues_on_individual_failure() {
        let (mgr, repo, _bc) = make_manager();

        let config_json = serde_json::to_string(&make_plugin_config()).unwrap();
        let encrypted = encrypt_string(&config_json, &test_key()).unwrap();

        // One valid plugin and one with bad encrypted config
        {
            let mut plugins = repo.plugins.lock().unwrap();
            plugins.push(ChannelPluginRow {
                id: "telegram".into(),
                r#type: "telegram".into(),
                name: "Telegram Bot".into(),
                enabled: true,
                config: encrypted,
                status: None,
                last_connected: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            });
            plugins.push(ChannelPluginRow {
                id: "lark".into(),
                r#type: "lark".into(),
                name: "Lark Bot".into(),
                enabled: true,
                config: "invalid-encrypted-data".into(),
                status: None,
                last_connected: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            });
        }

        let factory = make_factory();
        mgr.restore_plugins(&factory).await.unwrap();

        // Telegram should have started, Lark should have failed
        assert_eq!(mgr.active_plugin_count(), 1);
        assert!(mgr.is_plugin_running("telegram"));
        assert!(!mgr.is_plugin_running("lark"));

        // Lark should have error status in DB
        let plugins = repo.get_plugins();
        let lark = plugins.iter().find(|p| p.id == "lark").unwrap();
        assert_eq!(lark.status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn restore_empty_is_noop() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();
        mgr.restore_plugins(&factory).await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── shutdown ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn shutdown_stops_all_plugins() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        let lark_config = serde_json::json!({
            "credentials": {
                "appId": "cli_abc",
                "appSecret": "secret"
            }
        });
        mgr.enable_plugin("lark", &lark_config, &factory).await.unwrap();

        assert_eq!(mgr.active_plugin_count(), 2);

        mgr.shutdown().await;
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    // ── send_message / edit_message ────────────────────────────────────

    #[tokio::test]
    async fn send_message_through_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        let msg_id = mgr
            .send_message("telegram", "chat_1", make_test_outgoing())
            .await
            .unwrap();
        assert_eq!(msg_id, "mock_msg_id");
    }

    #[tokio::test]
    async fn send_message_plugin_not_found() {
        let (mgr, _repo, _bc) = make_manager();
        let err = mgr
            .send_message("telegram", "chat_1", make_test_outgoing())
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(_)));
    }

    #[tokio::test]
    async fn edit_message_through_plugin() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();

        mgr.edit_message("telegram", "chat_1", "msg_1", make_test_outgoing())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn edit_message_plugin_not_found() {
        let (mgr, _repo, _bc) = make_manager();
        let err = mgr
            .edit_message("telegram", "chat_1", "msg_1", make_test_outgoing())
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::PluginNotFound(_)));
    }

    // ── helper methods ─────────────────────────────────────────────────

    #[tokio::test]
    async fn active_plugin_count_tracks_correctly() {
        let (mgr, _repo, _bc) = make_manager();
        let factory = make_factory();

        assert_eq!(mgr.active_plugin_count(), 0);

        mgr.enable_plugin("telegram", &make_test_config(), &factory)
            .await
            .unwrap();
        assert_eq!(mgr.active_plugin_count(), 1);

        mgr.disable_plugin("telegram").await.unwrap();
        assert_eq!(mgr.active_plugin_count(), 0);
    }

    #[tokio::test]
    async fn is_plugin_running_false_for_missing() {
        let (mgr, _repo, _bc) = make_manager();
        assert!(!mgr.is_plugin_running("nonexistent"));
    }

    #[test]
    fn default_plugin_names() {
        let (mgr, _repo, _bc) = make_manager();
        assert_eq!(mgr.default_plugin_name(PluginType::Telegram), "Telegram Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Lark), "Lark Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Dingtalk), "DingTalk Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Weixin), "WeChat Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Slack), "Slack Bot");
        assert_eq!(mgr.default_plugin_name(PluginType::Discord), "Discord Bot");
    }
}
