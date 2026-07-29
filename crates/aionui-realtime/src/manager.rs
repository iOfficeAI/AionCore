use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use aionui_api_types::WebSocketMessage;
use dashmap::DashMap;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::broadcaster::EventBroadcaster;
use crate::types::{
    ClientInfo, ConnectionId, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT, RealtimeError, WebSocketCloseCode, WsOutbound,
};

/// Validates whether a JWT token is still valid.
/// Returns `true` if the token is valid, `false` if expired or revoked.
pub type TokenValidator = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Manages active WebSocket connections, heartbeat detection,
/// and provides broadcast/unicast messaging.
pub struct WebSocketManager {
    connections: Arc<DashMap<ConnectionId, ClientInfo>>,
    next_id: AtomicU64,
}

impl WebSocketManager {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a new client connection and return its assigned ID.
    pub fn add_client(&self, token: String, tx: mpsc::Sender<WsOutbound>) -> ConnectionId {
        self.add_client_for_user("system_default_user".to_owned(), token, tx)
    }

    /// Register a new authenticated client connection for an internal user ID.
    pub fn add_client_for_user(&self, user_id: String, token: String, tx: mpsc::Sender<WsOutbound>) -> ConnectionId {
        let id = ConnectionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let info = ClientInfo {
            user_id,
            token,
            last_ping: Instant::now(),
            tx,
        };
        self.connections.insert(id, info);
        debug!(%id, "client added");
        id
    }

    /// Remove a client connection by ID.
    pub fn remove_client(&self, conn_id: ConnectionId) {
        if self.connections.remove(&conn_id).is_some() {
            debug!(%conn_id, "client removed");
        }
    }

    /// Update the last heartbeat timestamp for a connection.
    pub fn update_last_ping(&self, conn_id: ConnectionId) {
        if let Some(mut client) = self.connections.get_mut(&conn_id) {
            client.last_ping = Instant::now();
        }
    }

    /// Returns the number of active connections.
    pub fn client_count(&self) -> usize {
        self.connections.len()
    }

    /// Returns the number of active connections authenticated as `user_id`.
    pub fn client_count_for_user(&self, user_id: &str) -> usize {
        self.connections
            .iter()
            .filter(|entry| entry.value().user_id == user_id)
            .count()
    }

