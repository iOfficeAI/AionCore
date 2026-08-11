use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::constants::SLACK_EVENT_DEDUP_TTL;
use crate::error::ChannelError;
use crate::types::UnifiedIncomingMessage;

use super::api::SlackApi;
use super::types::{EventCallback, SocketEnvelope, build_ack, event_to_unified};

/// Maximum reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Maximum backoff delay between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Shared event-deduplication cache: `event_id` → first-seen instant.
pub(super) type DedupCache = Arc<Mutex<HashMap<String, Instant>>>;

/// Maintain a Socket Mode connection: open a fresh WSS URL, listen until the
/// socket closes or Slack requests a reconnect, then retry with exponential
/// backoff. Exits on shutdown or after `MAX_RECONNECT_ATTEMPTS` failures.
pub(super) async fn socket_loop(
    api: Arc<SlackApi>,
    bot_user_id: String,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: DedupCache,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Slack socket loop received shutdown signal");
            break;
        }

        let ws_url = match api.open_connection().await {
            Ok(url) => url,
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Slack apps.connections.open failed");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("Slack max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => continue,
                    _ = shutdown_rx.changed() => break,
                }
            }
        };

        match connect_and_listen(&ws_url, &bot_user_id, &message_tx, &dedup_cache, &mut shutdown_rx).await {
            Ok(()) => {
                consecutive_errors = 0;
                debug!("Slack WS connection closed cleanly; reconnecting");
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Slack WS connection error");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("Slack max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.changed() => break,
                }
            }
        }
    }

    debug!("Slack socket loop exited");
}

/// Connect to the WSS URL and process envelopes until the socket closes,
/// Slack sends a `disconnect`, or shutdown is signalled.
async fn connect_and_listen(
    ws_url: &str,
    bot_user_id: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: &DedupCache,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Slack WS connect failed: {e}")))?;

    info!("Slack WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(txt))) => {
                        let envelope: SocketEnvelope = match serde_json::from_str(txt.as_str()) {
                            Ok(env) => env,
                            Err(e) => {
                                warn!(error = %e, "Failed to parse Slack envelope");
                                continue;
                            }
                        };

                        // Ack every envelope that carries an id (Slack expects a
                        // reply within 3s or it re-delivers the event).
                        if let Some(ref envelope_id) = envelope.envelope_id {
                            let ack = build_ack(envelope_id);
                            if let Err(e) = write.send(WsMessage::Text(ack.into())).await {
                                warn!(error = %e, "Failed to send Slack ack");
                            }
                        }

                        match envelope.envelope_type.as_str() {
                            "hello" => debug!("Slack Socket Mode hello"),
                            "disconnect" => {
                                debug!(reason = ?envelope.reason, "Slack requested disconnect; reconnecting");
                                return Ok(());
                            }
                            "events_api" => {
                                if let Some(payload) = envelope.payload {
                                    handle_event_callback(payload, bot_user_id, message_tx, dedup_cache).await;
                                }
                            }
                            other => debug!(envelope_type = other, "Slack unhandled envelope type"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Slack WS received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(format!("Slack WS read error: {e}")));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed("Slack WS stream ended unexpectedly".into()));
                    }
                    _ => {}
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("Slack WS shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Parse an `events_api` payload, dedup by `event_id`, normalize, and forward.
async fn handle_event_callback(
    payload: serde_json::Value,
    bot_user_id: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: &DedupCache,
) {
    let callback: EventCallback = match serde_json::from_value(payload) {
        Ok(cb) => cb,
        Err(e) => {
            warn!(error = %e, "Failed to parse Slack event callback");
            return;
        }
    };

    if let Some(event_id) = callback.event_id.as_deref()
        && is_duplicate(dedup_cache, event_id).await
    {
        debug!(event_id, "Slack duplicate event, skipping");
        return;
    }

    let Some(event) = callback.event else { return };
    if let Some(unified) = event_to_unified(&event, bot_user_id) {
        let _ = message_tx.send(unified).await;
    }
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

/// Return true if `event_id` was seen recently; otherwise record it.
pub(super) async fn is_duplicate(cache: &DedupCache, event_id: &str) -> bool {
    let mut map = cache.lock().await;
    if map.contains_key(event_id) {
        return true;
    }
    map.insert(event_id.to_string(), Instant::now());
    false
}

/// Drop expired entries from the dedup cache.
pub(super) async fn cleanup_expired_events(cache: &DedupCache) {
    let mut map = cache.lock().await;
    let before = map.len();
    map.retain(|_, instant| instant.elapsed() < SLACK_EVENT_DEDUP_TTL);
    let removed = before - map.len();
    if removed > 0 {
        debug!(removed, remaining = map.len(), "Slack dedup cache cleanup");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Exponential backoff delay, capped at `MAX_RECONNECT_DELAY`.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

/// Build a TLS connector pinned to `http/1.1` ALPN.
///
/// WebSocket requires an HTTP/1.1 upgrade handshake; without pinning ALPN some
/// servers negotiate h2 and the upgrade never completes.
fn build_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
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

#[cfg(test)]
#[path = "socket_test.rs"]
mod socket_test;
