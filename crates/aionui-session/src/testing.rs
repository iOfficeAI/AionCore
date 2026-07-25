//! In-crate test doubles (D2/D8), gated behind `test-support` / `cfg(test)`.
//! NEVER fabricates a `ManagedProcess` (its fields are private, only ctor is the
//! real `spawn`). Instead:
//! - `FakeAgentIo` implements the narrow `AgentIo` seam over `tokio::io::duplex`,
//!   replaying scripted fixture bytes + a scripted exit.
//! - `FakeSpawner` implements the real 001 `Spawner` trait, RECORDS that it was
//!   called, and returns `Err` (it cannot synthesize an `Arc<ManagedProcess>`
//!   without a real OS process — see the seam note). T14 uses the call-count to
//!   kill the "start_turn bypasses the injected spawner" mutation hermetically.
//! - `ScriptedConnection` implements `BackendConnection` by building a
//!   `ClaudeSessionBackend` over a `FakeAgentIo` (NDJSON bytes pre-loaded) for
//!   every `open_session` call. Records the last `SessionSpec` for resume assertions.
//! - `StdoutGate` is a thin handle around the `FakeAgentIo`'s internal stdout-gate
//!   pair (`Arc<Notify>` + `Arc<AtomicBool>`), cloned out at construction time so an
//!   external caller (e.g. `ScriptedConnection`) can release the gated bytes AFTER
//!   the orchestrator has subscribed — avoiding the late-subscriber event-loss race.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aionui_common::CommandSpec;
use aionui_process::{BoxedStdin, BoxedStdout, ManagedProcess, ProcessError, Spawner};
use tokio::sync::Mutex;

use crate::adapter::{AgentIo, BackendAdapter};
use crate::backend::{
    BackendConnection, BackendError, ClaudeSessionBackend, SessionBackend, SessionConfig, SessionSpec,
};
use crate::capability::Capabilities;
use crate::event::ExitStatusLite;

/// A duplex-backed `AgentIo`: hands the read half (preloaded with scripted
/// bytes) to the transport as stdout, and resolves `wait_for_exit` to a
/// scripted status once the bytes are consumed.
pub struct FakeAgentIo {
    /// stdout bytes the "process" will emit this turn.
    scripted_stdout: std::sync::Mutex<Option<Vec<u8>>>,
    /// Optional SECOND chunk emitted only after the stdout gate opens (models a
    /// handshake-then-turn split: the prefix — e.g. `thread/started` — flows
    /// immediately so `dispatch` can bind the threadId, while the turn-driving
    /// notifications wait until a subscriber is wired). `None` = no gated tail.
    gated_tail: std::sync::Mutex<Option<Vec<u8>>>,
    /// Optional ORDERED gated SEGMENTS, each released one-at-a-time (`release_next`).
    /// Independent of `gated_tail` (the single-shot tail): segments model a turn
    /// that must pause MID-STREAM for a host action — e.g. a permission round-trip
    /// (segment 0 = the `control_request`; segment 1 = the continuation+result that
    /// flows only after the user answers) or a multi-turn fixture (one segment per
    /// turn). Empty by default → existing single-tail behavior is unchanged. The
    /// writer task acquires one `segment_gate` permit before emitting each segment.
    gated_segments: std::sync::Mutex<Option<Vec<Vec<u8>>>>,
    /// One permit per released segment. A `Semaphore` (not `Notify`) so a release
    /// that races ahead of the writer parking is NOT lost — permits accumulate, so
    /// staged release is timing-independent (no lost-wakeup flakiness).
    segment_gate: Arc<tokio::sync::Semaphore>,
    /// scripted exit; `None` models "exited, status unknown" (D3).
    exit: Option<ExitStatusLite>,
    /// flips false→true once `take_stdio` has been called (once-only contract).
    taken: AtomicUsize,
    /// gate that `wait_for_exit` awaits, so a test can control exit timing.
    exit_gate: Arc<tokio::sync::Notify>,
    exit_ready: Arc<std::sync::atomic::AtomicBool>,
    /// gate the SCRIPTED STDOUT emission, so a test can defer the bytes until its
    /// subscribers are wired (broadcast does not replay to late subscribers — the
    /// end-to-end orchestrator fold needs run() subscribed before the turn drives).
    /// Default = open (emit immediately, preserving every existing test); a test
    /// opts into gating via `gated_stdout()` and opens it with `release_stdout()`.
    stdout_gate: Arc<tokio::sync::Notify>,
    stdout_ready: Arc<std::sync::atomic::AtomicBool>,
    /// All bytes written to the stdin half, captured for assertions (the
    /// single-writer / NDJSON-framing checks, I-13). The drain task appends
    /// here instead of discarding.
    captured_stdin: Arc<tokio::sync::Mutex<Vec<u8>>>,
    /// Scripted stderr tail returned by `peek_stderr` (S19 crash-diagnostic
    /// tests). Empty by default.
    scripted_stderr: String,
    /// When true, `take_stdio` returns `None` — models a DEGENERATE spawn whose
    /// stdio could not be taken (so the backend's stdin slot stays `None` and the
    /// first `deliver_prompt` hits the "stdin unavailable" arm). Used to exercise
    /// the first-send readiness classification without a real broken pipe.
    no_stdio: bool,
}

