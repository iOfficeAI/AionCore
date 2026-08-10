//! 007 §C1 the symmetric actor-mailbox backend seam (a-side). Two traits:
//! `BackendConnection` (connection-level: opens per-session handles) and
//! `SessionBackend` (per-session handle: `&self` dispatch + event stream +
//! capabilities). All transport is sealed inside impls; the only thing that
//! crosses the seam is `Command` down and `SessionEnvelope` up.
//!
//! This is the NEW seam (strangler, §8). The legacy `adapter::BackendAdapter`
//! still drives `session::run_turn` until codex (P1) forces the switch; both
//! compile in parallel.

mod acp_conn;
mod antigravity;
mod claude_conn;
mod cli_version;
mod codex_conn;
mod conversation_session;
mod orchestrator;
mod rehydrate;
mod suspend;
mod types;

pub use acp_conn::{AcpConnection, AcpSessionBackend, acp_capabilities};
pub use antigravity::{AntigravityConnection, AntigravitySessionBackend, antigravity_capabilities};
pub use claude_conn::{ClaudeConnection, ClaudeSessionBackend};
pub use cli_version::{
    VERIFIED_CLAUDE_VERSION, VERIFIED_CODEX_VERSION, VersionDrift, VersionVerdict, classify as classify_cli_version,
    parse_version as parse_cli_version, version_drift,
};
pub use codex_conn::{CodexConnection, CodexSessionBackend, codex_capabilities, slash_command_name};
pub use conversation_session::{ConversationSession, MsgStatus, PendingMessage};
pub use orchestrator::Orchestrator;
pub use rehydrate::rehydrate;
// `suspend::{SuspendController, ProcHandle, spawn_idle_timer}` is the F-4 idle
// core; it is consumed intra-crate by the claude/codex/acp backend impls (each
// embeds a `SuspendController`), so it is not re-exported from the crate root.
pub use types::{
    Admission, BackendError, CancelTarget, Command, CommandMeta, CommandReceipt, ContentBlock, PendingPermissionView,
    PermissionDecision, QuestionAnswer, SessionEnvelope, SessionInfoKind, SessionSpec, StateSnapshot, Tier2Checkpoint,
    TransitionReason, command_name,
};

use crate::capability::Capabilities;
use futures_util::stream::BoxStream;

