//! `aionui-session` — server-authoritative SessionState control plane (feature
//! 002). Domain layer.
//!
//! ## What this crate is
//! It spawns the claude CLI directly (via 001 `aionui-process`, no ACP/SDK),
//! frames + parses its stream-json output into a backend-agnostic
//! [`SessionEvent`] vocabulary, and folds that — through a single monomorphic
//! pure reducer ([`step`]) — into a 5-name / 4-variant [`SessionState`] FSM.
//! The unlock signal ([`can_send_message`] flipping true) is carried by a
//! [`Transition`] emitted AT THE MOMENT OF TRANSITION, decoupled from any
//! blocking call return.
//!
//! ## Honest boundary (necessary-not-sufficient)
//! This crate does NOT, by itself, fix the user-visible "workflow freezes the
//! conversation" bug. The load-bearing coupling (UI-unlock ← turn.completed ←
//! StreamRelay finalize ← blocking prompt() return) lives in
//! `aionui-conversation` + WebSocket — all Non-Goal here, untouched. This crate
//! PROVES the necessary mechanism: a transition-driven unlock signal decoupled
//! from blocking returns. Wiring it to UI-unlock is feature 003+.
//!
//! ## Layering
//! Domain layer, parallel to `aionui-ai-agent`. Depends ONLY on
//! `aionui-process` (001) + `aionui-common`. NO production caller this
//! iteration — built and verified in isolation (same discipline as 001).
//!
//! ## Two-stage seam
//! - a-side: the [`SessionBackend`] / [`BackendConnection`] traits (claude/codex/
//!   acp/aionrs impls in `backend/`) — the ONLY backend-aware code: startup+prompt
//!   / framing+parse / capability declaration. (The original claude lane wrapped a
//!   legacy `BackendAdapter`; `ClaudeConnection` still wraps it internally.)
//! - b-side: the monomorphic [`step`] reducer + oracles — shared by all
//!   backends; a backend can only PRODUCE `SessionEvent`, never touch the FSM.
//!   Adding a backend = a new `SessionBackend` impl; reducer/FSM/oracle change 0 lines.

mod adapter;
mod backend;
mod capability;
mod error;
mod event;
mod reducer;
mod state;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

// The `adapter` module is the legacy claude spawn+parse path, now wrapped by the
// `ClaudeConnection` strangler (backend/claude_conn.rs) which reaches it via
// `crate::adapter::`. Only the transport types (`AgentIo`/`ManagedProcessIo`) have
// external consumers; the legacy `BackendAdapter`/`ClaudeAdapter`/`SessionSpec` are
// internal-only and no longer re-exported (DUP-10).
pub use adapter::{AgentIo, ManagedProcessIo, is_valid_claude_permission_mode};
// 007 §C1 seam: the live Command/SessionSpec/etc. DUP-10 removed the legacy
// `command`/`session` modules that forced the `BackendCommand`/`BackendSessionSpec`
// aliases, so these are now exported under their plain names.
pub use backend::{
    AcpConnection, AcpSessionBackend, Admission, BackendConnection, BackendError, CancelTarget, ClaudeConnection,
    ClaudeSessionBackend, CodexConnection, CodexSessionBackend, Command, CommandMeta, CommandReceipt, ContentBlock,
    ConversationSession, McpServerSpec, McpTransport, MsgStatus, Orchestrator, PendingMessage, PendingPermissionView,
    PermissionDecision, QuestionAnswer, SessionBackend, SessionConfig, SessionEnvelope, SessionInfoKind, SessionInit,
    SessionSpec, StateSnapshot, Tier2Checkpoint, TransitionReason, acp_capabilities, codex_capabilities, command_name,
    rehydrate,
};
pub use capability::{
    BlockSet, Capabilities, CapabilityTier, CommandSet, ModeInfo, ModelInfo, PromptAcceptedSource, SignalSet,
    SlashCommandInfo, block_kind_name,
};
pub use error::SessionError;
pub use event::{
    CancelReason, CheckpointEntry, EventClass, ExitStatusLite, FinalizedMessage, ItemKind, NoticeLevel, Outcome,
    PermissionKind, PersistTier, PlanEntry, PlanPriority, PlanStatus, ProvisioningPhase, SessionEvent, StopReason,
    SubagentKind, SubagentStatus, ToolResultContent, TruncationInfo, TruncationKind, TurnOutcome, classify,
    persist_tier,
};
pub use reducer::{Transition, crash_outcome, step};
pub use state::{
    ErrorReason, RequiresActionSet, SessionState, SubagentState, WorkflowAgentState, WorkflowLoopState,
    WorkflowTaskStatus, background_active, can_cancel, can_queue_message, can_send_message, has_foreground_activity,
    is_requires_action, is_unrecoverable_resume_error,
};