impl FakeAgentIo {
    /// A turn that emits `bytes` then exits with `exit`.
    pub fn new(bytes: Vec<u8>, exit: Option<ExitStatusLite>) -> Self {
        Self {
            scripted_stdout: std::sync::Mutex::new(Some(bytes)),
            gated_tail: std::sync::Mutex::new(None),
            gated_segments: std::sync::Mutex::new(None),
            segment_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            exit,
            taken: AtomicUsize::new(0),
            exit_gate: Arc::new(tokio::sync::Notify::new()),
            exit_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            captured_stdin: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            scripted_stderr: String::new(),
            // Default: stdout flows immediately (every existing test relies on this).
            stdout_gate: Arc::new(tokio::sync::Notify::new()),
            stdout_ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            no_stdio: false,
        }
    }

    /// Model a DEGENERATE spawn: `take_stdio` returns `None`, so the backend's stdin
    /// slot stays `None` and the first `deliver_prompt` hits the "stdin unavailable"
    /// arm — used to exercise the first-send readiness classification.
    pub fn no_stdio() -> Self {
        let mut me = Self::new(Vec::new(), None);
        me.no_stdio = true;
        me
    }

    /// Set the scripted stderr tail that `peek_stderr` will return (models a
    /// startup-crash diagnostic, e.g. `error: unknown option '--foo'`).
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.scripted_stderr = stderr.into();
        self
    }

    /// A handle to the captured-stdin buffer (all bytes the manager wrote over
    /// stdin via `deliver_prompt`). Clone it BEFORE `take_stdio` to assert on
    /// prompt framing after sends settle.
    pub fn captured_stdin(&self) -> Arc<tokio::sync::Mutex<Vec<u8>>> {
        Arc::clone(&self.captured_stdin)
    }

    /// A turn whose process NEVER exits (for the never-returning negative
    /// control, T2) and emits the given bytes.
    pub fn never_exits(bytes: Vec<u8>) -> Self {
        let me = Self::new(bytes, None);
        // exit_ready stays false forever; wait_for_exit blocks on the gate.
        me
    }

    /// Signal that the scripted exit may now be observed by `wait_for_exit`.
    pub fn release_exit(&self) {
        self.exit_ready.store(true, Ordering::SeqCst);
        self.exit_gate.notify_waiters();
    }

    /// Split the scripted stdout into an immediate PREFIX and a gated TAIL: the
    /// prefix flows at `take_stdio` (so `dispatch` can bind the threadId from a
    /// `thread/started`), and the tail (the turn-driving notifications) waits until
    /// `release_stdout()` — used when a subscriber must be wired before the turn
    /// drives (broadcast does not replay to late subscribers). `new(prefix, exit)`
    /// supplies the prefix; this sets the gated tail.
    pub fn with_gated_tail(self, tail: Vec<u8>) -> Self {
        *self.gated_tail.lock().unwrap() = Some(tail);
        self.stdout_ready.store(false, Ordering::SeqCst);
        self
    }

    /// Open the stdout gate so the deferred TAIL bytes flow.
    pub fn release_stdout(&self) {
        self.stdout_ready.store(true, Ordering::SeqCst);
        self.stdout_gate.notify_waiters();
    }

    /// A release handle usable AFTER the fake is moved into a backend (which
    /// consumes the `Box<dyn AgentIo>`). Calling it opens the gated-tail gate.
    pub fn stdout_releaser(&self) -> impl Fn() + Send + Sync + 'static {
        let gate = Arc::clone(&self.stdout_gate);
        let ready = Arc::clone(&self.stdout_ready);
        move || {
            ready.store(true, Ordering::SeqCst);
            gate.notify_waiters();
        }
    }

    /// Split the scripted stdout into ORDERED gated SEGMENTS, each released
    /// one-at-a-time via [`release_next`](Self::release_next). Unlike the single
    /// `gated_tail`, segments model a stream that must pause MID-flight for a host
    /// action between chunks (a permission round-trip, or one segment per turn in a
    /// multi-turn fixture). The PREFIX (`new`'s bytes) still flows at `take_stdio`;
    /// then segment N flows only once `release_next` has been called N+1 times.
    /// Mutually exclusive with `with_gated_tail` (a fixture uses one mechanism or
    /// the other); the segments path leaves `gated_tail` None.
    pub fn with_gated_segments(self, segments: Vec<Vec<u8>>) -> Self {
        *self.gated_segments.lock().unwrap() = Some(segments);
        self.stdout_ready.store(false, Ordering::SeqCst);
        self
    }

    /// Release the NEXT gated segment (adds one permit). Call once per segment, in
    /// order. Extra calls past the last segment are harmless (surplus permits are
    /// never acquired — the writer has nothing more to emit).
    pub fn release_next(&self) {
        self.segment_gate.add_permits(1);
    }

    /// A per-segment release handle usable AFTER the fake is moved into a backend.
    /// Each call adds one permit (releases one segment).
    pub fn segment_releaser(&self) -> impl Fn() + Send + Sync + 'static {
        let gate = Arc::clone(&self.segment_gate);
        move || gate.add_permits(1)
    }
}