    /// Close and remove every active connection authenticated as `user_id`.
    ///
    /// Returns the number of connections removed. The close is best-effort:
    /// saturated or already-closed outbound queues are still removed from the
    /// manager so no future events are delivered to revoked sessions.
    pub fn disconnect_user(&self, user_id: &str, reason: &str) -> usize {
        let mut to_remove = Vec::new();

        for entry in self.connections.iter() {
            let conn_id = *entry.key();
            let client = entry.value();
            if client.user_id != user_id {
                continue;
            }

            let outbound = terminal_realtime_error(conn_id, RealtimeError::AuthExpired, reason);
            match client.tx.try_send(outbound) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {
                    to_remove.push(conn_id);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        %conn_id,
                        user_id = %user_id,
                        code = RealtimeError::Backpressure.code(),
                        "outbound channel full, user disconnect close dropped"
                    );
                    to_remove.push(conn_id);
                }
            }
        }

        for conn_id in &to_remove {
            self.remove_client(*conn_id);
        }

        let removed = to_remove.len();
        if removed > 0 {
            info!(user_id = %user_id, removed, "websocket user connections disconnected");
        }
        removed
    }

    /// Send a message to all connected clients.
    ///
    /// Uses `try_send` for backpressure. A saturated channel cannot reliably
    /// receive an additional `REALTIME_BACKPRESSURE` event on the same path, so
    /// broadcast backpressure is logged and the connection is left alive.
    /// Closed channels trigger client removal.
    pub fn broadcast_all(&self, msg: WebSocketMessage<serde_json::Value>) {
        let text = match serde_json::to_string(&msg) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "failed to serialize broadcast message");
                return;
            }
        };

        let mut disconnected = Vec::new();
        for entry in self.connections.iter() {
            let conn_id = *entry.key();
            match entry.value().tx.try_send(WsOutbound::Text(text.clone())) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        %conn_id,
                        code = RealtimeError::Backpressure.code(),
                        "outbound channel full, broadcast message dropped"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    disconnected.push(conn_id);
                }
            }
        }

        for conn_id in disconnected {
            self.remove_client(conn_id);
        }
    }

    /// Send a message to all connections authenticated as `user_id`.
    pub fn broadcast_to_user(&self, user_id: &str, msg: WebSocketMessage<serde_json::Value>) {
        let text = match serde_json::to_string(&msg) {
            Ok(t) => t,
            Err(e) => {
                warn!(user_id = %user_id, error = %e, "failed to serialize user broadcast message");
                return;
            }
        };

        let mut disconnected = Vec::new();
        for entry in self.connections.iter() {
            let conn_id = *entry.key();
            let client = entry.value();
            if client.user_id != user_id {
                continue;
            }

            match client.tx.try_send(WsOutbound::Text(text.clone())) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        %conn_id,
                        user_id = %user_id,
                        code = RealtimeError::Backpressure.code(),
                        "outbound channel full, user broadcast message dropped"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    disconnected.push(conn_id);
                }
            }
        }

        for conn_id in disconnected {
            self.remove_client(conn_id);
        }
    }

    /// Send a message to a specific connection.
    pub fn send_to(&self, conn_id: ConnectionId, msg: WebSocketMessage<serde_json::Value>) {
        let text = match serde_json::to_string(&msg) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    %conn_id, error = %e,
                    "failed to serialize unicast message"
                );
                return;
            }
        };

        self.send_raw_to(conn_id, WsOutbound::Text(text));
    }

    /// Strict scoped delivery for **ordered** streams: on backpressure the
    /// connection is dropped rather than the frame.
    ///
    /// For a feed whose consumer applies frames in arrival order with no
    /// gap/version detection (the fs monitor protocol), silently dropping one
    /// frame on a full channel desyncs the client with no way to notice or
    /// recover. So a `Full` (or `Closed`) channel removes the client instead —
    /// the connection tears down and the client reconnects and re-declares its
    /// subscriptions, restoring consistency. Unlike [`Self::send_to`], which
    /// keeps the connection alive and logs the drop (fine for the fire-and-forget
    /// broadcast path, not for an ordered stream).
    pub fn send_to_or_disconnect(&self, conn_id: ConnectionId, msg: WebSocketMessage<serde_json::Value>) {
        let text = match serde_json::to_string(&msg) {
            Ok(t) => t,
            Err(e) => {
                warn!(%conn_id, error = %e, "failed to serialize ordered unicast message");
                return;
            }
        };

        // Determine the outcome without holding the map guard across removal.
        let drop_connection = match self.connections.get(&conn_id) {
            Some(client) => matches!(
                client.tx.try_send(WsOutbound::Text(text)),
                Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_))
            ),
            None => false,
        };
        if drop_connection {
            warn!(
                %conn_id,
                code = RealtimeError::Backpressure.code(),
                "ordered stream backpressure, closing connection to force resync"
            );
            self.remove_client(conn_id);
        }
    }

    /// Send a raw outbound message to a specific connection.
    ///
    /// Used for non-`WebSocketMessage` payloads (e.g. error responses). A full
    /// channel cannot receive a send-failure event through the same queue, so
    /// backpressure is logged as the downgrade path.
    pub fn send_raw_to(&self, conn_id: ConnectionId, outbound: WsOutbound) {
        if let Some(client) = self.connections.get(&conn_id) {
            match client.tx.try_send(outbound) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        %conn_id,
                        code = RealtimeError::SendFailed.code(),
                        "outbound channel full, raw message dropped"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    drop(client);
                    self.remove_client(conn_id);
                }
            }
        }
    }

    /// Start the heartbeat check loop.
    ///
    /// Every `HEARTBEAT_INTERVAL` (30s), iterates all connections:
    /// 1. Timeout check — closes connections with no pong for `HEARTBEAT_TIMEOUT`
    /// 2. Token expiry — validates token and sends `realtime.error` if invalid
    /// 3. Sends a `ping` message with current timestamp
    ///
    /// Returns a `JoinHandle` — abort it to stop the heartbeat loop.
    pub fn start_heartbeat(&self, token_validator: TokenValidator) -> JoinHandle<()> {
        let connections = Arc::clone(&self.connections);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            loop {
                interval.tick().await;
                heartbeat_tick(&connections, &token_validator);
            }
        })
    }
}

