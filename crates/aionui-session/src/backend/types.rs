//! 007 §C1 frozen seam types — the a-side vocabulary the conversation layer
//! sends DOWN (`Command`) and the orchestration types that flow back UP
//! (`SessionEnvelope`, `StateSnapshot`). These are the NEW symmetric
//! actor-mailbox seam (replacing 002's transport-leaking `BackendAdapter` over
//! the strangler migration, §8). All field-level frozen — 02-impl may not
//! change shapes without re-approval (C.9).
//!
//! NOTE (strangler): the legacy `command::Command` (2-variant) + `adapter`
//! module still drive `session::run_turn`. This module is the new path; the
//! orchestrator selects it behind a feature flag until codex (P1) forces the
//! switch.

use crate::event::PermissionKind;
use crate::state::SessionState;

// ==========================================================================
// DOWNWARD — conversation → session (Commands)
// ==========================================================================

/// The 10-variant command vocabulary (§C1, frozen). Capability-gated via
/// `Capabilities.supported_commands`; an unsupported command is rejected at
/// dispatch with `BackendError::CommandNotSupported`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Start / continue a turn. Multimodal `Vec<ContentBlock>` (NOT a String).
    /// The adapter increments `turn_gen` on accept and echoes it in the receipt.
    Send {
        content: Vec<ContentBlock>,
        metadata: CommandMeta,
    },
    /// Scoped cancel. `Turn`/`Session` accepted by all; `Tool` only where
    /// `supported_commands.cancel_tool`.
    Cancel { target: CancelTarget },
    /// Inject content into the in-flight turn (adapter-private gated steering;
    /// the b-side FSM never sees Steer). Gated by `supported_commands.steer`.
    Steer { content: Vec<ContentBlock> },
    /// Answer a `Permission{request_id}` the backend raised. `decision` is the
    /// coarse allow/deny (sound for every generic tool approval).
    ///
    /// `selected` carries the SPECIFIC chosen option for backends whose permission
    /// is a single pick-one prompt: the ACP `optionId`, or a single-question claude
    /// `AskUserQuestion` label. `None` = a plain allow/deny (the claude adapter then
    /// falls back to the first option for an AskUserQuestion allow). Backends with a
    /// pure accept/decline approval (codex/aionrs) ignore it.
    ///
    /// `answers` carries the FULL claude `AskUserQuestion` answer set — one entry
    /// per question, multi-label for a `multiSelect:true` question (claude can ask
    /// 1–4 questions in one call, §C1 / task #83). When non-empty the claude adapter
    /// builds `updatedInput.answers` from THIS (every question keyed by its text, a
    /// multi-select value as a JSON array claude joins with ", "); when empty it
    /// degrades to the single-question `selected` path (back-compat). Captured wire:
    /// `protocols/samples/claude-cli/2.1.178/ask_user_question_multi_*.ndjson`.
    /// Non-claude backends ignore `answers`.
    AnswerPermission {
        request_id: String,
        decision: PermissionDecision,
        selected: Option<String>,
        answers: Vec<QuestionAnswer>,
    },
    /// Answer an `Ask{request_id}` (structured question card — its own command,
    /// NOT AnswerPermission: asking is not authorizing, 2026-08-04 ruling).
    ///
    /// `answers: Some(...)` = the user's per-question picks (claude wire shape,
    /// see [`QuestionAnswer`]); `None` = the user dismissed the card without
    /// answering. `None` MUST reach the claude adapter as a deny — claude
    /// silently DROPS unanswered questions on an allow (not a re-ask, live
    /// 2.1.178), so an allow-with-empty-answers is silent data loss. The
    /// `Option` makes "declined" and "answered" unconfusable at the type level.
    /// Backends without a question channel reject with `CommandNotSupported`.
    AnswerAsk {
        request_id: String,
        answers: Option<Vec<QuestionAnswer>>,
    },
    /// Answer a mid-session re-auth challenge (waiting_on_auth). Gated by
    /// `supported_commands.answer_auth`.
    AnswerAuth {
        method_id: String,
        credentials: serde_json::Value,
    },
    /// User acknowledged a completed turn (done-unseen → seen). Always accepted;
    /// folds at the conversation/fold-on-read layer, never the FSM.
    Acknowledge { node_id: String },
    /// Switch mode. Confirmed non-optimistically via `ConfigChanged`.
    SetMode { mode: String },
    /// Switch model. Confirmed via `ConfigChanged`.
    SetModel { model: String },
    /// Set a generic agent config option (#99). `option_id` is the
    /// backend-advertised option key (e.g. `effort`/`reasoning_effort`/`thought_level`
    /// → claude `apply_flag_settings{effortLevel}`); `value` is the chosen value.
    /// Distinct from SetMode/SetModel (which have dedicated wires); this is the
    /// catch-all for advertised config options that lack a first-class command.
    /// Confirmed via `ConfigChanged`. Backends with no such option reject it with
    /// `CommandNotSupported`.
    SetConfigOption { option_id: String, value: String },
    /// Rewind the backend's history by `num_turns` (logical turn count; the
    /// adapter translates to each backend's native unit — codex
    /// thread/rollback{numTurns} measured count-only; opencode message; hermes
    /// checkpoint; claude `/rewind` is TUI/SDK-only → CommandNotSupported, §9.9).
    /// Idle-only, blocking. Gated by `supported_commands.rewind`.
    Rewind { num_turns: u32 },
    /// Enumerate named checkpoints/history points (O2). Capability-gated by
    /// `supported_commands.list_checkpoints`; replies `SessionEvent::CheckpointList`.
    ListCheckpoints,
    /// Query the backend's cumulative session info (context-usage budget / session
    /// cost). Capability-gated by `supported_commands.query_session_info`; replies
    /// `SessionEvent::SessionInfo`. claude maps to `control_request{get_context_usage}`
    /// / `{get_session_cost}`. A read-only query — never moves the FSM.
    QuerySessionInfo { kind: SessionInfoKind },
}