#[async_trait::async_trait]
impl AgentIo for FakeAgentIo {
    async fn take_stdio(&self) -> Option<(BoxedStdin, BoxedStdout)> {
        // Degenerate spawn: stdio could not be taken at all.
        if self.no_stdio {
            return None;
        }
        // once-only: second call returns None.
        if self.taken.fetch_add(1, Ordering::SeqCst) != 0 {
            return None;
        }
        let bytes = self.scripted_stdout.lock().unwrap().take().unwrap_or_default();
        let tail = self.gated_tail.lock().unwrap().take();
        let segments = self.gated_segments.lock().unwrap().take();
        // a duplex pair: we feed `bytes` into one end, hand the other as stdout.
        let seg_max = segments
            .as_ref()
            .map_or(0, |segs| segs.iter().map(Vec::len).max().unwrap_or(0));
        let cap = bytes.len().max(tail.as_ref().map_or(0, Vec::len)).max(seg_max).max(1);
        let (mut feed, read) = tokio::io::duplex(cap);
        let stdout_gate = Arc::clone(&self.stdout_gate);
        let stdout_ready = Arc::clone(&self.stdout_ready);
        let segment_gate = Arc::clone(&self.segment_gate);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            // PREFIX flows immediately (handshake — e.g. thread/started).
            let _ = feed.write_all(&bytes).await;
            let _ = feed.flush().await;
            // Optional gated TAIL (the turn): held until release_stdout opens the
            // gate (default with no tail: gate is already open, nothing to wait for).
            if let Some(tail) = tail {
                while !stdout_ready.load(Ordering::SeqCst) {
                    stdout_gate.notified().await;
                }
                let _ = feed.write_all(&tail).await;
            }
            // Optional ORDERED gated SEGMENTS: acquire one permit before each
            // segment, so segment N flows only after the N+1-th `release_next`.
            // A Semaphore (not Notify) makes this timing-independent — a release
            // that races ahead of the writer parking is preserved as a permit.
            if let Some(segments) = segments {
                for seg in segments {
                    // `forget` the permit so it is consumed (one permit ⇒ one segment).
                    if let Ok(permit) = segment_gate.acquire().await {
                        permit.forget();
                    }
                    let _ = feed.write_all(&seg).await;
                    let _ = feed.flush().await;
                }
            }
            let _ = feed.shutdown().await;
        });
        // stdin: a sink whose peer is continuously drained INTO the capture
        // buffer, so prompt writes (F1 `deliver_prompt`, including frames larger
        // than the duplex buffer) succeed instead of breaking the pipe, and the
        // I-13 single-writer / NDJSON-framing checks can assert on the bytes.
        let (sink, mut discard) = tokio::io::duplex(256);
        let captured = Arc::clone(&self.captured_stdin);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut scratch = [0u8; 256];
            loop {
                match discard.read(&mut scratch).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => captured.lock().await.extend_from_slice(&scratch[..n]),
                }
            }
        });
        Some((Box::new(sink) as BoxedStdin, Box::new(read) as BoxedStdout))
    }

    async fn wait_for_exit(&self) -> Option<ExitStatusLite> {
        // block until released (never, for never_exits); then yield scripted.
        while !self.exit_ready.load(Ordering::SeqCst) {
            self.exit_gate.notified().await;
        }
        self.exit
    }

    async fn peek_stderr(&self, _max_lines: usize) -> String {
        self.scripted_stderr.clone()
    }
}

