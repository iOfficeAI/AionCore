//! The backend seam (§C 6.4, frozen): `AgentIo` (narrow process-I/O) +
//! `BackendAdapter` (the "how to add other agents" entry: 3 responsibilities).
//!
//! Isolation (I9): a `BackendAdapter` can ONLY produce canonical `SessionEvent`
//! — its method signatures reference NO `SessionState`/reducer/oracle. This is
//! stronger than a base class: a backend physically cannot corrupt the FSM.
//! Adding a backend = a new impl; reducer/FSM/oracle change 0 lines.

use std::sync::Arc;

use aionui_process::{BoxedStdin, BoxedStdout, ManagedProcess, ProcessError, Spawner};

use crate::capability::Capabilities;
use crate::event::{ExitStatusLite, SessionEvent};

mod claude;
pub(crate) use claude::claude_permission_modes;
pub use claude::{ClaudeAdapter, is_valid_claude_permission_mode};

/// How a persistent session process should be started (feature 004 R16/D12/S17).
/// Backend-agnostic: the adapter translates it into backend flags (claude:
/// `--session-id <uuid>` vs `--resume <id>`). Live-probed (claude 2.1.168):
/// reusing `--session-id` for an existing session id hard-errors
/// (`already in use`); continuation is ONLY via `--resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSpec {
    /// Start a brand-new session with this freshly-minted id (first turn, or
    /// no persisted id). claude: `--session-id <uuid>`.
    Fresh(String),
    /// Resume an existing session by its persisted id (crash / idle-reap
    /// respawn). claude: `--resume <id>`. On a missing/corrupt id the process
    /// 0-frame-exits (`No conversation found`); the manager detects that and
    /// falls back to `Fresh` (R16/S17).
    Resume(String),
}

/// Narrow process-I/O seam (D2). Exposes only the two methods P0 uses: the
/// byte-duplex handoff and the exit watch. Both async (mirrors real 001
/// `process.rs`). A thin impl wraps `Arc<ManagedProcess>`; an in-crate
/// duplex-backed fake replays fixtures. kill/interrupt are P1 (R12), out of
/// this trait.
#[async_trait::async_trait]
pub trait AgentIo: Send + Sync {
    /// Take the byte-duplex once (mirrors 001 `take_stdio`: second call returns
    /// `None`). Fits D6 one-shot (one spawn / one take per turn).
    async fn take_stdio(&self) -> Option<(BoxedStdin, BoxedStdout)>;

    /// Watch for process exit (the R7 listen point). `Some(status)` = exited
    /// with a known status; `None` = exited with unknown status (wait errored /
    /// process gone) → MUST be treated as a terminal exit (D3), NOT "still
    /// running". Resolving at all means it has exited (001 semantics).
    async fn wait_for_exit(&self) -> Option<ExitStatusLite>;

    /// Peek the last few lines of buffered stderr for diagnostics (S19): on a
    /// 0-frame / startup crash the real cause ("unknown option …", "Invalid MCP
    /// configuration", "No conversation found") is only on stderr. Default
    /// returns empty so fakes / non-process backends need not implement it; the
    /// real `ManagedProcessIo` delegates to 001 `peek_stderr_tail`.
    async fn peek_stderr(&self, _max_lines: usize) -> String {
        String::new()
    }
}

/// How many trailing stderr lines G2 peeks when a backend process exits. The
/// real cause line (usage/rate limit, auth, network) is almost always among the
/// last few; a small window keeps the redaction cheap and bounds exposure.
pub(crate) const STDERR_PEEK_LINES: usize = 32;

/// G2: capture a redacted, allowlisted one-line summary of a backend process's
/// stderr tail for the terminal `Detached` event. Shared by every backend's
/// reader task (and the `run_turn` core) so redaction happens in exactly one
/// place — raw/untrusted stderr never crosses the backend boundary. Returns
/// `None` when nothing matches the `aionui_common::error_extract` allowlist
/// (caller keeps a bare exit description). Call only on a REAL exit/EOF (where
/// stderr has had a chance to fill); the startup double-take guards pass `None`.
pub(crate) async fn redact_exit_stderr(io: &dyn AgentIo) -> Option<String> {
    let tail = io.peek_stderr(STDERR_PEEK_LINES).await;
    aionui_common::error_extract::extract_error_message(&tail)
}