/// Which cumulative session-info query [`Command::QuerySessionInfo`] requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInfoKind {
    /// Context-window budget (claude `get_context_usage`: totalTokens / maxTokens
    /// + per-category breakdown).
    ContextUsage,
    /// Cumulative session cost (claude `get_session_cost`: a preformatted text
    /// report).
    SessionCost,
}

/// Multimodal input block (§C1). An adapter that can't carry a variant rejects
/// the `Send` at dispatch; `Capabilities.prompt_blocks` advertises support
/// up-front so the UI never offers an image picker to a text-only backend.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(String),
    Image { data: Vec<u8>, media_type: String },
    Audio { data: Vec<u8>, media_type: String },
    ResourceLink { uri: String, mime_type: Option<String> },
    AtMention { user_id: String },
}

/// Scoped cancel target (§C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTarget {
    Tool(String),
    Turn,
    Session,
}

/// Permission answer (§C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Approved,
    Denied,
    AllowAlways,
}

/// One answered claude `AskUserQuestion` question (§C1 / task #83). `question` is
/// the exact question TEXT (claude's `answers` map is keyed by it — the input
/// schema guarantees question texts are unique). `labels` are the chosen option
/// labels: exactly one for a single-select question, one-or-more for a
/// `multiSelect:true` question. Carried on `Command::AnswerPermission.answers`;
/// the claude adapter emits `labels` as a JSON array, which claude's own zod
/// preprocess joins with ", " (live-captured 2.1.178).
/// `#[derive(Deserialize)]` so the conversation layer can parse the frontend's
/// `[{question, labels}]` answer payload (threaded as JSON through `ConfirmRequest`
/// → `ConvCommand::Confirm`) directly into this type at the session bridge.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct QuestionAnswer {
    pub question: String,
    pub labels: Vec<String>,
}

