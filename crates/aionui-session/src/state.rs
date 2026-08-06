//! `SessionState` — the server-authoritative session FSM (§C 6.1, frozen).
//!
//! P0 has EXACTLY 4 enum variants. `RequiresAction` is NOT a variant — it is a
//! sub-condition of `Running` (`requires_action.waiting_on_approval > 0`), so
//! resolving it returns seamlessly to plain `Running` without losing the
//! per-turn carry (FIX 1). The reducer stays a pure fn because all per-turn
//! memory lives inside the `Running` variant (I1).

/// Server-authoritative session state. P0 = exactly 4 variants.
///
/// (§A froze a 5-name set incl. a standalone `RequiresAction`; FIX 1 folds it
/// into `Running.requires_action` ⇒ the enum is 4 variants and `RequiresAction`
/// is a derived view via [`is_requires_action`].)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// `TurnStarted` seen, adapter is spawning the process + delivering the
    /// prompt, no stream-json frame seen yet.
    Starting,
    /// Turn in progress (triggered by `TurnStarted`, NEVER system/status).
    /// Carries the per-turn bookkeeping the pure reducer needs.
    Running {
        /// This turn's epoch (= `current_epoch` at `TurnStarted`). Diagnostics
        /// / `Transition`. Also the reducer's epoch baseline (I3).
        since_epoch: u64,
        /// OUTPUT-PRESENCE accumulator (C5/I5): set true by a non-empty
        /// `MessageDelta` or a `ToolResult`; `ThoughtDelta` never sets it;
        /// monotonic (never flips back within the span).
        saw_substantive_output: bool,
        /// Crash-discrimination flag (C6/I6): has this turn seen a terminal
        /// `TurnResult`? drain-before-honor (I11) guarantees it reflects stdout
        /// before `step(Detached)`.
        terminal_result_seen: bool,
        /// RequiresAction sub-condition (FIX 1/FIX 3): ref-counted set.
        /// count > 0 ⇒ requires-action sub-state (can_send still false; a user may
        /// leave it pending indefinitely — no deadline). count == 0 ⇒ plain Running.
        requires_action: RequiresActionSet,
        /// ⭐ 007 §6b b1 (the ONE intentional reducer state change): the live
        /// subagent roster. `SubagentUpdate` upserts here (key=`ref`,
        /// last-write-wins); fed by claude Task/Workflow, codex collab-agent,
        /// opencode child-session. Flat Vec + `parent_ref` edges model
        /// multi-level (§9.13). I14 prune (terminal entries removed at turn
        /// boundary) is enforced at the ORCHESTRATOR layer, NOT here — `step()`
        /// only upserts. `can_send_message` does NOT read this (unlock stays
        /// FSM-phase + waiting_on_approval/auth only, §9.12 M-12).
        subagents: Vec<SubagentState>,
    },
    /// Terminal error. P0 carries only `{ reason }` (NO `retryable` — later
    /// feature). Absorbing state (I10).
    Error { reason: ErrorReason },
    /// Idle — may send a new prompt. Successful terminal of a turn. Absorbing
    /// state (I10).
    Idle,
}

/// Ref-counted flag set (u32, not bool): resolving ONE does not unlock; only
/// the whole set reaching zero returns to plain `Running` (I7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiresActionSet {
    /// Number of pending control-requests (P0 mock-driven, R9).
    /// `Permission{kind:Tool}` +1 / its `PermissionResolved` -1.
    pub waiting_on_approval: u32,
    /// ⭐ 007 §6b b3 (the SECOND intentional reducer change, Addendum 9): pending
    /// mid-session re-auth challenges. `Permission{kind:Auth}` +1 / its
    /// `PermissionResolved` -1. SEPARATE counter so the UI distinguishes "approve
    /// tool" from "please re-login", and `AnswerAuth` vs `AnswerPermission` have
    /// distinct homes. `is_requires_action` = either counter > 0. Gated by
    /// `Capabilities.auth_methods` non-empty. K2 (re-auth continue-vs-abort, §10)
    /// is an adapter-behavior question; the reducer structure (just a counter)
    /// accommodates both outcomes unchanged.
    pub waiting_on_auth: u32,
    /// Pending structured questions to the user (`Ask` +1 / `AskResolved` -1,
    /// 2026-08-04 AskUserQuestion spec). SEPARATE counter for the same reason
    /// `waiting_on_auth` is one: "answer the agent's question" is neither a tool
    /// approval nor a re-auth, and the UI badges them differently. Mechanically a
    /// copy of `waiting_on_approval` — the reducer never reads the payload.
    pub waiting_on_question: u32,
}