impl Default for WebSocketManager {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBroadcaster for WebSocketManager {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        let Some(user_id) = event
            .data
            .get("user_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
        else {
            warn!(
                event_name = %event.name,
                "dropping websocket manager event without user_id"
            );
            return;
        };
        self.broadcast_to_user(&user_id, event);
    }
}

/// Single heartbeat tick: check timeouts, token validity, send pings.
fn heartbeat_tick(connections: &DashMap<ConnectionId, ClientInfo>, token_validator: &TokenValidator) {
    let now = Instant::now();
    let mut to_remove = Vec::new();

    for entry in connections.iter() {
        let conn_id = *entry.key();
        let client = entry.value();

        // 1. Heartbeat timeout
        if now.duration_since(client.last_ping) > HEARTBEAT_TIMEOUT {
            info!(%conn_id, "heartbeat timeout, closing connection");
            let _ = client.tx.try_send(terminal_realtime_error(
                conn_id,
                RealtimeError::HeartbeatTimeout,
                "heartbeat timeout",
            ));
            to_remove.push(conn_id);
            continue;
        }

        // 2. Token expiry
        if !token_validator(&client.token) {
            info!(%conn_id, "token expired, closing connection");
            let outbound = terminal_realtime_error(conn_id, RealtimeError::AuthExpired, "token expired");

            match client.tx.try_send(outbound) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {
                    to_remove.push(conn_id);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        %conn_id,
                        code = RealtimeError::Backpressure.code(),
                        "outbound channel full, terminal auth close dropped"
                    );
                    to_remove.push(conn_id);
                }
            }
            continue;
        }

        // 3. Send ping
        let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let timestamp = duration.as_secs() * 1000 + u64::from(duration.subsec_millis());

        let ping = WebSocketMessage::new("ping", json!({"timestamp": timestamp}));
        if let Ok(text) = serde_json::to_string(&ping) {
            match client.tx.try_send(WsOutbound::Text(text)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(%conn_id, "outbound channel full, ping dropped");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    to_remove.push(conn_id);
                }
            }
        }
    }

    for conn_id in to_remove {
        connections.remove(&conn_id);
        debug!(%conn_id, "connection removed by heartbeat");
    }
}