/// Shared startup-handshake budget for the bound-thread / bound-session waits
/// (bug-hunt codex-500 + regression-by-rewrite audit). The clean-slate rewrite of
/// the codex/acp handshake wait collapsed the legacy ACP `INIT_TIMEOUT_SECS = 30`
/// (aionui-agent-rest/src/protocol/acp.rs:55, an `oneshot` + `tokio::time::timeout`)
/// into a hardcoded `for _ in 0..40 { sleep(50ms) }` = 2s busy-poll on BOTH backends
/// — a magic constant with no reference to the legacy value. 2s passes on a warm dev
/// box but a cold start / untrusted project slows agent init past it → timeout → 500.
/// Restore the legacy 30s budget (env-overridable via `AIONUI_HANDSHAKE_TIMEOUT_SECS`
/// for genuinely slow environments). Returned as a poll count at the existing 50ms
/// tick so callers keep their loop shape; on exhaustion they return the RETRYABLE
/// `BackendError::HandshakeTimeout` (not a bare Transport→500).
pub(crate) fn handshake_budget() -> std::time::Duration {
    let secs = std::env::var("AIONUI_HANDSHAKE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30); // legacy INIT_TIMEOUT_SECS parity
    std::time::Duration::from_secs(secs)
}

/// Per-session handle (§C1). `&self` (NOT `&mut`) so a connection's many sessions
/// run concurrently — interior mutability is session-scoped behind the impl's
/// own locks/channels; the only shared serialized resource is the transport's
/// frame writer (a microsecond byte-frame lock, not a per-turn lock).
#[async_trait::async_trait]
pub trait SessionBackend: Send + Sync {
    /// Dispatch a command. FAST: returns a receipt the instant the adapter has
    /// accepted+queued it on the wire — does NOT block on turn completion (the
    /// turn flows up `events()`). Capability-gated: an unsupported command
    /// returns `BackendError::CommandNotSupported`.
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError>;

    /// The upward event stream, wrapped in `SessionEnvelope{session_id, turn_gen,
    /// event}`. A process crash flows OUT as `Detached{exit}` (no `wait_for_exit`
    /// on the trait). The orchestrator unwraps `.event` before `step()`.
    fn events(&self) -> BoxStream<'static, SessionEnvelope>;

    /// Read-once capability snapshot (gates UI affordances + dispatch). Immutable
    /// within a session; reconnect mints a NEW handle with a fresh snapshot.
    fn capabilities(&self) -> Capabilities;

    /// Read-only snapshot of currently-open (unanswered) permission requests, for
    /// REST recovery (`GET /confirmations`) of a reloaded `waiting_confirmation`
    /// conversation — the live process is still alive, but the permission-card
    /// detail (transient, never persisted) was lost on the frontend reload. Reads
    /// the adapter's transient pending registry; NOT reducer/FSM/persisted.
    ///
    /// Default empty: only backends with no unanswered-permission registry and every
    /// test double need no change. claude and codex both override it (claude from its
    /// `can_use_tool` pending map; codex from its `*/requestApproval` + elicitation
    /// registry). NOTE: the ACP `SessionBackend` (`acp_conn`) DOES keep such a
    /// registry (`pending_perm_options`, populated on `session/request_permission`)
    /// but does not yet override this — a latent recovery gap for the day the ACP
    /// SessionBackend is routed into `AgentInstance::Session` (today only claude/codex
    /// are; opencode/gemini/hermes still use the legacy `AgentInstance::Acp` path,
    /// which recovers via its own `permission_router`).
    fn pending_permission_requests(&self) -> Vec<PendingPermissionView> {
        Vec::new()
    }

    /// Force-terminate this session's process tree NOW (the `UserCancelTimeout`
    /// force-kill path), WITHOUT waiting for the last `Arc<dyn SessionBackend>`
    /// to drop. The Drop-driven reap needs the last handle released to fire
    /// `kill_on_drop`; during an in-flight turn the orchestrator legitimately
    /// holds a clone, so Drop never fires and the process outlives the kill.
    /// This gives the force-kill path a synchronous teardown that does not
    /// depend on the Arc being last. Default no-op: backends whose teardown is
    /// purely Drop-driven (and every test double) are unchanged — only the
    /// direct-CLI claude/codex backends override this.
    async fn terminate(&self) {}

    /// Raise a permission request that originated OUTSIDE this backend's own
    /// wire, and block until the user answers it.
    ///
    /// Only Antigravity needs this. agy cannot prompt for tool permission in
    /// headless mode, so AionUi registers itself as agy's `PreToolUse` hook;
    /// the request therefore arrives over HTTP from a separate hook process
    /// rather than up the backend's stream. Every other backend raises
    /// permissions on its own wire and never calls this.
    ///
    /// Default is `Denied`, deliberately: a backend that does not implement
    /// externally-raised permissions must not silently let the tool run.
    async fn request_external_permission(&self, _tool_name: String, _input: serde_json::Value) -> PermissionDecision {
        PermissionDecision::Denied
    }
}

/// Connection-level factory (§C1): holds the transport singleton and mints many
/// per-session `SessionBackend` actors (one connection multiplexes many sessions).
#[async_trait::async_trait]
pub trait BackendConnection: Send + Sync {
    /// Open (Fresh) or re-attach (Resume) a session. On `Resume`, the adapter
    /// uses `backend_session_id` to re-attach transport (or mints a fresh backend
    /// session if None/dead), then re-registers the SAME logical `session_id`.
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<std::sync::Arc<dyn SessionBackend>, BackendError>;

    /// Close + release a session's transport binding.
    async fn close_session(&self, session_id: &str) -> Result<(), BackendError>;

    /// Connection-level capabilities (the same shape a freshly-opened session
    /// reports; lets the caller gate UI before opening).
    fn capabilities(&self) -> Capabilities;
}

/// A neutral, SDK-free MCP server spec (Wave 0c). Carries one resolved MCP server
/// into `open_session` so each backend serializes it into ITS OWN wire shape (acp
/// → `session/new`|`session/load` `mcpServers[]`; codex → `thread/start`; aionrs →
/// its `McpServerConfig` map). Deliberately crate-local — `aionui-session` stays
/// self-contained (no `aionui-api-types`/ACP-SDK dep, §Cargo.toml). The app
/// boundary (which owns the catalog + async command resolution) converts its own
/// `SessionMcpServer` into this AFTER resolving stdio launch commands, so the spec
/// that arrives here is final (a backend never re-resolves a launch command).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransport,
}