/// A `Spawner` that records its call count and returns `Err`. Used by T14 to
/// prove `start_turn` routes through the INJECTED spawner: a bypassing impl
/// leaves the count at 0. Cannot return a real `ManagedProcess` (no public
/// ctor / no real process), which is fine — the seam test only needs the call.
#[derive(Default)]
pub struct FakeSpawner {
    calls: AtomicUsize,
    last_command: Mutex<Option<CommandSpec>>,
}

impl FakeSpawner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub async fn last_command(&self) -> Option<CommandSpec> {
        self.last_command.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl Spawner for FakeSpawner {
    async fn spawn(
        &self,
        spec: CommandSpec,
        _extra_env: &[(String, String)],
        _opaque_owner_tag: &str,
    ) -> Result<Arc<ManagedProcess>, ProcessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_command.lock().await = Some(spec);
        // Cannot synthesize an Arc<ManagedProcess> without a real process.
        Err(ProcessError::internal(
            "FakeSpawner: records the call, does not spawn a real process",
        ))
    }
}

// ─── ScriptedConnection ──────────────────────────────────────────────────────

/// A `BackendConnection` that builds a `ClaudeSessionBackend` over scripted NDJSON
/// bytes (a `FakeAgentIo`) for every `open_session` call. Designed for integration
/// tests that want a complete fake→registry→orchestrator→facade→WS pipeline without
/// spawning a real agent process.
///
/// - `script` — the NDJSON bytes the fake "process" will emit (e.g. a real claude
///   CLI stream-json fixture). The backend's reader task drains them as if they came
///   from a live process.
/// - `last_spec` — records the `SessionSpec` passed to the most recent
///   `open_session` call; lets a test assert resume semantics.
///
/// # Timing note
///
/// The script bytes are placed in the FakeAgentIo's GATED TAIL (not the immediate
/// prefix). This prevents the backend reader task from emitting events before the
/// orchestrator has subscribed to `backend.events()` — a late-subscriber event-loss
/// race that causes `streamDelta`/`blockFinal` to silently disappear. The test must
/// call [`ScriptedConnection::release_pending`] after `session.send(...)` (i.e.
/// after the orchestrator has a live receiver) to unblock the bytes.
///
/// # Usage
/// ```ignore
/// let conn = Arc::new(ScriptedConnection::from_bytes(FIXTURE));
/// // pass conn to TestBackends / ClaudeSessionRegistry...
/// // ... drive session.send(...)...
/// conn.release_pending();  // ← release gated bytes so reader can emit events
/// ```
pub struct ScriptedConnection {
    script: Vec<u8>,
    /// When set, `open_session` builds a SEGMENTED io (one gated segment per entry)
    /// instead of a single gated tail. Releases are staged via `release_next_segment`
    /// — used by the permission round-trip (control_request, then continuation) and
    /// the multi-turn fixture (one segment per turn). `None` = single-tail mode.
    segments: Option<Vec<Vec<u8>>>,
    /// The `SessionSpec` from the most recent `open_session` call. `None` before
    /// any session is opened.
    pub last_spec: Mutex<Option<SessionSpec>>,
    /// Releaser closures for every `FakeAgentIo` created by `open_session`. Each
    /// closure opens that io's gated-tail stdout gate. Accumulated so that
    /// `release_pending` drains all of them at once.
    pending_releasers: Mutex<Vec<Box<dyn Fn() + Send + Sync + 'static>>>,
    /// Per-segment releaser closures for every SEGMENTED `FakeAgentIo`. Each
    /// `release_next_segment` call invokes each one once (adds one permit), so every
    /// open session advances to its next gated segment.
    segment_releasers: Mutex<Vec<Box<dyn Fn() + Send + Sync + 'static>>>,
}