fn terminal_realtime_error(conn_id: ConnectionId, error: RealtimeError, reason: &str) -> WsOutbound {
    match serde_json::to_string(&error.into_event()) {
        Ok(text) => WsOutbound::TextThenClose(text, WebSocketCloseCode::PolicyViolation, reason.into()),
        Err(e) => {
            warn!(%conn_id, error = %e, code = error.code(), "failed to serialize terminal realtime error");
            WsOutbound::Close(WebSocketCloseCode::PolicyViolation, reason.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PER_CONNECTION_BUFFER;

    fn always_valid() -> TokenValidator {
        Arc::new(|_| true)
    }

    fn always_expired() -> TokenValidator {
        Arc::new(|_| false)
    }

    fn new_client_tx() -> (mpsc::Sender<WsOutbound>, mpsc::Receiver<WsOutbound>) {
        mpsc::channel(PER_CONNECTION_BUFFER)
    }

    fn assert_realtime_auth_expired(text: &str) {
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["name"], "realtime.error");
        assert_eq!(parsed["data"]["code"], "REALTIME_AUTH_EXPIRED");
        assert!(parsed["data"]["message"].is_string());
        assert_eq!(parsed["data"]["recoverable"], false);
        assert!(parsed["data"]["details"].is_object());
    }

    fn assert_realtime_heartbeat_timeout(text: &str) {
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["name"], "realtime.error");
        assert_eq!(parsed["data"]["code"], "REALTIME_HEARTBEAT_TIMEOUT");
        assert!(parsed["data"]["message"].is_string());
        assert_eq!(parsed["data"]["recoverable"], false);
        assert!(parsed["data"]["details"].is_object());
    }

    #[test]
    fn add_client_assigns_sequential_ids() {
        let mgr = WebSocketManager::new();
        let (tx1, _rx1) = new_client_tx();
        let (tx2, _rx2) = new_client_tx();

        let id1 = mgr.add_client("token-a".into(), tx1);
        let id2 = mgr.add_client("token-b".into(), tx2);

        assert_eq!(id1, ConnectionId(1));
        assert_eq!(id2, ConnectionId(2));
        assert_eq!(mgr.client_count(), 2);
    }

    #[test]
    fn remove_client_decrements_count() {
        let mgr = WebSocketManager::new();
        let (tx, _rx) = new_client_tx();
        let id = mgr.add_client("token".into(), tx);

        assert_eq!(mgr.client_count(), 1);
        mgr.remove_client(id);
        assert_eq!(mgr.client_count(), 0);
    }

    #[test]
    fn remove_nonexistent_client_is_noop() {
        let mgr = WebSocketManager::new();
        mgr.remove_client(ConnectionId(999));
        assert_eq!(mgr.client_count(), 0);
    }

    #[test]
    fn update_last_ping_refreshes_timestamp() {
        let mgr = WebSocketManager::new();
        let (tx, _rx) = new_client_tx();
        let id = mgr.add_client("token".into(), tx);

        let before = mgr.connections.get(&id).map(|c| c.last_ping).unwrap();

        // Small busy-wait to ensure time advances
        std::thread::sleep(std::time::Duration::from_millis(5));

        mgr.update_last_ping(id);

        let after = mgr.connections.get(&id).map(|c| c.last_ping).unwrap();

        assert!(after > before);
    }

    #[test]
    fn update_last_ping_nonexistent_is_noop() {
        let mgr = WebSocketManager::new();
        mgr.update_last_ping(ConnectionId(999));
    }

    #[test]
    fn broadcast_all_delivers_to_all() {
        let mgr = WebSocketManager::new();
        let (tx1, mut rx1) = new_client_tx();
        let (tx2, mut rx2) = new_client_tx();

        mgr.add_client("t1".into(), tx1);
        mgr.add_client("t2".into(), tx2);

        let event = WebSocketMessage::new("test-event", json!({"key": "val"}));
        mgr.broadcast_all(event);

        let msg1 = rx1.try_recv().unwrap();
        let msg2 = rx2.try_recv().unwrap();

        match (&msg1, &msg2) {
            (WsOutbound::Text(t1), WsOutbound::Text(t2)) => {
                assert_eq!(t1, t2);
                assert!(t1.contains("test-event"));
            }
            _ => panic!("expected Text messages"),
        }
    }

    #[test]
    fn broadcast_to_user_delivers_only_matching_connections() {
        let mgr = WebSocketManager::new();
        let (tx1, mut rx1) = new_client_tx();
        let (tx2, mut rx2) = new_client_tx();
        let (tx3, mut rx3) = new_client_tx();

        mgr.add_client_for_user("user-a".into(), "t1".into(), tx1);
        mgr.add_client_for_user("user-b".into(), "t2".into(), tx2);
        mgr.add_client_for_user("user-a".into(), "t3".into(), tx3);

        assert_eq!(mgr.client_count_for_user("user-a"), 2);
        assert_eq!(mgr.client_count_for_user("user-b"), 1);

        let event = WebSocketMessage::new("scoped-event", json!({"user_id": "user-a"}));
        mgr.broadcast_to_user("user-a", event);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        assert!(rx3.try_recv().is_ok());
    }

    #[test]
    fn disconnect_user_closes_and_removes_only_matching_connections() {
        let mgr = WebSocketManager::new();
        let (tx1, mut rx1) = new_client_tx();
        let (tx2, mut rx2) = new_client_tx();
        let (tx3, mut rx3) = new_client_tx();

        mgr.add_client_for_user("user-a".into(), "t1".into(), tx1);
        mgr.add_client_for_user("user-b".into(), "t2".into(), tx2);
        mgr.add_client_for_user("user-a".into(), "t3".into(), tx3);

        let removed = mgr.disconnect_user("user-a", "session revoked");

        assert_eq!(removed, 2);
        assert_eq!(mgr.client_count_for_user("user-a"), 0);
        assert_eq!(mgr.client_count_for_user("user-b"), 1);
        for msg in [rx1.try_recv().unwrap(), rx3.try_recv().unwrap()] {
            match msg {
                WsOutbound::TextThenClose(text, code, reason) => {
                    assert_realtime_auth_expired(&text);
                    assert_eq!(code, WebSocketCloseCode::PolicyViolation);
                    assert_eq!(reason, "session revoked");
                }
                other => panic!("expected terminal auth close, got {other:?}"),
            }
        }
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn disconnect_user_removes_matching_connections_when_queue_is_full() {
        let mgr = WebSocketManager::new();
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(WsOutbound::Text("queued".into())).unwrap();
        mgr.add_client_for_user("user-a".into(), "token".into(), tx);

        let removed = mgr.disconnect_user("user-a", "session revoked");

        assert_eq!(removed, 1);
        assert_eq!(mgr.client_count(), 0);
        assert_eq!(rx.try_recv().unwrap(), WsOutbound::Text("queued".into()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn broadcast_all_removes_closed_channels() {
        let mgr = WebSocketManager::new();
        let (tx1, rx1) = new_client_tx();
        let (tx2, _rx2) = new_client_tx();

        mgr.add_client("t1".into(), tx1);
        mgr.add_client("t2".into(), tx2);

        // Drop rx1 to close the channel
        drop(rx1);

        let event = WebSocketMessage::new("test", json!(null));
        mgr.broadcast_all(event);

        // Client 1 should be removed
        assert_eq!(mgr.client_count(), 1);
    }

    #[test]
    fn broadcast_all_handles_full_channel() {
        let mgr = WebSocketManager::new();
        // Use a channel with capacity 1
        let (tx, _rx) = mpsc::channel(1);
        mgr.add_client("tok".into(), tx);

        // Fill the channel
        mgr.broadcast_all(WebSocketMessage::new("e1", json!(null)));
        // This should warn but not remove the client
        mgr.broadcast_all(WebSocketMessage::new("e2", json!(null)));

        assert_eq!(mgr.client_count(), 1);
    }

    #[test]
    fn send_to_delivers_to_target_only() {
        let mgr = WebSocketManager::new();
        let (tx1, mut rx1) = new_client_tx();
        let (tx2, mut rx2) = new_client_tx();

        let id1 = mgr.add_client("t1".into(), tx1);
        mgr.add_client("t2".into(), tx2);

        let msg = WebSocketMessage::new("unicast", json!({"for": "id1"}));
        mgr.send_to(id1, msg);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn send_to_nonexistent_is_noop() {
        let mgr = WebSocketManager::new();
        let msg = WebSocketMessage::new("ghost", json!(null));
        mgr.send_to(ConnectionId(999), msg);
    }

    #[test]
    fn send_to_removes_closed_channel() {
        let mgr = WebSocketManager::new();
        let (tx, rx) = new_client_tx();
        let id = mgr.add_client("tok".into(), tx);
        drop(rx);

        mgr.send_to(id, WebSocketMessage::new("test", json!(null)));
        assert_eq!(mgr.client_count(), 0);
    }

    #[test]
    fn send_to_or_disconnect_delivers_when_capacity_available() {
        let mgr = WebSocketManager::new();
        let (tx, mut rx) = new_client_tx();
        let id = mgr.add_client("tok".into(), tx);

        mgr.send_to_or_disconnect(id, WebSocketMessage::new("fs", json!({"ok": true})));

        assert!(rx.try_recv().is_ok(), "frame delivered");
        assert_eq!(mgr.client_count(), 1, "connection kept when it fits");
    }

    #[test]
    fn send_to_or_disconnect_closes_connection_on_full() {
        let mgr = WebSocketManager::new();
        // Capacity 1 so the second send saturates the channel.
        let (tx, _rx) = mpsc::channel(1);
        let id = mgr.add_client("tok".into(), tx);

        mgr.send_to_or_disconnect(id, WebSocketMessage::new("fs", json!({})));
        assert_eq!(mgr.client_count(), 1, "first send fills the buffer, connection alive");

        // Second send hits a full channel → backpressure close (no silent drop).
        mgr.send_to_or_disconnect(id, WebSocketMessage::new("fs", json!({})));
        assert_eq!(
            mgr.client_count(),
            0,
            "ordered-stream backpressure closes the connection"
        );
    }

    #[test]
    fn send_to_or_disconnect_removes_on_closed() {
        let mgr = WebSocketManager::new();
        let (tx, rx) = new_client_tx();
        let id = mgr.add_client("tok".into(), tx);
        drop(rx);

        mgr.send_to_or_disconnect(id, WebSocketMessage::new("fs", json!({})));
        assert_eq!(mgr.client_count(), 0);
    }

    #[test]
    fn heartbeat_tick_sends_ping_to_healthy_connection() {
        let connections = Arc::new(DashMap::new());
        let (tx, mut rx) = new_client_tx();

        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "valid".into(),
                last_ping: Instant::now(),
                tx,
            },
        );

        heartbeat_tick(&connections, &always_valid());

        // Should still be connected
        assert_eq!(connections.len(), 1);

        // Should have received a ping
        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["name"], "ping");
                assert!(parsed["data"]["timestamp"].is_u64());
            }
            _ => panic!("expected Text ping"),
        }
    }

    #[test]
    fn heartbeat_tick_removes_timed_out_connection() {
        let connections = Arc::new(DashMap::new());
        let (tx, mut rx) = new_client_tx();

        // Set last_ping to well past the timeout
        let old_ping = Instant::now() - (HEARTBEAT_TIMEOUT * 2);

        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "valid".into(),
                last_ping: old_ping,
                tx,
            },
        );

        heartbeat_tick(&connections, &always_valid());

        // Connection should be removed
        assert_eq!(connections.len(), 0);

        // Should have received realtime heartbeat-timeout event and close as one terminal outbound.
        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::TextThenClose(text, code, reason) => {
                assert_realtime_heartbeat_timeout(&text);
                assert_eq!(code, WebSocketCloseCode::PolicyViolation);
                assert_eq!(reason, "heartbeat timeout");
            }
            other => panic!("expected realtime heartbeat-timeout terminal message, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_tick_removes_expired_token_connection() {
        let connections = Arc::new(DashMap::new());
        let (tx, mut rx) = new_client_tx();

        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "expired-token".into(),
                last_ping: Instant::now(),
                tx,
            },
        );

        heartbeat_tick(&connections, &always_expired());

        // Connection should be removed
        assert_eq!(connections.len(), 0);

        // Should have received realtime auth-expired event and close as one terminal outbound.
        let msg1 = rx.try_recv().unwrap();
        match msg1 {
            WsOutbound::TextThenClose(text, code, reason) => {
                assert_realtime_auth_expired(&text);
                assert_eq!(code, WebSocketCloseCode::PolicyViolation);
                assert_eq!(reason, "token expired");
            }
            _ => panic!("expected realtime auth-expired terminal message"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn heartbeat_tick_removes_expired_token_connection_when_terminal_queue_is_full() {
        let connections = Arc::new(DashMap::new());
        let (tx, mut rx) = mpsc::channel(1);

        tx.try_send(WsOutbound::Text("queued".into())).unwrap();
        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "expired-token".into(),
                last_ping: Instant::now(),
                tx,
            },
        );

        heartbeat_tick(&connections, &always_expired());

        assert_eq!(connections.len(), 0);
        assert_eq!(rx.try_recv().unwrap(), WsOutbound::Text("queued".into()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn heartbeat_tick_timeout_takes_priority_over_token_check() {
        let connections = Arc::new(DashMap::new());
        let (tx, mut rx) = new_client_tx();

        // Both timed out AND expired token
        let old_ping = Instant::now() - (HEARTBEAT_TIMEOUT * 2);
        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "expired".into(),
                last_ping: old_ping,
                tx,
            },
        );

        heartbeat_tick(&connections, &always_expired());

        assert_eq!(connections.len(), 0);

        // Only heartbeat timeout terminal message (no auth-expired event)
        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::TextThenClose(text, code, reason) => {
                assert_realtime_heartbeat_timeout(&text);
                assert_eq!(code, WebSocketCloseCode::PolicyViolation);
                assert_eq!(reason, "heartbeat timeout");
            }
            other => panic!("expected realtime heartbeat-timeout terminal message, got {other:?}"),
        }
        // No more messages
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn heartbeat_tick_mixed_connections() {
        let connections = Arc::new(DashMap::new());

        // Healthy connection
        let (tx1, _rx1) = new_client_tx();
        connections.insert(
            ConnectionId(1),
            ClientInfo {
                user_id: "user-a".into(),
                token: "good".into(),
                last_ping: Instant::now(),
                tx: tx1,
            },
        );

        // Timed-out connection
        let (tx2, _rx2) = new_client_tx();
        connections.insert(
            ConnectionId(2),
            ClientInfo {
                user_id: "user-a".into(),
                token: "good".into(),
                last_ping: Instant::now() - (HEARTBEAT_TIMEOUT * 2),
                tx: tx2,
            },
        );

        let selective_validator: TokenValidator = Arc::new(|_| true);
        heartbeat_tick(&connections, &selective_validator);

        // Only healthy connection remains
        assert_eq!(connections.len(), 1);
        assert!(connections.contains_key(&ConnectionId(1)));
    }

    #[test]
    fn event_broadcaster_impl_routes_to_event_user() {
        let mgr = WebSocketManager::new();
        let (tx, mut rx) = new_client_tx();
        mgr.add_client_for_user("user-a".into(), "tok".into(), tx);
        let (tx_other, mut rx_other) = new_client_tx();
        mgr.add_client_for_user("user-b".into(), "tok-other".into(), tx_other);

        let broadcaster: &dyn EventBroadcaster = &mgr;
        broadcaster.broadcast(WebSocketMessage::new("via-trait", json!({"user_id": "user-a"})));

        let msg = rx.try_recv().unwrap();
        match msg {
            WsOutbound::Text(text) => {
                assert!(text.contains("via-trait"));
            }
            _ => panic!("expected Text"),
        }
        assert!(rx_other.try_recv().is_err());
    }

    #[test]
    fn event_broadcaster_impl_drops_unscoped_events() {
        let mgr = WebSocketManager::new();
        let (tx, mut rx) = new_client_tx();
        mgr.add_client_for_user("user-a".into(), "tok".into(), tx);

        let broadcaster: &dyn EventBroadcaster = &mgr;
        broadcaster.broadcast(WebSocketMessage::new("via-trait", json!({})));

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn default_creates_empty_manager() {
        let mgr = WebSocketManager::default();
        assert_eq!(mgr.client_count(), 0);
    }
}