/// MCP transport variants (SDK-free mirror of the neutral session-snapshot shape).
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    /// stdio: a resolved launch command + args + env (already runtime-resolved by
    /// the app boundary — NOT re-resolved here).
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// HTTP (or streamable-HTTP) transport: url + headers.
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    /// SSE transport: url + headers.
    Sse {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// The session-INITIALIZATION surface (Wave 0c) the legacy ai-agent factory
/// provided, carried neutrally so each backend's `open_session` serializes it into
/// its own wire shape. `Default` = empty = byte-identical to the pre-0c handshake,
/// so adding this to `SessionConfig` is a zero-behavior-change additive widening
/// until a caller populates it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInit {
    /// User/team/guide MCP servers, already resolved + flattened by the app
    /// boundary. Empty = no injection (the pre-0c default).
    pub mcp_servers: Vec<McpServerSpec>,
    /// Skill ids/names to surface to the agent on the first turn (acp/aionrs
    /// deliver these via the first prompt, not a `session/new` param).
    ///
    /// NB: claude/codex direct-CLI backends intentionally do NOT consume this —
    /// skills reach them as workspace symlinks (conversation service links the
    /// resolved skills into the agent's `native_skills_dirs`, e.g.
    /// `.claude/skills` / `.codex/skills`) and both CLIs discover them natively
    /// (codex LIVE-verified 0.144.1: `skills/list` returns a `<cwd>/.codex/skills`
    /// entry as scope=repo; see samples/codex-cli/0.144.1/_probe_workspace_skills.py).
    /// The field is carried for the aionrs/acp first-prompt delivery path only.
    pub skills: Vec<String>,
    /// Composed system prompt / preset context (the `compose_preset_context`
    /// output + aionrs `preset_rules`-merged prompt). Delivered first-message.
    pub preset_context: Option<String>,
    /// Opaque persisted session snapshot for resume (aionrs `SessionManager` JSON
    /// blob). Kept as `serde_json::Value` so `aionui-session` need not depend on
    /// the `aion-agent` `Session` type; the aionrs backend deserializes it.
    pub session_snapshot: Option<serde_json::Value>,
    /// Explicit resume flag. acp/codex resume via `SessionSpec::Resume`; this is the
    /// aionrs equivalent + the single source of truth for "sanitize on resume".
    pub resume: bool,
}