/// Read-only snapshot of ONE currently-open (unanswered) permission request, for
/// REST recovery (`GET /confirmations`) of a reloaded `waiting_confirmation`
/// conversation. Transient adapter-side state — NOT reducer/FSM/persisted. Carries
/// only the safe correlation + title surface (`request_id`/`tool_name`); the raw
/// tool `input` (command body / args) is deliberately NOT exposed (TIO-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermissionView {
    /// The control correlation key (claude `request_id`); the recovery card's
    /// id == call_id == this, matching the live `ConfirmationAdded` path.
    pub request_id: String,
    /// The tool the permission gates (used as the recovered card's title). May be
    /// empty if the backend did not name it.
    pub tool_name: String,
    /// AskUserQuestion recovery: the BARE `questions[]` array (NOT the
    /// `{questions:[…]}` wrapper — claude_conn stores `input.get("questions")`;
    /// consumers re-wrap if they need the tool-input shape) when
    /// `tool_name == "AskUserQuestion"`, else `None`. Lets the REST `/confirmations`
    /// recovery path rebuild a question card symmetric to the live `ConfirmationAdded`
    /// frame (rather than degrading to an Allow/Deny permission card). Only
    /// AskUserQuestion carries `input` from the adapter, so this is `None` for every
    /// ordinary tool.
    pub questions: Option<serde_json::Value>,
}

/// Per-command metadata (§C1). `client_msg_id` is the pending-queue correlation
/// key the adapter echoes on `PromptAccepted` (Addendum 3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandMeta {
    pub command_id: u64,
    pub cwd: Option<String>,
    pub extra_args: Vec<String>,
    pub client_msg_id: Option<String>,
}

// ==========================================================================
// dispatch return + errors
// ==========================================================================

/// Fast, synchronous-in-spirit dispatch receipt (§C1). `accepted` = the adapter
/// accepted+queued the command on the wire; it does NOT mean the turn finished
/// (the turn flows up `events()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    pub accepted: bool,
    pub admission: Admission,
    pub turn_gen: u64,
}

/// HOW a command was admitted (§C1 / §9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// In-flight now; `TurnStarted{turn_gen}` imminent. NO wall-clock turn
    /// timeout (long turns are normal; liveness = StuckDetector dead-silence).
    Started,
    /// Accepted but not yet running (backend busy). Waits until the current turn
    /// → Idle; NO admission timeout (§9.5 ⭐, O4).
    Queued,
    /// Steer/Acknowledge/AnswerPermission folded into the live turn (no new
    /// turn_gen, no TurnStarted).
    NoTurn,
}

/// Seam errors (§C1). `CommandNotSupported.command` is a `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("command not supported by this backend: {command}")]
    CommandNotSupported { command: &'static str },
    #[error("backend transport error: {0}")]
    Transport(String),
    /// The session workspace (spawn cwd) is missing, not a directory, or not
    /// accessible. Carried as its own class (not `Transport`) so the app layer
    /// can keep the legacy #410 workspace-unavailable UX instead of an opaque
    /// 502; payload = the path.
    #[error("workspace unavailable: {0}")]
    WorkspaceUnavailable(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("backend setup/handshake rejected: {0}")]
    SetupRejected(String),
    /// The backend process is still completing its startup handshake (e.g. codex's
    /// `thread/started` has not arrived within the bound-thread window — common on a
    /// cold start or an untrusted project that slows codex init). This is RETRYABLE:
    /// the agent is coming up, not broken. Mapped to a user-readable "agent starting,
    /// please retry" (503-class) instead of an opaque 500 (bug-hunt codex-500).
    #[error("backend handshake timed out (agent still starting): {0}")]
    HandshakeTimeout(String),
}

impl BackendError {
    /// Map a spawn-layer failure into the seam error. The classified
    /// `WorkspaceUnavailable` passes through as its own class (the app layer
    /// surfaces the legacy #410 workspace-unavailable UX from it); everything
    /// else collapses to `Transport` under the caller's context prefix, as
    /// before.
    pub(crate) fn from_spawn(context: &'static str, e: aionui_process::ProcessError) -> Self {
        match e {
            aionui_process::ProcessError::WorkspaceUnavailable(path) => Self::WorkspaceUnavailable(path),
            e => Self::Transport(format!("{context}: {e}")),
        }
    }
}

// ==========================================================================
// UPWARD wrapper + state snapshot
// ==========================================================================

/// Routing wrapper (Addendum 1 / §C1): a connection multiplexes many sessions;
/// downstream demuxes by `session_id`. The backend's transport key (codex
/// threadId / hermes sessionId / opencode sessionId) NEVER appears here — the
/// adapter maps it to the canonical logical `session_id` (§4.1 two-id).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionEnvelope {
    pub session_id: String,
    pub turn_gen: u64,
    pub event: crate::event::SessionEvent,
}