impl ScriptedConnection {
    /// Build a new `ScriptedConnection` with the given scripted NDJSON bytes.
    pub fn new(script: Vec<u8>) -> Self {
        Self {
            script,
            segments: None,
            last_spec: Mutex::new(None),
            pending_releasers: Mutex::new(Vec::new()),
            segment_releasers: Mutex::new(Vec::new()),
        }
    }

    /// Convenience: build from an `&[u8]` slice (e.g. `include_bytes!(...)`).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::new(bytes.to_vec())
    }

    /// Build a SEGMENTED connection: `open_session` gives the backend a FakeAgentIo
    /// whose stdout is split into ordered gated segments, each released one-at-a-time
    /// via [`release_next_segment`](Self::release_next_segment). Use for fixtures that
    /// must pause MID-stream for a host action — a permission round-trip (segment 0 =
    /// the `control_request`; segment 1 = the post-answer continuation + result) or a
    /// multi-turn fixture (one segment per turn, released after each `session.send`).
    pub fn from_segments(segments: Vec<Vec<u8>>) -> Self {
        Self {
            script: Vec::new(),
            segments: Some(segments),
            last_spec: Mutex::new(None),
            pending_releasers: Mutex::new(Vec::new()),
            segment_releasers: Mutex::new(Vec::new()),
        }
    }

    /// Release the gated stdout bytes for every session opened so far. Call this
    /// AFTER `session.send(...)` returns (i.e. after the orchestrator has subscribed
    /// to `backend.events()`) so the reader task's events land on active receivers.
    /// Safe to call more than once (extra calls are no-ops: the `AtomicBool` is
    /// already true and `notify_waiters` with no waiters is a no-op).
    pub async fn release_pending(&self) {
        let releasers = std::mem::take(&mut *self.pending_releasers.lock().await);
        for r in releasers {
            r();
        }
    }

    /// Release the NEXT gated segment for every segmented session opened so far
    /// (adds one permit to each). Call once per stage, in order, AFTER the dispatch
    /// that should precede that segment (e.g. after `session.send` for segment 0,
    /// after `session.answerPermission` for segment 1). Idempotent past the last
    /// segment (surplus permits are never consumed).
    pub async fn release_next_segment(&self) {
        for r in self.segment_releasers.lock().await.iter() {
            r();
        }
    }
}