/// Orchestration-side (NOT b-side) session config carried into `open_session`.
/// Holds the runtime knobs the adapter needs but the reducer never sees.
///
/// `Eq` is intentionally NOT derived: `SessionInit.session_snapshot` is a
/// `serde_json::Value` (which is `PartialEq` but not `Eq`). No consumer needs
/// `SessionConfig: Eq` (verified — no `HashSet`/`BTreeSet`/`==` use).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionConfig {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub extra_args: Vec<String>,
    /// F-4 idle self-suspend TTL (ms). `None` (default) = never suspend: the
    /// backend keeps its process resident for life, byte-identical to the pre-F-4
    /// path. `Some(ttl)` = a backend that supports graceful resume (claude
    /// `--resume`, codex `thread/resume`, acp `session/load`) closes its process
    /// after `ttl` ms idle and re-spawns it (resuming the same backend session) on
    /// the next dispatch. Opt-in so the claude parse zero-diff acceptance is
    /// unaffected unless explicitly enabled. See `backend/suspend.rs`.
    pub idle_ttl_ms: Option<i64>,
    /// Wave 0c: the session-initialization surface (MCP / skills / preset / snapshot
    /// / resume). `Default` (empty) = pre-0c handshake byte-identical, so existing
    /// callers that `..Default::default()` are unaffected until they populate it.
    pub init: SessionInit,
    /// #103: extra environment variables injected into the spawned backend
    /// process's `CommandSpec.env`. The orchestration layer (app registry) fills
    /// this — e.g. the claude direct-CLI path injects the cc-switch provider env
    /// (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`) here for backend == "claude",
    /// mirroring the legacy ACP path that read it from cc-switch at spawn. The
    /// session/backend layer stays cc-switch-agnostic: it only forwards this
    /// neutral data into the spawn. `Default` (empty) = inherit the parent env
    /// only (byte-identical to the pre-#103 spawn). Carried into the F-4 wake
    /// recipe so a resume-respawn re-applies the same env (R16 continuity).
    pub spawn_env: Vec<aionui_common::EnvVar>,
    /// Antigravity only: the `.agents/hooks.json` body that registers AionUi as
    /// agy's approval gate.
    ///
    /// Carried rather than written unconditionally because a session that starts
    /// in full auto installs no hook — and the backend cannot rebuild this body
    /// itself (it knows the workspace, not where AionUi's binary lives). Holding
    /// it lets a later switch OUT of full auto restore the gate; without it that
    /// switch would silently leave the session running unattended.
    pub permission_hook_body: Option<String>,
    /// G1-A: the codex sandbox policy injected into `thread/start` (the native
    /// codex app-server `sandbox` field; codex_conn serializes it data-driven
    /// instead of the hardcoded `"workspace-write"`). `None` (default) ⇒
    /// `workspace-write`, byte-identical to the pre-G1-A handshake. The
    /// orchestration layer (app registry) resolves the agent's requested mode
    /// (e.g. a yolo agent → `"danger-full-access"`) and fills this — mirroring the
    /// #103 spawn_env seam: the session/backend layer stays policy-agnostic and
    /// only forwards the neutral string. Backends other than codex ignore it.
    /// Carried into the codex F-4 wake recipe so a resume re-applies the same
    /// sandbox (R16 continuity).
    pub sandbox_mode: Option<String>,
    /// The codex approval policy injected into `thread/start` (the native codex
    /// app-server `approvalPolicy` field; codex_conn serializes it data-driven
    /// instead of the hardcoded `"on-request"`). `None` (default) ⇒ `"on-request"`,
    /// byte-identical to the pre-data-driven handshake. The orchestration layer
    /// (app registry) resolves the agent's requested mode (e.g. a yolo / full-access
    /// agent → `"never"`, never prompt) and fills this — the exact sibling of
    /// `sandbox_mode`: the session/backend layer stays policy-agnostic and only
    /// forwards the neutral string. Valid codex values: `untrusted` / `on-failure`
    /// / `on-request` / `never`. Carried into the codex F-4 wake recipe (it rides
    /// `CodexWakeRecipe.config`) so a resume re-applies the same policy (R16
    /// continuity). Backends other than codex ignore it.
    pub approval_policy: Option<String>,
    /// The orchestration layer's resolved bundled-CLI absolute path. `None` ⇒ the
    /// backend uses its historical bare command name ("claude"/"codex", resolved
    /// via PATH), byte-identical to pre-bundling. Filled by the app registry
    /// (session_agent) via `managed_cli::resolve_bundled_cli`; the session/backend
    /// layer stays bundling-agnostic and only forwards it into the spawn. Carried
    /// into the F-4 wake recipe (rides the cloned `config`) so a resume re-spawn
    /// uses the same binary (R16 continuity).
    pub cli_program: Option<std::path::PathBuf>,
    /// The conversation's cumulative cost (USD) BEFORE this backend instance
    /// existed, read back from the persisted usage snapshot by the orchestration
    /// layer on resume. claude only: its `result.total_cost_usd` is
    /// PROCESS-cumulative (live-captured 2.1.221 — a `--resume` respawn restarts
    /// the counter at zero), so the backend seeds its cost ledger with this base
    /// and reports `base + raw`, keeping the broadcast/persisted cost
    /// SESSION-cumulative across process recycles and app restarts. `None`
    /// (default) = fresh conversation, base 0. Other backends ignore it.
    pub initial_cost_usd: Option<f64>,
}
