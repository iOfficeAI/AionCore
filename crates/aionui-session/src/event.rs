//! `SessionEvent` — the canonical, backend-agnostic event vocabulary (§C 6.2,
//! frozen). The `BackendAdapter` framing+parse stage emits these; the
//! monomorphic reducer consumes ONLY these. Orchestration also lowers
//! `Command::Send` into this enum so `step()` has a single entry point.
//!
//! Invariant I8: NO variant name / field name / field value ever contains a
//! claude-specific literal token (api_retry / subtype / compact_boundary / …) —
//! all normalized at the adapter.

/// Canonical session event. backend-agnostic. Every variant traces to a
/// measured fixture or is an explicit mock/lower input.
///
/// FIX 4a (derive choice, the TRUE reason): derives `PartialEq` only, NOT `Eq`.
/// This is NOT because `serde_json::Value` cannot be `Eq` — it actually can
/// (serde_json rejects NaN/Inf at construction, so its float is always finite;
/// `impl Eq for Number` / `impl Eq for Map` hold). The real reason: the
/// `AdapterSpecific.payload` is an opaque escape hatch this contract does NOT
/// promise total equality over; `assert_eq!` only needs `PartialEq`. Deriving
/// fewer traits than available is always legal ⇒ the shape compiles as-is.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)] // +Serialize/Deserialize (Addendum 2a, 007 §C2). only PartialEq — see FIX 4a above
pub enum SessionEvent {
    // ---- command lower (FIX 2): orchestration lowers Command::Send here ----
    /// Turn starter (C3/D1). Carries this turn's epoch (current_epoch, already
    /// +1 at lower time, I3/D9). The ONLY Running trigger; on receipt the
    /// reducer goes Idle/Starting → Running{ since_epoch: epoch, .. all
    /// false/empty }. NEVER keyed on system/init or the first assistant.
    TurnStarted { epoch: u64 },

    /// User-initiated cancel (feature 004 S14/R14). Orchestration lowers
    /// `Command::Cancel` here. Folds into the EXISTING `Idle` terminal
    /// (behavior-preserving, mirrors old ACP `cancel()→emit_finish(None)`),
    /// NEVER into `Error{Crashed}`. CRITICAL (crash≠cancel): cancel-then-kill makes
    /// the process exit, but the FSM is already terminal (Idle) by the time the
    /// resulting `Detached` arrives, so I10 absorbs it — a cancel never
    /// mislabels as a crash. MUST NOT be lowered as `Detached`.
    Cancel,

    // (No clock/timeout lower: AionCore does NOT auto-time-out a turn. A user may
    // leave a permission unanswered indefinitely; a genuinely wedged agent is
    // ended by user Cancel, and a dead process by `Detached`. The former deadline
    // janitor + `Timeout` event were removed 2026-06-11, user-decided — no
    // system-imposed deadline on any state.)

