use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::constants::DISCORD_EVENT_DEDUP_TTL;
use crate::error::ChannelError;
use crate::types::UnifiedIncomingMessage;

use super::api::DiscordApi;
use super::types::{
    GatewayFrame, Hello, MessageCreate, Ready, build_heartbeat, build_identify, build_resume, message_to_unified, op,
};

/// Maximum reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Maximum backoff delay between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Shared message-deduplication cache: message id → first-seen instant.
/// Guards against MESSAGE_CREATE replays after a RESUME.
pub(super) type DedupCache = Arc<Mutex<HashMap<String, Instant>>>;

/// What the connection asks the outer loop to do next.
enum ConnResult {
    /// Reconnect (resuming when a session is still cached).
    Reconnect,
    /// Shutdown requested — stop the loop.
    Shutdown,
}

/// Session state carried across (re)connections.
#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    last_seq: Option<u64>,
}

/// Maintain the Discord Gateway connection: connect, IDENTIFY (or RESUME),
/// heartbeat, dispatch MESSAGE_CREATE, and reconnect with backoff. Exits on
/// shutdown or after `MAX_RECONNECT_ATTEMPTS` consecutive failures.
pub(super) async fn gateway_loop(
    api: Arc<DiscordApi>,
    token: String,
    bot_user_id: String,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: DedupCache,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut session = Session::default();
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        // Resume against the cached gateway URL when a session exists;
        // otherwise fetch a fresh URL and do a full IDENTIFY.
        let resume = session.id.is_some() && session.resume_url.is_some();
        let base_url = if resume {
            session.resume_url.clone().expect("resume_url present")
        } else {
            match api.get_gateway_bot().await {
                Ok(url) => url,
                Err(e) => {
                    consecutive_errors += 1;
                    warn!(error = %e, consecutive_errors, "Discord GET /gateway/bot failed");
                    if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                        error!("Discord max reconnect attempts reached");
                        break;
                    }
                    if sleep_or_shutdown(backoff_delay(consecutive_errors), &mut shutdown_rx).await {
                        break;
                    }
                    continue;
                }
            }
        };

        match connect_and_run(
            &gateway_ws_url(&base_url),
            resume,
            &token,
            &bot_user_id,
            &mut session,
            &message_tx,
            &dedup_cache,
            &mut shutdown_rx,
        )
        .await
        {
            Ok(ConnResult::Shutdown) => break,
            Ok(ConnResult::Reconnect) => {
                consecutive_errors = 0;
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Discord gateway connection error");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("Discord max reconnect attempts reached");
                    break;
                }
                if sleep_or_shutdown(backoff_delay(consecutive_errors), &mut shutdown_rx).await {
                    break;
                }
            }
        }
    }

    debug!("Discord gateway loop exited");
}

