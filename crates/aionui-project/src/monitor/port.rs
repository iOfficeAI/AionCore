//! Outbound push port + inbound command envelope for the monitor actor.
//!
//! These keep the actor transport-agnostic: it consumes [`FsInbound`] events and
//! emits frames through [`FsWirePush`], so the composition layer owns the actual
//! WS socket (envelope wrapping + unicast) without the domain depending on it.

use serde_json::Value;

/// Identifies one WS connection (= one session). A string mirroring the
/// transport's connection id (matches the runtime [`Subscriber::session`] type),
/// so the domain does not depend on the realtime crate's `ConnectionId`.
///
/// [`Subscriber::session`]: crate::runtime::Subscriber::session
pub type SessionId = String;

/// Narrow outbound port: deliver one inner JSON-RPC frame to a connection.
///
/// The composition layer implements this over the WS manager's unicast, wrapping
/// each frame in the transport envelope (`{ name: "fs", data: <frame> }`).
pub trait FsWirePush: Send + Sync {
    /// Push one JSON-RPC frame (response or server notification) to `session`.
    fn push(&self, session: &str, frame: Value);
}

/// An inbound event delivered to the actor from the transport layer.
#[derive(Debug, Clone)]
pub enum FsInbound {
    /// An outer-envelope `fs` frame's payload (the inner JSON-RPC value).
    /// `user_id` is the connection's authenticated Core user (resolved at WS
    /// connect); every user-scoped resolve in the actor is gated on it.
    Frame {
        session: SessionId,
        user_id: String,
        frame: Value,
    },
    /// The connection closed — release its subscriptions (`drop_session`).
    Disconnect { session: SessionId },
}