    // ---- data events (adapter framing+parse output) ----
    /// assistant content block type="text". Non-empty = substantive (C5).
    MessageDelta { item_id: String, text: String },
    /// assistant content block type="thinking". NOT substantive (C5).
    ThoughtDelta { item_id: String, text: String },
    /// assistant content block type="tool_use". tool_use_id demux key is
    /// reliable (measured). `subagent` (007 §5.6/§9.14) distinguishes an inline
    /// delegate / a spawned child session / a claude Workflow node — consumed
    /// ONLY by the RFC §9 promote layer + UI display-kind, NEVER by `step()`
    /// (which only checks "is this a tool_use → mark substantive"). Default
    /// `Inline` keeps existing single-agent behavior unchanged.
    ///
    /// Gap #4 / H2: `input` carries the tool's ARGUMENTS JSON (claude tool_use
    /// `input`, ACP tool_call `rawInput`, codex command/tool item fields) the
    /// parser used to drop — so the downstream `ConvDomainEvent::ToolStep.input`
    /// (WS step 1) is no longer always empty. `serde_json::Value` is the
    /// backend-neutral carrier; `Value::Null` = no/unknown args. The pure
    /// `step()` reducer NEVER reads it (mirrors the ToolResult-informational law);
    /// it is consumed only by the conversation `TurnFinalizer`. `#[serde(default)]`
    /// so older persisted Tier-1 frames (no `input` key) deserialize to
    /// `Value::Null` — same additive discipline as `subagent` / `ToolResult.content`
    /// / `TurnResult.outcome`. ⚠️ TIO-13: this slot carries Bash/Write bodies — no
    /// info-level log may print it.
    ToolCall {
        tool_use_id: String,
        name: String,
        #[serde(default)]
        subagent: SubagentKind,
        #[serde(default)]
        input: serde_json::Value,
        /// 009 H5: the `tool_use_id` of the SUBAGENT whose turn produced this call,
        /// when claude attributes the top-level frame to one (frame `parent_tool_use_id`).
        /// `None` = the main agent. The pure `step()` reducer NEVER reads it (attribution
        /// is a display concern); it is consumed only by the conversation `TurnFinalizer`
        /// to hang the tool step under the right subagent node (`ConvDomainEvent::ToolStep
        /// .parent_tool_use_id`). `#[serde(default)]` so older persisted frames (no key)
        /// deserialize to `None` — same additive discipline as `subagent` / `input`.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// user content block type="tool_result", referring back by tool_use_id.
    /// A completed tool_use = substantive output (C5).
    /// 009 R7 / §12.9 H3: carry `is_error` so a FAILED tool is not rendered as a
    /// success — the wire `tool_result` block has it (claude/codex), and dropping
    /// it was the bug where a red tool showed green. `#[serde(default)]` → false
    /// (success) so a pre-R7 persisted ToolResult frame (no key) still deserializes —
    /// same additive discipline as `content` / `parent_tool_use_id` below.
    ///
    /// 009 R8: `content` carries the backend-normalized tool OUTPUT the parser used
    /// to drop (text / produced file paths / inline images — e.g. a claude Read-tool
    /// image, a codex command's stdout, an ACP diff). The pure `step()` reducer
    /// NEVER reads it (the ToolResult-informational law holds, reducer.rs); it is
    /// consumed only by the conversation `TurnFinalizer` → `ToolResultDisplay`.
    /// `#[serde(default)]` so older persisted Tier-1 frames (no key) deserialize to
    /// an empty Vec — same additive discipline as `TurnResult.outcome` / `ToolCall.subagent`.
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        content: Vec<ToolResultContent>,
        /// 009 H5: the `tool_use_id` of the SUBAGENT whose turn produced this result
        /// (frame `parent_tool_use_id`); `None` = the main agent. Same role + additive
        /// discipline as `ToolCall.parent_tool_use_id` — consumed only by the
        /// conversation `TurnFinalizer` for subagent attribution, never by `step()`.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// backend-neutral "alive, no progress" heartbeat. claude sources (normalized
    /// two ways by ClaudeAdapter): system/api_retry (network backoff) + compaction
    /// (compact_boundary / SDK compacting, the no-chunk window). Since the deadline
    /// janitor was removed (no auto-timeout), this is now a pure reducer no-op kept
    /// as an HONEST liveness/diagnostic signal — a future liveness consumer (e.g. a
    /// UI "still working" hint) can read it without re-introducing a deadline.
    Heartbeat,
    /// Terminal result. Routing is by `is_error` (C7/R10), NEVER by subtype
    /// (measured: error turns still carry subtype="success").
    TurnResult {
        /// Normalized error bit (claude source: result.is_error). The ONLY
        /// error-routing basis.
        is_error: bool,
        /// Normalized HTTP status (claude source: result.api_error_status,
        /// non-null only on error turns). typed Option<u16> = backend-neutral
        /// diagnostic, not a claude token.
        api_error_status: Option<u16>,
        /// Normalized turn text (claude source: result.result; empty string on
        /// success = empty-turn signal; non-empty on error).
        result_text: String,
        /// Turn identity (the `TurnStarted{epoch}` this result belongs to). The
        /// persistent process breaks the reducer's D9 "turn == process boundary"
        /// assumption: a CANCELLED turn's trailing `result` (claude flushes one
        /// ~2.6s after an interrupt) can arrive DURING the next turn's `Running`.
        /// The reducer drops such a stale result (`epoch < Running.since_epoch`),
        /// mirroring the `TurnStarted` epoch guard — so a cross-turn leak can
        /// never mislabel the new turn as `Error`. The 002 adapter is
        /// epoch-agnostic (frame parsing has no turn context) and emits `epoch: 0`;
        /// the ai-agent reader stamps the live epoch when forwarding the b-side
        /// event. A backend that never cancels mid-turn can leave it 0 (then every
        /// result is `>= since_epoch=0` and settles normally).
        epoch: u64,
        /// Rich terminal outcome (007 §C2/O3, 2026-06-10). `is_error:bool` above
        /// is the LOSSY routing bit the reducer reads; `outcome` is the typed
        /// richness the adapter fills from each backend's terminal wire (claude
        /// `stop_reason` / 12-value `terminal_reason`; codex `turn.status`
        /// Completed|Interrupted|Failed; hermes ACP `StopReason`; opencode
        /// `StepFinishPart.reason`). ADDITIVE: `step()` NEVER reads `outcome` (it
        /// routes on `is_error` only); persisted Tier-2 `last_outcome` AND rides
        /// this event. The UI truncation/refusal badge consumes it (deferred P4). `#[serde(default)]`
        /// so older persisted frames deserialize as `Completed{EndTurn}`.
        #[serde(default)]
        outcome: TurnOutcome,
    },
    /// Process-exit edge (from `AgentIo::wait_for_exit`). exit=None ⇒ exited
    /// with unknown status (treat as terminal exit, D3). I11 drain-before-honor:
    /// orchestration MUST drain+parse stdout (apply any pending TurnResult)
    /// before stepping this event.
    Detached {
        exit: Option<ExitStatusLite>,
        /// G2: a redacted, allowlisted one-line summary of the process's stderr
        /// tail, captured AT THE BACKEND on a real EOF/exit. Lets the user see a
        /// *reason* (e.g. "usage limit exceeded", "connection refused") instead
        /// of a bare exit code, while never letting raw/untrusted stderr (which
        /// can carry API keys) cross the backend boundary — only lines matching
        /// the 14-keyword allowlist in `aionui_common::error_extract` survive,
        /// capped at 240 chars. `None` when: nothing matched the allowlist, the
        /// process never took stdio (startup double-take guard — unsafe to
        /// attribute), or a backend that has no stderr. `#[serde(default)]` so
        /// older persisted frames (no field) deserialize as `None`.
        #[serde(default)]
        redacted_summary: Option<String>,
    },
    /// Unknown message-type catch-all, never panics (S3/T3b). NO node_id
    /// (single node). The reducer MUST treat this as opaque — count or ignore
    /// only, MUST NOT match on tag/payload content (I8/I13).
    AdapterSpecific { tag: String, payload: serde_json::Value },

    /// LC-8a: a to-do / task PLAN snapshot (codex `turn/plan/updated`, ACP
    /// `session/update{sessionUpdate:"plan"}`). FULL-REPLACE snapshot semantics — each
    /// emission carries the entire current plan, the consumer replaces (never merges).
    /// FSM-ORTHOGONAL: the reducer treats this as a no-op (plan is *content within a
    /// Running turn*, not a state — cross-protocol verified); only the conversation
    /// layer projects it to the UI to-do panel. The ACP `PlanEntry` is a superset of
    /// codex `TurnPlanStep`, so this unified shape follows ACP: `priority`/`explanation`
    /// are `Option` (codex has no priority; ACP has no explanation).
    Plan {
        entries: Vec<PlanEntry>,
        explanation: Option<String>,
    },

    // ---- interactive permission / control-request (R9; F3 real claude parse) ----
    /// A claude `control_request` (subtype `can_use_tool`) that needs a user
    /// answer (F3): enters the requires-action sub-state (waiting_on_approval
    /// +1). `request_id` is the control correlation key (echoed in the
    /// control_response), NOT the tool_use_id. The reducer ref-counts on
    /// `request_id` ONLY — it never inspects the tool / questions payload (that
    /// rich detail rides the a-side card + the manager's pending map). In F1's
    /// permission mode only AskUserQuestion routes here; routine tools auto-run.
    /// `kind` (007 §9.17/§6b b3) distinguishes a tool/write approval from a
    /// mid-session re-auth challenge so they ref-count into SEPARATE counters
    /// (`waiting_on_approval` vs `waiting_on_auth`) and the UI can tell "approve
    /// tool" from "please re-login". Default `Tool` keeps existing behavior.
    Permission {
        request_id: String,
        #[serde(default)]
        kind: PermissionKind,
        /// G3: backend-parsed, NON-authoritative context the conversation layer
        /// uses to DECIDE auto-approval (e.g. team-MCP allowlist) WITHOUT the
        /// backend deciding anything. Shape (when present, ACP only):
        /// `{ "server_name": "aionui-team"?, "options": [{"option_id","kind"}] }`.
        /// The backend only PARSES the `session/request_permission` params into
        /// this; the conversation facade consults an injected `PermissionAuthorizer`
        /// and, if it auto-approves, dispatches `AnswerPermission` instead of
        /// surfacing a card. The reducer NEVER reads it (ref-counts on request_id
        /// only — §R9). `None` for backends/paths that carry no MCP context
        /// (claude direct-CLI raises permission only for AskUserQuestion).
        /// `#[serde(default)]` so older persisted frames deserialize as `None`.
        /// ⚠️ TIO-13: may carry a tool title — never log at info level.
        #[serde(default)]
        metadata: Option<serde_json::Value>,
        /// The raised tool's name (claude-direct: every `can_use_tool`), used as
        /// the permission card's title and to recognize an `AskUserQuestion`
        /// permission (rendered as a question card, not a generic allow/deny).
        /// `None` for paths that name no tool (ACP MCP, auth). The reducer NEVER
        /// reads it (ref-counts on request_id only — §R9). `#[serde(default)]` so
        /// older persisted frames deserialize as `None`.
        #[serde(default)]
        tool_name: Option<String>,
        /// The raised tool's raw `input`, shown on the permission card so the
        /// approver can review WHAT they are approving (a Bash `command`, an
        /// AskUserQuestion `{questions:[…]}` — AionUi issue #3779: without it the
        /// card showed only the tool name and the user approved blind). This is
        /// display-to-the-approver, NOT logging — TIO-13 (never log tool input at
        /// info) still applies to log output. `None` for paths that carry no tool
        /// input (auth, codex approvals until their params are probed). Reducer
        /// no-op. `#[serde(default)]` for back-compat.
        #[serde(default)]
        input: Option<serde_json::Value>,
    },
    /// The matching resolve (symmetric with `Permission`): decrements the SAME
    /// counter the originating `Permission` incremented (Tool→waiting_on_approval,
    /// Auth→waiting_on_auth); only the whole set reaching zero leaves the
    /// requires-action sub-state back to plain Running (I7). Emitted when the user
    /// answers (manager) OR when claude retracts via `control_cancel_request`.
    /// `kind` (007 §6b b3) lets the PURE reducer pick the right counter without an
    /// external request_id→kind map; the adapter echoes the originating kind.
    /// Default `Tool` preserves existing behavior.
    PermissionResolved {
        request_id: String,
        #[serde(default)]
        kind: PermissionKind,
    },

    // ======================================================================
    // ADDITIVE backend-produced / orchestration variants (007 §C2 / §9.0).
    // The reducer takes explicit no-op arms for ALL of these EXCEPT SubagentUpdate
    // (the ONE §6b b1 exception it READS). Adding them is the mechanical Rust
    // exhaustiveness requirement, NOT a decision-logic change (§6a).
    // ======================================================================
    /// Backend-CONFIRMED prompt handoff (Addendum 3 / U14) — DISTINCT from the
    /// optimistic, locally-lowered `TurnStarted`. Carries the correlation key the
    /// conversation pending-queue needs to drop exactly the admitted message.
    /// codex `turn/started` (Native) / claude stdin-flush-ok (Synthesized) /
    /// hermes prompt-result (Native) / opencode POST-ack (Native). Reducer no-op.
    PromptAccepted { client_msg_id: String },

    /// Per-turn typed usage/cost (Addendum 5 / U15). Adapter normalizes a
    /// cumulative wire counter to a per-turn DELTA (G6). Reducer no-op (pure
    /// consumer signal). codex `thread/tokenUsage/updated.last`; claude
    /// `result.usage`; cost_usd may be None.
    UsageDelta {
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        cost_usd: Option<f64>,
    },

    /// MCP / tool provisioning as a LIVE event (Addendum 5 / U16). Reducer no-op.
    Provisioning { phase: ProvisioningPhase },

    /// Backend receipt of a `Command::Rewind` (Addendum 6 / U24). `to_turn` =
    /// post-rollback history-end turn_gen. Reducer no-op; TRIGGERS the
    /// orchestrator's local rehydrate (drop Tier-1/2 after `to_turn`, §9.9).
    Rewound { to_turn: u64 },

    /// Non-optimistic mode/model switch confirm (Addendum 7 / U24). Reducer
    /// no-op (config is orthogonal). Closes reverse-coverage B8/B9/C3: adapters
    /// emit THIS instead of `AdapterSpecific{tag:"config"}`.
    ConfigChanged {
        mode: Option<String>,
        model: Option<String>,
    },

    /// The selectable model/mode/slash-command catalog became available. Unlike
    /// ACP (whose `session/new` RESPONSE carries the catalog synchronously, so the
    /// frontend picker is populated the instant `open_session` returns), the direct-
    /// CLI backends learn their catalog ASYNCHRONOUSLY, well after `open_session`:
    /// claude from the `control_request{initialize}` RESPONSE (~6s post-open on
    /// Bedrock), codex from the `model/list` / `collaborationMode/list` RESPONSEs.
    /// Before this event existed the adapters merely wrote the catalog into their
    /// private `discovered` cache with NO upward signal, so the frontend — which had
    /// already read an empty `config_options` on open — never re-fetched and the
    /// model selector stayed permanently disabled. This event is the direct-CLI
    /// analogue of the ACP path's `emit_snapshot_events` catalog push: the event-pump
    /// translates it into an `AcpConfigOption` frame so the frontend's existing
    /// `useAcpConfigOptions` re-fetch path fires. Reducer no-op (pure UI surface, like
    /// `ConfigChanged`); non-persistent (a fresh open re-discovers it). The payload is
    /// self-contained because the pump holds ONLY the event stream, never a backend
    /// handle (leak-prevention), so it cannot call `capabilities()` to assemble it.
    CatalogUpdated {
        models: Vec<crate::capability::ModelInfo>,
        /// The selectable mode catalog. For codex (feature 012) this carries the three
        /// fixed permission-profile tiers (`default`/`full-access`/`read-only`) mapped
        /// from `permissionProfile/list` — codex exposes NO separate collaborationMode
        /// selector; its mode axis IS the permission axis. Other backends carry their
        /// own mode semantics (claude permission-mode, etc.).
        modes: Vec<crate::capability::ModeInfo>,
        slash_commands: Vec<crate::capability::SlashCommandInfo>,
    },

    /// ⭐ Subagent lifecycle normalization (Addendum 8 / §9.12-9.14 / U22). THE
    /// ONE additive variant the reducer READS (§6b b1): `step()` upserts it into
    /// `Running.subagents` (key=`r#ref`, last-write-wins). claude Task subagent
    /// (from `parent_tool_use_id`/`task_*`); claude Workflow (adapter privately
    /// tails on-disk journal — paths never cross the seam, §9.14); codex
    /// collab-agent; opencode child-session. Flat-Vec + `parent_ref` edges
    /// model multi-level (§9.13).
    SubagentUpdate {
        r#ref: String,
        label: Option<String>,
        status: SubagentStatus,
        parent_ref: Option<String>,
    },

    /// 009 R6b / §3 / H1: RICH per-agent workflow detail for the background-plane
    /// roster (claude `workflow_progress[].workflow_agent`). Distinct from
    /// `SubagentUpdate` (which carries only the lifecycle status for the FSM
    /// foreground plane): this carries the display fields (model / tokens / tools /
    /// loop state / preview) that the per-agent panel renders. Reducer NO-OP (the
    /// FSM never reads detail); the orchestrator folds it into `workflow_roster`.
    /// Keyed by `ref` (= claude `agentId` once running, else `label`); `parent_ref`
    /// = the container `task_id` (the 1:N workflow→agents relation). All rich
    /// fields Option — only claude fills them; codex/ACP/aionrs never emit this.
    SubagentDetail {
        r#ref: String,
        parent_ref: Option<String>,
        label: Option<String>,
        /// LLM-loop phase (start→progress→done), orthogonal to task outcome.
        loop_state: Option<crate::state::WorkflowLoopState>,
        model: Option<String>,
        tokens: Option<u64>,
        tool_calls: Option<u64>,
        last_tool_name: Option<String>,
    },

    /// Out-of-turn diagnostic NOTICE (codex `warning` / `guardianWarning` /
    /// `deprecationNotice` / `configWarning`). A user-facing message that is NOT a
    /// turn terminal and NOT a tool/MCP provisioning phase — e.g. "this config key is
    /// deprecated", a guardian (sandbox) advisory, a malformed-config warning. Before
    /// this they fell into the catch-all `AdapterSpecific{codex_notif:*}` and never
    /// reached the user. DISTINCT from `TurnResult{is_error}` (a turn outcome) and
    /// `Provisioning` (MCP/tool lifecycle): a Notice is an advisory the UI surfaces as
    /// a status/banner. The codex `error` notification is NOT mapped here — it carries
    /// `{turnId, willRetry}` and is either a transient retry (→ Heartbeat) or the
    /// turn's terminal cause (→ already covered by `turn/completed`). Reducer NO-OP
    /// (an advisory does not move the FSM). Only the conversation layer projects it.
    Notice { level: NoticeLevel, message: String },

    /// Live tool-OUTPUT delta (codex `item/commandExecution/outputDelta`). The
    /// incremental stdout/stderr of a RUNNING tool, keyed by the owning tool's
    /// `item_id` (= the `ToolCall.tool_use_id` / codex `call_N`). PLAINTEXT (codex's
    /// turn-scoped item stream is NOT base64 — verified live 0.139.0,
    /// missing-wire-probe). DISTINCT from `MessageDelta` (assistant prose) and from
    /// `ToolResult.content` (the AUTHORITATIVE full output at tool completion): this
    /// is a pure liveness/UX stream so the UI can show a command's output AS IT RUNS.
    /// Ephemeral (never persisted — the durable record is the completed `ToolResult`,
    /// whose `aggregatedOutput` is the full text; a reconnect re-renders from there).
    /// The pure `step()` reducer NEVER reads it (no FSM meaning — a tool streaming
    /// output is just a Running turn making progress). Only the conversation layer
    /// projects it to a live tool-output pane. Claude/ACP/aionrs never emit it today.
    ToolOutputDelta { item_id: String, text: String },

    /// Live cumulative turn DIFF (codex `turn/diff/updated`). The FULL git-style
    /// unified diff of ALL file edits in the current turn so far (re-sent cumulatively
    /// as edits land — full-replace snapshot, NOT a per-chunk delta; verified live
    /// 0.139.0). DISTINCT from a `ToolResult` FilePath (the per-file authoritative
    /// result at fileChange completion): this is the turn-level live diff-view feed.
    /// Ephemeral (re-derivable; the durable per-file diffs ride completed ToolResults).
    /// Reducer NO-OP (a diff updating is just a Running turn making progress). Only the
    /// conversation layer projects it to a live diff panel. codex-only today.
    TurnDiffUpdated { diff: String },

    /// Streaming-item birth bracket (§9.2 / U17). Establishes who owns `item_id`.
    /// Reducer no-op; the `TurnFinalizer` (conversation side) opens a PendingItem.
    ItemStarted { item_id: String, kind: ItemKind },

    /// Streaming-item end bracket (§9.2 / U18). `truncation` set if the item was
    /// cut (max_tokens / context_window / wire-end). Reducer no-op.
    ItemCompleted {
        item_id: String,
        truncation: Option<TruncationInfo>,
    },

    /// The folded Tier-1 display record (§9.2 / U19) — the durable replacement
    /// for ephemeral deltas. NOT a wire event; a `TurnFinalizer` fold PRODUCT.
    /// Reducer no-op.
    MessageFinalized(FinalizedMessage),

    /// Reconnect catch-up (G1 / §9.3 / U20): the orchestrator emits the CURRENT
    /// in-flight state first on a re-subscribed stream. Orchestration-lowered
    /// (NOT persisted). Reducer no-op.
    Snapshot { state_repr: String, turn_gen: u64 },

    /// Backpressure overflow signal (§9.4 / U21): a broadcast receiver fell
    /// behind. Orchestration-lowered. Reducer no-op; conversation triggers a
    /// reconnect() to refetch.
    Lagged { skipped: u64 },

    /// 009 R6 (cleanup path 3): the F-4 idle-reap suspended this session's backend
    /// process. Orchestration-lowered (NOT persisted). Reducer NO-OP — suspend is
    /// FSM-invisible by design (the wake re-spawns on the same event_tx). The ONE
    /// orchestrator effect: CLEAR this session's workflow_roster. Otherwise a
    /// workflow that was running when the process was reaped would never receive
    /// its `task_notification` (the process is gone), leaving has_activity stuck
    /// true forever — the §12.7 liveness leak this signal closes.
    BackendSuspended,

    /// ListCheckpoints receipt (O2 / §1 / U-new). The adapter normalizes each
    /// backend's checkpoint/history enumeration capability (hermes `list` / codex
    /// `thread/turns/list` / opencode history endpoint).
    /// Reducer no-op; UI integration (checkpoint selector) deferred. capability-gated.
    CheckpointList { entries: Vec<CheckpointEntry> },

    /// `QuerySessionInfo` receipt: the backend's cumulative session info, replied to
    /// an on-demand query (claude `control_request{get_context_usage}` /
    /// `{get_session_cost}`, live-confirmed 2.1.186). DISTINCT from the per-turn
    /// `UsageDelta` (a streamed turn cost) — this is the WHOLE-SESSION snapshot the
    /// user explicitly asks for (context-window fill %, cumulative cost). Exactly one
    /// of the payload fields is set, matching the requested `SessionInfoKind`.
    /// Reducer no-op (a query reply never moves the FSM); capability-gated by
    /// `query_session_info`. Ephemeral (a fresh snapshot each query, not history).
    SessionInfo {
        /// Context-window budget (claude get_context_usage): `(used, max)` tokens.
        /// `None` unless the query was `ContextUsage`.
        context_usage: Option<ContextUsage>,
        /// Cumulative cost report text (claude get_session_cost). `None` unless the
        /// query was `SessionCost`. A preformatted multi-line string (the backend
        /// owns the formatting — we do not parse it).
        cost_text: Option<String>,
    },

    /// Addendum 9 (consumer-driven, conversation Tier-2): the adapter lowers its
    /// current `(session_id → backend_session_id)` binding downstream so the
    /// conversation layer can persist `conversations.backend_session_id` as the
    /// rewind/resume anchor (`--resume <id> --fork-session`, codex `thread/resume`,
    /// …). This is the SOLE channel by which `backend_session_id` escapes the
    /// adapter (§4.1 Addendum 4's "transport key never escapes" — minimally,
    /// explicitly broken for this ONE value); all other transport detail (bytes /
    /// handles / JSON-RPC) still never leaves the adapter. Orchestration-lowered
    /// (classify ⇒ OrchestrationLowered ⇒ NOT persisted as an append-only event;
    /// it is a pass-through the conversation reads, not part of rewind replay).
    /// Reducer no-op (never touches SessionState / can_send). Emitted whenever the
    /// adapter's private binding row updates: open_session success, Resume
    /// re-attach, fork, or backend-session loss. `None` = backend session lost /
    /// not yet established.
    BackendBound { backend_session_id: Option<String> },
}

// ==========================================================================
// ADDITIVE supporting types (007 §C2 / §9). All serde-derived for Tier-1/2
// persistence. None is read by `step()` except where wired through
// SubagentUpdate → Running.subagents (§6b b1).
// ==========================================================================

/// 009 R8: one backend-normalized piece of a completed tool's OUTPUT (carried on
/// `SessionEvent::ToolResult.content`). Backend-neutral union the conversation
/// `TurnFinalizer` maps into its own `ToolResultDisplay`:
///  - `Text` — textual output (codex command stdout, ACP text content, a claude
///    text tool_result block).
///  - `FilePath` — a file the tool wrote/read/diffed, referenced by PATH (the
///    durable artifact anchor; codex fileChange/imageGeneration, ACP diff/locations,
///    a claude on-disk file). `old_text`/`new_text` carry a diff when the wire has one.
///  - `Image` — INLINE image bytes (the ONLY producer is a claude Read-tool
///    tool_result `image` block, source base64). Transient/in-flight: the bytes
///    must be spilled to disk before persistence (deferred — see TurnFinalizer);
///    NEVER persisted base64-inline (Tier-1 row / WS-frame bloat).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolResultContent {
    Text(String),
    FilePath {
        path: String,
        #[serde(default)]
        mime: Option<String>,
        #[serde(default)]
        old_text: Option<String>,
        #[serde(default)]
        new_text: Option<String>,
    },
    Image {
        media_type: String,
        data: Vec<u8>,
    },
}

