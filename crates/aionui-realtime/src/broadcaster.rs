use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use tokio::sync::broadcast;
use tracing::warn;

use crate::event_log::{DEFAULT_EVENT_LOG_CAPACITY, EventLog, ResumeSlice};
use aionui_api_types::EventCursor;

/// Trait for broadcasting WebSocket events to all connected clients.
///
/// Business modules depend on this trait (via `Arc<dyn EventBroadcaster>`)
/// to push events without coupling to WebSocket internals.
///
/// Note: `send_to` (unicast) is intentionally NOT part of this trait.
/// Unicast is a connection-management concern handled by `WebSocketManager`.
pub trait EventBroadcaster: Send + Sync {
    /// Broadcast an event to all connected WebSocket clients.
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>);
}

/// Default implementation of [`EventBroadcaster`] backed by
/// `tokio::sync::broadcast` channel plus an [`EventLog`] resume ring (E14).
///
/// The broadcast channel is used for module-to-WebSocket event fan-out.
/// Each `WebSocketManager` connection subscribes to this channel and
/// forwards received events to its per-connection `mpsc` sender.
pub struct BroadcastEventBus {
    tx: broadcast::Sender<WebSocketMessage<serde_json::Value>>,
    log: Arc<EventLog>,
}

impl BroadcastEventBus {
    /// Create a new event bus with the given channel capacity and default
    /// resume ring (`DEFAULT_EVENT_LOG_CAPACITY`).
    pub fn new(capacity: usize) -> Self {
        Self::with_event_log(capacity, Arc::new(EventLog::new(DEFAULT_EVENT_LOG_CAPACITY)))
    }

    /// Create a bus that shares an existing [`EventLog`] (tests / custom epoch).
    pub fn with_event_log(capacity: usize, log: Arc<EventLog>) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, log }
    }

    /// Shared resume ring (epoch/sequence).
    pub fn event_log(&self) -> Arc<EventLog> {
        self.log.clone()
    }

    pub fn current_cursor(&self) -> EventCursor {
        self.log.current_cursor()
    }

    pub fn events_after(&self, after: EventCursor) -> ResumeSlice {
        self.log.events_after(after)
    }

    /// Subscribe to receive broadcast events.
    ///
    /// Each WebSocket connection calls this once to get its own receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<WebSocketMessage<serde_json::Value>> {
        self.tx.subscribe()
    }

    /// Returns the number of active subscribers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl EventBroadcaster for BroadcastEventBus {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        let event = self.log.record(event);
        if let Err(e) = self.tx.send(event) {
            warn!(
                event_name = %e.0.name,
                "broadcast failed: no active receivers"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_bus_has_zero_receivers() {
        let bus = BroadcastEventBus::new(16);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn subscribe_increments_receiver_count() {
        let bus = BroadcastEventBus::new(16);
        let _rx1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        let _rx2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);
    }

    #[test]
    fn drop_receiver_decrements_count() {
        let bus = BroadcastEventBus::new(16);
        let rx = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        drop(rx);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn broadcast_without_receivers_does_not_panic() {
        let bus = BroadcastEventBus::new(16);
        let event = WebSocketMessage::new("test", json!({}));
        bus.broadcast(event);
    }

    #[tokio::test]
    async fn broadcast_delivers_to_subscriber() {
        let bus = BroadcastEventBus::new(16);
        let mut rx = bus.subscribe();

        let event = WebSocketMessage::new("chat:update", json!({"id": 1}));
        bus.broadcast(event);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.name, "chat:update");
        assert_eq!(received.data["id"], 1);
        assert_eq!(received.cursor, Some(EventCursor::new(1, 1)));
    }

    #[tokio::test]
    async fn broadcast_delivers_to_all_subscribers() {
        let bus = BroadcastEventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let event = WebSocketMessage::new("ping", json!({"ts": 100}));
        bus.broadcast(event);

        let msg1 = rx1.recv().await.unwrap();
        let msg2 = rx2.recv().await.unwrap();
        assert_eq!(msg1.name, "ping");
        assert_eq!(msg2.name, "ping");
        assert_eq!(msg1.data, msg2.data);
        assert_eq!(msg1.cursor, msg2.cursor);
    }

    #[tokio::test]
    async fn broadcast_multiple_events_in_order() {
        let bus = BroadcastEventBus::new(16);
        let mut rx = bus.subscribe();

        for i in 0..5 {
            let event = WebSocketMessage::new(format!("evt-{i}"), json!({"n": i}));
            bus.broadcast(event);
        }

        for i in 0..5 {
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.name, format!("evt-{i}"));
            assert_eq!(msg.data["n"], i);
            assert_eq!(msg.cursor, Some(EventCursor::new(1, (i + 1) as u64)));
        }
    }

    #[test]
    fn trait_object_broadcast_works() {
        let bus = BroadcastEventBus::new(16);
        let broadcaster: &dyn EventBroadcaster = &bus;
        let event = WebSocketMessage::new("trait-test", json!({}));
        broadcaster.broadcast(event);
        assert_eq!(bus.current_cursor().sequence, 1);
    }

    #[test]
    fn resume_after_broadcast() {
        let bus = BroadcastEventBus::new(16);
        bus.broadcast(WebSocketMessage::new("a", json!({})));
        bus.broadcast(WebSocketMessage::new("b", json!({})));
        let slice = bus.events_after(EventCursor::new(1, 1));
        assert_eq!(slice.events.len(), 1);
        assert_eq!(slice.events[0].name, "b");
    }
}