#[allow(clippy::too_many_arguments)]
async fn connect_and_run(
    ws_url: &str,
    resume: bool,
    token: &str,
    bot_user_id: &str,
    session: &mut Session,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: &DedupCache,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<ConnResult, ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord WS connect failed: {e}")))?;

    let (mut write, mut read) = ws_stream.split();

    // First frame must be Hello (op 10) carrying the heartbeat interval.
    let heartbeat_interval = match read.next().await {
        Some(Ok(WsMessage::Text(txt))) => {
            let frame: GatewayFrame = serde_json::from_str(txt.as_str())
                .map_err(|e| ChannelError::ConnectionFailed(format!("Discord Hello parse failed: {e}")))?;
            if frame.op != op::HELLO {
                return Err(ChannelError::ConnectionFailed(format!(
                    "Discord expected Hello, got op {}",
                    frame.op
                )));
            }
            let hello: Hello = frame
                .d
                .and_then(|d| serde_json::from_value(d).ok())
                .ok_or_else(|| ChannelError::ConnectionFailed("Discord Hello missing heartbeat_interval".into()))?;
            hello.heartbeat_interval
        }
        _ => return Err(ChannelError::ConnectionFailed("Discord did not send Hello".into())),
    };

    // Resume an existing session, or identify anew.
    let opening = if resume {
        match (&session.id, session.last_seq) {
            (Some(sid), Some(seq)) => build_resume(token, sid, seq),
            _ => build_identify(token),
        }
    } else {
        build_identify(token)
    };
    write
        .send(WsMessage::Text(opening.into()))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord IDENTIFY/RESUME send failed: {e}")))?;

    info!(resume, "Discord gateway connected");

    let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval));
    interval.tick().await; // consume the immediate first tick
    let mut heartbeat_acked = true;

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(txt))) => {
                        let frame: GatewayFrame = match serde_json::from_str(txt.as_str()) {
                            Ok(f) => f,
                            Err(e) => { warn!(error = %e, "Discord frame parse failed"); continue; }
                        };
                        if let Some(s) = frame.s {
                            session.last_seq = Some(s);
                        }
                        match frame.op {
                            op::HEARTBEAT => {
                                let _ = write.send(WsMessage::Text(build_heartbeat(session.last_seq).into())).await;
                            }
                            op::HEARTBEAT_ACK => heartbeat_acked = true,
                            op::RECONNECT => {
                                debug!("Discord op 7 Reconnect; will resume");
                                return Ok(ConnResult::Reconnect);
                            }
                            op::INVALID_SESSION => {
                                let resumable = frame.d.as_ref().and_then(|d| d.as_bool()).unwrap_or(false);
                                if !resumable {
                                    debug!("Discord Invalid Session (not resumable); clearing session");
                                    session.id = None;
                                    session.resume_url = None;
                                    session.last_seq = None;
                                }
                                return Ok(ConnResult::Reconnect);
                            }
                            op::DISPATCH => {
                                handle_dispatch(&frame, bot_user_id, session, message_tx, dedup_cache).await;
                            }
                            other => debug!(op = other, "Discord unhandled opcode"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => { let _ = write.send(WsMessage::Pong(data)).await; }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Discord WS received close frame");
                        return Ok(ConnResult::Reconnect);
                    }
                    Some(Err(e)) => return Err(ChannelError::ConnectionFailed(format!("Discord WS read error: {e}"))),
                    None => return Err(ChannelError::ConnectionFailed("Discord WS stream ended".into())),
                    _ => {}
                }
            }
            _ = interval.tick() => {
                if !heartbeat_acked {
                    // No ACK since the last heartbeat — connection is a zombie.
                    warn!("Discord heartbeat not acked; reconnecting");
                    return Err(ChannelError::ConnectionFailed("Discord heartbeat ACK missing".into()));
                }
                heartbeat_acked = false;
                let _ = write.send(WsMessage::Text(build_heartbeat(session.last_seq).into())).await;
            }
            _ = shutdown_rx.changed() => {
                debug!("Discord gateway shutdown during listen");
                return Ok(ConnResult::Shutdown);
            }
        }
    }
}

/// Handle an op-0 dispatch: track session on READY, normalize MESSAGE_CREATE.
async fn handle_dispatch(
    frame: &GatewayFrame,
    bot_user_id: &str,
    session: &mut Session,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    dedup_cache: &DedupCache,
) {
    match frame.t.as_deref() {
        Some("READY") => {
            if let Some(ready) = frame.d.clone().and_then(|d| serde_json::from_value::<Ready>(d).ok()) {
                session.id = Some(ready.session_id);
                // Store the RAW resume URL; the connect site wraps it with the
                // `?v=10&encoding=json` query exactly once (same as the fresh
                // identify path). Wrapping here too would double the query and
                // make the resume connect URL invalid.
                session.resume_url = Some(ready.resume_gateway_url);
                info!(bot_user_id = %ready.user.id, "Discord gateway READY");
            }
        }
        Some("RESUMED") => debug!("Discord gateway RESUMED"),
        Some("MESSAGE_CREATE") => {
            let Some(d) = frame.d.clone() else { return };
            let msg: MessageCreate = match serde_json::from_value(d) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "Discord MESSAGE_CREATE parse failed");
                    return;
                }
            };
            if is_duplicate(dedup_cache, &msg.id).await {
                debug!(message_id = %msg.id, "Discord duplicate message, skipping");
                return;
            }
            if let Some(unified) = message_to_unified(&msg, bot_user_id) {
                let _ = message_tx.send(unified).await;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

pub(super) async fn is_duplicate(cache: &DedupCache, id: &str) -> bool {
    let mut map = cache.lock().await;
    if map.contains_key(id) {
        return true;
    }
    map.insert(id.to_string(), Instant::now());
    false
}

pub(super) async fn cleanup_expired_events(cache: &DedupCache) {
    let mut map = cache.lock().await;
    let before = map.len();
    map.retain(|_, instant| instant.elapsed() < DISCORD_EVENT_DEDUP_TTL);
    let removed = before - map.len();
    if removed > 0 {
        debug!(removed, remaining = map.len(), "Discord dedup cache cleanup");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Append the required `?v=10&encoding=json` query to a gateway base URL.
fn gateway_ws_url(base: &str) -> String {
    format!("{}/?v=10&encoding=json", base.trim_end_matches('/'))
}

/// Exponential backoff delay, capped at `MAX_RECONNECT_DELAY`.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

/// Sleep for `delay`, returning true if shutdown fired first.
async fn sleep_or_shutdown(delay: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown_rx.changed() => true,
    }
}

/// Build a TLS connector pinned to `http/1.1` ALPN (WebSocket upgrade needs it).
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
#[path = "gateway_test.rs"]
mod gateway_test;