/// 007 §5.6/§9.14: how a `ToolCall` delegates. Default `Inline` (same session,
/// folds to ToolCall/ToolResult). `Spawned` = a real child `SessionBackend`.
/// `Workflow` = a claude Workflow node (pure marker, NO payload — internals
/// surface as ordinary child subagents the adapter mints by tailing disk; paths
/// never cross the seam, §9.14). Consumed by the RFC §9 promote layer + UI only.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum SubagentKind {
    #[default]
    Inline,
    Spawned {
        session_id: String,
    },
    Workflow,
}

/// 007 §9.17/§6b b3: a `Permission` request's class. Default `Tool` (existing
/// behavior). `Auth` = mid-session re-auth challenge → ref-counts into the
/// SEPARATE `waiting_on_auth` counter so the UI can distinguish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PermissionKind {
    #[default]
    Tool,
    Auth,
}

/// LC-8a: one entry in a [`SessionEvent::Plan`] to-do snapshot. Unified shape over
/// codex `TurnPlanStep{step,status}` (priority absent) and ACP `PlanEntry{content,
/// status,priority}`. The reducer never reads it (plan is FSM-orthogonal content);
/// only the conversation `TurnFinalizer` projects it to the UI to-do panel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanStatus,
    /// ACP-only (codex has no per-step priority). `None` for codex / when absent.
    #[serde(default)]
    pub priority: Option<PlanPriority>,
}