/// ⭐ 007 §6b b1/§9.12/§9.13: a subagent's live state in `Running.subagents`.
/// Flat-Vec + `parent_ref` edges model multi-level (top-level = None). `r#ref`
/// is the upsert key (last-write-wins). For a claude Workflow node the adapter
/// mints these by privately tailing on-disk transcripts — the disk paths NEVER
/// appear here (§9.14).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubagentState {
    /// Stable per-subagent ref (codex agentId / opencode child sessionId /
    /// claude task id / workflow agent id). The upsert key.
    pub r#ref: String,
    /// User-visible label (subagent_type / workflow_name), optional.
    pub label: Option<String>,
    /// Lifecycle status (6-state, codex 7 minus NotFound).
    pub status: crate::event::SubagentStatus,
    /// Parent subagent ref, or None for a top-level subagent (§9.13).
    pub parent_ref: Option<String>,
}

/// 009 R6 / §3: a background workflow agent in the orchestrator-level
/// `workflow_roster` — the BACKGROUND plane, distinct from the FSM's
/// `Running.subagents` (foreground plane). A workflow lives in `Running.subagents`
/// during the turn that spawned it; once the turn folds Idle the FSM roster is
/// gone, but the workflow_roster entry OUTLIVES it (a Workflow/Task is non-blocking
/// and runs past its turn), so `has_activity` keeps reporting true → semantic-②
/// (the user can talk while a background workflow runs). Cleared only when the
/// task reaches a terminal `task_status` (then retained per §11.3) or on crash /
/// idle-reap.
///
/// Per-backend fillability (§10 F7): only `ref_id`/`task_status` are mandatory;
/// every rich field is `Option` because claude fills all of them (workflow_progress[]),
/// codex fills only label, and ACP/aionrs have no workflow concept (empty roster).
/// The frontend renders by field PRESENCE — it must not assume any rich field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowAgentState {
    /// Mandatory upsert key (= agent_id / task_id).
    pub ref_id: String,
    /// Mandatory task outcome, ORTHOGONAL to the LLM-loop `state` below: a failed
    /// agent can be `state:Done` (its loop finished) yet `task_status:Failed`.
    pub task_status: WorkflowTaskStatus,
    /// Whether this entry has a LIFECYCLE signal — i.e. it was ever touched by a
    /// `SubagentUpdate` (the container `task_id`, which receives a `task_notification`
    /// terminal). `false` = a `SubagentDetail`-ONLY entry (a per-agent `agentId`/label
    /// child whose only frames are enrichment: model/tokens/loop_state). Such a child
    /// has NO terminal signal — `task_notification` terminalizes the container, not the
    /// child, and many children emit only a single `state:start` and never a `done`
    /// (fixture-verified). It must NOT drive `background_active`, else a finished
    /// workflow's child pins has_activity=true forever across all later turns. Only
    /// lifecycle-bearing entries (containers) count toward background activity.
    /// `#[serde(default)]` (→ false): a future wire/persisted entry lacking this field
    /// deserializes as detail-only — the conservative "does not pin activity" default.
    #[serde(default)]
    pub has_lifecycle: bool,
    /// Terminal-retention flag (§11.3): a terminal entry is kept for UI history
    /// rather than removed immediately. `None` = default (transient).
    pub retain: Option<bool>,
    // ── rich fields, all Option (per-backend fillability §10 F7) ──
    /// User-visible label (claude workflow_name / codex spawn model).
    pub label: Option<String>,
    /// claude-only per-agent LLM-loop phase (start→progress→done; done on
    /// success OR failure — orthogonal to `task_status`).
    pub state: Option<WorkflowLoopState>,
    /// claude-only: model, last tool, token/tool counters, attempt, previews.
    pub model: Option<String>,
    pub last_tool_name: Option<String>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
}

