//! Shared-process opencode backend (one `opencode serve` for ALL opencode
//! conversations in this AionUi instance).
//!
//! * [`pool`] — lifecycle of the single shared `opencode serve` (discover →
//!   adopt → spawn; refcount; never kills a foreign server).
//! * [`client`] — typed V2 HTTP/SSE client (endpoints + payloads verified
//!   live against opencode 1.18.30 `/doc`).
//! * [`translate`] — durable/SSE event → `SessionEvent` mapping.
//! * [`backend`] — `BackendConnection`/`SessionBackend` implementation the
//!     factory wires into `AgentInstance::Session`.
//!
//! Enabled via the environment: `AIONUI_OPENCODE_SHARED_SERVER=1` (opt-in;
//! unset/`0` keeps the battle-tested per-conversation `opencode acp` ACP path
//! untouched, including its per-conversation `AIONUI_*` identity env).

mod backend;
mod client;
mod pool;
mod translate;

pub use backend::{OpencodeConnection, OpencodeSessionBackend, opencode_capabilities};
pub use client::OpencodeClient;
pub use pool::{ServerLease, global_pool};

use serde_json::Value;

/// Toggle for shared-server mode. Opt-in: `AIONUI_OPENCODE_SHARED_SERVER`
/// set to `1`/`true`/`on`/`yes` (case-insensitive). Anything else keeps the
/// legacy ACP spawn path.
pub fn shared_server_enabled() -> bool {
    matches!(
        std::env::var("AIONUI_OPENCODE_SHARED_SERVER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Error surface for the typed client; stringified into `BackendError`s by
/// the backend.
#[derive(Debug)]
pub enum HttpError {
    /// Connect/timeout/bad-json style failures.
    Transport(String),
    /// The endpoint answered 404 (e.g. session anchor gone after storage
    /// wipe) — callers map this to typed outcomes.
    NotFound(String),
    /// Non-2xx with a JSON body (opencode errors are `{name,message,…}`).
    Api { status: u16, ctx: String, body: Value },
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Transport(m) => write!(f, "transport: {m}"),
            HttpError::NotFound(c) => write!(f, "404: {c}"),
            HttpError::Api { status, ctx, body } => {
                let brief = body
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| body.to_string());
                write!(f, "{status} {ctx}: {brief}")
            }
        }
    }
}

impl std::error::Error for HttpError {}