/// LC-8a: plan-step status. Wire: codex camelCase `inProgress`, ACP snake_case
/// `in_progress` — both normalized HERE to the canonical `InProgress` (I8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// LC-8a: ACP plan-step priority. codex never sets one (the field is `Option`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

/// 007 §9.12/§9.13: a subagent's lifecycle status (codex 7-state minus
/// NotFound). Carried by `SubagentUpdate` → upserted into `Running.subagents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubagentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
}

impl SubagentStatus {
    /// True for the lifecycle-final states. The active complement
    /// (`PendingInit | Running`) is what `has_foreground_activity` keys on, so
    /// this is its exact dual — kept here as the single source of truth.
    /// Feature 009 §11.4 terminal-absorption uses it to reject a late
    /// non-terminal update that would otherwise resurrect a finished subagent.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SubagentStatus::Interrupted
                | SubagentStatus::Completed
                | SubagentStatus::Errored
                | SubagentStatus::Shutdown
        )
    }
}

/// 007 §C2/O3: rich terminal outcome riding `TurnResult.outcome`. The reducer
/// NEVER reads this (routes on `is_error:bool`); it carries the {clean-done,
/// truncated, refused, cancelled, failed} distinction the one-bit projection
/// loses. Default `Completed{EndTurn}` so older frames deserialize cleanly.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum TurnOutcome {
    Completed {
        stop_reason: StopReason,
    },
    Cancelled {
        reason: CancelReason,
    },
    Failed,
    #[default]
    EndTurn,
}

/// 007 §9.5: why a `Completed` turn stopped.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    #[default]
    EndTurn,
    Truncated(TruncationKind),
    Refused {
        category: Option<String>,
    },
}

/// 007 §9.5: truncation cause (max_tokens / context / max_turns / budget /
/// wire-end-without-ItemCompleted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TruncationKind {
    MaxTokens,
    ContextWindow,
    MaxTurns,
    Budget,
    Truncated,
}

/// 007 §9.5: why a turn was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CancelReason {
    UserCancel,
    BargeIn,
    SocketAbort,
    ToolDenyChain,
}

