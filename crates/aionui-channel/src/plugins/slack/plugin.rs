//! Slack channel plugin — Socket Mode (Hermes-style), DM + allowlisted @mentions.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{SLACK_MAX_RECONNECT_ATTEMPTS, SLACK_MAX_RECONNECT_DELAY, SLACK_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks};
use crate::types::{
    BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage, UnifiedMessageContent,
    UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::SlackApi;
use super::types::{
    AcceptDecision, ChatPostMessageRequest, ChatUpdateRequest, EventsApiPayload, SocketEnvelope, SlackEvent,
    classify_event, is_dm_event, parse_allowed_channels, strip_bot_mention,
};

/// Slack Bot plugin (Socket Mode).
///
/// Credentials:
/// - `token` — bot user OAuth token (`xoxb-…`)
/// - `app_token` — app-level token (`xapp-…`) with `connections:write`
///
/// Config:
/// - `allowed_channels` — comma-separated conversation IDs (`C…`/`G…`). Empty = DM only.
///   In listed channels the bot only responds when @mentioned.
pub struct SlackPlugin {
    status: PluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<SlackApi>>,
    callbacks: Option<PluginCallbacks>,
    allowed_channels: HashSet<String>,
    bot_user_id: String,
    /// Last thread root per channel for outbound replies (personal-bot MVP).
    last_thread_ts: Arc<DashMap<String, String>>,
    ws_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl Default for SlackPlugin {
    fn default() -> Self {
        Self {
            status: PluginStatus::Created,
            bot_info: None,
            last_error: None,
            api: None,
            callbacks: None,
            allowed_channels: HashSet::new(),
            bot_user_id: String::new(),
            last_thread_ts: Arc::new(DashMap::new()),
            ws_handle: None,
            shutdown_tx: None,
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

        let bot_token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing Slack bot token (xoxb-…)".into());
                ChannelError::InvalidConfig("Missing Slack bot token (xoxb-…)".into())
            })?;

        let app_token = config
            .credentials
            .app_token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status = PluginStatus::Error;
                self.last_error = Some("Missing Slack app token (xapp-…)".into());
                ChannelError::InvalidConfig("Missing Slack app token (xapp-…)".into())
            })?;

        self.allowed_channels =
            parse_allowed_channels(config.config.as_ref().and_then(|c| c.allowed_channels.as_deref()));

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status = PluginStatus::Error;
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(SlackApi::new(client, bot_token, app_token));

        let me = api.auth_test().await.map_err(|e| {
            self.status = PluginStatus::Error;
            self.last_error = Some(format!("auth.test failed: {e}"));
            e
        })?;

        let user_id = me.user_id.clone().unwrap_or_default();
        self.bot_user_id = user_id.clone();
        self.bot_info = Some(BotInfo {
            id: user_id.clone(),
            username: me.user.clone(),
            display_name: me.user.clone().unwrap_or_else(|| "Slack Bot".into()),
        });

        info!(
            bot_user_id = %user_id,
            team = ?me.team,
            allowed = self.allowed_channels.len(),
            "Slack bot initialized"
        );

        self.api = Some(api);
        self.callbacks = Some(callbacks);
        self.status = PluginStatus::Ready;
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status = PluginStatus::Starting;

        if self.ws_handle.is_some() {
            self.status = PluginStatus::Running;
            return Ok(());
        }

        let api = self
            .api
            .as_ref()
            .cloned()
            .ok_or_else(|| ChannelError::PlatformApi("Slack plugin not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Slack callbacks not initialized".into()))?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let allowed = self.allowed_channels.clone();
        let bot_user_id = self.bot_user_id.clone();
        let last_thread_ts = self.last_thread_ts.clone();

        self.ws_handle = Some(tokio::spawn(socket_mode_loop(
            api,
            callbacks.message_tx,
            shutdown_rx,
            allowed,
            bot_user_id,
            last_thread_ts,
        )));

        self.status = PluginStatus::Running;
        info!("Slack plugin started (Socket Mode)");
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
        self.callbacks = None;
        self.last_thread_ts.clear();
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
        let thread_ts = message
            .reply_to_message_id
            .clone()
            .or_else(|| self.last_thread_ts.get(chat_id).map(|v| v.clone()));

        let req = ChatPostMessageRequest {
            channel: chat_id,
            text: &text,
            thread_ts: thread_ts.as_deref(),
            mrkdwn: Some(true),
        };

        api.post_message(&req).await
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
        let req = ChatUpdateRequest {
            channel: chat_id,
            ts: message_id,
            text: &text,
        };
        api.update_message(&req).await
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

// ---------------------------------------------------------------------------
// Socket Mode loop
// ---------------------------------------------------------------------------

async fn socket_mode_loop(
    api: Arc<SlackApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    mut shutdown_rx: watch::Receiver<bool>,
    allowed: HashSet<String>,
    bot_user_id: String,
    last_thread_ts: Arc<DashMap<String, String>>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Slack Socket Mode loop received shutdown");
            break;
        }

        match connect_and_listen(
            &api,
            &message_tx,
            &mut shutdown_rx,
            &allowed,
            &bot_user_id,
            &last_thread_ts,
        )
        .await
        {
            Ok(()) => {
                consecutive_errors = 0;
                if *shutdown_rx.borrow() {
                    break;
                }
                // Clean disconnect — reconnect after a short pause
                warn!("Slack Socket Mode disconnected cleanly; reconnecting");
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Slack Socket Mode error");
                if consecutive_errors >= SLACK_MAX_RECONNECT_ATTEMPTS {
                    error!("Slack max reconnect attempts reached; stopping loop");
                    break;
                }
            }
        }

        let backoff = backoff_delay(consecutive_errors.max(1));
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    debug!("Slack Socket Mode loop exited");
}