#[async_trait::async_trait]
impl BackendConnection for ScriptedConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        _config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        *self.last_spec.lock().await = Some(spec.clone());
        let session_id = match &spec {
            SessionSpec::Fresh { session_id } => session_id.clone(),
            SessionSpec::Resume { session_id, .. } => session_id.clone(),
        };
        // Gate the NDJSON bytes so the reader does NOT emit events before the
        // orchestrator subscribes to `backend.events()` (a late-subscriber drop).
        // Single-tail mode (the default) releases everything at once via
        // `release_pending`; segmented mode releases one segment per stage via
        // `release_next_segment`.
        let io = match &self.segments {
            Some(segments) => {
                let io = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(segments.clone());
                self.segment_releasers
                    .lock()
                    .await
                    .push(Box::new(io.segment_releaser()));
                io
            }
            None => {
                let io = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(self.script.clone());
                self.pending_releasers.lock().await.push(Box::new(io.stdout_releaser()));
                io
            }
        };
        let backend = ClaudeSessionBackend::build_with_io(session_id, Box::new(io)).await;
        Ok(Arc::new(backend))
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        // Return the standard claude capabilities snapshot.
        crate::adapter::ClaudeAdapter::new().capabilities()
    }
}

/// Cross-backend capability/dispatch invariant ASSERTIONS — the single logic
/// source shared by the in-session invariant tests (claude/codex/acp) AND the
/// `aionui-aionrs` crate's own invariant test (aionrs depends on session, so it
/// cannot live in session's `tests/` and be reached from aionrs — it lives here,
/// callable by both). Plan B (no logic duplication across crates): change an
/// assertion once here and all 4 backends follow.
///
/// These are the verbatim bodies hoisted out of `tests/cap_behavior_invariant.rs`
/// and `tests/cap_emits_invariant.rs` — see those files' module docs for the
/// defect class each pins (GAP-A advertised-but-stub / Gap-1 silent-block-drop /
/// mis-drain pending queue / auth-triangle).
pub mod invariants {
    use crate::backend::{
        BackendError, CancelTarget, Command, CommandMeta, ContentBlock, PermissionDecision, SessionBackend,
    };
    use crate::capability::Capabilities;
    use crate::event::SessionEvent;
    use futures_util::StreamExt;

