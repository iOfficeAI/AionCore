use serde::{Deserialize, Serialize};

/// Resume cursor for a headless Host event stream (E14 / RACP-inspired).
///
/// Clients reconnecting over an SSH tunnel (or any transport) send the last
/// acknowledged `(epoch, sequence)` pair; the Host may then skip already-seen
/// events. Both fields default to `0` so older envelopes without a cursor
/// remain deserializable. Full event-log replay is a follow-up — this type is
/// the shared contract only (no AGPL dependencies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EventCursor {
    /// Host process / stream generation. Bumps when the Host restarts or
    /// rotates its event log so a stale sequence cannot be replayed across
    /// generations.
    #[serde(default)]
    pub epoch: u64,
    /// Monotonic per-epoch event id. Clients resume with `after_sequence`
    /// semantics (events with `sequence > cursor.sequence`).
    #[serde(default)]
    pub sequence: u64,
}

impl EventCursor {
    pub const fn new(epoch: u64, sequence: u64) -> Self {
        Self { epoch, sequence }
    }

    /// True when `other` is strictly after `self` in the same epoch, or any
    /// event from a newer epoch.
    pub const fn precedes(self, other: EventCursor) -> bool {
        other.epoch > self.epoch || (other.epoch == self.epoch && other.sequence > self.sequence)
    }
}

/// WebSocket message envelope.
///
/// All WebSocket communication follows this format: a `name` field
/// identifying the event type and a `data` field with the payload.
/// Optional `cursor` carries epoch/sequence resume metadata for headless
/// Host reconnect (E14); omitted/None on legacy clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketMessage<T> {
    pub name: String,
    pub data: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<EventCursor>,
}

impl<T> WebSocketMessage<T> {
    pub fn new(name: impl Into<String>, data: T) -> Self {
        Self {
            name: name.into(),
            data,
            cursor: None,
        }
    }

    pub fn with_cursor(mut self, cursor: EventCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_websocket_message_new() {
        let msg = WebSocketMessage::new("chat:message", "hello");
        assert_eq!(msg.name, "chat:message");
        assert_eq!(msg.data, "hello");
    }

    #[test]
    fn test_websocket_message_serialization() {
        let msg = WebSocketMessage::new("status:update", json!({"online": true}));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["name"], "status:update");
        assert_eq!(json["data"]["online"], true);
    }

    #[test]
    fn test_websocket_message_deserialization() {
        let raw = r#"{"name":"ping","data":null}"#;
        let msg: WebSocketMessage<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.name, "ping");
        assert!(msg.data.is_null());
    }

    #[test]
    fn test_websocket_message_with_complex_data() {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        struct Payload {
            count: u32,
            items: Vec<String>,
        }
        let payload = Payload {
            count: 2,
            items: vec!["a".into(), "b".into()],
        };
        let msg = WebSocketMessage::new("list:update", payload.clone());
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: WebSocketMessage<Payload> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "list:update");
        assert_eq!(deserialized.data, payload);
    }

    #[test]
    fn event_cursor_defaults_and_precedes() {
        let zero = EventCursor::default();
        assert_eq!(zero.epoch, 0);
        assert_eq!(zero.sequence, 0);
        assert!(zero.precedes(EventCursor::new(0, 1)));
        assert!(zero.precedes(EventCursor::new(1, 0)));
        assert!(!EventCursor::new(1, 5).precedes(EventCursor::new(1, 5)));
        assert!(!EventCursor::new(2, 0).precedes(EventCursor::new(1, 99)));
    }

    #[test]
    fn websocket_message_cursor_roundtrip_and_legacy() {
        let msg = WebSocketMessage::new("ping", serde_json::Value::Null).with_cursor(EventCursor::new(3, 42));
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["cursor"]["epoch"], 3);
        assert_eq!(v["cursor"]["sequence"], 42);

        let legacy = r#"{"name":"ping","data":null}"#;
        let parsed: WebSocketMessage<serde_json::Value> = serde_json::from_str(legacy).unwrap();
        assert!(parsed.cursor.is_none());
    }
}