/// 009 R6: a background workflow agent's task outcome (orthogonal to its LLM
/// loop state). `Running` = still working; the rest are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkflowTaskStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl WorkflowTaskStatus {
    /// True for the terminal outcomes (not `Running`) — drives roster cleanup +
    /// the `background_active` derivation (a terminal agent no longer counts).
    pub fn is_terminal(self) -> bool {
        !matches!(self, WorkflowTaskStatus::Running)
    }
}

/// 009 R6: claude-only per-agent LLM-loop phase (workflow_progress[].state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkflowLoopState {
    Start,
    Progress,
    Done,
}

/// 009 R6 / §3: does this session have a background workflow still RUNNING? The
/// BACKGROUND half of `has_activity` (the foreground half is
/// `has_foreground_activity`). True iff any LIFECYCLE-bearing roster entry is
/// non-terminal. An empty / all-terminal roster → false (a finished workflow stops
/// the spinner; a session with no workflow never spuriously shows activity).
///
/// A roster entry counts toward background activity iff BOTH hold:
///   - `has_lifecycle` — it was touched by a `SubagentUpdate` (the container,
///     keyed by `task_id`, which DOES receive a `task_notification` terminal), AND
///   - `task_status` is non-terminal.
///
/// The `has_lifecycle` conjunct closes the stale-has-activity leak (WS-captured
/// 2026-06-22): claude's per-AGENT `SubagentDetail` entries (keyed by `agentId`/label)
/// default `task_status: Running` and are only ENRICHED (model/tokens/loop_state),
/// NEVER terminalized — `task_notification` terminalizes the CONTAINER (`task_id`),
/// not its per-agent children, and many children emit a single `state:start` and never
/// a `done` (fixture-verified: ~half of workflow_agent refs never reach Done). Because
/// the orchestrator's roster map lives across the whole process (all turns), a finished
/// workflow's detail-only child would otherwise pin the background half `true` FOREVER
/// → every later turn (even plain chat) reports has_activity=true → sidebar spins
/// forever. A detail-only child is display metadata, not a lifecycle; only the
/// lifecycle-bearing container (with its `task_notification` terminal) drives
/// background activity. Real semantic-② (a Workflow CONTAINER outliving its spawning
/// turn) is preserved: that entry is lifecycle-bearing and stays non-terminal until its
/// `task_notification` arrives. The §12.7 process-gone closers
/// (Detached/BackendSuspended clear the roster) handle a container that dies mid-flight.
pub fn background_active(roster: &std::collections::HashMap<String, WorkflowAgentState>) -> bool {
    roster.values().any(|w| w.has_lifecycle && !w.task_status.is_terminal())
}

/// P0 error reasons: exactly 4 variants (§A frozen names). NO 35-variant
/// AgentErrorCode structure (later feature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorReason {
    /// Process exited before this turn saw a terminal result (C6/R7a).
    Crashed,
    /// `is_error:false` yet no text and no completed tool_use (C5/R5;
    /// thinking does not count).
    EmptyTurn,
    /// Terminal `result{is_error:true}` (C7/R10). Carries backend-neutral
    /// diagnostics.
    Backend {
        /// Normalized HTTP status (claude source: result.api_error_status,
        /// err=400 / anthauthfail=401). backend-neutral, not a claude token.
        /// `None` = non-HTTP error.
        api_error_status: Option<u16>,
        /// Normalized error text (claude source: result.result, non-empty on
        /// error turns).
        message: String,
    },
}

/// The ONLY unlock decision. Pure fn: no I/O, no clock, no interior mutability.
/// Running (incl. the requires_action sub-state) is always false — only Idle
/// admits a new prompt.
///
/// Truth table (frozen, §C 6.1):
/// | state                              | can_send |
/// |------------------------------------|----------|
/// | Starting                           | false    |
/// | Running { requires_action empty }  | false    |
/// | Running { requires_action count>0 }| false    |
/// | Error { .. }                       | false    |
/// | Idle                               | true     |
pub fn can_send_message(state: &SessionState) -> bool {
    matches!(state, SessionState::Idle)
}

