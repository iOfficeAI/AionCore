//! Project Explorer monitor — the WS-facing protocol surface over the runtime.
//!
//! Bridges the JSON-RPC monitor protocol (`formal/runtime/protocol.md`) to the
//! stage-0 runtime core: a single actor event loop owns the [`Shard`] and drives
//! subscribe/unsubscribe/commands + watcher-driven fan-out. The actor depends
//! only on a narrow outbound port ([`FsWirePush`]) and never on a concrete WS
//! transport, so the composition layer supplies the socket adapter.
//!
//! [`Shard`]: crate::runtime::Shard
//!
//! Module files hold implementation; this file only declares and re-exports.

// Wire types are consumed only within this crate (dispatch/actor); the app layer
// speaks the transport envelope, not these payload types. Keep them crate-local.
pub(crate) mod wire;

mod actor;
mod dispatch;
mod port;
mod search;

pub use actor::FsMonitorActor;
pub use port::{FsInbound, FsWirePush, SessionId};