/// Full state snapshot (Addendum 8 / §9.12 / §C7) — pushed on EVERY state change
/// (NOT incremental). Carries the complete FSM + the pre-derived `can_send` so a
/// reconnecting / late subscriber gets complete truth in its first message.
/// Replaces the incremental `RoutedTransition` model.
#[derive(Debug, Clone, PartialEq)]
pub struct StateSnapshot {
    pub session_id: String,
    pub state: SessionState,
    /// Pre-derived unlock (orchestrator calls `can_send_message`); conversation
    /// reads this field, NEVER recomputes.
    pub can_send: bool,
    /// Pre-derived UI activity/spinner signal, orthogonal to `can_send` (§1.6
    /// session-surface contract). `= has_foreground_activity(state) ||
    /// background_active`. Frontend renders the spinner from THIS; `can_send`
    /// gates the input box. The two combine into the two activity semantics
    /// (`has_activity ∧ !can_send` = busy foreground; `has_activity ∧ can_send` =
    /// detached background). The backend half is currently a stub (always false).
    pub has_activity: bool,
    /// Pre-derived (009 R2/§1): can the user proactively queue a next-turn message
    /// while a turn is in flight? `= can_queue_message(state, caps.accepts_proactive_input)`.
    /// The input box enables on `can_send || can_queue`; conversation reads this,
    /// never recomputes. Capability-gated: true only on backends with a proactive
    /// input path (claude stdin FIFO); false (degrades to can_send) elsewhere.
    pub can_queue: bool,
    /// Pre-derived (009 R2/§1): can the user cancel the in-flight turn right now?
    /// `= can_cancel(state)` (Starting||Running, incl. requires_action). Esc maps to
    /// Cancel when true, no-op otherwise. False in Idle/Error so a dead session never
    /// fires a phantom interrupt.
    pub can_cancel: bool,
    pub turn_gen: u64,
    pub last_reason: Option<TransitionReason>,
}

/// Why the last transition happened (§C2/O5, StateSnapshot.last_reason). Typed
/// (agents emit rich reasons — claude `terminal_reason`, codex `TurnStatus`,
/// hermes `StopReason`); REUSES the existing typed families rather than being a
/// new load-bearing type. Orchestration-derived; reducer never reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionReason {
    Started,
    Completed(crate::event::StopReason),
    Errored(crate::state::ErrorReason),
    Cancelled(crate::event::CancelReason),
    PermissionRequested(PermissionKind),
    PermissionResolved,
}

// ==========================================================================
// session identity (two-id model, §4.1 / Addendum 4)
// ==========================================================================

/// How a session is (re)opened. `session_id` is the STABLE AionCore-minted
/// logical id (immutable across fork/resume/crash-respawn; the demux key).
/// `backend_session_id` is the MUTABLE transport binding (adapter-private,
/// lives only on the checkpoint; None = backend session lost → adapter mints a
/// fresh one and re-registers the same logical id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSpec {
    Fresh {
        session_id: String,
    },
    Resume {
        session_id: String,
        backend_session_id: Option<String>,
    },
    /// Open by forking an EXISTING backend session into a new one: the parent's
    /// transport id is only the fork SOURCE, never this session's binding. The
    /// adapter learns its own binding from the backend (codex `thread/started`,
    /// claude `system/init`) and reports it via `BackendBound`. Used exactly
    /// once per forked conversation — as soon as a binding is persisted, later
    /// opens go back to `Resume`.
    Fork {
        session_id: String,
        /// The parent conversation's backend session id (fork source).
        parent_backend_session_id: String,
        /// Backend turn to fork at (inclusive); `None` = fork at HEAD. Only
        /// codex (`thread/fork`'s `lastTurnId`) consumes this today.
        at_turn_id: Option<String>,
    },
}

// ==========================================================================
// Tier-2 persistence checkpoint (§C4 / §7.3, ⭐ COLLAPSED per conversation C §17,
// user-decided 2026-06-11 "1A"). The persisted Tier-2 state is a SINGLE column
// (`conversations.backend_session_id`); every FSM mid-turn substate
// (turn_boundaries / last_outcome / outstanding_permissions / waiting_on_auth /
// process_attached / last_subagents / workflow_tail) is turn-INTERNAL transient,
// NOT persisted — a crash loses it (accepted). So cold recovery is always `Idle`
// (the resume anchor + turn_gen continuity do NOT change the rebuilt phase).
// ==========================================================================