/// 007 §9.6 (RFC §15③): MCP/tool provisioning phase. Connected-but-degraded is
/// distinct from connect/auth failure.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProvisioningPhase {
    Injecting,
    ToolsWaiting,
    ToolsReady,
    Degraded { reason: String },
    LoadFailed { reason: String },
}

/// Severity of an out-of-turn [`SessionEvent::Notice`]. `Warning` covers codex
/// `warning` / `guardianWarning` / `configWarning` (something the user should know
/// but the turn can still proceed); `Info` covers `deprecationNotice` (advisory,
/// non-urgent). Backend-neutral so a future backend's advisory maps here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NoticeLevel {
    Info,
    Warning,
}

/// 007 §9.2: kind of a streaming item being bracketed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ItemKind {
    Text,
    Thinking,
    Tool,
    Image,
}

/// 007 §9.2: truncation info on `ItemCompleted`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TruncationInfo {
    pub kind: TruncationKind,
    pub partial_text: Option<String>,
}

/// 007 §9.2: the folded Tier-1 display record (the `MessageFinalized` payload).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FinalizedMessage {
    pub item_id: String,
    pub kind: ItemKind,
    pub content: String,
    pub truncation: Option<TruncationInfo>,
    pub seq: u64,
}

/// 007 O2: a named checkpoint/history point (the `CheckpointList` entry). The
/// adapter normalizes each backend's list capability; `id` is backend-native
/// (adapter-private semantics, UI only echoes it back).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckpointEntry {
    pub id: String,
    pub label: Option<String>,
    pub turn_gen: Option<u64>,
}

/// Context-window budget on a [`SessionEvent::SessionInfo`] (claude
/// `get_context_usage`: `totalTokens` / `maxTokens`). `categories` carries the
/// optional per-bucket breakdown (system prompt / skills / messages / free) as
/// neutral `(name, tokens)` pairs — the backend owns the names; we do not
/// interpret them. `used`/`max` are the headline numbers a context-budget bar reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    pub used: u64,
    pub max: u64,
    #[serde(default)]
    pub categories: Vec<ContextUsageCategory>,
}

/// One per-bucket entry in a [`ContextUsage`] breakdown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextUsageCategory {
    pub name: String,
    pub tokens: u64,
}

/// 007 §1.2/§7.2 (Addendum 2b): the produced/lowered classifier — first-class
/// pure total fn, NOT an enum split (which would break the reducer's single
/// `match` entry). The storage layer calls this to decide what to append to the
/// durable rewind log. `step()` NEVER calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    /// Self-describing observation of what happened → candidate for PERSIST.
    BackendProduced,
    /// Internal decision lowered from Command/janitor/orchestration → NOT persisted.
    OrchestrationLowered,
}

/// Total over `SessionEvent` (§9.0): adding a future variant forces a decision
/// here — no silent misclassification. `step()` does not consult it.
pub fn classify(event: &SessionEvent) -> EventClass {
    use SessionEvent::*;
    match event {
        // orchestration-lowered (internal; NOT persisted). BackendBound is here
        // (Addendum 9): an adapter-translated binding pass-through, NOT a backend
        // observation — it must NOT enter the append-only stream or rewind replay.
        TurnStarted { .. }
        | Cancel
        | Heartbeat
        | Snapshot { .. }
        | Lagged { .. }
        | BackendBound { .. }
        | BackendSuspended => EventClass::OrchestrationLowered,
        // backend-produced (self-describing; PERSISTED — tier decided separately, §7.2)
        MessageDelta { .. }
        | ThoughtDelta { .. }
        | ToolCall { .. }
        | ToolResult { .. }
        | TurnResult { .. }
        | Detached { .. }
        | AdapterSpecific { .. }
        | Plan { .. }
        | Permission { .. }
        | PermissionResolved { .. }
        | PromptAccepted { .. }
        | UsageDelta { .. }
        | Provisioning { .. }
        | Rewound { .. }
        | ConfigChanged { .. }
        | CatalogUpdated { .. }
        | SubagentUpdate { .. }
        | SubagentDetail { .. }
        | Notice { .. }
        | ToolOutputDelta { .. }
        | TurnDiffUpdated { .. }
        | ItemStarted { .. }
        | ItemCompleted { .. }
        | MessageFinalized(..)
        | SessionInfo { .. }
        | CheckpointList { .. } => EventClass::BackendProduced,
    }
}

/// Two-axis persistence routing (§7.2): `classify` is the first gate; within
/// `BackendProduced`, the storage owner decides display-vs-state. Consulted ONLY
/// by the persistence layer, never the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistTier {
    /// UI live stream, never stored (deltas pushed-every-chunk-but-not-persisted).
    Ephemeral,
    /// Tier-1 history render (finalized fold + tool pairs + usage + config).
    Display,
    /// Tier-2 state-rebuild skeleton (turn boundary / outcome / permission).
    State,
    /// Both display AND state (e.g. TurnResult: text→display, outcome→state).
    DisplayAndState,
}

pub fn persist_tier(event: &SessionEvent) -> PersistTier {
    use SessionEvent::*;
    match classify(event) {
        EventClass::OrchestrationLowered => PersistTier::Ephemeral,
        EventClass::BackendProduced => match event {
            MessageDelta { .. } | ThoughtDelta { .. } => PersistTier::Ephemeral, // deltas folded → MessageFinalized
            // live tool-output / turn-diff streams: never persisted — the durable
            // record is the completed ToolResult (full aggregatedOutput / per-file diff).
            ToolOutputDelta { .. } | TurnDiffUpdated { .. } => PersistTier::Ephemeral,
            ItemStarted { .. } | ItemCompleted { .. } => PersistTier::Ephemeral, // lifecycle brackets
            CheckpointList { .. } => PersistTier::Ephemeral,                     // query response, not history
            CatalogUpdated { .. } => PersistTier::Ephemeral, // async catalog discovery, re-discovered on open (not history)
            SessionInfo { .. } => PersistTier::Ephemeral, // on-demand query snapshot, re-queryable (not history)
            SubagentDetail { .. } => PersistTier::Ephemeral, // transient per-agent progress (roster fill, re-derivable)
            Plan { .. } => PersistTier::Ephemeral, // LC-8a: live to-do snapshot, full-replace + re-derivable (not history)
            ToolCall { .. }
            | ToolResult { .. }
            | UsageDelta { .. }
            | Provisioning { .. }
            | ConfigChanged { .. }
            // an out-of-turn advisory the user should keep seeing in history
            | Notice { .. }
            | MessageFinalized(..) => PersistTier::Display,
            TurnResult { .. } => PersistTier::DisplayAndState, // text→display; outcome→state
            Permission { .. } | PermissionResolved { .. } | Detached { .. } | PromptAccepted { .. } => {
                PersistTier::DisplayAndState
            }
            SubagentUpdate { .. } => PersistTier::DisplayAndState, // roster→display; resumable→Tier2.last_subagents
            Rewound { .. } => PersistTier::State,                  // turn-truncation anchor
            AdapterSpecific { tag, .. } if is_raw_timing(tag) => PersistTier::Ephemeral,
            AdapterSpecific { .. } => PersistTier::Display,
            // Orchestration-lowered already handled above; this arm is unreachable
            // for them but keeps the match total over the enum.
            TurnStarted { .. }
            | Cancel
            | Heartbeat
            | Snapshot { .. }
            | Lagged { .. }
            | BackendBound { .. }
            | BackendSuspended => PersistTier::Ephemeral,
        },
    }
}

/// AdapterSpecific tags carrying raw timing diagnostics (ttft_ms / elapsed) are
/// Tier-0; structured tags are Tier-1.
fn is_raw_timing(tag: &str) -> bool {
    matches!(tag, "ttft_ms" | "elapsed" | "latency")
}

/// Trimmed exit status (does not leak the full `std::process::ExitStatus`
/// surface into the contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExitStatusLite {
    /// `None` when killed by a signal (no exit code).
    pub code: Option<i32>,
    /// Unix signal, if any.
    pub signal: Option<i32>,
}