/// Derived view (009 R2/§1): can the user PROACTIVELY queue a next-turn message
/// while a turn is in flight? Two conjuncts:
///   (a) FSM half — `Running` with NO requires_action (a turn is genuinely in
///       flight, not blocked on a permission/auth the user must answer first).
///       `Starting`/`Error`/`Idle` never queue (Idle is `can_send`, not queue).
///   (b) capability half — the backend `accepts_proactive_input` (claude's stdin
///       FIFO). ⚠️ NOT `caps.supported_commands.steer`: codex advertises steer but
///       the conv layer doesn't route it, so keying off steer would surface a dead
///       queue affordance (MX-QUEUE-3). ACP/aionrs lack the path → degrade to false.
///
/// Orthogonal to `can_send` (Idle): `can_send || can_queue` is the input-box gate.
/// Truth table (× backend `accepts_proactive_input`):
/// | state                              | claude(true) | codex/acp/aionrs(false) |
/// |------------------------------------|--------------|-------------------------|
/// | Idle / Starting / Error            | false        | false                   |
/// | Running { requires_action empty }  | true         | false                   |
/// | Running { requires_action count>0 }| false        | false                   |
pub fn can_queue_message(state: &SessionState, accepts_proactive_input: bool) -> bool {
    accepts_proactive_input && matches!(state, SessionState::Running { .. }) && !is_requires_action(state)
}

/// Derived view (009 R2/§1): can the user cancel right now? `Starting || Running`
/// (a turn is being set up or is running) — INCLUDING the requires_action
/// sub-condition (Esc while waiting on a permission cancels the whole turn, no
/// special case, §1 Esc ruling). `Idle`/`Error` are not cancellable (nothing is
/// in flight; a dead session must NOT report can_cancel=true and fire a phantom
/// interrupt at an already-gone process — CR-15/MX-ERROR-6).
/// Truth table:
/// | Starting | Running { .. } | Error | Idle | → can_cancel |
/// |   true   |     true       | false | false|              |
pub fn can_cancel(state: &SessionState) -> bool {
    matches!(state, SessionState::Starting | SessionState::Running { .. })
}

/// Derived view: is the session in the requires-action sub-condition? Used by
/// the UI to render the pending-action badge; introduces NO new state.
pub fn is_requires_action(state: &SessionState) -> bool {
    // 007 §6b b3: requires-action is EITHER a pending tool approval OR a pending
    // re-auth. Both block can_send; the UI reads the two counters separately.
    matches!(
        state,
        SessionState::Running { requires_action, .. }
            if requires_action.waiting_on_approval > 0
                || requires_action.waiting_on_auth > 0
                || requires_action.waiting_on_question > 0
    )
}

