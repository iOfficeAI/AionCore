//! Composition wiring for the Project Explorer filesystem monitor.
//!
//! Adapts the transport-agnostic [`FsMonitorActor`] (in `aionui-project`) to the
//! realtime WebSocket layer: an [`FsMessageRouter`] forwards inbound `fs` frames
//! (and disconnects) into the actor's channel, and [`WsManagerPush`] implements
//! the actor's outbound port over the WS manager's unicast, wrapping each inner
//! JSON-RPC frame in the transport envelope (`{ name: "fs", data }`). The actor
//! runs as one background task (desktop N = 1).

use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_project::ProjectService;
use aionui_project::monitor::{FsInbound, FsMonitorActor, FsWirePush};
use aionui_realtime::{ConnectionId, MessageRouter, NoopMessageRouter, WebSocketManager};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Warm-node watch budget for the single desktop shard. Generous — desktop
/// observes only the directories the user has expanded.
const WARM_BUDGET: usize = 4096;

/// Inbound adapter: routes outer-envelope `fs` frames to the monitor actor.
///
/// The realtime layer has already unwrapped the envelope, so `route` receives
/// `name == "fs"` and `data == <inner JSON-RPC frame>`. Returns `true` to claim
/// the message; the actual reply/notification is delivered asynchronously by the
/// actor via [`WsManagerPush`].
struct FsMessageRouter {
    inbound: UnboundedSender<FsInbound>,
}

impl MessageRouter for FsMessageRouter {
    fn route(&self, conn_id: ConnectionId, name: &str, data: Value) -> bool {
        if name != "fs" {
            return false;
        }
        // A closed channel means the actor task has stopped; the frame is
        // dropped (the connection will observe silence and can reconnect).
        let _ = self.inbound.send(FsInbound::Frame {
            session: conn_id.0.to_string(),
            frame: data,
        });
        true
    }

    fn on_disconnect(&self, conn_id: ConnectionId) {
        let _ = self.inbound.send(FsInbound::Disconnect {
            session: conn_id.0.to_string(),
        });
    }
}

/// Outbound adapter: deliver one inner JSON-RPC frame to a connection, wrapped
/// in the transport envelope, via the WS manager's unicast.
struct WsManagerPush {
    manager: Arc<WebSocketManager>,
}

impl FsWirePush for WsManagerPush {
    fn push(&self, session: &str, frame: Value) {
        // `session` is a stringified ConnectionId (see FsMessageRouter).
        if let Ok(id) = session.parse::<u64>() {
            // Ordered stream: on backpressure the connection is dropped (the
            // client reconnects + re-subscribes) rather than silently dropping a
            // frame, which would desync the tree with no gap detection. See
            // protocol.md §契约规则 ("背压=关连接、不静默丢帧").
            self.manager
                .send_to_or_disconnect(ConnectionId(id), WebSocketMessage::new("fs", frame));
        }
    }
}

/// Spawn the monitor actor as a background task and return the inbound router to
/// install on the WS handler. On init failure the fs feature degrades to a no-op
/// router (the rest of the WS layer stays up).
///
/// Must be called from within a Tokio runtime (spawns the actor loop).
pub fn spawn_fs_monitor(project: Arc<ProjectService>, manager: Arc<WebSocketManager>) -> Arc<dyn MessageRouter> {
    let push: Arc<dyn FsWirePush> = Arc::new(WsManagerPush { manager });
    match FsMonitorActor::new(project, push, WARM_BUDGET) {
        Ok((actor, raw_rx)) => {
            let (inbound, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(actor.run(inbound_rx, raw_rx));
            Arc::new(FsMessageRouter { inbound })
        }
        Err(err) => {
            tracing::error!(error = %err, "fs monitor init failed; filesystem protocol disabled");
            Arc::new(NoopMessageRouter)
        }
    }
}

#[cfg(test)]
mod tests {
    use aionui_realtime::{PER_CONNECTION_BUFFER, WsOutbound};
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn router_forwards_fs_frame_with_stringified_session() {
        let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let router = FsMessageRouter { inbound };

        let handled = router.route(ConnectionId(5), "fs", json!({"method": "initialize"}));
        assert!(handled, "fs frames are claimed");

        match rx.try_recv().unwrap() {
            FsInbound::Frame { session, frame } => {
                assert_eq!(session, "5", "ConnectionId is stringified into the session id");
                assert_eq!(frame["method"], "initialize");
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn router_ignores_non_fs_messages() {
        let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let router = FsMessageRouter { inbound };

        let handled = router.route(ConnectionId(1), "conversation.send-message", json!({}));
        assert!(!handled, "non-fs messages fall through to other routing");
        assert!(rx.try_recv().is_err(), "nothing forwarded for non-fs");
    }

    #[test]
    fn router_on_disconnect_forwards_disconnect() {
        let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let router = FsMessageRouter { inbound };

        router.on_disconnect(ConnectionId(9));
        match rx.try_recv().unwrap() {
            FsInbound::Disconnect { session } => assert_eq!(session, "9"),
            other => panic!("expected Disconnect, got {other:?}"),
        }
    }

    #[test]
    fn push_wraps_frame_in_fs_envelope_and_unicasts() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx, mut rx) = mpsc::channel::<WsOutbound>(PER_CONNECTION_BUFFER);
        let conn = manager.add_client("tok".to_owned(), tx);

        let push = WsManagerPush {
            manager: Arc::clone(&manager),
        };
        push.push(&conn.0.to_string(), json!({"result": {"ok": true}}));

        match rx.try_recv().unwrap() {
            WsOutbound::Text(text) => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                // Outer transport envelope carries name "fs"; inner is the frame.
                assert_eq!(parsed["name"], "fs");
                assert_eq!(parsed["data"]["result"]["ok"], true);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn push_to_unparseable_session_is_noop() {
        let manager = Arc::new(WebSocketManager::new());
        let push = WsManagerPush { manager };
        // Must not panic on a non-numeric session id.
        push.push("not-a-number", json!({}));
    }
}