/// Crash-discriminator return (FIX 4b/FIX 6b). `crash_outcome` is a pure
/// `fn(terminal_result_seen, exit) -> Outcome`; orchestration uses it to route
/// the `step` of a `Detached`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exited ABNORMALLY (signal, or non-zero code) before a terminal result
    /// this turn → maps to `SessionState::Error{ reason: Crashed }`.
    Crashed,
    /// Exited CLEANLY (code 0, no signal) but before any terminal result this
    /// turn → maps to `SessionState::Error{ reason: EmptyTurn }`. F46: a clean
    /// exit-0 with no result is NOT a crash — conflating it with a SIGKILL crash
    /// fed the wrong recovery disposition to the control plane. The exit payload
    /// (code/signal) is the discriminator the reducer previously discarded.
    CleanNoResult,
    /// A result was seen (or EOF-synthesized for a terminal_result=false
    /// backend) → follow the result path; the real terminal was already decided
    /// by the prior `TurnResult`, and the `Detached` is absorbed (I10).
    FollowResult,
}

#[cfg(test)]
mod additive_tests {
    //! 007 §C2 verification items for the additive vocabulary (P0a). Proves:
    //! (1) classify/persist_tier are TOTAL and route new variants correctly;
    //! (2) SessionEvent serde round-trips (Tier-1/2 durability, Addendum 2a);
    //! (3) the additive defaults keep older frames deserializable.
    use super::*;