/// Thin `AgentIo` over an `Arc<ManagedProcess>` (the real 001 handle). Converts
/// the std `ExitStatus` into the contract's `ExitStatusLite`.
pub struct ManagedProcessIo {
    proc: Arc<ManagedProcess>,
}

impl ManagedProcessIo {
    pub fn new(proc: Arc<ManagedProcess>) -> Self {
        Self { proc }
    }
}

#[async_trait::async_trait]
impl AgentIo for ManagedProcessIo {
    async fn take_stdio(&self) -> Option<(BoxedStdin, BoxedStdout)> {
        self.proc.take_stdio().await
    }

    async fn wait_for_exit(&self) -> Option<ExitStatusLite> {
        self.proc.wait_for_exit().await.map(exit_status_to_lite)
    }

    async fn peek_stderr(&self, max_lines: usize) -> String {
        self.proc.peek_stderr_tail(max_lines).await
    }
}

/// Convert a std `ExitStatus` into the trimmed contract type.
fn exit_status_to_lite(status: std::process::ExitStatus) -> ExitStatusLite {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;

    ExitStatusLite {
        code: status.code(),
        signal,
    }
}

/// The backend protocol seam (D1/D10) — the ONLY entry for "how to add other
/// agents". Thin polymorphic adapter; the reducer/FSM are monomorphic and NOT
/// in this trait. P0's only impl = `ClaudeAdapter`; Codex/Gemini/OpenAI have no
/// real fixtures (Non-Goal). Instance lifecycle = one per turn (D6): the
/// parse half-line buffer MUST be empty at `start_turn` (no cross-turn leak).
///
/// Forward-pointer (trait-widening, pre-second-backend, compendium §5.2): P0 is
/// 3 responsibilities — correct + frozen for claude `--print` stdio (a pure
/// forward pipe, no inbound reverse-RPC). ACP-dialect backends widen the
/// written contract to ~5 (serve fs/terminal/elicitation reverse-RPC + session
/// lifecycle verbs); those never reach the reducer. See RFC §8.
#[async_trait::async_trait]
pub trait BackendAdapter: Send + Sync {
    /// Responsibility (1) STARTUP: build the `CommandSpec`, spawn via the
    /// INJECTED `Spawner`. MUST spawn via the injected spawner (S14, MUST NOT
    /// raw-spawn / hardcode RealSpawner|bare). Returns the `AgentIo` for this
    /// session; the impl type is not named in the public contract.
    ///
    /// Feature 004 (R16/D12/S17): persistent multi-turn. `start_turn` only
    /// SPAWNS the persistent process (per `SessionSpec`: fresh vs resume); the
    /// prompt is NOT a spawn argument. Each turn's user message is delivered via
    /// `deliver_prompt` over the retained stdin — including the FIRST turn. The
    /// `system`-prompt / `mcp-config` injection (S18) is the manager's concern,
    /// passed in `extra_args` (kept backend-neutral here).
    ///
    /// `cwd` is the conversation workspace — it MUST be set so the process runs
    /// (and its file tools operate) in the conversation's directory, AND because
    /// claude keys its on-disk session by cwd: a `--resume` only succeeds when
    /// the process runs with the SAME cwd the session was created under
    /// (verified: a cwd mismatch yields "No conversation found"). `None` =
    /// inherit the parent cwd (only for tests / cwd-agnostic backends).
    /// `env` (#103): extra environment variables injected into the spawned
    /// process's `CommandSpec.env` (on top of the inherited parent env). Empty =
    /// inherit parent env only. The claude direct-CLI path uses this to carry the
    /// cc-switch provider env; the adapter only forwards it, never sources it.
    /// `cli_program`: the orchestration-resolved bundled-CLI absolute path.
    /// `None` ⇒ the adapter uses its historical bare command name resolved via
    /// PATH (byte-identical to pre-bundling). Forwarded only — never sourced here.
    async fn start_turn(
        &self,
        spawner: &dyn Spawner,
        session: &SessionSpec,
        cwd: Option<&str>,
        extra_args: &[String],
        env: &[aionui_common::EnvVar],
        cli_program: Option<&std::path::Path>,
    ) -> Result<Box<dyn AgentIo>, ProcessError>;