/// 007 §C4 (collapsed): the minimal cold-start rebuild input. The persisted state
/// is just the resume anchor (`backend_session_id`) + `turn_gen` for epoch
/// continuity (so a resumed session's new turns don't reuse old epochs / collide
/// with the persisted block-stream grouping). All serde-derived.
///
/// NOTE: the former rich fields are gone. The FSM never needs them at cold start —
/// recovery is `Idle` (C §17). If a future feature needs mid-turn-state recovery
/// (e.g. restore a pending-permission badge across a crash) or workflow disk-tail
/// resume, that is a NEW additive design, not a regression here.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tier2Checkpoint {
    /// Stable logical id (demux key, §4.1).
    pub session_id: String,
    /// The resume anchor: the backend's session id (codex threadId / claude on-disk
    /// uuid). Persisted as `conversations.backend_session_id` (C §17). The CALLER
    /// reads it to build `SessionSpec::Resume`; `rehydrate` does not consult it for
    /// the rebuilt phase. None = backend session lost → resume rebinds a fresh one.
    pub backend_session_id: Option<String>,
    /// Epoch continuity: the next turn resumes at `turn_gen + 1` so epochs stay
    /// monotonic across a restart (and don't collide with the persisted block
    /// stream's `turn_gen` grouping). Recoverable as `MAX(turn_gen)` over blocks.
    pub turn_gen: u64,
}

/// Map a `Command` to the `&'static str` used in `CommandNotSupported` (so
/// gating code has one canonical name per variant).
pub fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Send { .. } => "send",
        Command::Cancel { .. } => "cancel",
        Command::Steer { .. } => "steer",
        Command::AnswerPermission { .. } => "answer_permission",
        Command::AnswerAsk { .. } => "answer_ask",
        Command::AnswerAuth { .. } => "answer_auth",
        Command::Acknowledge { .. } => "acknowledge",
        Command::SetMode { .. } => "set_mode",
        Command::SetModel { .. } => "set_model",
        Command::SetConfigOption { .. } => "set_config_option",
        Command::Rewind { .. } => "rewind",
        Command::ListCheckpoints => "list_checkpoints",
        Command::QuerySessionInfo { .. } => "query_session_info",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_envelope_round_trips() {
        let env = SessionEnvelope {
            session_id: "logical-1".into(),
            turn_gen: 3,
            event: crate::event::SessionEvent::PromptAccepted {
                client_msg_id: "m1".into(),
            },
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: SessionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn tier2_checkpoint_round_trips() {
        // §C4 verification (collapsed, C §17): the minimal anchor + epoch skeleton
        // serializes losslessly.
        let ck = Tier2Checkpoint {
            session_id: "logical-1".into(),
            backend_session_id: Some("thread-abc".into()),
            turn_gen: 5,
        };
        let json = serde_json::to_string(&ck).unwrap();
        let back: Tier2Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(ck, back);
    }

    #[test]
    fn command_names_are_stable() {
        assert_eq!(command_name(&Command::Rewind { num_turns: 2 }), "rewind");
        assert_eq!(command_name(&Command::ListCheckpoints), "list_checkpoints");
    }

    /// #410 parity: the classified workspace-unavailable spawn failure crosses
    /// the seam as its own `BackendError` class (path payload intact); every
    /// other spawn failure collapses to `Transport` under the context prefix.
    #[test]
    fn from_spawn_keeps_workspace_unavailable_class() {
        let err = BackendError::from_spawn(
            "codex spawn failed",
            aionui_process::ProcessError::workspace_unavailable("/gone/workspace"),
        );
        assert_eq!(err, BackendError::WorkspaceUnavailable("/gone/workspace".into()));

        let err = BackendError::from_spawn("codex spawn failed", aionui_process::ProcessError::internal("boom"));
        assert_eq!(
            err,
            BackendError::Transport("codex spawn failed: internal error: boom".into())
        );
    }
}
