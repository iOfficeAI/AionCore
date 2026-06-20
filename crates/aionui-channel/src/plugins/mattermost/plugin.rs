use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{
    BotInfo, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::MattermostApi;
use super::types::{
    CreatePostRequest, MattermostConfig, MattermostPost, MattermostUser, PatchPostRequest, parse_posted_event,
    post_to_content,
};

const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MattermostPlugin {
    status: PluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<MattermostApi>>,
    config: Option<MattermostConfig>,
    ws_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl Default for MattermostPlugin {
    fn default() -> Self {
        Self {
            status: PluginStatus::Created,
            bot_info: None,
            last_error: None,
            api: None,
            config: None,
            ws_handle: None,
            shutdown_tx: None,
        }
    }
}

impl MattermostPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for MattermostPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status = PluginStatus::Initializing;

        let parsed_config = MattermostConfig::from_plugin_config(&config).inspect_err(|e| {
            self.status = PluginStatus::Error;
            self.last_error = Some(e.to_string());
        })?;

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status = PluginStatus::Error;
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;
        let api = Arc::new(MattermostApi::new(
            client,
            parsed_config.server_url.clone(),
            parsed_config.access_token.clone(),
        ));

        let me = api.get_me().await.map_err(|e| {
            self.status = PluginStatus::Error;
            self.last_error = Some(format!("Credential validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: me.id.clone(),
            username: me.username.clone(),
            display_name: me.display_name(),
        });

        info!(
            user_id = %me.id,
            username = me.username.as_deref().unwrap_or(""),
            "Mattermost identity loaded"
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);
        self.ws_handle = Some(tokio::spawn(ws_loop(
            Arc::clone(&api),
            parsed_config.clone(),
            me,
            callbacks.message_tx,
            shutdown_rx,
        )));

        self.api = Some(api);
        self.config = Some(parsed_config);
        self.status = PluginStatus::Ready;
        info!("Mattermost plugin initialized");
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Starting;
        self.status = PluginStatus::Running;
        info!("Mattermost plugin started");
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

        self.api = None;
        self.config = None;
        self.status = PluginStatus::Stopped;
        info!("Mattermost plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = message.text.unwrap_or_default();
        let root_id = config
            .reply_in_thread
            .then(|| message.reply_to_message_id.filter(|id| !id.is_empty()))
            .flatten();
        let req = CreatePostRequest {
            channel_id: chat_id.to_owned(),
            message: text,
            root_id,
        };

        api.create_post(&req).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let req = PatchPostRequest {
            message: message.text.unwrap_or_default(),
        };
        api.patch_post(message_id, &req).await
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Mattermost
    }

    fn status(&self) -> PluginStatus {
        self.status
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

async fn ws_loop(
    api: Arc<MattermostApi>,
    config: MattermostConfig,
    me: MattermostUser,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut attempts = 0u32;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        match connect_once(&api, &config, &me, &message_tx, &mut shutdown_rx).await {
            Ok(()) => {
                attempts = 0;
            }
            Err(e) => {
                attempts = attempts.saturating_add(1);
                let delay = reconnect_delay(attempts);
                warn!(error = %e, delay_ms = delay.as_millis(), "Mattermost WebSocket reconnect scheduled");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        }
    }

    debug!("Mattermost WebSocket loop stopped");
}

async fn connect_once(
    api: &MattermostApi,
    config: &MattermostConfig,
    me: &MattermostUser,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use tokio_tungstenite::connect_async_tls_with_config;

    let ws_url = api.websocket_url();
    info!(url = %ws_url, "Mattermost WebSocket connecting");

    let connector = build_ws_tls_connector()?;
    let (mut ws, _) = tokio::time::timeout(
        WS_CONNECT_TIMEOUT,
        connect_async_tls_with_config(&ws_url, None, false, Some(connector)),
    )
    .await
    .map_err(|_| {
        ChannelError::ConnectionFailed(format!(
            "Mattermost WebSocket connect timed out after {}s",
            WS_CONNECT_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost WebSocket connect failed: {e}")))?;

    let auth = serde_json::json!({
        "seq": 1,
        "action": "authentication_challenge",
        "data": {
            "token": api.access_token(),
        },
    });
    tokio::time::timeout(WS_CONNECT_TIMEOUT, ws.send(Message::Text(auth.to_string().into())))
        .await
        .map_err(|_| {
            ChannelError::ConnectionFailed(format!(
                "Mattermost WebSocket auth send timed out after {}s",
                WS_CONNECT_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost WebSocket auth send failed: {e}")))?;
    info!("Mattermost WebSocket connected");

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
            next = ws.next() => {
                let Some(message) = next else {
                    return Err(ChannelError::ConnectionFailed("Mattermost WebSocket closed".into()));
                };
                let message = message.map_err(|e| ChannelError::ConnectionFailed(format!("Mattermost WebSocket read failed: {e}")))?;
                match message {
                    Message::Text(text) => handle_ws_text(&text, config, me, message_tx).await,
                    Message::Ping(payload) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Message::Close(_) => {
                        return Err(ChannelError::ConnectionFailed("Mattermost WebSocket closed".into()));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Build a TLS connector for WebSocket connections.
///
/// WebSocket upgrade requires HTTP/1.1. Some Mattermost deployments sit behind
/// reverse proxies that negotiate h2 by ALPN unless the client pins HTTP/1.1.
fn build_ws_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
    use std::sync::Arc;
    use tokio_tungstenite::Connector;

    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelError::ConnectionFailed(format!("TLS config error: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Connector::Rustls(Arc::new(config)))
}

async fn handle_ws_text(
    text: &str,
    config: &MattermostConfig,
    me: &MattermostUser,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
) {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            warn!(error = %e, "Mattermost WebSocket message parse failed");
            return;
        }
    };

    let Some(post) = parse_posted_event(&value) else {
        return;
    };
    info!(
        post_id = %post.id,
        channel_id = %post.channel_id,
        user_id = %post.user_id,
        "Mattermost post event received"
    );
    if let Some(msg) = post_to_unified(&post, config, me)
        && let Err(e) = message_tx.send(msg).await
    {
        warn!(error = %e, "Mattermost incoming message dispatch failed");
    }
}

pub(crate) fn post_to_unified(
    post: &MattermostPost,
    config: &MattermostConfig,
    me: &MattermostUser,
) -> Option<UnifiedIncomingMessage> {
    if post.id.is_empty() || post.channel_id.is_empty() || post.user_id.is_empty() || post.message.trim().is_empty() {
        return None;
    }
    if !config.channel_allowed(&post.channel_id) {
        return None;
    }
    if config.ignore_self_messages && post.user_id == me.id {
        return None;
    }
    if post.r#type == "system_join_channel" || post.r#type == "system_leave_channel" {
        return None;
    }

    Some(UnifiedIncomingMessage {
        owner_user_id: None,
        id: post.id.clone(),
        platform: PluginType::Mattermost,
        chat_id: post.channel_id.clone(),
        user: UnifiedUser {
            id: post.user_id.clone(),
            username: None,
            display_name: post.user_id.clone(),
            avatar_url: None,
        },
        content: post_to_content(post),
        timestamp: post.create_at / 1000,
        reply_to_message_id: post.root_id.clone().filter(|id| !id.is_empty()),
        action: None,
        raw: Some(serde_json::json!({
            "post_id": post.id,
            "channel_id": post.channel_id,
            "root_id": post.root_id,
            "props": post.props,
        })),
    })
}

fn reconnect_delay(attempts: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempts.min(4)).min(MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    fn config() -> MattermostConfig {
        MattermostConfig {
            server_url: "https://mm.example".into(),
            access_token: "token".into(),
            allowed_channel_ids: HashSet::new(),
            reply_in_thread: true,
            ignore_self_messages: true,
        }
    }

    fn me() -> MattermostUser {
        MattermostUser {
            id: "me".into(),
            username: Some("bot".into()),
            nickname: None,
        }
    }

    fn post(user_id: &str, channel_id: &str) -> MattermostPost {
        MattermostPost {
            id: "p1".into(),
            channel_id: channel_id.into(),
            user_id: user_id.into(),
            message: "hello".into(),
            create_at: 1_000,
            root_id: Some("root1".into()),
            r#type: String::new(),
            props: Some(Value::Object(serde_json::Map::new())),
        }
    }

    #[test]
    fn post_maps_to_unified_message() {
        let msg = post_to_unified(&post("u1", "c1"), &config(), &me()).unwrap();
        assert_eq!(msg.platform, PluginType::Mattermost);
        assert_eq!(msg.chat_id, "c1");
        assert_eq!(msg.reply_to_message_id.as_deref(), Some("root1"));
        assert_eq!(msg.content.text, "hello");
    }

    #[test]
    fn post_ignores_self_message() {
        assert!(post_to_unified(&post("me", "c1"), &config(), &me()).is_none());
    }

    #[test]
    fn post_ignores_disallowed_channel() {
        let mut cfg = config();
        cfg.allowed_channel_ids = HashSet::from(["allowed".into()]);
        assert!(post_to_unified(&post("u1", "blocked"), &cfg, &me()).is_none());
    }

    #[test]
    fn plugin_initial_state() {
        let plugin = MattermostPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert_eq!(plugin.plugin_type(), PluginType::Mattermost);
    }

    #[test]
    fn create_post_payload_omits_empty_root() {
        let req = CreatePostRequest {
            channel_id: "c1".into(),
            message: "hello".into(),
            root_id: None,
        };
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["channel_id"], "c1");
        assert!(value.get("root_id").is_none());
    }

    #[test]
    fn post_raw_excludes_credentials() {
        let msg = post_to_unified(&post("u1", "c1"), &config(), &me()).unwrap();
        let raw = msg.raw.unwrap();
        assert!(raw.get("access_token").is_none());
        assert!(raw.get("token").is_none());
    }

    #[test]
    fn user_display_name_prefers_nickname_then_username() {
        let user = MattermostUser {
            id: "u1".into(),
            username: Some("username".into()),
            nickname: Some("nickname".into()),
        };
        assert_eq!(user.display_name(), "nickname");
    }

    #[test]
    fn mattermost_plugin_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MattermostPlugin>();
    }

    #[test]
    fn reconnect_delay_is_bounded() {
        assert!(reconnect_delay(100) <= MAX_RECONNECT_DELAY);
    }

    #[test]
    fn empty_post_is_ignored() {
        let mut p = post("u1", "c1");
        p.message = " ".into();
        assert!(post_to_unified(&p, &config(), &me()).is_none());
    }

    #[test]
    fn system_post_is_ignored() {
        let mut p = post("u1", "c1");
        p.r#type = "system_join_channel".into();
        assert!(post_to_unified(&p, &config(), &me()).is_none());
    }

    #[test]
    fn no_allowed_channels_allows_all() {
        let cfg = config();
        assert!(cfg.channel_allowed("any"));
    }

    #[test]
    fn allowed_channels_are_checked() {
        let mut cfg = config();
        cfg.allowed_channel_ids = HashSet::from(["c1".into()]);
        assert!(cfg.channel_allowed("c1"));
        assert!(!cfg.channel_allowed("c2"));
    }

    #[test]
    fn display_name_falls_back_to_id() {
        let user = MattermostUser {
            id: "u1".into(),
            username: None,
            nickname: None,
        };
        assert_eq!(user.display_name(), "u1");
    }

    #[test]
    fn create_post_payload_includes_root() {
        let req = CreatePostRequest {
            channel_id: "c1".into(),
            message: "hello".into(),
            root_id: Some("p1".into()),
        };
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["root_id"], "p1");
    }

    #[test]
    fn patch_post_payload_updates_message_only() {
        let req = PatchPostRequest {
            message: "updated".into(),
        };
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["message"], "updated");
        assert!(value.get("channel_id").is_none());
        assert!(value.get("root_id").is_none());
    }

    #[test]
    fn mattermost_plugin_as_trait_object() {
        let plugin = MattermostPlugin::new();
        let boxed: Box<dyn ChannelPlugin> = Box::new(plugin);
        assert_eq!(boxed.plugin_type(), PluginType::Mattermost);
    }
}