    type GatedCommand = (&'static str, Command, fn(&Capabilities) -> bool);

    fn gated_commands() -> Vec<GatedCommand> {
        vec![
            ("steer", Command::Steer { content: Vec::new() }, |c| {
                c.supported_commands.steer
            }),
            (
                "cancel_tool",
                Command::Cancel {
                    target: CancelTarget::Tool("t".into()),
                },
                |c| c.supported_commands.cancel_tool,
            ),
            (
                "answer_permission",
                Command::AnswerPermission {
                    request_id: "r".into(),
                    decision: PermissionDecision::Denied,
                    selected: None,
                    answers: Vec::new(),
                },
                |c| c.supported_commands.answer_permission,
            ),
            (
                "answer_auth",
                Command::AnswerAuth {
                    method_id: "m".into(),
                    credentials: serde_json::Value::Null,
                },
                |c| c.supported_commands.answer_auth,
            ),
            ("acknowledge", Command::Acknowledge { node_id: "n".into() }, |c| {
                c.supported_commands.acknowledge
            }),
            ("set_mode", Command::SetMode { mode: "plan".into() }, |c| {
                c.supported_commands.set_mode
            }),
            ("set_model", Command::SetModel { model: "m".into() }, |c| {
                c.supported_commands.set_model
            }),
            ("rewind", Command::Rewind { num_turns: 1 }, |c| {
                c.supported_commands.rewind
            }),
            ("list_checkpoints", Command::ListCheckpoints, |c| {
                c.supported_commands.list_checkpoints
            }),
        ]
    }

    /// false cap ⟺ dispatch rejects CommandNotSupported; true cap ⟹ NOT rejected
    /// as unsupported (may Err for a precondition — that is fulfilled-but-gated).
    pub async fn assert_cap_dispatch_consistent(backend: &dyn SessionBackend, label: &str) {
        let caps = backend.capabilities();
        for (name, cmd, advertised) in gated_commands() {
            let is_advertised = advertised(&caps);
            let res = backend.dispatch(cmd).await;
            let rejected = matches!(res, Err(BackendError::CommandNotSupported { command }) if command == name);
            if is_advertised {
                assert!(
                    !rejected,
                    "[{label}] supported_commands.{name}=true but dispatch returned \
                     CommandNotSupported{{{name}}} — declared-but-not-fulfilled (GAP-A class)"
                );
            } else {
                assert!(
                    rejected,
                    "[{label}] supported_commands.{name}=false but dispatch did NOT reject with \
                     CommandNotSupported{{{name}}} (got {res:?}) — accepts an un-advertised command"
                );
            }
        }
    }

    type GatedBlock = (&'static str, ContentBlock, fn(&Capabilities) -> bool);

    fn gated_blocks() -> Vec<GatedBlock> {
        vec![
            ("content_block:text", ContentBlock::Text("hi".into()), |c| {
                c.prompt_blocks.text
            }),
            (
                "content_block:image",
                ContentBlock::Image {
                    data: vec![0u8, 1, 2],
                    media_type: "image/png".into(),
                },
                |c| c.prompt_blocks.image,
            ),
            (
                "content_block:audio",
                ContentBlock::Audio {
                    data: vec![0u8, 1, 2],
                    media_type: "audio/wav".into(),
                },
                |c| c.prompt_blocks.audio,
            ),
            (
                "content_block:resource",
                ContentBlock::ResourceLink {
                    uri: "file:///x".into(),
                    mime_type: None,
                },
                |c| c.prompt_blocks.resource,
            ),
            (
                "content_block:at_mention",
                ContentBlock::AtMention { user_id: "u1".into() },
                |c| c.prompt_blocks.at_mention,
            ),
        ]
    }

    /// un-advertised block ⟹ Send rejects CommandNotSupported{content_block:<kind>}
    /// (never silently dropped); advertised block ⟹ NOT rejected as unsupported.
    pub async fn assert_block_dispatch_consistent(backend: &dyn SessionBackend, label: &str) {
        let caps = backend.capabilities();
        for (name, block, advertised) in gated_blocks() {
            let is_advertised = advertised(&caps);
            let res = backend
                .dispatch(Command::Send {
                    content: vec![block],
                    metadata: Default::default(),
                })
                .await;
            let rejected = matches!(res, Err(BackendError::CommandNotSupported { command }) if command == name);
            if is_advertised {
                assert!(
                    !rejected,
                    "[{label}] prompt_blocks {name}=true but Send rejected it CommandNotSupported \
                     — gated a block it claims to accept"
                );
            } else {
                assert!(
                    rejected,
                    "[{label}] prompt_blocks {name}=false but Send did NOT reject (got {res:?}) \
                     — silently accepted/dropped an un-advertised block (Gap-1)"
                );
            }
        }
    }

    /// Send WITHOUT a client_msg_id must emit NO PromptAccepted (a spurious one
    /// would mis-drain the conversation pending queue). Uniform across backends.
    pub async fn assert_send_without_id_emits_no_prompt_accepted<B: SessionBackend>(backend: &B, label: &str) {
        let mut events = backend.events();
        let _ = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: CommandMeta {
                    client_msg_id: None,
                    ..Default::default()
                },
            })
            .await;
        let saw_pa = tokio::time::timeout(std::time::Duration::from_millis(400), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::PromptAccepted { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            !saw_pa,
            "{label}: Send with no client_msg_id must emit NO PromptAccepted (would mis-drain pending queue)"
        );
    }

    /// auth_methods non-empty ⟺ answer_auth (the third leg of the auth triangle;
    /// cap_behavior ties answer_auth⟺AnswerAuth-accepted).
    pub fn assert_auth_methods_match_answer_auth<B: SessionBackend>(backend: &B, label: &str) {
        let caps = backend.capabilities();
        assert_eq!(
            !caps.auth_methods.is_empty(),
            caps.supported_commands.answer_auth,
            "{label}: auth_methods non-empty ({:?}) must agree with answer_auth ({}) — inconsistent",
            caps.auth_methods,
            caps.supported_commands.answer_auth
        );
    }
}
