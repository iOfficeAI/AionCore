//! Composition wiring for the source-control monitor.
//!
//! Adapts the transport-agnostic [`ScmActor`] (in `aionui-project`) to the
//! realtime WebSocket layer, mirroring the explorer's `fs_monitor` wiring: an
//! inbound router forwards `scm` frames and disconnects into the actor's channel,
//! and an outbound adapter implements the actor's push port over the manager's
//! unicast, wrapping each inner JSON-RPC frame in `{ name: "scm", data }`.

use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_project::ProjectService;
use aionui_project::scm::{ScmActor, ScmInbound, ScmWirePush};
use aionui_realtime::{ConnectionId, MessageRouter, WebSocketManager};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Inbound adapter: routes outer-envelope `scm` frames to the actor.
struct ScmMessageRouter {
    inbound: UnboundedSender<ScmInbound>,
}

impl MessageRouter for ScmMessageRouter {
    fn route(&self, conn_id: ConnectionId, user_id: &str, name: &str, data: Value) -> bool {
        if name != "scm" {
            return false;
        }
        // A closed channel means the actor stopped; the frame is dropped and the
        // connection observes silence (it can reconnect and re-subscribe).
        let _ = self.inbound.send(ScmInbound::Frame {
            session: conn_id.0.to_string(),
            user_id: user_id.to_owned(),
            frame: data,
        });
        true
    }

    fn on_disconnect(&self, conn_id: ConnectionId) {
        // Load-bearing: without this, a reconnect churn would leave one armed
        // metadata watch per dropped connection with nothing to release it.
        let _ = self.inbound.send(ScmInbound::Disconnect {
            session: conn_id.0.to_string(),
        });
    }
}

/// Outbound adapter: unicast one inner frame inside the `scm` envelope.
struct WsManagerPush {
    manager: Arc<WebSocketManager>,
}

impl ScmWirePush for WsManagerPush {
    fn push(&self, session: &str, frame: Value) {
        if let Ok(id) = session.parse::<u64>() {
            // Same backpressure policy as the explorer link: drop the connection
            // rather than silently drop a frame, since a client that missed a
            // status frame would keep showing a stale change list with no way to
            // notice. It reconnects and re-subscribes.
            self.manager
                .send_to_or_disconnect(ConnectionId(id), WebSocketMessage::new("scm", frame));
        }
    }
}

/// Spawn the source-control actor and return its inbound router.
///
/// `None` when initialisation fails (e.g. no filesystem watcher available): the
/// caller then installs only the other routers, so a source-control failure never
/// takes the rest of the WebSocket surface down with it.
pub fn spawn_scm_monitor(
    project: Arc<ProjectService>,
    manager: Arc<WebSocketManager>,
) -> Option<Arc<dyn MessageRouter>> {
    let push: Arc<dyn ScmWirePush> = Arc::new(WsManagerPush { manager });
    let (inbound, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    match ScmActor::new(Arc::clone(&project), push) {
        Ok(actor) => {
            // Direct wiring (乙), not an event bus (甲): source control is the only
            // subscriber to project-explorer root changes, so feeding the service's
            // attach/detach notifications straight into this actor's inbound is
            // cheaper and clearer than a general bus. All `ProjectService` handles
            // are clones of one instance, so the sender installed here is the same
            // one the HTTP attach/detach handlers observe. Revisit if a second
            // subscriber to root changes ever appears.
            project.set_scm_roots_sender(inbound.clone());
            tokio::spawn(actor.run(inbound_rx));
            Some(Arc::new(ScmMessageRouter { inbound }))
        }
        Err(err) => {
            tracing::error!(error = %err, "scm monitor init failed; source-control protocol disabled");
            None
        }
    }
}

/// Routes a message to the first sub-router that claims it.
///
/// The realtime layer holds exactly one router, while each feature owns its own
/// envelope name and already declines names it does not serve — so composing them
/// is just "ask in turn". Disconnects go to **all** of them: every stateful
/// router has per-connection state to release.
pub struct CompositeMessageRouter {
    routers: Vec<Arc<dyn MessageRouter>>,
}

impl CompositeMessageRouter {
    pub fn new(routers: Vec<Arc<dyn MessageRouter>>) -> Self {
        Self { routers }
    }
}

impl MessageRouter for CompositeMessageRouter {
    fn route(&self, conn_id: ConnectionId, user_id: &str, name: &str, data: Value) -> bool {
        for router in &self.routers {
            // `data` is cloned per attempt because a declining router must leave
            // the value intact for the next one; only the claiming router uses it.
            if router.route(conn_id, user_id, name, data.clone()) {
                return true;
            }
        }
        false
    }

    fn on_disconnect(&self, conn_id: ConnectionId) {
        for router in &self.routers {
            // Isolated per router: disconnect is the only signal that frees a
            // connection's resources, so one subsystem failing here must not stop
            // the others from being told — that would leak their watches and
            // subscriptions for the lifetime of the process.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| router.on_disconnect(conn_id))).is_err() {
                tracing::error!(
                    conn_id = conn_id.0,
                    "a message router panicked while releasing a connection; continuing with the rest"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "scm_monitor_test.rs"]
mod scm_monitor_test;