    #[test]
    fn classify_routes_orchestration_vs_backend() {
        // orchestration-lowered → NOT persisted
        assert_eq!(classify(&SessionEvent::Heartbeat), EventClass::OrchestrationLowered);
        assert_eq!(
            classify(&SessionEvent::Lagged { skipped: 3 }),
            EventClass::OrchestrationLowered
        );
        assert_eq!(
            classify(&SessionEvent::Snapshot {
                state_repr: "Idle".into(),
                turn_gen: 1
            }),
            EventClass::OrchestrationLowered
        );
        // backend-produced → persisted (tier decided separately)
        assert_eq!(
            classify(&SessionEvent::PromptAccepted {
                client_msg_id: "m1".into()
            }),
            EventClass::BackendProduced
        );
        assert_eq!(
            classify(&SessionEvent::SubagentUpdate {
                r#ref: "a1".into(),
                label: None,
                status: SubagentStatus::Running,
                parent_ref: None
            }),
            EventClass::BackendProduced
        );
        assert_eq!(
            classify(&SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: None
            }),
            EventClass::BackendProduced
        );
        // Addendum 9: BackendBound is OrchestrationLowered → NOT persisted (a
        // pass-through for the conversation, never an append-only/rewind event).
        assert_eq!(
            classify(&SessionEvent::BackendBound {
                backend_session_id: Some("th-1".into())
            }),
            EventClass::OrchestrationLowered
        );
        assert_eq!(
            persist_tier(&SessionEvent::BackendBound {
                backend_session_id: None
            }),
            PersistTier::Ephemeral,
            "BackendBound is never persisted as an event"
        );
    }

    /// ENUMERATION INVARIANT (§7.2/§9.0). `classify_routes_orchestration_vs_backend`
    /// and `persist_tier_two_axis_routing` each SPOT-CHECK a handful of variants — the
    /// compiler proves the matches are TOTAL, but a mis-wired arm (e.g. a BackendProduced
    /// variant returning OrchestrationLowered, silently dropping it from persistence)
    /// compiles fine and the spot-checks miss it. This pins the routing DECISION for
    /// EVERY variant in one table: each SessionEvent paired with its expected
    /// (EventClass, PersistTier). A new variant forces this table to grow (count assert),
    /// so it cannot be added without an explicit persistence decision recorded here.
    #[test]
    fn every_session_event_variant_routes_to_expected_class_and_tier() {
        use EventClass::*;
        use PersistTier::*;
        // (label, event, expected class, expected tier). Keep in lockstep with the
        // SessionEvent enum — the count assert below is the tripwire.
        let table: Vec<(&str, SessionEvent, EventClass, PersistTier)> = vec![
            // ── orchestration-lowered (never persisted → Ephemeral) ──
            (
                "TurnStarted",
                SessionEvent::TurnStarted { epoch: 1 },
                OrchestrationLowered,
                Ephemeral,
            ),
            ("Cancel", SessionEvent::Cancel, OrchestrationLowered, Ephemeral),
            ("Heartbeat", SessionEvent::Heartbeat, OrchestrationLowered, Ephemeral),
            (
                "Snapshot",
                SessionEvent::Snapshot {
                    state_repr: "Idle".into(),
                    turn_gen: 1,
                },
                OrchestrationLowered,
                Ephemeral,
            ),
            (
                "Lagged",
                SessionEvent::Lagged { skipped: 1 },
                OrchestrationLowered,
                Ephemeral,
            ),
            (
                "BackendBound",
                SessionEvent::BackendBound {
                    backend_session_id: None,
                },
                OrchestrationLowered,
                Ephemeral,
            ),
            (
                "BackendSuspended",
                SessionEvent::BackendSuspended,
                OrchestrationLowered,
                Ephemeral,
            ),
            // ── backend-produced, Ephemeral (live/re-derivable, not history) ──
            (
                "MessageDelta",
                SessionEvent::MessageDelta {
                    item_id: "i".into(),
                    text: "t".into(),
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "ThoughtDelta",
                SessionEvent::ThoughtDelta {
                    item_id: "i".into(),
                    text: "t".into(),
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "Notice",
                SessionEvent::Notice {
                    level: NoticeLevel::Warning,
                    message: "config key X is deprecated".into(),
                },
                BackendProduced,
                Display,
            ),
            (
                "ToolOutputDelta",
                SessionEvent::ToolOutputDelta {
                    item_id: "call_0".into(),
                    text: "line-2\n".into(),
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "TurnDiffUpdated",
                SessionEvent::TurnDiffUpdated {
                    diff: "diff --git a/x b/x".into(),
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "ItemStarted",
                SessionEvent::ItemStarted {
                    item_id: "i".into(),
                    kind: ItemKind::Tool,
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "ItemCompleted",
                SessionEvent::ItemCompleted {
                    item_id: "i".into(),
                    truncation: None,
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "CheckpointList",
                SessionEvent::CheckpointList { entries: vec![] },
                BackendProduced,
                Ephemeral,
            ),
            (
                "SessionInfo",
                SessionEvent::SessionInfo {
                    context_usage: None,
                    cost_text: Some("Total cost: $0".into()),
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "SubagentDetail",
                SessionEvent::SubagentDetail {
                    r#ref: "a".into(),
                    parent_ref: None,
                    label: None,
                    loop_state: None,
                    model: None,
                    tokens: None,
                    tool_calls: None,
                    last_tool_name: None,
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "Plan",
                SessionEvent::Plan {
                    entries: vec![],
                    explanation: None,
                },
                BackendProduced,
                Ephemeral,
            ),
            (
                "AdapterSpecific(raw-timing)",
                SessionEvent::AdapterSpecific {
                    tag: "ttft_ms".into(),
                    payload: serde_json::json!(1),
                },
                BackendProduced,
                Ephemeral,
            ),
            // ── backend-produced, Display ──
            (
                "ToolCall",
                SessionEvent::ToolCall {
                    tool_use_id: "t".into(),
                    name: "Bash".into(),
                    subagent: SubagentKind::Inline,
                    input: serde_json::Value::Null,
                    parent_tool_use_id: None,
                },
                BackendProduced,
                Display,
            ),
            (
                "ToolResult",
                SessionEvent::ToolResult {
                    tool_use_id: "t".into(),
                    is_error: false,
                    content: vec![],
                    parent_tool_use_id: None,
                },
                BackendProduced,
                Display,
            ),
            (
                "UsageDelta",
                SessionEvent::UsageDelta {
                    input_tokens: 1,
                    output_tokens: 1,
                    total_tokens: 2,
                    cost_usd: None,
                },
                BackendProduced,
                Display,
            ),
            (
                "Provisioning",
                SessionEvent::Provisioning {
                    phase: ProvisioningPhase::ToolsWaiting,
                },
                BackendProduced,
                Display,
            ),
            (
                "ConfigChanged",
                SessionEvent::ConfigChanged {
                    mode: None,
                    model: Some("opus".into()),
                },
                BackendProduced,
                Display,
            ),
            (
                "MessageFinalized",
                SessionEvent::MessageFinalized(FinalizedMessage {
                    item_id: "m".into(),
                    kind: ItemKind::Text,
                    content: "c".into(),
                    truncation: None,
                    seq: 1,
                }),
                BackendProduced,
                Display,
            ),
            (
                "AdapterSpecific(structured)",
                SessionEvent::AdapterSpecific {
                    tag: "config".into(),
                    payload: serde_json::json!({}),
                },
                BackendProduced,
                Display,
            ),
            // ── backend-produced, DisplayAndState ──
            (
                "TurnResult",
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: "done".into(),
                    epoch: 1,
                    outcome: TurnOutcome::default(),
                },
                BackendProduced,
                DisplayAndState,
            ),
            (
                "Permission",
                SessionEvent::Permission {
                    request_id: "r".into(),
                    kind: PermissionKind::Tool,
                    metadata: None,
                    tool_name: None,
                    input: None,
                },
                BackendProduced,
                DisplayAndState,
            ),
            (
                "PermissionResolved",
                SessionEvent::PermissionResolved {
                    request_id: "r".into(),
                    kind: PermissionKind::Tool,
                },
                BackendProduced,
                DisplayAndState,
            ),
            (
                "Detached",
                SessionEvent::Detached {
                    exit: None,
                    redacted_summary: None,
                },
                BackendProduced,
                DisplayAndState,
            ),
            (
                "PromptAccepted",
                SessionEvent::PromptAccepted {
                    client_msg_id: "m".into(),
                },
                BackendProduced,
                DisplayAndState,
            ),
            (
                "SubagentUpdate",
                SessionEvent::SubagentUpdate {
                    r#ref: "a".into(),
                    label: None,
                    status: SubagentStatus::Running,
                    parent_ref: None,
                },
                BackendProduced,
                DisplayAndState,
            ),
            // ── backend-produced, State ──
            ("Rewound", SessionEvent::Rewound { to_turn: 1 }, BackendProduced, State),
        ];

        // Tripwire: every SessionEvent variant must appear. 32 variants today
        // (7 orchestration-lowered + 25 backend-produced, incl. Notice +
        // ToolOutputDelta + TurnDiffUpdated + SessionInfo); AdapterSpecific appears
        // twice for its raw-timing vs structured split → 33 rows. A new variant trips.
        assert_eq!(
            table.len(),
            33,
            "every SessionEvent variant (+ the AdapterSpecific timing split) must be routed here"
        );

        for (label, ev, want_class, want_tier) in &table {
            assert_eq!(
                classify(ev),
                *want_class,
                "[{label}] classify mismatch — a misrouted class silently changes persistence"
            );
            assert_eq!(persist_tier(ev), *want_tier, "[{label}] persist_tier mismatch");
        }
    }

    #[test]
    fn persist_tier_two_axis_routing() {
        // deltas: ephemeral (pushed-not-stored)
        assert_eq!(
            persist_tier(&SessionEvent::MessageDelta {
                item_id: "i".into(),
                text: "hi".into()
            }),
            PersistTier::Ephemeral
        );
        // finalized fold + config: display
        assert_eq!(
            persist_tier(&SessionEvent::ConfigChanged {
                mode: None,
                model: Some("opus".into())
            }),
            PersistTier::Display
        );
        // subagent roster: display + state (resumable)
        assert_eq!(
            persist_tier(&SessionEvent::SubagentUpdate {
                r#ref: "a".into(),
                label: None,
                status: SubagentStatus::Completed,
                parent_ref: None
            }),
            PersistTier::DisplayAndState
        );
        // rewind anchor: state only
        assert_eq!(persist_tier(&SessionEvent::Rewound { to_turn: 4 }), PersistTier::State);
        // raw timing AdapterSpecific: ephemeral; structured: display
        assert_eq!(
            persist_tier(&SessionEvent::AdapterSpecific {
                tag: "ttft_ms".into(),
                payload: serde_json::json!(120)
            }),
            PersistTier::Ephemeral
        );
        assert_eq!(
            persist_tier(&SessionEvent::AdapterSpecific {
                tag: "config".into(),
                payload: serde_json::json!({})
            }),
            PersistTier::Display
        );
    }

    #[test]
    fn session_event_serde_round_trip() {
        // GAP-G (R12 / V4-serde): EXHAUSTIVE over the BackendProduced variants (the
        // persisted set) PLUS the named boundary cases — MessageDelta text="",
        // Detached exit=None, SubagentUpdate parent_ref=None, BackendBound None.
        // (Orchestration-lowered variants are never persisted, so they're not part
        // of the round-trip contract; included a couple anyway since they derive
        // serde and must not regress.)
        let events = vec![
            // --- boundary cases (V4-serde named) ---
            SessionEvent::MessageDelta {
                item_id: "m1".into(),
                text: String::new(), // empty text boundary
            },
            SessionEvent::Detached {
                exit: None,
                redacted_summary: None,
            }, // exit=None boundary
            SessionEvent::SubagentUpdate {
                r#ref: "a0".into(),
                label: None,
                status: SubagentStatus::Running,
                parent_ref: None, // top-level boundary
            },
            SessionEvent::BackendBound {
                backend_session_id: None, // lost-binding boundary
            },
            // --- every BackendProduced variant (the persisted set) ---
            SessionEvent::ThoughtDelta {
                item_id: "th1".into(),
                text: "reasoning".into(),
            },
            SessionEvent::ToolCall {
                tool_use_id: "t1".into(),
                name: "Bash".into(),
                subagent: SubagentKind::Spawned {
                    session_id: "child-1".into(),
                },
                input: serde_json::json!({"command": "ls -la"}),
                // 009 H5: a non-None parent exercises the new field's serde round-trip.
                parent_tool_use_id: Some("toolu_parent_1".into()),
            },
            SessionEvent::ToolResult {
                tool_use_id: "t1".into(),
                is_error: false,
                content: vec![
                    ToolResultContent::Text("out".into()),
                    ToolResultContent::FilePath {
                        path: "/w/a.png".into(),
                        mime: Some("image/png".into()),
                        old_text: None,
                        new_text: None,
                    },
                ],
                parent_tool_use_id: Some("toolu_parent_1".into()),
            },
            SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "done".into(),
                epoch: 7,
                outcome: TurnOutcome::Completed {
                    stop_reason: StopReason::Truncated(TruncationKind::MaxTokens),
                },
            },
            SessionEvent::AdapterSpecific {
                tag: "x".into(),
                payload: serde_json::json!({"k": 1}),
            },
            SessionEvent::Permission {
                request_id: "r1".into(),
                kind: PermissionKind::Auth,
                metadata: None,
                tool_name: None,
                input: None,
            },
            SessionEvent::PermissionResolved {
                request_id: "r1".into(),
                kind: PermissionKind::Tool,
            },
            SessionEvent::PromptAccepted {
                client_msg_id: "m-7".into(),
            },
            SessionEvent::UsageDelta {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                cost_usd: Some(0.0021),
            },
            SessionEvent::Provisioning {
                phase: ProvisioningPhase::ToolsWaiting,
            },
            SessionEvent::Rewound { to_turn: 3 },
            SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: None,
            },
            SessionEvent::SubagentUpdate {
                r#ref: "agent-9".into(),
                label: Some("reviewer".into()),
                status: SubagentStatus::Interrupted,
                parent_ref: Some("wf-root".into()),
            },
            SessionEvent::Notice {
                level: NoticeLevel::Info,
                message: "deprecated: use --foo".into(),
            },
            SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line-2\n".into(),
            },
            SessionEvent::TurnDiffUpdated {
                diff: "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".into(),
            },
            SessionEvent::ItemStarted {
                item_id: "i1".into(),
                kind: ItemKind::Tool,
            },
            SessionEvent::ItemCompleted {
                item_id: "i1".into(),
                truncation: None,
            },
            SessionEvent::MessageFinalized(FinalizedMessage {
                item_id: "m1".into(),
                kind: ItemKind::Text,
                content: "final".into(),
                truncation: None,
                seq: 1,
            }),
            SessionEvent::CheckpointList {
                entries: vec![CheckpointEntry {
                    id: "ck1".into(),
                    label: Some("before refactor".into()),
                    turn_gen: Some(3),
                }],
            },
            SessionEvent::SessionInfo {
                context_usage: Some(ContextUsage {
                    used: 3025,
                    max: 200000,
                    categories: vec![ContextUsageCategory {
                        name: "System prompt".into(),
                        tokens: 1460,
                    }],
                }),
                cost_text: None,
            },
            SessionEvent::SessionInfo {
                context_usage: None,
                cost_text: Some("Total cost: $0.1180".into()),
            },
        ];
        for ev in events {
            let json = serde_json::to_string(&ev).expect("serialize");
            let back: SessionEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(ev, back, "round-trip must be lossless for {json}");
        }
    }

    /// ENUMERATION INVARIANT (wire-durability, §9.5). `TurnResult.outcome` is persisted
    /// (Tier-2 state-rebuild) and read back on rehydrate, so the `TurnOutcome` /
    /// `StopReason` / `TruncationKind` / `CancelReason` family must serde round-trip
    /// losslessly for EVERY variant — a silent `#[serde]` rename or an un-round-trippable
    /// shape would corrupt a rebuilt turn's outcome. The session_event_serde_round_trip
    /// test only carried ONE outcome (Truncated(MaxTokens)); this sweeps the whole family.
    #[test]
    fn every_turn_outcome_variant_round_trips() {
        let truncations = [
            TruncationKind::MaxTokens,
            TruncationKind::ContextWindow,
            TruncationKind::MaxTurns,
            TruncationKind::Budget,
            TruncationKind::Truncated,
        ];
        let cancels = [
            CancelReason::UserCancel,
            CancelReason::BargeIn,
            CancelReason::SocketAbort,
            CancelReason::ToolDenyChain,
        ];

        // The full TurnOutcome value space: EndTurn, Failed, every Completed{stop_reason}
        // (EndTurn / Refused{Some,None} / Truncated(each kind)), and every Cancelled{reason}.
        let mut outcomes: Vec<TurnOutcome> = vec![
            TurnOutcome::EndTurn,
            TurnOutcome::Failed,
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
            TurnOutcome::Completed {
                stop_reason: StopReason::Refused {
                    category: Some("safety".into()),
                },
            },
            TurnOutcome::Completed {
                stop_reason: StopReason::Refused { category: None },
            },
        ];
        outcomes.extend(truncations.iter().map(|k| TurnOutcome::Completed {
            stop_reason: StopReason::Truncated(*k),
        }));
        outcomes.extend(cancels.iter().map(|r| TurnOutcome::Cancelled { reason: *r }));

        // Tripwire: 5 base/Completed shapes + 5 truncation kinds + 4 cancel reasons.
        // A new TruncationKind / CancelReason / StopReason / TurnOutcome variant grows
        // this and forces an explicit round-trip decision.
        assert_eq!(
            outcomes.len(),
            5 + truncations.len() + cancels.len(),
            "every TurnOutcome / StopReason / TruncationKind / CancelReason variant must be swept"
        );

        for outcome in &outcomes {
            let json = serde_json::to_string(outcome).expect("serialize outcome");
            let back: TurnOutcome = serde_json::from_str(&json).expect("deserialize outcome");
            assert_eq!(outcome, &back, "TurnOutcome must round-trip losslessly: {json}");
        }

        // Also pin TruncationKind in isolation (it rides ItemCompleted.truncation +
        // FinalizedMessage.truncation too, not only via StopReason).
        for k in &truncations {
            let json = serde_json::to_string(k).expect("serialize truncation");
            let back: TruncationKind = serde_json::from_str(&json).expect("deserialize truncation");
            assert_eq!(k, &back, "TruncationKind must round-trip: {json}");
        }
    }

    #[test]
    fn additive_defaults_keep_old_frames_deserializable() {
        // An OLD TurnResult frame (no `outcome`) must still deserialize (the
        // #[serde(default)] guarantee) → outcome defaults to EndTurn.
        let old = r#"{"TurnResult":{"is_error":false,"api_error_status":null,"result_text":"hi","epoch":2}}"#;
        let ev: SessionEvent = serde_json::from_str(old).expect("old frame deserializes");
        match ev {
            SessionEvent::TurnResult { outcome, .. } => {
                assert_eq!(outcome, TurnOutcome::default(), "missing outcome → default");
            }
            other => panic!("expected TurnResult, got {other:?}"),
        }
        // An OLD ToolCall frame (no `subagent`, no `input`) → subagent defaults
        // Inline AND input defaults to Value::Null (Gap #4 additive #[serde(default)]).
        let old_tc = r#"{"ToolCall":{"tool_use_id":"t","name":"Read"}}"#;
        let ev: SessionEvent = serde_json::from_str(old_tc).expect("old ToolCall deserializes");
        match ev {
            SessionEvent::ToolCall {
                subagent, ref input, ..
            } => {
                assert_eq!(subagent, SubagentKind::Inline, "missing subagent → Inline");
                assert!(input.is_null(), "missing input → Value::Null (Gap #4 default)");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        // An OLD Permission frame (no `kind`, no `metadata`) → defaults Tool + None.
        let old_p = r#"{"Permission":{"request_id":"r"}}"#;
        let ev: SessionEvent = serde_json::from_str(old_p).expect("old Permission deserializes");
        assert!(matches!(
            ev,
            SessionEvent::Permission {
                kind: PermissionKind::Tool,
                metadata: None,
                ..
            }
        ));
        // A NEW Permission frame round-trips its G3 metadata verbatim.
        let new_p = r#"{"Permission":{"request_id":"r","kind":"Tool","metadata":{"server_name":"aionui-team"}}}"#;
        let ev: SessionEvent = serde_json::from_str(new_p).expect("new Permission deserializes");
        match ev {
            SessionEvent::Permission { metadata, .. } => {
                assert_eq!(
                    metadata
                        .as_ref()
                        .and_then(|m| m.get("server_name"))
                        .and_then(|v| v.as_str()),
                    Some("aionui-team")
                );
            }
            other => panic!("expected Permission, got {other:?}"),
        }
        // The OLDEST ToolResult frame pre-dates BOTH `is_error` (009 R7) and
        // `content` (009 R8) → both #[serde(default)]: is_error→false, content→[].
        // (A pre-R7 frame carried only `tool_use_id`; without the is_error default it
        // would fail to deserialize and a reload would lose the whole tool step.)
        let old_tr = r#"{"ToolResult":{"tool_use_id":"t"}}"#;
        let ev: SessionEvent = serde_json::from_str(old_tr).expect("oldest ToolResult deserializes");
        match ev {
            SessionEvent::ToolResult { is_error, content, .. } => {
                assert!(!is_error, "missing is_error → false (success)");
                assert!(content.is_empty(), "missing content → empty Vec");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // An OLD Detached frame (no `redacted_summary`, G2 post-dates it) → None.
        let old_d = r#"{"Detached":{"exit":null}}"#;
        let ev: SessionEvent = serde_json::from_str(old_d).expect("old Detached deserializes");
        match ev {
            SessionEvent::Detached { redacted_summary, .. } => {
                assert!(
                    redacted_summary.is_none(),
                    "missing redacted_summary → None (G2 default)"
                );
            }
            other => panic!("expected Detached, got {other:?}"),
        }
        // LC-8a: a Plan frame round-trips entries (status enum + optional priority).
        let plan = r#"{"Plan":{"entries":[{"content":"a","status":"InProgress","priority":"High"},{"content":"b","status":"Pending"}],"explanation":"why"}}"#;
        let ev: SessionEvent = serde_json::from_str(plan).expect("Plan deserializes");
        match ev {
            SessionEvent::Plan { entries, explanation } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].status, PlanStatus::InProgress);
                assert_eq!(entries[0].priority, Some(PlanPriority::High));
                assert!(entries[1].priority.is_none(), "missing priority → None (serde default)");
                assert_eq!(explanation.as_deref(), Some("why"));
            }
            other => panic!("expected Plan, got {other:?}"),
        }

        // A NEW Detached frame round-trips its summary verbatim.
        let new_d = r#"{"Detached":{"exit":null,"redacted_summary":"usage limit exceeded"}}"#;
        let ev: SessionEvent = serde_json::from_str(new_d).expect("new Detached deserializes");
        match ev {
            SessionEvent::Detached { redacted_summary, .. } => {
                assert_eq!(redacted_summary.as_deref(), Some("usage limit exceeded"));
            }
            other => panic!("expected Detached, got {other:?}"),
        }
    }
}