    /// Responsibility (1b) PROMPT DELIVERY: write ONE user turn to the retained
    /// stdin as a single NDJSON line + flush (NOT close — the process stays
    /// alive for the next turn). Feature 004 S16: the line MUST be built with
    /// `serde_json` over a typed value (NEVER `format!`/interpolation) so any
    /// `\n`/quote/unicode in any text is escaped into exactly one frame.
    ///
    /// `content` is the canonical multimodal block slice; each adapter maps it to
    /// its own wire. The sole claude impl maps `Text → {type:text}`, `Image →
    /// the native base64 image block` (so the model sees it), and `ResourceLink →
    /// a `[Attached file: <uri>]` text element` (so claude's Read tool fetches it
    /// from cwd — headless claude has no working document/resource input block).
    /// Blocks a backend does not advertise (`Capabilities::prompt_blocks`) are
    /// rejected at dispatch BEFORE this is called, so unreachable variants are a
    /// defensive no-op here.
    ///
    /// `client_msg_id` is our per-message correlation id (the conversation layer's
    /// `client_msg_id`). When `Some`, an adapter MAY stamp it on the wire frame so
    /// the backend echoes it back for attribution — the claude impl writes it as the
    /// user frame's `uuid`, which claude replays verbatim under `--replay-user-messages`
    /// (see protocols/design/claude-midturn-input-turn-gen-design.md §3.3). Adapters
    /// that have no such echo channel ignore it. `None` ⇒ no correlation requested.
    async fn deliver_prompt(
        &self,
        stdin: &mut BoxedStdin,
        content: &[crate::ContentBlock],
        client_msg_id: Option<&str>,
    ) -> Result<(), ProcessError>;

    /// Responsibility (1c) CONTROL RESPONSE (feature 004 F3): write ONE
    /// already-serialized `control_response` JSON value to the retained stdin as
    /// a single NDJSON line + flush (NOT close). This answers a `control_request`
    /// (e.g. `can_use_tool` / AskUserQuestion) the agent emitted and is BLOCKING
    /// on. The reducer/FSM never sees this; it is a pure transport write, kept on
    /// the adapter so the stdin framing (serde, single `\n`, flush-not-close)
    /// lives in one place. `value` is the full `{"type":"control_response",...}`
    /// object the caller built per the backend's wire contract.
    async fn write_control_response(
        &self,
        stdin: &mut BoxedStdin,
        value: &serde_json::Value,
    ) -> Result<(), ProcessError>;

    /// Responsibility (2) FRAMING + PARSE: raw bytes → canonical `SessionEvent`
    /// sequence. `&mut self`: maintains a half-line buffer across calls
    /// (stateful, S3). The buffer persists ACROSS turns/result spans (the
    /// persistent process's stdout does not EOF between turns) — it is NOT
    /// reset per turn. Unknown frames → `AdapterSpecific` catch-all, never
    /// panics (S3/I4). claude tokens are normalized here (I8).
    ///
    /// DUP-10: the sole trait-object caller was the deleted `session::run_turn`.
    /// `ClaudeConnection` now drives the CONCRETE `ClaudeAdapter::parse_chunk`
    /// (inherent dispatch) + parity tests still exercise it, so the trait method
    /// is dead only as a vtable entry. Kept on the trait pending the
    /// `BackendAdapter` teardown (F7); `#[allow]` so `-D warnings` stays green.
    #[allow(dead_code)]
    fn parse_chunk(&mut self, bytes: &[u8]) -> Vec<SessionEvent>;

    /// Responsibility (3) CAPABILITY: tier + the canonical signals it can emit
    /// (RFC §8 min-capability degradation). Const / pure. Read only by the
    /// adapter, NEVER by the pure reducer `step()` (I12).
    fn capabilities(&self) -> Capabilities;
}