async fn connect_and_listen(
    api: &SlackApi,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    shutdown_rx: &mut watch::Receiver<bool>,
    allowed: &HashSet<String>,
    bot_user_id: &str,
    last_thread_ts: &DashMap<String, String>,
) -> Result<(), ChannelError> {
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let ws_url = api.connections_open().await?;
    debug!(%ws_url, "Connecting to Slack Socket Mode");

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(&ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Slack WS connect failed: {e}")))?;

    info!("Slack Socket Mode connected");

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("Slack WS shutdown during listen");
                    break;
                }
            }
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        handle_socket_text(
                            &text,
                            &mut write,
                            message_tx,
                            allowed,
                            bot_user_id,
                            last_thread_ts,
                        ).await;
                    }
                    Some(Ok(WsMessage::Ping(payload))) => {
                        let _ = write.send(WsMessage::Pong(payload)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Slack WS close frame");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(format!("Slack WS read error: {e}")));
                    }
                    None => {
                        debug!("Slack WS stream ended");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_socket_text<S>(
    text: &str,
    write: &mut S,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    allowed: &HashSet<String>,
    bot_user_id: &str,
    last_thread_ts: &DashMap<String, String>,
) where
    S: SinkExt<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let envelope: SocketEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "Failed to parse Slack Socket Mode frame");
            return;
        }
    };

    // Always ack envelopes that carry an id (required by Socket Mode).
    if let Some(ref eid) = envelope.envelope_id {
        let ack = serde_json::json!({ "envelope_id": eid });
        if let Ok(payload) = serde_json::to_string(&ack) {
            let _ = write.send(WsMessage::Text(payload.into())).await;
        }
    }

    match envelope.envelope_type.as_str() {
        "hello" => {
            info!("Slack Socket Mode hello (connection ready for events)");
        }
        "disconnect" => {
            warn!(reason = ?envelope.reason, "Slack Socket Mode disconnect requested");
        }
        "events_api" => {
            let Some(payload_val) = envelope.payload else {
                warn!("Slack events_api envelope missing payload");
                return;
            };
            let payload: EventsApiPayload = match serde_json::from_value(payload_val) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "Failed to parse events_api payload");
                    return;
                }
            };
            if let Some(event) = payload.event {
                handle_slack_event(event, message_tx, allowed, bot_user_id, last_thread_ts).await;
            } else {
                warn!("Slack events_api payload missing event");
            }
        }
        other => {
            info!(envelope_type = other, "Slack Socket Mode envelope (ignored)");
        }
    }
}

async fn handle_slack_event(
    event: SlackEvent,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    allowed: &HashSet<String>,
    bot_user_id: &str,
    last_thread_ts: &DashMap<String, String>,
) {
    let decision = classify_event(&event, bot_user_id, allowed);
    info!(
        event_type = %event.event_type,
        channel = ?event.channel,
        channel_type = ?event.channel_type,
        subtype = ?event.subtype,
        user = ?event.user,
        bot_id = ?event.bot_id,
        text_len = event.text.as_ref().map(|t| t.len()).unwrap_or(0),
        ?decision,
        "Slack event received"
    );

    if !matches!(
        decision,
        AcceptDecision::AcceptDm | AcceptDecision::AcceptMention
    ) {
        return;
    }

    let channel = match event.channel.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            warn!("Slack accepted event missing channel");
            return;
        }
    };
    let user_id = match event.user.as_deref() {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            warn!(channel = %channel, "Slack accepted event missing user");
            return;
        }
    };
    let ts = event.ts.clone().unwrap_or_else(|| chrono_now().to_string());
    let raw_text = event.text.clone().unwrap_or_default();
    let text = if is_dm_event(&event) {
        raw_text
    } else {
        strip_bot_mention(&raw_text, bot_user_id)
    };

    // Thread root for outbound replies: existing thread or this message.
    let thread_root = event.thread_ts.clone().unwrap_or_else(|| ts.clone());
    if !is_dm_event(&event) {
        last_thread_ts.insert(channel.clone(), thread_root);
    }

    let unified = UnifiedIncomingMessage {
        owner_user_id: None,
        id: ts,
        platform: PluginType::Slack,
        chat_id: channel.clone(),
        user: UnifiedUser {
            id: user_id.clone(),
            username: None,
            display_name: event.user.clone().unwrap_or_default(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: if text.starts_with('/') {
                MessageContentType::Command
            } else {
                MessageContentType::Text
            },
            text,
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: event.thread_ts,
        action: None,
        raw: None,
    };

    if message_tx.send(unified).await.is_err() {
        error!("Slack message channel closed; orchestrator not receiving events");
        return;
    }
    info!(channel = %channel, user = %user_id, "Slack message forwarded to channel pipeline");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate_message(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{truncated}...")
}

fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(SLACK_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build a TLS connector with ALPN `http/1.1` only (WebSocket upgrade).
fn build_ws_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
    use rustls::ClientConfig;
    use std::sync::Arc as StdArc;
    use tokio_tungstenite::Connector;

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }

    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Connector::Rustls(StdArc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_plugin_initial_state() {
        let plugin = SlackPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Slack);
    }

    #[test]
    fn truncate_message_basic() {
        assert_eq!(truncate_message("hi", 10), "hi");
        let long = "a".repeat(20);
        let out = truncate_message(&long, 10);
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= 10);
    }

    #[test]
    fn backoff_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(10), SLACK_MAX_RECONNECT_DELAY);
    }
}
