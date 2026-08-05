//! Wiring tests: envelope routing, disconnect propagation, and composition.

use aionui_realtime::{PER_CONNECTION_BUFFER, WsOutbound};
use serde_json::json;
use tokio::sync::mpsc;

use super::*;

#[test]
fn router_claims_scm_frames_with_a_stringified_session() {
    let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = ScmMessageRouter { inbound };

    let handled = router.route(ConnectionId(5), "user-1", "scm", json!({"method": "scm/status"}));
    assert!(handled, "scm frames are claimed");

    match rx.try_recv().expect("frame forwarded") {
        ScmInbound::Frame {
            session,
            user_id,
            frame,
        } => {
            assert_eq!(session, "5", "the connection id becomes the session id");
            assert_eq!(user_id, "user-1", "the connection's user is threaded through");
            assert_eq!(frame["method"], "scm/status");
        }
        other => panic!("expected Frame, got {other:?}"),
    }
}

#[test]
fn router_declines_other_envelopes() {
    let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = ScmMessageRouter { inbound };

    // Declining is what lets several feature routers share one slot.
    assert!(!router.route(ConnectionId(1), "user-1", "fs", json!({})));
    assert!(rx.try_recv().is_err(), "nothing is forwarded for another feature");
}

#[test]
fn router_forwards_disconnect() {
    let (inbound, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = ScmMessageRouter { inbound };

    // Load-bearing: this is the only signal that releases a connection's watches.
    router.on_disconnect(ConnectionId(9));
    match rx.try_recv().expect("disconnect forwarded") {
        ScmInbound::Disconnect { session } => assert_eq!(session, "9"),
        other => panic!("expected Disconnect, got {other:?}"),
    }
}

#[test]
fn push_wraps_frames_in_the_scm_envelope() {
    let manager = Arc::new(WebSocketManager::new());
    let (tx, mut rx) = mpsc::channel::<WsOutbound>(PER_CONNECTION_BUFFER);
    let conn = manager.add_client("tok".to_owned(), tx);

    let push = WsManagerPush {
        manager: Arc::clone(&manager),
    };
    push.push(&conn.0.to_string(), json!({"result": {}}));

    match rx.try_recv().expect("frame delivered") {
        WsOutbound::Text(text) => {
            let parsed: Value = serde_json::from_str(&text).expect("valid json");
            // The name is what routes it back to the source-control client, and it
            // must not collide with the explorer's own envelope.
            assert_eq!(parsed["name"], "scm");
            assert!(parsed["data"]["result"].is_object());
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn push_to_an_unparseable_session_is_a_noop() {
    let manager = Arc::new(WebSocketManager::new());
    let push = WsManagerPush { manager };
    // Must not panic: session ids arrive as strings from the transport.
    push.push("not-a-number", json!({}));
}

/// Records which envelope names it was offered, claiming only the one it owns.
struct SpyRouter {
    name: &'static str,
    seen: std::sync::Mutex<Vec<String>>,
    disconnects: std::sync::Mutex<Vec<u64>>,
}

impl SpyRouter {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            seen: std::sync::Mutex::new(Vec::new()),
            disconnects: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MessageRouter for SpyRouter {
    fn route(&self, _conn_id: ConnectionId, _user_id: &str, name: &str, _data: Value) -> bool {
        self.seen.lock().expect("spy poisoned").push(name.to_owned());
        name == self.name
    }

    fn on_disconnect(&self, conn_id: ConnectionId) {
        self.disconnects.lock().expect("spy poisoned").push(conn_id.0);
    }
}

#[test]
fn composite_routes_each_envelope_to_its_owner() {
    let fs = Arc::new(SpyRouter::new("fs"));
    let scm = Arc::new(SpyRouter::new("scm"));
    let composite = CompositeMessageRouter::new(vec![
        Arc::clone(&fs) as Arc<dyn MessageRouter>,
        Arc::clone(&scm) as Arc<dyn MessageRouter>,
    ]);

    assert!(composite.route(ConnectionId(1), "u", "scm", json!({"a": 1})));
    assert!(composite.route(ConnectionId(1), "u", "fs", json!({})));
    assert_eq!(
        scm.seen.lock().expect("spy").len(),
        1,
        "the scm router is only consulted for the frame fs declined"
    );
}

#[test]
fn composite_reports_unclaimed_envelopes_as_unhandled() {
    let fs = Arc::new(SpyRouter::new("fs"));
    let composite = CompositeMessageRouter::new(vec![fs as Arc<dyn MessageRouter>]);

    // Returning false is what lets the realtime layer answer "unsupported message"
    // instead of silently swallowing it.
    assert!(!composite.route(ConnectionId(1), "u", "something-else", json!({})));
}

#[test]
fn composite_broadcasts_disconnect_to_every_router() {
    // Each stateful router has its own per-connection state to release, so a
    // disconnect must reach all of them — not just the one that last claimed a
    // frame.
    let fs = Arc::new(SpyRouter::new("fs"));
    let scm = Arc::new(SpyRouter::new("scm"));
    let composite = CompositeMessageRouter::new(vec![
        Arc::clone(&fs) as Arc<dyn MessageRouter>,
        Arc::clone(&scm) as Arc<dyn MessageRouter>,
    ]);

    composite.on_disconnect(ConnectionId(7));
    assert_eq!(*fs.disconnects.lock().expect("spy"), vec![7]);
    assert_eq!(*scm.disconnects.lock().expect("spy"), vec![7]);
}

/// A router that panics on disconnect, standing in for a sub-router with a bug.
struct PanickingRouter;

impl MessageRouter for PanickingRouter {
    fn route(&self, _conn_id: ConnectionId, _user_id: &str, _name: &str, _data: Value) -> bool {
        false
    }

    fn on_disconnect(&self, _conn_id: ConnectionId) {
        panic!("sub-router failed while releasing");
    }
}

#[test]
fn one_failing_router_does_not_block_the_others_release() {
    // Disconnect is the only signal that frees per-connection resources. If one
    // sub-router panics partway through the broadcast, every router after it would
    // never be told — one subsystem's bug would leak another subsystem's watches.
    let scm = Arc::new(SpyRouter::new("scm"));
    let composite = CompositeMessageRouter::new(vec![
        Arc::new(PanickingRouter) as Arc<dyn MessageRouter>,
        Arc::clone(&scm) as Arc<dyn MessageRouter>,
    ]);

    composite.on_disconnect(ConnectionId(3));

    assert_eq!(
        *scm.disconnects.lock().expect("spy"),
        vec![3],
        "the router after the failing one still got its disconnect"
    );
}