/// Derived view (UI "spinner" signal — the FOREGROUND half of `has_activity`,
/// see session-surface-and-ws-contract §1.6). Pure fn, no new state. Orthogonal
/// to `can_send`: `can_send` answers "can I talk?" (Idle only), this answers "is
/// a task running?" (turn-active spinner). The two combine on the frontend into
/// the two activity semantics:
///   has_activity ∧ !can_send → task running + can't talk (foreground turn busy)
///   has_activity ∧  can_send → task running + can talk    (detached background)
///
/// `requires_action` is EXCLUDED **unless a subagent is still running**: waiting
/// on the user to approve is not the main agent working (show a confirmation card,
/// not a spinner) — BUT a subagent spawned earlier may still be executing while the
/// main turn blocks on approval, and THAT is real work → keep spinning. So the
/// foreground signal is "main turn working OR any subagent active", and the
/// requires_action mute only applies when no subagent is active.
///
/// Truth table:
/// | state                                              | foreground_activity |
/// |----------------------------------------------------|---------------------|
/// | Starting                                           | true                |
/// | Running { requires_action empty }                  | true                |
/// | Running { requires_action count>0, no subagent run}| false               |
/// | Running { requires_action count>0, subagent run }  | true                |
/// | Error { .. }                                       | false               |
/// | Idle                                               | false               |
///
/// The backend half (`background_active`, tasks that outlive the turn that
/// started them) is OR'd in by the orchestrator when it builds
/// `StateSnapshot.has_activity`. It is constant-false today — a KNOWN GAP, NOT a
/// correct terminal value.
///
/// ⚠️ CORRECTION (2026-06-13): an earlier version claimed background_active=false
/// was "empirically correct" / semantic-② "structurally unreachable", citing a C1
/// capture of a `run_in_background` bash dying with the turn. That conflated bash
/// with a Workflow and was WRONG. Re-measured on claude 2.1.176 (AionCore's exact
/// persistent stream-json flags): a Workflow (Task tool) is NON-BLOCKING and
/// outlives its turn — the process replies to a new message ~2s later while the
/// workflow's 60s sleep still runs, and `result` is deferred to workflow
/// completion. So semantic-② (a task running while you can still talk) IS reachable.
/// See `orchestrator::fold_one` + protocols/samples/claude-cli/2.1.176/
/// WORKFLOW-VS-BASH-BACKGROUND.md for the full finding and the planned fix.
pub fn has_foreground_activity(state: &SessionState) -> bool {
    match state {
        SessionState::Starting => true,
        SessionState::Running { subagents, .. } => !is_requires_action(state) || any_subagent_active(subagents),
        SessionState::Error { .. } | SessionState::Idle => false,
    }
}

/// Is any subagent in the roster still doing work? `PendingInit`/`Running` count
/// as active; the terminal statuses (`Interrupted`/`Completed`/`Errored`/
/// `Shutdown`) do not. Mirrors the `is_active` convention used elsewhere.
fn any_subagent_active(subagents: &[SubagentState]) -> bool {
    use crate::event::SubagentStatus;
    subagents
        .iter()
        .any(|s| matches!(s.status, SubagentStatus::PendingInit | SubagentStatus::Running))
}

