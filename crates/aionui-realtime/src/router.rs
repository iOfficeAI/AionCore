use crate::types::ConnectionId;

/// Routes upstream WebSocket messages to business logic handlers.
///
/// The `name` field of the incoming `WebSocketMessage` determines
/// which handler processes the message. Phase 4 provides only a
/// no-op implementation; concrete routing is added in later phases.
pub trait MessageRouter: Send + Sync {
    /// Route an upstream message to the appropriate handler.
    ///
    /// Called for any message whose `name` is not handled internally
    /// by the WebSocket layer (i.e. not `pong` or `subscribe-show-open`).
    /// `user_id` is the authenticated Core user of the connection, resolved at
    /// connect time — routers gate all user-scoped reads/writes on it.
    fn route(&self, conn_id: ConnectionId, user_id: &str, name: &str, data: serde_json::Value) -> bool;

    /// Notify the router that a connection has closed.
    ///
    /// Called once by the WebSocket layer after the receive loop for `conn_id`
    /// exits (client disconnect, transport error, or backpressure close), so
    /// stateful routers can release per-connection state (e.g. drop a session's
    /// filesystem subscriptions). Default is a no-op for stateless routers.
    fn on_disconnect(&self, conn_id: ConnectionId) {
        let _ = conn_id;
    }
}

/// A no-op message router that reports every message as unhandled.
///
/// Used as a placeholder until business modules provide real routing. The
/// WebSocket handler turns the `false` return value into
/// `REALTIME_UNSUPPORTED_MESSAGE`.
pub struct NoopMessageRouter;

impl MessageRouter for NoopMessageRouter {
    fn route(&self, conn_id: ConnectionId, _user_id: &str, name: &str, _data: serde_json::Value) -> bool {
        tracing::debug!(
            %conn_id,
            message_name = name,
            "no router registered, message discarded"
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn noop_router_does_not_panic() {
        let router = NoopMessageRouter;
        let handled = router.route(ConnectionId(1), "user-1", "some-event", json!({"key": "val"}));

        assert!(!handled);
    }

    #[test]
    fn noop_router_is_trait_object_compatible() {
        let router: Box<dyn MessageRouter> = Box::new(NoopMessageRouter);
        let handled = router.route(ConnectionId(42), "user-1", "test", json!(null));

        assert!(!handled);
    }

    #[test]
    fn on_disconnect_default_is_noop() {
        // Stateless routers inherit the default no-op; must not panic.
        let router: Box<dyn MessageRouter> = Box::new(NoopMessageRouter);
        router.on_disconnect(ConnectionId(7));
    }

    #[test]
    fn on_disconnect_override_receives_conn_id() {
        use std::sync::Mutex;

        struct RecordingRouter {
            disconnected: Mutex<Vec<ConnectionId>>,
        }
        impl MessageRouter for RecordingRouter {
            fn route(&self, _conn_id: ConnectionId, _user_id: &str, _name: &str, _data: serde_json::Value) -> bool {
                false
            }
            fn on_disconnect(&self, conn_id: ConnectionId) {
                self.disconnected.lock().unwrap().push(conn_id);
            }
        }

        let router = RecordingRouter {
            disconnected: Mutex::new(Vec::new()),
        };
        router.on_disconnect(ConnectionId(99));
        assert_eq!(router.disconnected.lock().unwrap().as_slice(), &[ConnectionId(99)]);
    }
}