/// R16/3.9 crash-resume self-heal predicate: does this error reason mean a
/// `--resume` failed because the persisted session id is stale/corrupt? When
/// true, the conversation layer clears the persisted `claude_session_id` +
/// evicts the dead task so the next send rebuilds Fresh instead of wedging on
/// the same bad id.
///
/// Two signals, because claude 2.1.168 surfaces a failed resume TWO ways (both
/// probe-verified):
///   1. `"No conversation found …"` text — when the cause lands in the result
///      frame's `result`/`errors[]` (older shape / some paths).
///   2. `"error_during_execution"` (the `subtype`, folded into the message by
///      `parse_result` when result+errors are empty) — the ACTUAL shape a
///      stale-id resume takes today: a single `result{subtype:
///      "error_during_execution", is_error:true}` whose cause is on STDERR
///      only, then the process exits. Matching the subtype is what makes the
///      self-heal fire when the human-readable cause never reached the frame —
///      without it the conversation wedges permanently (every send re-resumes
///      the dead id). `error_during_execution` is a STRUCTURAL failure: a normal
///      turn — even one with a tool error — terminates `subtype:"success"`, so
///      this never misfires on ordinary errors (probe-verified).
///
/// Single source of the match (was inlined in the conversation
/// transition-subscriber): keeping it here, beside `ErrorReason`, makes it
/// unit-testable and means a backend wording change is fixed in ONE place.
pub fn is_unrecoverable_resume_error(reason: &ErrorReason) -> bool {
    matches!(
        reason,
        ErrorReason::Backend { message, .. }
            // claude: `--resume <uuid>` against an unknown session exits with
            // "No conversation found with session ID: <uuid>".
            if message.contains("No conversation found") || message.contains("error_during_execution")
            // codex: thread/resume against a threadId with no on-disk rollout
            // ("no rollout found for thread id <uuid>") and turn/start against a
            // threadId unknown to the process ("thread not found: <uuid>").
            // verified: samples/codex-cli/0.144.1/dead_resume.jsonl
            || message.contains("no rollout found for thread id")
            || message.contains("thread not found:")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ra(count: u32) -> SessionState {
        SessionState::Running {
            since_epoch: 1,
            saw_substantive_output: false,
            terminal_result_seen: false,
            requires_action: RequiresActionSet {
                waiting_on_approval: count,
                waiting_on_auth: 0,
                waiting_on_question: 0,
            },
            subagents: Vec::new(),
        }
    }

    fn ra_with_subagent(count: u32, sub_status: crate::event::SubagentStatus) -> SessionState {
        SessionState::Running {
            since_epoch: 1,
            saw_substantive_output: false,
            terminal_result_seen: false,
            requires_action: RequiresActionSet {
                waiting_on_approval: count,
                waiting_on_auth: 0,
                waiting_on_question: 0,
            },
            subagents: vec![SubagentState {
                r#ref: "sub-1".into(),
                label: None,
                status: sub_status,
                parent_ref: None,
            }],
        }
    }

    #[test]
    fn can_send_only_idle() {
        assert!(can_send_message(&SessionState::Idle));
        assert!(!can_send_message(&SessionState::Starting));
        assert!(!can_send_message(&ra(0)));
        assert!(!can_send_message(&ra(2)));
        assert!(!can_send_message(&SessionState::Error {
            reason: ErrorReason::Crashed
        }));
    }

    #[test]
    fn can_queue_truth_table_capability_gated() {
        // 009 R2: can_queue = (Running ∧ no requires_action) ∧ accepts_proactive_input.
        // claude (accepts_proactive_input=true):
        assert!(can_queue_message(&ra(0), true), "Running no-RA + claude → can queue");
        assert!(
            !can_queue_message(&ra(1), true),
            "Running+RA must NOT queue (answer first)"
        );
        assert!(!can_queue_message(&ra(2), true), "Running+RA (2) must NOT queue");
        assert!(
            !can_queue_message(&SessionState::Idle, true),
            "Idle is can_send, not queue"
        );
        assert!(
            !can_queue_message(&SessionState::Starting, true),
            "Starting cannot queue"
        );
        assert!(
            !can_queue_message(
                &SessionState::Error {
                    reason: ErrorReason::Crashed
                },
                true
            ),
            "Error cannot queue"
        );
        // codex/acp/aionrs (accepts_proactive_input=false): degrades to false in
        // EVERY state — including Running no-RA where claude would queue. This is
        // the MX-QUEUE-3 dead-button guard: the gate is the proactive-input bit,
        // NOT supported_commands.steer (which codex sets true).
        assert!(
            !can_queue_message(&ra(0), false),
            "no proactive-input path → never queue"
        );
        assert!(!can_queue_message(&ra(1), false));
        assert!(!can_queue_message(&SessionState::Idle, false));
    }

    #[test]
    fn background_active_is_any_non_terminal_roster_entry() {
        use std::collections::HashMap;
        // Lifecycle-bearing entries (the container case — touched by SubagentUpdate),
        // so task_status drives background_active per the §11.4 terminal absorption.
        let mk = |status: WorkflowTaskStatus| WorkflowAgentState {
            ref_id: "w".into(),
            task_status: status,
            has_lifecycle: true,
            retain: None,
            label: None,
            state: None,
            model: None,
            last_tool_name: None,
            tokens: None,
            tool_calls: None,
        };
        let mut roster: HashMap<String, WorkflowAgentState> = HashMap::new();
        assert!(!background_active(&roster), "empty roster → no background activity");
        roster.insert("a".into(), mk(WorkflowTaskStatus::Completed));
        assert!(!background_active(&roster), "all-terminal roster → false");
        roster.insert("b".into(), mk(WorkflowTaskStatus::Running));
        assert!(background_active(&roster), "any Running entry → true");
        roster.insert("b".into(), mk(WorkflowTaskStatus::Failed));
        assert!(!background_active(&roster), "Running→terminal flips it back to false");
        // is_terminal coverage
        assert!(!WorkflowTaskStatus::Running.is_terminal());
        for t in [
            WorkflowTaskStatus::Completed,
            WorkflowTaskStatus::Failed,
            WorkflowTaskStatus::Stopped,
        ] {
            assert!(t.is_terminal(), "{t:?} is terminal");
        }
    }

    /// stale-has-activity fix: the `has_lifecycle` conjunct. A per-agent
    /// `SubagentDetail`-only entry (has_lifecycle=false) carries no terminal signal —
    /// `task_notification` terminalizes the container, not the child, and many children
    /// emit only a single `state:start`, never a `done` (fixture-verified). It must NEVER
    /// drive background_active, REGARDLESS of task_status or loop_state; otherwise a
    /// finished workflow's child pins has_activity=true forever across later turns. A
    /// lifecycle-bearing entry (has_lifecycle=true, a SubagentUpdate container) DOES drive
    /// it per task_status (preserving real semantic-② until its task_notification).
    #[test]
    fn background_active_ignores_detail_only_entries() {
        use std::collections::HashMap;
        let mk = |has_lifecycle: bool, status: WorkflowTaskStatus, loop_state: Option<WorkflowLoopState>| {
            WorkflowAgentState {
                ref_id: "x".into(),
                task_status: status,
                has_lifecycle,
                retain: None,
                label: None,
                state: loop_state,
                model: None,
                last_tool_name: None,
                tokens: None,
                tool_calls: None,
            }
        };
        let mut roster: HashMap<String, WorkflowAgentState> = HashMap::new();
        // Detail-only (has_lifecycle=false): NEVER active, whatever its task_status /
        // loop_state — this is the leak the fix closes (used to stay Running forever).
        for ls in [Some(WorkflowLoopState::Start), Some(WorkflowLoopState::Progress), None] {
            roster.insert("x".into(), mk(false, WorkflowTaskStatus::Running, ls));
            assert!(
                !background_active(&roster),
                "a detail-only child (loop_state={ls:?}) must NOT drive background_active (closes the leak)"
            );
        }
        // Lifecycle-bearing container: Running → active, terminal → not.
        roster.insert("x".into(), mk(true, WorkflowTaskStatus::Running, None));
        assert!(
            background_active(&roster),
            "a lifecycle-bearing Running container drives background_active (real semantic-②)"
        );
        roster.insert("x".into(), mk(true, WorkflowTaskStatus::Completed, None));
        assert!(
            !background_active(&roster),
            "a lifecycle-bearing container, once terminal, stops driving background_active"
        );
    }

    #[test]
    fn can_cancel_truth_table_includes_requires_action() {
        // 009 R2: can_cancel = Starting || Running (incl. requires_action; Esc
        // while waiting on a permission cancels the whole turn — no special case).
        assert!(can_cancel(&SessionState::Starting), "Starting cancellable");
        assert!(can_cancel(&ra(0)), "Running cancellable");
        assert!(can_cancel(&ra(1)), "Running+RA cancellable (no special case)");
        assert!(can_cancel(&ra(2)));
        assert!(!can_cancel(&SessionState::Idle), "Idle: nothing to cancel");
        assert!(
            !can_cancel(&SessionState::Error {
                reason: ErrorReason::Crashed
            }),
            "Error: dead session must NOT report cancellable (no phantom interrupt)"
        );
    }

    #[test]
    fn is_requires_action_predicate() {
        // directly pins the `> 0` boundary: 0 = false, 1 = true, 2 = true.
        assert!(!is_requires_action(&ra(0)), "count 0 is plain Running");
        assert!(is_requires_action(&ra(1)), "count 1 is requires-action");
        assert!(is_requires_action(&ra(2)), "count 2 is requires-action");
        assert!(!is_requires_action(&SessionState::Idle));
    }

    #[test]
    fn has_foreground_activity_truth_table() {
        // §1.6 spinner signal (foreground half): Starting/Running(working)=true;
        // Running(requires_action)=false (waiting on user, show card not spinner);
        // Idle/Error=false. Orthogonal to can_send.
        assert!(has_foreground_activity(&SessionState::Starting), "Starting → spinner");
        assert!(has_foreground_activity(&ra(0)), "Running working → spinner");
        assert!(
            !has_foreground_activity(&ra(1)),
            "Running waiting-on-approval, no subagent → NOT spinner (confirmation card)"
        );
        // ⭐ requires_action BUT a subagent still running → spinner (the subagent is
        // doing real work even though the main turn blocks on approval).
        assert!(
            has_foreground_activity(&ra_with_subagent(1, crate::event::SubagentStatus::Running)),
            "Running waiting-on-approval + subagent running → spinner"
        );
        assert!(
            !has_foreground_activity(&ra_with_subagent(1, crate::event::SubagentStatus::Completed)),
            "Running waiting-on-approval + subagent DONE → NOT spinner (no active work)"
        );
        // Subagent active even with no requires_action is still a spinner (the
        // ra(0) path already true, but pin it via a subagent on a working turn).
        assert!(has_foreground_activity(&ra_with_subagent(
            0,
            crate::event::SubagentStatus::PendingInit
        )));
        assert!(!has_foreground_activity(&SessionState::Idle), "Idle → no spinner");
        assert!(
            !has_foreground_activity(&SessionState::Error {
                reason: ErrorReason::Crashed
            }),
            "Error → no spinner"
        );
        // Orthogonality with can_send: Starting is busy (spinner) but NOT sendable.
        assert!(has_foreground_activity(&SessionState::Starting) && !can_send_message(&SessionState::Starting));
        // Idle is sendable but NOT busy.
        assert!(can_send_message(&SessionState::Idle) && !has_foreground_activity(&SessionState::Idle));
    }

    #[test]
    fn unrecoverable_resume_error_predicate() {
        let backend = |status: Option<u16>, msg: &str| ErrorReason::Backend {
            api_error_status: status,
            message: msg.into(),
        };
        // The real bad-resume terminal (probe-verified wording) → true.
        assert!(is_unrecoverable_resume_error(&backend(
            None,
            "No conversation found with session ID: stale-xyz"
        )));
        // The ACTUAL shape a stale-id resume takes on claude 2.1.168: the cause
        // ("No conversation found") is on STDERR, so the frame carries only
        // `subtype:"error_during_execution"`, which parse_result folds into the
        // message. Self-heal MUST fire on this (else the conversation wedges).
        assert!(
            is_unrecoverable_resume_error(&backend(None, "error_during_execution")),
            "the error_during_execution subtype (stderr-only cause) MUST self-heal"
        );
        // codex dead-anchor wire messages, as carried into the synthesized
        // terminals (verified: samples/codex-cli/0.144.1/dead_resume.jsonl):
        // thread/resume against a threadId with no on-disk rollout, and
        // turn/start against a threadId unknown to the process.
        assert!(is_unrecoverable_resume_error(&backend(
            None,
            "codex thread/resume failed: no rollout found for thread id 0199-dead"
        )));
        assert!(is_unrecoverable_resume_error(&backend(
            None,
            "codex rejected the turn request: thread not found: 0199-dead"
        )));
        // A codex transient failure must NOT clear the anchor.
        assert!(!is_unrecoverable_resume_error(&backend(
            None,
            "codex rejected the turn request: server overloaded, please retry"
        )));
        // A genuine backend error that is NOT a bad resume → false (must NOT
        // clear the session id / evict on a normal 429 or auth failure).
        assert!(!is_unrecoverable_resume_error(&backend(Some(429), "rate limited")));
        assert!(!is_unrecoverable_resume_error(&backend(Some(401), "invalid x-api-key")));
        assert!(!is_unrecoverable_resume_error(&backend(None, "")));
        // A normal turn — even one whose tool failed — terminates subtype:"success"
        // (probe-verified), so an ordinary error message never misfires.
        assert!(!is_unrecoverable_resume_error(&backend(
            None,
            "the command failed with exit code 3"
        )));
        // Non-Backend error reasons never self-heal the session id.
        assert!(!is_unrecoverable_resume_error(&ErrorReason::Crashed));
        assert!(!is_unrecoverable_resume_error(&ErrorReason::EmptyTurn));
    }
}
