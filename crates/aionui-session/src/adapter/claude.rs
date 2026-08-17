//! `ClaudeAdapter` — the ONLY claude-aware code (seam a-side). Spawns the
//! claude CLI headless, frames + parses its stream-json (NDJSON) into the
//! canonical `SessionEvent` vocabulary, and declares its capabilities.
//!
//! Invariant I8: every claude-specific token (system/api_retry, compact_*,
//! subtype, content-block "type" strings) is normalized HERE; none leaks past
//! the canonical `SessionEvent` into the reducer.

use aionui_common::CommandSpec;
use aionui_process::{BoxedStdin, ProcessError, Spawner};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{AgentIo, BackendAdapter, ManagedProcessIo, SessionSpec};
use crate::capability::{Capabilities, CapabilityTier, ModeInfo, SignalSet};
use crate::event::SessionEvent;

/// The claude backend adapter. Feature 004: ONE instance per persistent
/// SESSION (not per turn). `buf` (the NDJSON half-line buffer) persists across
/// turns/result spans — the persistent process's stdout does not EOF between
/// turns, so the buffer is drained line-by-line continuously and is NOT reset
/// per turn.
#[derive(Default)]
pub struct ClaudeAdapter {
    /// Half-line buffer: bytes received since the last newline (S3). Spans
    /// result boundaries (persistent multi-turn).
    buf: Vec<u8>,
    /// P3 partial streaming: per-assistant-message state so `content_block_delta`
    /// text fragments emit as incremental `MessageDelta`s (typewriter) and the
    /// consolidated `assistant` frame skips re-emitting text it already streamed.
    stream: StreamState,
    /// Context occupancy from the most recent API call in the session, summed as
    /// `input + cache_creation + cache_read + output` off the last
    /// `message_delta.usage`.
    ///
    /// Deliberately NOT per-message state: it must outlive `message_start`, since
    /// the figure we want is the LAST call of the turn. `result.usage` cannot
    /// supply it — that field AGGREGATES every API call in the turn, so a
    /// four-tool-call turn reported 107_974 where the real occupancy was 27_467
    /// (live-captured, 2.1.220; its `cache_read` 95_709 is exactly the four calls'
    /// 15_493+26_345+26_485+27_386 summed). Feeding the aggregate to the usage
    /// indicator drove it past 100% of the context window.
    last_call_tokens: Option<u64>,
    /// Thinking tokens accumulated across the turn's API calls. `result.usage`
    /// reports `output_tokens_details: null` (live-captured), so the only source is
    /// each `message_delta.usage.output_tokens_details.thinking_tokens` — 60 on the
    /// first call of a captured four-call turn. Summed here because the breakdown
    /// is a PER-TURN figure; reset when the `result` frame consumes it.
    turn_thought_tokens: u64,
    /// The model id claude last announced on `system{subtype:"init"}` — the
    /// authoritative "what am I running now", re-emitted on model switch and after
    /// a compaction. Spelled exactly like the `modelUsage` keys
    /// (`"claude-opus-5"`), so it selects the right entry directly.
    current_model: Option<String>,
    /// request_ids of outstanding `AskUserQuestion` can_use_tool requests, so a
    /// `control_cancel_request` resolves to the RIGHT event: `AskResolved` for a
    /// question, `PermissionResolved` for a tool approval. The reducer keeps two
    /// separate counters and a cancel that decrements the wrong one wedges the
    /// other at >0 (requires-action never clears). Entries are removed on cancel;
    /// answered requests are cleared by the conn layer's own pending map — a
    /// stale leftover here is harmless (set membership only biases WHICH resolve
    /// event a cancel emits, and ids are unique per control request).
    ask_requests: std::collections::HashSet<String>,
}

/// Per-message streaming state for `--include-partial-messages`. Reset on each
/// `message_start` (the per-assistant-message fence; a turn may hold several
/// across tool rounds, each with a fresh `message.id`), so it is naturally
/// bounded and never grows across a session.
#[derive(Default)]
struct StreamState {
    /// `message.id` of the in-flight assistant message — the stem of the per-kind
    /// delta item_id (`<id>:text` / `<id>:think`). Empty before the first
    /// `message_start` (degrades to `:text`/`:think`, still kind-separated).
    item_id: String,
    /// content-block `index` → kind, captured at `content_block_start`, used to
    /// route each `content_block_delta` (the delta frames carry only `index`).
    block_kind: std::collections::HashMap<u64, StreamBlockKind>,
    /// Whether live text was streamed for THIS message — the consolidated
    /// `assistant` text block is then suppressed (the finalizer APPENDS, so
    /// re-emitting would double the persisted reply). Reset per `message_start`.
    streamed_text: bool,
    /// Symmetric guard for thinking. NB: Bedrock never emits `thinking_delta`
    /// (LIVE-probed) so this stays false there and thinking falls back to the
    /// consolidated frame (empty on Bedrock); direct-API backends may stream it.
    streamed_thinking: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum StreamBlockKind {
    Text,
    Thinking,
    Other,
}

/// The EXACT wire values claude's `--permission-mode` / `set_permission_mode`
/// accept — the SINGLE source for the seed-time whitelist
/// ([`is_valid_claude_permission_mode`]). Feeding claude anything outside this set
/// makes the spawn fail (exit 1), so any value sourced from unconstrained storage
/// (e.g. an assistant default `permission_value`, a free-text TEXT column) MUST be
/// validated against this before it reaches a `--permission-mode` flag.
///
/// These are the 6 canonical `PermissionMode` values the bundled SDK
/// (`@anthropic-ai/claude-agent-sdk` 0.3.156, `sdk.d.ts` `PermissionMode`) and the
/// CLI both accept: `default | acceptEdits | bypassPermissions | plan | dontAsk |
/// auto`. NOTE this whitelist is a SUPERSET of the advertised picker
/// ([`claude_permission_modes`], which omits `auto` — see there): validation must
/// accept `auto` because a session resumed/persisted from the legacy ACP path (which
/// DID advertise `auto` when the model reported `supportsAutoMode`) may carry
/// `current_mode = "auto"`, and downgrading/rejecting it would crash the spawn.
/// The CLI-only alias `manual` is deliberately excluded — we never emit it.
const CLAUDE_PERMISSION_MODE_IDS: [&str; 6] =
    ["default", "acceptEdits", "bypassPermissions", "plan", "dontAsk", "auto"];

/// True iff `mode` is one of claude's exact accepted permission-mode wire values.
/// Use this to guard any seed/persist of `current_mode_id` for a claude session
/// when the source value domain is not already constrained — an invalid value
/// would crash the next spawn (`build_claude_init_args` passes it verbatim to
/// `--permission-mode`).
pub fn is_valid_claude_permission_mode(mode: &str) -> bool {
    CLAUDE_PERMISSION_MODE_IDS.contains(&mode)
}

/// claude's permission-mode picker, advertised as `available_modes` so the UI mode
/// picker has data (the write path — `--permission-mode` /
/// control_request{set_permission_mode} — was already wired). The `id`s are the
/// EXACT wire values claude accepts; name/description are display copy.
///
/// VERBATIM-EQUIVALENT to the legacy ACP bridge's `buildAvailableModes(modelInfo)`
/// (`@agentclientprotocol/claude-agent-acp` 0.39.0, `acp-agent.js`): same ids, same
/// names, same descriptions, same order — so the frontend (which renders whatever
/// `available_modes` the backend sends, i18n-keyed on `agentMode.<id>`) receives
/// byte-identical data and needs zero change (the claude arm of feature-012 "Plan B").
///
/// Two of the bridge's six modes are conditionally advertised; we reproduce the
/// condition, not just the mode list:
///
/// - `auto` — bridge gates it on `modelInfo?.supportsAutoMode === true` (recomputed
///   on model switch). The direct-CLI `initialize`/`system.init` response carries NO
///   `supportsAutoMode` for ANY model (live-verified across default/sonnet/opus/
///   haiku/fable-5; SDK 0.3.156 passes CLI models through without synthesizing the
///   field), and [`ModelInfo`] has no such field to thread. So legacy-exact behavior
///   under the current CLI = `auto` is NEVER advertised. We OMIT it here rather than
///   emit dead structure. `is_valid_claude_permission_mode` still ACCEPTS `auto`
///   (superset) so a session that carries it (legacy-persisted / future CLI) does not
///   crash the spawn. TRIPWIRE: if a future CLI starts reporting `supportsAutoMode`,
///   this omission becomes a real divergence — thread the current model's flag in and
///   prepend the `auto` entry below.
/// - `bypassPermissions` — bridge gates on `ALLOW_BYPASS` (`!IS_ROOT ||
///   $IS_SANDBOX`), i.e. shown for every non-root user. We advertise it
///   unconditionally (per product decision: bypass always selectable), which matches
///   the legacy path for the non-root desktop user it actually runs as.
///
/// Unlike models/effort these are static (no `initialize`-response discovery).
pub(crate) fn claude_permission_modes() -> Vec<ModeInfo> {
    // `auto` is intentionally absent — see the doc comment (gated on
    // supportsAutoMode, which the direct CLI never reports). Order and copy match the
    // bridge's buildAvailableModes verbatim for the remaining five.
    [
        (
            "default",
            "Default",
            "Standard behavior, prompts for dangerous operations",
        ),
        ("acceptEdits", "Accept Edits", "Auto-accept file edit operations"),
        ("plan", "Plan Mode", "Planning mode, no actual tool execution"),
        (
            "dontAsk",
            "Don't Ask",
            "Don't prompt for permissions, deny if not pre-approved",
        ),
        (
            "bypassPermissions",
            "Bypass Permissions",
            "Bypass all permission checks",
        ),
    ]
    .into_iter()
    .map(|(id, name, description)| ModeInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: Some(description.to_string()),
    })
    .collect()
}

/// Mint the per-kind streaming item_id from a claude `message.id`. claude shares
/// ONE `message.id` across a turn's thinking AND text blocks; suffixing by kind
/// routes each to its own finalizer buffer (→ separate blocks, no thinking↔text
/// leak), mirroring the ACP/aionrs per-kind id convention. `Other` has no
/// streaming path, so it maps to the bare id (never used as a delta item_id).
fn stream_item_key(item_id: &str, kind: StreamBlockKind) -> String {
    match kind {
        StreamBlockKind::Text => format!("{item_id}:text"),
        StreamBlockKind::Thinking => format!("{item_id}:think"),
        StreamBlockKind::Other => item_id.to_string(),
    }
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain complete NDJSON lines from the half-line buffer, yielding one
    /// `(Option<Value>, Vec<SessionEvent>)` per line. The byte→`Value` parse
    /// happens EXACTLY ONCE per line here — this is the parse-once seam the F1
    /// dual-fanout (`ClaudeFanoutParser`) wraps: it derives its a-side
    /// (`AgentStreamEvent`) from the SAME `Value` returned here, while the
    /// b-side (`SessionEvent`) is this method's `parse_value` output, so the
    /// frame→event mapping lives in ONE place (I8, no cross-crate drift).
    ///
    /// `Value` is `None` for a malformed line (no a-side projection possible) —
    /// the b-side still gets the `AdapterSpecific` escape hatch. The half-line
    /// buffer persists across calls AND across result spans (persistent
    /// multi-turn; the process's stdout does not EOF between turns).
    pub fn frame_lines(&mut self, bytes: &[u8]) -> Vec<(Option<Value>, Vec<SessionEvent>)> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            // Lossy is fine: malformed UTF-8 → AdapterSpecific via the serde
            // failure path below; we never index mid-char.
            let line = String::from_utf8_lossy(&line_bytes);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => {
                    let events = self.parse_value(&v);
                    out.push((Some(v), events));
                }
                Err(_) => {
                    // malformed line: opaque escape hatch, never panic. No
                    // Value ⇒ a-side has nothing to project (the FanoutParser
                    // emits its own System fallback).
                    out.push((
                        None,
                        vec![SessionEvent::AdapterSpecific {
                            tag: "malformed_json".to_string(),
                            payload: Value::String(trimmed.to_string()),
                        }],
                    ));
                }
            }
        }
        out
    }

    /// 009 R1a: drain a trailing half-line (bytes with no terminating `\n`) at
    /// EOF. `frame_lines` only yields complete `\n`-terminated lines, so a final
    /// frame truncated mid-write — e.g. an OOM/SIGKILL during the `result` line —
    /// otherwise sits in `buf` forever and is silently lost (the turn is then
    /// misclassified as crashed/empty and its last content vanishes). The reader
    /// MUST call this once after its read loop breaks (EOF), before emitting
    /// `Detached`, and run the same sniff/emit processing on the result. Returns
    /// empty when the buffer holds nothing parseable (clean EOF on a `\n`
    /// boundary, or only whitespace). The buffer is consumed either way.
    pub fn flush_tail(&mut self) -> Vec<(Option<Value>, Vec<SessionEvent>)> {
        let tail: Vec<u8> = std::mem::take(&mut self.buf);
        let line = String::from_utf8_lossy(&tail);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => {
                let events = self.parse_value(&v);
                vec![(Some(v), events)]
            }
            // A genuinely truncated final frame is not valid JSON → opaque escape
            // hatch (never panic), same as a malformed mid-stream line. The
            // crash discriminator still sees a Detached afterward.
            Err(_) => vec![(
                None,
                vec![SessionEvent::AdapterSpecific {
                    tag: "truncated_tail".to_string(),
                    payload: Value::String(trimmed.to_string()),
                }],
            )],
        }
    }

    /// Map one already-parsed frame `Value` into zero or more canonical events.
    /// A single claude frame (e.g. an assistant message with several content
    /// blocks) can yield several `SessionEvent`s; unknown frames yield one
    /// `AdapterSpecific`. Never panics (I4). This is the SOLE frame→SessionEvent
    /// mapping (the b-side of the seam); it takes a `&Value` so the same parsed
    /// frame can feed the a-side without re-parsing.
    fn parse_value(&mut self, v: &Value) -> Vec<SessionEvent> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "system" => self.parse_system(v),
            "assistant" => self.parse_assistant(v),
            "user" => self.parse_user(v),
            "result" => self.parse_result(v),
            // F3 control channel (--permission-prompt-tool stdio). A
            // `control_request` whose `request.subtype == can_use_tool` needs a
            // user answer → `Permission{request_id}` (the reducer ref-counts; the
            // tool/questions detail rides the a-side card). `control_cancel_request`
            // retracts a pending one → `PermissionResolved{request_id}`. Other
            // control subtypes (keep_alive / streamlined_* / elicitation) carry no
            // FSM signal → opaque (the manager declines elicitation on the a-side).
            "control_request" => self.parse_control_request(v),
            "control_cancel_request" => self.parse_control_cancel_request(v),
            // R5 (009): `--include-partial-messages` wraps the streaming Anthropic
            // events in `stream_event`. The ONE we read is `message_delta`, whose
            // `delta.stop_reason` is the REAL-TIME per-turn boundary — it lands as
            // soon as a turn's reply finishes, vs the `result` frame which claude
            // headless defers to all-background-tasks-done (a workflow pins it 60s+,
            // §2 "unlock signal source"). Everything else (message_start / content_block_* /
            // message_stop) carries no FSM signal — the regular `assistant`/`user`
            // frames already deliver the content — so they stay opaque.
            "stream_event" => self.parse_stream_event(v),
            // Task 2 (mid-turn interjection observability): claude echoes back the
            // uuid WE minted on the user frame, reporting where it landed in the
            // turn lifecycle (verified 2.1.226, design spec §6.1).
            "command_lifecycle" => self.parse_command_lifecycle(v),
            // unknown top-level type → opaque catch-all (I8/I13, never panic).
            other => vec![SessionEvent::AdapterSpecific {
                tag: other.to_string(),
                payload: v.clone(),
            }],
        }
    }

    /// system frames: `init` carries no state signal; `api_retry` and the
    /// compaction milestones normalize to the backend-neutral `Heartbeat`;
    /// anything else is opaque.
    fn parse_system(&mut self, v: &Value) -> Vec<SessionEvent> {
        match v.get("subtype").and_then(Value::as_str).unwrap_or("") {
            "init" => {
                // Carries the live model id; latch it for `result_context_window`.
                if let Some(model) = v.get("model").and_then(Value::as_str) {
                    self.current_model = Some(model.to_owned());
                }
                Vec::new()
            }
            "compact_boundary" => {
                // A compaction rewrites the context, so the occupancy latched off
                // the last API call is now stale — and the compaction turn itself
                // streams NO `message_delta` to refresh it (live-captured: five
                // turns climbing to 27_349, then `compact_boundary{pre:27349,
                // post:3158}` followed by a `result` with `num_turns:0` and a zero
                // aggregate, then the next real turn reporting 23_779).
                //
                // Without this reset the stale 27_349 would survive the fallback
                // and sail past the zero-usage guard, re-publishing a
                // pre-compaction figure as if it were current. Dropping it lets the
                // compaction's zero aggregate be discarded as intended, so the
                // indicator holds its previous reading until the next real turn
                // measures the new context.
                //
                // The boundary's own `post_tokens` cannot substitute: it counts the
                // compacted transcript only (3_158 here) while occupancy also
                // carries the system prompt and tools — the next turn measured
                // 23_779, ~20k higher. Different baselines.
                self.last_call_tokens = None;
                vec![SessionEvent::Heartbeat]
            }
            // network backoff + the no-chunk compaction window both = liveness.
            "api_retry" | "compacting" => vec![SessionEvent::Heartbeat],
            other => vec![SessionEvent::AdapterSpecific {
                tag: format!("system/{other}"),
                payload: v.clone(),
            }],
        }
    }

    /// assistant frames carry a `message.content` array of blocks
    /// (text / thinking / tool_use). The message `id` is used as item_id.
    fn parse_assistant(&mut self, v: &Value) -> Vec<SessionEvent> {
        let msg = v.get("message");
        let item_id = msg
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 009 H5: the top-level frame's `parent_tool_use_id` (sibling of `message`)
        // attributes this frame's content to a SUBAGENT's turn; `None`/absent = the
        // main agent. Read once here, threaded into each emitted ToolCall so the
        // conversation TurnFinalizer can hang the tool step under the right node.
        let parent_tool_use_id = v.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let blocks = msg
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // P3 dedup: whether the in-flight message's text/thinking already streamed
        // live as `content_block_delta`s. The consolidated frame's matching block is
        // then suppressed (the finalizer APPENDS deltas, so re-emitting the full block
        // would double the persisted reply). The streamed flags belong to THIS message
        // (`stream.item_id`); a consolidated frame for a different/earlier message
        // (no matching live stream) falls through to the emit path so nothing is lost
        // (resume turns, or a backend that did not stream). Capture them before the
        // loop so the per-block `out.push` paths read a stable view.
        let streamed_text = self.stream.item_id == item_id && self.stream.streamed_text;
        let streamed_thinking = self.stream.item_id == item_id && self.stream.streamed_thinking;
        let text_key = stream_item_key(&item_id, StreamBlockKind::Text);
        let think_key = stream_item_key(&item_id, StreamBlockKind::Thinking);

        let mut out = Vec::new();
        for b in &blocks {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                // Suppress the consolidated text block if it already streamed live
                // (else fall through and emit the whole block — resume / no-stream path).
                "text" if streamed_text => {}
                "text" => out.push(SessionEvent::MessageDelta {
                    // Per-kind item_id so text and thinking land in SEPARATE finalizer
                    // buffers (claude shares one message.id across both blocks).
                    item_id: text_key.clone(),
                    text: b.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                }),
                "thinking" if streamed_thinking => {}
                // A thinking block whose plaintext is absent carries only its
                // encrypted `signature` — that is what the CLI emits whenever
                // thinking display resolves to `omitted` (verified: claude 2.1.220,
                // capture with/without `--thinking-display summarized`). An empty
                // ThoughtDelta becomes a thinking card that expands to nothing, so
                // drop it. The streaming path (parse_stream_event) already skips
                // empty `thinking_delta`s; this mirrors that guard on the
                // consolidated frame, which previously lacked it.
                "thinking" => {
                    let text = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                    if !text.is_empty() {
                        out.push(SessionEvent::ThoughtDelta {
                            item_id: think_key.clone(),
                            text: text.to_string(),
                        });
                    }
                }
                "tool_use" => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    // #486 (parity with aionrs output_sink): DROP a malformed empty-name
                    // tool_use before it reaches persistence. claude occasionally emits a
                    // tool_use block with a missing/blank `name`; emitting it produces a
                    // nameless `tool_step{name:""}` row that renders as a ghost tool line
                    // (LIVE-observed 2026-06-22). A blank name carries no actionable info,
                    // so suppress the ToolCall entirely (the paired tool_result, keyed on
                    // tool_use_id, then finds no call and is inert). warn, not info: this
                    // is malformed-but-handled upstream data (AGENTS.md); never log `input`.
                    if name.trim().is_empty() {
                        tracing::warn!(item_id = %item_id, "claude tool_use has an empty name; dropping malformed call");
                    } else {
                        out.push(SessionEvent::ToolCall {
                            tool_use_id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                            name,
                            // 002/F1 single-agent path: inline tool. subagent topology
                            // (Task/Workflow → Spawned/Workflow) is the new ClaudeConnection's
                            // job (007 §9.14); this legacy adapter stays Inline.
                            subagent: crate::event::SubagentKind::Inline,
                            // Gap #4 / H2: carry the tool ARGUMENTS (Anthropic `input` object).
                            // Absent → Value::Null. TIO-13: never logged at info.
                            input: b.get("input").cloned().unwrap_or(Value::Null),
                            // 009 H5: attribute to the subagent's turn (frame-level), main = None.
                            parent_tool_use_id: parent_tool_use_id.clone(),
                        });
                    }
                }
                // unknown block type: opaque, never panic.
                other => out.push(SessionEvent::AdapterSpecific {
                    tag: format!("assistant/{other}"),
                    payload: b.clone(),
                }),
            }
        }
        out
    }

    /// user frames carry synthesized `tool_result` blocks, referring back by
    /// tool_use_id.
    fn parse_user(&self, v: &Value) -> Vec<SessionEvent> {
        // 009 H5: same top-level attribution as parse_assistant — a subagent's
        // tool_result frame carries the parent's tool_use_id beside `message`.
        let parent_tool_use_id = v.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let blocks = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for b in &blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                out.push(SessionEvent::ToolResult {
                    tool_use_id: b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    // 009 R7/H3: the wire block carries is_error on a failed/rejected
                    // tool (default false = success). Carrying it keeps a red tool red.
                    is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    // 009 R8: carry the tool OUTPUT the parser used to drop. claude's
                    // tool_result.content is polymorphic — a String (e.g. Bash stdout)
                    // or an Array of blocks (text / a base64 `image` block, e.g. a
                    // Read-tool image). A generated image is written to disk by the
                    // tool, so it surfaces only as text here (path in the text).
                    content: parse_tool_result_content(b.get("content")),
                    // 009 H5: attribute the result to the subagent's turn, main = None.
                    parent_tool_use_id: parent_tool_use_id.clone(),
                });
            }
            // non-tool_result user blocks are ignored (not P0-relevant).
        }
        out
    }

    /// terminal result. Routing basis = `is_error` (NEVER subtype, which stays
    /// "success" even on error). `api_error_status` and `result` text are
    /// normalized to backend-neutral typed fields.
    ///
    /// `result_text` source (U11), error-terminal fallback chain — prefer the
    /// `result` field; then the human-readable `errors[]` array; then (LAST) the
    /// `subtype` token itself. The final `subtype` fallback is essential for a
    /// failed `--resume`: claude 2.1.168 emits `result{subtype:
    /// "error_during_execution", is_error:true}` with NO `result` and NO
    /// `errors[]` — the real cause ("No conversation found …") is on STDERR,
    /// which this frame-parser cannot see. Without the subtype fallback the
    /// message would be EMPTY → the reducer's `Error{Backend{message:""}}` →
    /// the crash-resume self-heal (`is_unrecoverable_resume_error`) could not
    /// detect it → permanent resume wedge. Mirrors the official ACP adapter,
    /// which uses `errors.join() || subtype` for `error_during_execution`.
    ///
    /// The `subtype` fallback is gated on `is_error:true` — a SUCCESS turn must
    /// keep an empty result_text (an `is_error:false` empty turn carries
    /// `subtype:"success"`, and leaking that token would break the reducer's
    /// EmptyTurn detection, which keys on `result_text.is_empty()`).
    /// A `result` frame → the terminal `TurnResult` PLUS (C-2) a `UsageDelta` when
    /// the frame carries `usage` (claude direct-CLI puts token usage + cost INLINE
    /// on the same result frame). Returns a Vec so the usage rides alongside the
    /// terminal — codex already emits UsageDelta (map_usage); this closes the
    /// claude/codex asymmetry. The wrapping ClaudeConnection inherits both for free.
    fn parse_result(&mut self, v: &Value) -> Vec<SessionEvent> {
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let result_text = match v.get("result").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let from_errors = v
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            // Drop claude's internal `[ede_diagnostic] …` template — it
                            // is a CLI-internal debug string (result_type/last_content_type/
                            // stop_reason), NOT a user-readable error. claude emits it in
                            // `errors[]` for an abnormal-but-contentless terminal (e.g. a
                            // turn interrupted before any assistant output), so without
                            // this filter a cancel surfaced "[ede_diagnostic] …" verbatim
                            // as the error message. Precise prefix match on claude's own
                            // tag — genuine user-readable errors[] entries are kept.
                            .filter(|e| !e.trim_start().starts_with("[ede_diagnostic]"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                match (from_errors.is_empty(), is_error) {
                    // result + errors both empty AND this is an ERROR terminal →
                    // last resort: the structural failure token (e.g.
                    // "error_during_execution"), so the message is non-empty and
                    // the self-heal can detect a stderr-only resume failure.
                    (true, true) => v.get("subtype").and_then(Value::as_str).unwrap_or("").to_string(),
                    // errors present → use it; OR a SUCCESS terminal → stay empty
                    // (never leak "success" into result_text; EmptyTurn needs it
                    // empty).
                    _ => from_errors,
                }
            }
        };
        let mut out = vec![SessionEvent::TurnResult {
            is_error,
            api_error_status: v.get("api_error_status").and_then(Value::as_u64).map(|n| n as u16),
            result_text,
            // Epoch-agnostic: the adapter parses one frame with no turn context.
            // The ai-agent reader stamps the live epoch when forwarding the b-side
            // event (run_session read_loop). 0 = "unstamped" (settles normally for
            // backends that never cancel mid-turn).
            epoch: 0,
            // C-1 (007 §C2/O3): map claude's terminal to the typed outcome. Prefer
            // the rich `terminal_reason` (12-value enum); fall back to `stop_reason`.
            outcome: Self::result_outcome(v),
        }];
        // C-2: emit a UsageDelta when the result carries `usage`. claude usage is
        // snake_case {input_tokens, output_tokens, cache_creation_input_tokens,
        // cache_read_input_tokens}; there is no `total_tokens` field, so compute it.
        // H4 FIX (race/scenario audit): the true total is base input + output PLUS
        // BOTH cache buckets — cache_read/cache_creation ARE billed input tokens, so
        // omitting them under-reported the total ~10x on a cache-heavy turn (and was
        // inconsistent with codex, whose native `last.totalTokens` already includes
        // cache). `input_tokens`/`output_tokens` stay the wire's base counts; only
        // `total_tokens` becomes the genuine total. cost from total_cost_usd —
        // the RAW wire value, which is only PROCESS-cumulative (a `--resume`
        // respawn restarts it); the wrapping conn's CostLedger re-bases it to
        // session-cumulative before broadcast. The adapter stays a pure frame
        // parser with no cross-process state.
        if let Some(usage) = v.get("usage").and_then(Value::as_object) {
            let get = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
            let input_tokens = get("input_tokens");
            let output_tokens = get("output_tokens");
            let cache_creation = get("cache_creation_input_tokens");
            let cache_read = get("cache_read_input_tokens");
            // CONTEXT OCCUPANCY, not the turn's billed volume. `result.usage`
            // aggregates every API call in the turn, so summing its buckets
            // overstates occupancy by roughly the number of tool rounds (measured
            // 107_974 vs a true 27_467 on a four-call turn) and pushed the usage
            // indicator past 100%. Prefer the last call's own usage, tracked off
            // `message_delta`. The aggregate remains the fallback for a turn that
            // streamed no `message_delta` at all (no `--include-partial-messages`,
            // or a resumed/consolidated frame); it is exact for a single-call turn
            // and only overstates once tool rounds appear.
            let total_tokens = self.last_call_tokens.unwrap_or_else(|| {
                usage
                    .get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(input_tokens + output_tokens + cache_creation + cache_read)
            });
            out.push(SessionEvent::UsageDelta {
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
                context_window: self.result_context_window(v),
                // Per-turn detail line. These stay on the AGGREGATE `result.usage`
                // (unlike `total_tokens`, which must be the last call's occupancy):
                // the breakdown answers "what did this turn cost", so counting every
                // API call is the correct reading.
                breakdown: crate::event::UsageBreakdown {
                    cached_read_tokens: cache_read,
                    cached_write_tokens: cache_creation,
                    thought_tokens: std::mem::take(&mut self.turn_thought_tokens),
                },
            });
        }
        out
    }

    /// The model's total context window from a `result` frame, for the usage
    /// indicator's denominator. It rides `modelUsage.<model>.contextWindow`
    /// (live-captured, claude 2.1.220: `{"claude-opus-5":{…,"contextWindow":1000000}}`).
    ///
    /// The entry cannot be selected by model id — the frame's own `model` field is
    /// `null` on the wire. One entry (the observed shape) is used directly; when a
    /// turn touched several models (a subagent on a different one) the DOMINANT
    /// entry by `inputTokens + outputTokens` wins, i.e. the main conversation model.
    /// That tie-break is a heuristic: only the single-entry shape has been captured.
    /// Absent / non-integer → `None`, which renders as a counter with no percentage.
    fn result_context_window(&self, v: &Value) -> Option<u64> {
        let entries = v.get("modelUsage")?.as_object()?;
        // Prefer the model claude says it is CURRENTLY running. `modelUsage` is
        // session-CUMULATIVE (live-measured: `cacheReadInputTokens` 18_384 after
        // turn 1, 44_791 after turn 2), so after a model switch the outgoing
        // model's accumulated volume still dwarfs the new one's first turn — a
        // volume ranking would pin the window to the old model forever. That is
        // exactly the reported bug: switching opus -> haiku kept showing 1M
        // instead of 200k. `system{subtype:"init"}` announces the live model and
        // is re-emitted on a switch, spelled identically to these keys.
        if let Some(current) = self.current_model.as_deref()
            && let Some(window) = entries
                .get(current)
                .and_then(|m| m.get("contextWindow"))
                .and_then(Value::as_u64)
        {
            return Some(window);
        }
        // Fallback for a session that never announced a model (or announced one
        // absent from `modelUsage`): the heaviest entry, cache included, since the
        // main conversation model is the one carrying the history as cache.
        entries
            .values()
            .filter_map(|m| {
                let window = m.get("contextWindow").and_then(Value::as_u64)?;
                // Weigh by the FULL volume including both cache buckets. A
                // `modelUsage` entry's `inputTokens`/`outputTokens` EXCLUDE cache,
                // and the main conversation model is precisely the one carrying the
                // history as cache: live-captured, the main entry read
                // `inputTokens:10, outputTokens:34, cacheReadInputTokens:27953` —
                // 44 against 27_953. Ranking on in+out alone let any Task subagent
                // doing uncached work outrank it, handing the indicator the
                // subagent's window (e.g. haiku's 200k in place of opus's 1M) and
                // inflating the percentage fivefold.
                let field = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
                let volume = field("inputTokens")
                    + field("outputTokens")
                    + field("cacheReadInputTokens")
                    + field("cacheCreationInputTokens");
                Some((volume, window))
            })
            .max_by_key(|(volume, _)| *volume)
            .map(|(_, window)| window)
    }

    /// C-1: map a claude result frame's `terminal_reason` (preferred, 12-value) or
    /// `stop_reason` to the typed `TurnOutcome`. Unknown/end_turn → EndTurn;
    /// max_tokens/max-turns → Truncated; refusal → Refused; cancelled → Cancelled.
    fn result_outcome(v: &Value) -> crate::event::TurnOutcome {
        use crate::event::{CancelReason, StopReason, TruncationKind, TurnOutcome};
        let reason = v
            .get("terminal_reason")
            .and_then(Value::as_str)
            .or_else(|| v.get("stop_reason").and_then(Value::as_str))
            .unwrap_or("");
        match reason {
            "cancelled" | "aborted_streaming" | "aborted_tools" => TurnOutcome::Cancelled {
                reason: CancelReason::UserCancel,
            },
            "max_tokens" | "prompt_too_long" => TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTokens),
            },
            "max_turns" => TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTurns),
            },
            "refusal" => TurnOutcome::Completed {
                stop_reason: StopReason::Refused { category: None },
            },
            // end_turn / stop_sequence / model_error / hook_* / unknown → EndTurn
            // (errors route via is_error, not outcome).
            _ => TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
        }
    }

    /// R5 (009): parse a `--include-partial-messages` `stream_event` frame. The
    /// only sub-event with an FSM signal is `message_delta`, whose
    /// `event.delta.stop_reason` ends a turn in REAL TIME:
    ///   - `tool_use` → the turn continues (a tool round follows); emit NOTHING,
    ///     so the FSM stays Running. (Emitting a TurnResult here would fold Idle
    ///     mid-turn and let the user "send" into a still-running turn.)
    ///   - `end_turn` / `max_tokens` / `max_turns` / `refusal` (any terminal stop)
    ///     → emit a real-time `TurnResult{is_error:false}` so the FSM folds Idle
    ///     NOW, not when the lagged `result` frame finally arrives. result_text is
    ///     empty (the content already streamed as MessageDelta, which set
    ///     saw_substantive_output, so OUTPUT-PRESENCE folds Idle not EmptyTurn).
    ///     The later `result` TurnResult is harmlessly absorbed by I10 (terminal).
    ///
    /// P3 partial streaming: the content sub-events drive incremental output —
    ///   - `message_start` → record `message.id` as the per-message item_id stem +
    ///     RESET the per-message stream state (block-kind map + streamed flags).
    ///   - `content_block_start` → record `index → kind` (text / thinking / other)
    ///     so the deltas (which carry only `index`) can be routed.
    ///   - `content_block_delta{text_delta}` → emit an incremental `MessageDelta`
    ///     (item_id `<message.id>:text`) — this is the typewriter stream. The
    ///     consolidated `assistant` text block is then suppressed (dedup) so the
    ///     finalizer (which APPENDS) does not double the persisted reply.
    ///     `thinking_delta` → `ThoughtDelta` (`<id>:think`) for direct-API backends;
    ///     Bedrock never emits it (LIVE-probed), so thinking falls back to the
    ///     consolidated frame. `input_json_delta`/`signature_delta` carry no display
    ///     text → emit nothing (tool_use args arrive intact on the `assistant` frame).
    ///   - `content_block_stop` / `message_stop` → no signal, emit nothing.
    ///
    /// The per-kind item_id (`:text`/`:think`) is load-bearing: claude shares ONE
    /// `message.id` across a turn's thinking AND text blocks, so a single-keyed
    /// finalizer buffer would merge them; the suffix routes each kind to its own
    /// buffer → separate blocks, no leak.
    fn parse_stream_event(&mut self, v: &Value) -> Vec<SessionEvent> {
        let event = v.get("event");
        let sub_ty = event.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or("");
        match sub_ty {
            "message_start" => {
                self.stream.item_id = event
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.stream.block_kind.clear();
                self.stream.streamed_text = false;
                self.stream.streamed_thinking = false;
                Vec::new()
            }
            "content_block_start" => {
                if let Some(index) = event.and_then(|e| e.get("index")).and_then(Value::as_u64) {
                    let kind = match event
                        .and_then(|e| e.get("content_block"))
                        .and_then(|b| b.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                    {
                        "text" => StreamBlockKind::Text,
                        "thinking" => StreamBlockKind::Thinking,
                        _ => StreamBlockKind::Other,
                    };
                    self.stream.block_kind.insert(index, kind);
                }
                Vec::new()
            }
            "content_block_delta" => {
                let index = event.and_then(|e| e.get("index")).and_then(Value::as_u64);
                let kind = index.and_then(|i| self.stream.block_kind.get(&i).copied());
                let delta = event.and_then(|e| e.get("delta"));
                let delta_ty = delta.and_then(|d| d.get("type")).and_then(Value::as_str).unwrap_or("");
                match (kind, delta_ty) {
                    (Some(StreamBlockKind::Text), "text_delta") => {
                        let text = delta.and_then(|d| d.get("text")).and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            return Vec::new();
                        }
                        self.stream.streamed_text = true;
                        vec![SessionEvent::MessageDelta {
                            item_id: stream_item_key(&self.stream.item_id, StreamBlockKind::Text),
                            text: text.to_string(),
                        }]
                    }
                    (Some(StreamBlockKind::Thinking), "thinking_delta") => {
                        let text = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if text.is_empty() {
                            return Vec::new();
                        }
                        self.stream.streamed_thinking = true;
                        vec![SessionEvent::ThoughtDelta {
                            item_id: stream_item_key(&self.stream.item_id, StreamBlockKind::Thinking),
                            text: text.to_string(),
                        }]
                    }
                    // input_json_delta (tool args) / signature_delta (thinking sig) /
                    // any other → no display text. tool_use args arrive intact on the
                    // consolidated `assistant` frame; we do not reconstruct partials.
                    _ => Vec::new(),
                }
            }
            // No FSM signal, now an EXPECTED frame (not unknown) → emit nothing.
            "content_block_stop" | "message_stop" => Vec::new(),
            "message_delta" => {
                // Each `message_delta` closes ONE API call and carries that call's
                // complete usage. The last one of the turn is therefore the current
                // context occupancy — the figure the usage indicator needs, and the
                // one `result.usage` cannot give (it sums every call; see
                // `last_call_tokens`). Live-captured tail of a four-call turn:
                // `{input_tokens:2, cache_creation:79, cache_read:27386, output:5}`
                // = 27_472, against an aggregate of 107_974.
                if let Some(usage) = event.and_then(|e| e.get("usage")).and_then(Value::as_object) {
                    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let occupancy = field("input_tokens")
                        + field("cache_creation_input_tokens")
                        + field("cache_read_input_tokens")
                        + field("output_tokens");
                    if occupancy > 0 {
                        self.last_call_tokens = Some(occupancy);
                    }
                    self.turn_thought_tokens += usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("thinking_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
                let stop_reason = event
                    .and_then(|e| e.get("delta"))
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match stop_reason {
                    // Turn continues — a tool round follows. No fold.
                    "" | "tool_use" => Vec::new(),
                    // Any terminal stop_reason → real-time turn end. Map via the shared
                    // outcome helper (reads `delta.stop_reason`); empty result_text
                    // (content streamed already), is_error:false (errors arrive on the
                    // result frame).
                    _ => vec![SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: String::new(),
                        epoch: 0,
                        outcome: Self::result_outcome(event.and_then(|e| e.get("delta")).unwrap_or(&Value::Null)),
                    }],
                }
            }
            // genuinely unknown sub-type → opaque escape hatch (I8/I13, never panic).
            _ => vec![SessionEvent::AdapterSpecific {
                tag: format!("stream_event/{sub_ty}"),
                payload: v.clone(),
            }],
        }
    }

    /// A `control_request` frame. ONLY the `can_use_tool` subtype that needs a
    /// user decision maps to a b-side `Permission` (ref-count +1, correlation key
    /// `request_id`). Every other control subtype (`keep_alive`,
    /// `streamlined_*`, `elicitation`, an unknown one) carries no FSM signal and
    /// is opaque — the reducer must never block the turn on a non-permission
    /// control frame. `request_id` is the TOP-LEVEL field (distinct from the
    /// nested `request.tool_use_id`); a frame missing it cannot be answered, so
    /// it degrades to opaque rather than wedging a request we can't resolve.
    fn parse_control_request(&mut self, v: &Value) -> Vec<SessionEvent> {
        let subtype = v
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let request_id = v.get("request_id").and_then(Value::as_str).unwrap_or("");
        match (subtype, request_id.is_empty()) {
            ("can_use_tool", false) => {
                let request = v.get("request");
                let tool_name = request
                    .and_then(|r| r.get("tool_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // AskUserQuestion is NOT a permission — it is the agent asking the
                // USER a structured question. Route it to its own `Ask` event
                // (own counter, own answer command; 2026-08-04 ruling). Remember
                // the request_id so a later control_cancel_request resolves the
                // matching counter, not the approval one.
                if tool_name.as_deref() == Some("AskUserQuestion") {
                    let questions = request
                        .and_then(|r| r.get("input"))
                        .and_then(|i| i.get("questions"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.ask_requests.insert(request_id.to_string());
                    return vec![SessionEvent::Ask {
                        request_id: request_id.to_string(),
                        questions,
                    }];
                }
                // Carry the raw `input` for EVERY tool. The permission card must show
                // the user WHAT they are approving (a Bash `command`, a Write target)
                // — with only `tool_name` the card reads "命令: Bash" and the user
                // approves blind (AionUi issue #3779). This is display-to-the-approver,
                // not logging: TIO-13 ("never log tool input at info") still holds, and
                // the SAME `input` already reaches the frontend via the assistant
                // tool_use block (ToolBegin carries it), so the card adds no new
                // exposure. For AskUserQuestion the `{questions:[…]}` payload
                // additionally drives the real question card.
                let input = request.and_then(|r| r.get("input")).cloned();
                vec![SessionEvent::Permission {
                    request_id: request_id.to_string(),
                    // 007 §9.17: claude can_use_tool is a TOOL approval (not auth).
                    kind: crate::event::PermissionKind::Tool,
                    // G3 auto-approval is ACP-only; no team-MCP approval flows here, so
                    // no allowlist context to carry. (Under the production `default`
                    // permission mode, claude raises can_use_tool for routine tools too
                    // — Write/Bash PROMPT, LIVE 2.1.191 — not only AskUserQuestion; this
                    // arm correctly surfaces a Permission for ANY can_use_tool.)
                    metadata: None,
                    tool_name,
                    input,
                }]
            }
            _ => vec![SessionEvent::AdapterSpecific {
                tag: format!("control_request/{subtype}"),
                payload: v.clone(),
            }],
        }
    }

    /// A `control_cancel_request` frame: claude retracts a pending permission.
    /// Maps to `PermissionResolved{request_id}` (ref-count -1) so the
    /// requires-action sub-state can clear without a user answer. The host sends
    /// NO control_response for a cancel (the request is gone); the manager drops
    /// the pending card on the a-side.
    fn parse_control_cancel_request(&mut self, v: &Value) -> Vec<SessionEvent> {
        match v.get("request_id").and_then(Value::as_str) {
            // A retracted AskUserQuestion resolves the QUESTION counter — emitting
            // PermissionResolved here would wedge waiting_on_question at >0 forever.
            Some(id) if !id.is_empty() && self.ask_requests.remove(id) => {
                vec![SessionEvent::AskResolved {
                    request_id: id.to_string(),
                }]
            }
            Some(id) if !id.is_empty() => vec![SessionEvent::PermissionResolved {
                request_id: id.to_string(),
                // claude control_cancel_request retracts a TOOL approval (§9.17).
                kind: crate::event::PermissionKind::Tool,
            }],
            _ => vec![SessionEvent::AdapterSpecific {
                tag: "control_cancel_request".to_string(),
                payload: v.clone(),
            }],
        }
    }

    /// A `command_lifecycle` frame: claude echoing back the `uuid` WE minted on
    /// a user frame, reporting where it landed (§6.1). `command_uuid` is that
    /// SAME correlation key; missing it degrades to no event (never guess an id).
    fn parse_command_lifecycle(&self, v: &Value) -> Vec<SessionEvent> {
        let Some(id) = v.get("command_uuid").and_then(Value::as_str) else {
            return Vec::new();
        };
        // An unrecognised phase degrades to NO event rather than a guess: this
        // frame is additive and claude may grow states we have not probed.
        let phase = match v.get("state").and_then(Value::as_str) {
            Some("queued") => crate::event::MessageLifecyclePhase::Queued,
            Some("started") => crate::event::MessageLifecyclePhase::Started,
            Some("completed") => crate::event::MessageLifecyclePhase::Completed,
            Some("cancelled") | Some("canceled") => crate::event::MessageLifecyclePhase::Cancelled,
            _ => return Vec::new(),
        };
        vec![SessionEvent::MessageLifecycle {
            client_msg_id: id.to_string(),
            phase,
        }]
    }
}

#[async_trait::async_trait]
impl BackendAdapter for ClaudeAdapter {
    async fn start_turn(
        &self,
        spawner: &dyn Spawner,
        session: &SessionSpec,
        cwd: Option<&str>,
        extra_args: &[String],
        env: &[aionui_common::EnvVar],
        cli_program: Option<&std::path::Path>,
    ) -> Result<Box<dyn AgentIo>, ProcessError> {
        // Feature 004 R2/D2: persistent stream-json-input process (multi-turn).
        // The prompt is NOT a spawn arg — it is delivered per-turn over stdin via
        // `deliver_prompt`. D7: bare `claude` is PATH-resolved by the 001 Builder.
        //
        // SessionSpec → the session flag (R16/D12/S17, live-probed): a fresh id
        // uses `--session-id`; a resume uses `--resume` (reusing `--session-id`
        // for an existing id hard-errors `already in use`, so continuation is
        // ONLY via `--resume`).
        let mut args = vec![
            "--print".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            // R5 (009): stream partial messages so the per-turn boundary arrives
            // in REAL TIME as `stream_event{message_delta{stop_reason}}`. Without
            // this flag the only turn-end signal is the `result` frame, which
            // claude headless defers until ALL background tasks finish — a
            // Workflow pins it 60s+, so the FSM (which folds Idle on TurnResult)
            // stays locked through the whole flight period (§2 unlock signal source). The
            // adapter maps message_delta{end_turn} → a real-time TurnResult; the
            // later result TurnResult is absorbed by the reducer's I10.
            "--include-partial-messages".to_string(),
            "--replay-user-messages".to_string(),
            // NOTE: `--thinking-display summarized` (ask for PLAINTEXT thinking —
            // current models default the display to `omitted` and then stream
            // signature-only thinking blocks) is NOT built here. It is version-gated
            // and therefore supplied by the manager through `extra_args`
            // (aionui-ai-agent `claude_flags`): the CLI rejects an unknown option at
            // parse time, so passing it to an older PATH-resolved claude would kill
            // the spawn outright. This crate is self-contained (aionui-process +
            // aionui-common only) and cannot probe `--version` itself.
            // Feature 004 F3: enable the bidirectional control channel. With
            // `--permission-prompt-tool stdio`, claude emits a `control_request`
            // (subtype `can_use_tool`) on stdout and BLOCKS until the host writes
            // a matching `control_response` to stdin. Under the PRODUCTION
            // `--permission-mode default` (the fail-closed spawn default,
            // build_claude_init_args), routine tools (Write/Bash) PROMPT via
            // can_use_tool too — LIVE 2.1.191, NOT only AskUserQuestion (an earlier
            // note claimed routine tools auto-run; that was the 2.1.168 bypass-mode
            // behavior, stale for the default mode we now ship). Our reader surfaces
            // a Permission for ANY can_use_tool, so all are answerable. Without this
            // flag claude auto-stubs AskUserQuestion and the user can never answer.
            "--permission-prompt-tool".to_string(),
            "stdio".to_string(),
        ];
        match session {
            SessionSpec::Fresh(id) => {
                args.push("--session-id".to_string());
                args.push(id.clone());
            }
            SessionSpec::Resume(id) => {
                args.push("--resume".to_string());
                args.push(id.clone());
            }
            SessionSpec::ForkFrom(id) => {
                // Resume the PARENT session but copy it into a NEW one instead
                // of appending (`--fork-session`, live help 2.1.221). claude
                // mints the new id itself and echoes it in `system/init` —
                // `--session-id` must NOT be combined here.
                args.push("--resume".to_string());
                args.push(id.clone());
                args.push("--fork-session".to_string());
            }
        }
        // Manager-supplied flags (S18: --system-prompt / --mcp-config /
        // --strict-mcp-config). Kept backend-neutral: the adapter does not build
        // them, only appends them.
        args.extend(extra_args.iter().cloned());

        let spec = CommandSpec {
            // Orchestration-resolved bundled CLI (packaged app) or bare "claude"
            // (dev → PATH). See SessionConfig.cli_program.
            command: cli_program.map(|p| p.to_path_buf()).unwrap_or_else(|| "claude".into()),
            args,
            // #103: provider env injected by the orchestration layer (e.g. cc-switch
            // ANTHROPIC_BASE_URL/AUTH_TOKEN for backend == "claude"). Empty =
            // inherit parent env only (pre-#103 byte-identical). The adapter only
            // forwards it — it never reads cc-switch.
            env: env.to_vec(),
            // The conversation workspace: claude runs (and its file tools
            // operate) here, AND claude keys its on-disk session by cwd — so a
            // later `--resume` only finds the session when respawned with the
            // SAME cwd. Threading the workspace here is what makes cross-process
            // resume (idle-reap / backend-restart respawn) actually work.
            cwd: cwd.map(str::to_owned),
        };
        // S14: spawn via the INJECTED spawner (never raw-spawn). opaque_owner_tag
        // is passed through verbatim; P0 uses a static tag.
        let proc = spawner.spawn(spec, &[], "aionui-session").await?;
        Ok(Box::new(ManagedProcessIo::new(proc)))
    }

    async fn deliver_prompt(
        &self,
        stdin: &mut BoxedStdin,
        content: &[crate::ContentBlock],
        client_msg_id: Option<&str>,
    ) -> Result<(), ProcessError> {
        use crate::ContentBlock;
        use base64::Engine as _;

        // S16: build the user line with serde_json over TYPED values (never
        // format!/interpolation) so any \n / " / \ / unicode is escaped into
        // exactly one NDJSON frame. The whole multimodal block slice collapses
        // into ONE user frame's `content[]` array (a raw-newline-bearing text
        // element must not split the frame).
        let blocks: Vec<serde_json::Value> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(json!({ "type": "text", "text": t })),
                // Native base64 image block — the ONLY inline non-text modality
                // headless claude accepts. The `source` wrapper is REQUIRED; a flat
                // {type:image,data,...} is rejected (error_during_execution).
                // Pinned: protocols/samples/claude-cli/2.1.177/image_input_frame.OK.json
                ContentBlock::Image { data, media_type } => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                    Some(json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": media_type, "data": b64 }
                    }))
                }
                // claude has no working document/resource INPUT block, so a file is
                // delivered by REFERENCE: a text element naming the path, which
                // claude's Read tool fetches from cwd (the file must be reachable
                // from the spawn cwd / an --add-dir root).
                ContentBlock::ResourceLink { uri, .. } => {
                    Some(json!({ "type": "text", "text": format!("[Attached file: {uri}]") }))
                }
                // Audio / AtMention are not advertised in prompt_blocks, so dispatch
                // rejects them before we get here — defensive skip.
                _ => None,
            })
            .collect();
        let mut line = json!({
            "type": "user",
            "message": { "role": "user", "content": blocks }
        });
        // Stamp OUR correlation id as the frame's `uuid`. claude echoes it verbatim
        // in the `--replay-user-messages` frame BEFORE that turn's result, which is
        // how the ClaudeConnection reader attributes which client_msg_id a turn
        // consumed (incl. merged/steered turns where N sends map to M<N turns) —
        // see protocols/design/claude-midturn-input-turn-gen-design.md §3.3/§4-B.
        // Omitted when None so the wire shape is byte-identical to pre-correlation.
        if let Some(id) = client_msg_id {
            line["uuid"] = json!(id);
        }
        write_ndjson_line(stdin, &line).await
    }

    async fn write_control_response(
        &self,
        stdin: &mut BoxedStdin,
        value: &serde_json::Value,
    ) -> Result<(), ProcessError> {
        // F3: the caller built the full `{"type":"control_response","response":
        // {"subtype":"success"|"error","request_id":<echo>,...}}` per the frozen
        // contract; we only frame + flush it (same stdin, same NDJSON rules as a
        // user line — single `\n`, do NOT close).
        write_ndjson_line(stdin, value).await
    }

    fn parse_chunk(&mut self, bytes: &[u8]) -> Vec<SessionEvent> {
        // Thin wrapper over the parse-once seam: drop the per-line `Value`
        // (b-side only). `frame_lines` does the byte-buffering + one JSON parse
        // per complete line. The F1 dual-fanout calls `frame_lines` directly to
        // also get the `Value` for its a-side projection.
        self.frame_lines(bytes)
            .into_iter()
            .flat_map(|(_v, events)| events)
            .collect()
    }

    fn capabilities(&self) -> Capabilities {
        // claude is fully parsed and emits all three signals.
        // 007 §C6: the legacy ClaudeAdapter declares the DISCOVERY fields too.
        // claude headless (stream-json control plane) supports answer_permission
        // (control_response) but rewind is NOT WIRED YET — deferred, not impossible.
        // (Correction, gap-reaudit: the prior "§9.9 measured: /rewind not on the
        // control plane" was WRONG — 2.1.191 binary HAS rewind_files/rewind_conversation
        // control arms; a probe returns {canRewind, error}.) The protocol EXISTS; we
        // just haven't built the client side: it needs a num_turns→user_message_id
        // history map (the wire rewinds by message id, not turn count) + checkpoint
        // infra (CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING), and a live success path we
        // can't verify without a checkpoint-enabled session. So cap is false FOR NOW;
        // this is a known, reachable follow-up, not a permanent exclusion — wire it
        // when rewind UX is wanted (probe shapes captured: protocols/samples/claude-cli/
        // 2.1.187/_all_rewind_{off,on}.jsonl). NOT answer_auth (auth failures terminate
        // the turn, no mid-session re-auth on the local path). models/modes are
        // CLI-static (filled by ClaudeConnection).
        Capabilities {
            tier: CapabilityTier::Parsed,
            // Static default only. The live backend OVERRIDES this per call, because
            // claude's answer depends on what it is doing: a `set_permission_mode` written
            // while idle takes effect at once (verified:
            // samples/claude-cli/2.1.227/set_permission_mode/), but one raised mid-turn is
            // queued by `write_or_queue_control` and drained before the NEXT prompt.
            mode_switch_effect: crate::capability::ModeSwitchEffect::Immediate,
            emits: SignalSet {
                heartbeat: true,
                tool_lifecycle: true,
                terminal_result: true,
            },
            supported_commands: crate::capability::CommandSet {
                // B5: mid-turn delivery routes Command::Steer to a direct stdin
                // user-frame write (claude's persistent stdin accepts writes any
                // time; the CLI queues and consumes them itself — command_lifecycle
                // echoes our uuid, design spec §6.1/§6甲.2). No turn_gen bump, no
                // FSM involvement (NoTurn admission).
                steer: true,
                cancel_tool: false,
                answer_permission: true,
                answer_auth: false,
                acknowledge: true,
                // G2: set_mode/set_model = true — the 007 claude seam now wires the
                // in-band control_request{set_permission_mode|set_model} over the
                // retained stdin (probe-verified, mirrors F1), queuing a mid-turn
                // switch to the next prompt and emitting ConfigChanged on dispatch.
                // cap=true ↔ dispatch accepts (the cap-behavior invariant holds).
                set_mode: true,
                set_model: true,
                rewind: false, // not wired YET (protocol EXISTS in 2.1.191; deferred follow-up, see note above)
                list_checkpoints: false,
                // claude exposes control_request{get_context_usage}+{get_session_cost}
                // (live-confirmed 2.1.186) → QuerySessionInfo dispatch over the same
                // in-band control plane, sniffed back as SessionEvent::SessionInfo.
                query_session_info: true,
            },
            prompt_blocks: crate::capability::BlockSet {
                text: true,
                // image = true: deliver_prompt emits a native base64 image block
                // ({type:image, source:{type:base64,...}}) the model truly sees
                // (pinned: protocols/samples/claude-cli/2.1.177/image_input_frame.OK.json).
                image: true,
                audio: false,
                // resource = true: a ResourceLink is delivered by reference as a
                // `[Attached file: <uri>]` text element for claude's Read tool
                // (headless claude has no working document INPUT block; the file
                // must be reachable from the spawn cwd / an --add-dir root).
                resource: true,
                at_mention: false,
            },
            // Native: claude echoes our user-frame `uuid` in the
            // `--replay-user-messages` frame ONLY when it truly consumes the message
            // into a turn — that echo is a REAL prompt-ack (the ClaudeConnection
            // reader's sniff_replay_prompt_ack emits PromptAccepted on it). Was
            // Synthesized (flush-ok), which lied for a proactively-queued message that
            // claude had not yet drained (or dropped on cancel). See protocols/design/
            // claude-midturn-input-turn-gen-design.md §3.3/§4-B.
            prompt_accepted: crate::capability::PromptAcceptedSource::Native,
            available_models: Vec::new(),
            // claude's permission modes are a FIXED known enum (NOT discovered, unlike
            // models/effort which come from the initialize response): the exact values
            // `--permission-mode` / control_request{set_permission_mode} accept. Static
            // here so `config_options_from_caps` projects a `mode` option (it gates on
            // `!available_modes.is_empty()`); without this the mode picker had no data
            // even though the write path (SetMode) was fully wired.
            available_modes: claude_permission_modes(),
            current_model: None,
            current_mode: None,
            current_effort: None,
            auth_methods: Vec::new(), // no mid-session re-auth on local path
            // 009 R2: claude's persistent stdin is a FIFO — a write while a turn
            // is in flight is buffered and consumed as the next turn, so the conv
            // layer CAN proactively queue. This (NOT supported_commands.steer)
            // is what can_queue gates on.
            accepts_proactive_input: true,
            // Verified backend matrix (see `Capabilities::supports_midturn_delivery`):
            // claude is a direct-CLI backend that can deliver a mid-turn message to
            // the agent without waiting for the current turn to end.
            supports_midturn_delivery: true,
            // #101: static default empty; the clean-slate ClaudeConnection fills it
            // from the control_request{initialize} response (the legacy adapter has
            // no discovery wire). capabilities() merges the discovered set on read.
            slash_commands: Vec::new(),
        }
    }
}

/// Max bytes of textual tool output carried per `Text` part (009 R8) — bounds the
/// Tier-1 row / WS-frame size for a huge tool dump (e.g. a megabyte file Read or a
/// noisy Bash). Truncated text gets a trailing marker.
const TOOL_TEXT_CAP: usize = 16 * 1024;

/// 009 R8: parse a claude `tool_result` block's `content` (polymorphic: a String,
/// or an Array of blocks) into the backend-neutral `ToolResultContent` Vec the
/// `TurnFinalizer` renders. A `text` block → `Text`; a base64 `image` block (the
/// shape a Read-tool image arrives in) → `Image{media_type, data}` (decoded);
/// anything else (tool_reference, …) is skipped. `None`/absent → empty Vec.
fn parse_tool_result_content(content: Option<&Value>) -> Vec<crate::event::ToolResultContent> {
    use crate::event::ToolResultContent;
    use base64::Engine as _;

    fn cap_text(mut s: String) -> String {
        if s.len() > TOOL_TEXT_CAP {
            // Truncate on a char boundary: `String::truncate` PANICS if the byte
            // index splits a multi-byte UTF-8 char (e.g. a 16 KiB Read result whose
            // 16384th byte lands mid-CJK-char). A parser panic on the reader task
            // silently drops the stdout reader WITHOUT emitting a terminal Detached,
            // so the turn never finalizes and the conversation is stuck `pending`.
            // `floor_char_boundary` rounds down to the nearest boundary (≤ cap).
            s.truncate(s.floor_char_boundary(TOOL_TEXT_CAP));
            s.push_str("…[truncated]");
        }
        s
    }

    match content {
        Some(Value::String(s)) => vec![ToolResultContent::Text(cap_text(s.clone()))],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|el| match el.get("type").and_then(Value::as_str) {
                Some("text") => el
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|t| ToolResultContent::Text(cap_text(t.to_string()))),
                Some("image") => {
                    let src = el.get("source")?;
                    if src.get("type").and_then(Value::as_str) != Some("base64") {
                        return None;
                    }
                    let media_type = src.get("media_type").and_then(Value::as_str)?.to_string();
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(src.get("data").and_then(Value::as_str)?)
                        .ok()?;
                    Some(ToolResultContent::Image { media_type, data })
                }
                _ => None, // tool_reference / unknown — skip
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Serialize `value` as exactly one NDJSON line (serde — never `format!`, so any
/// `\n`/quote/unicode is escaped into one frame, S16) + a single trailing `\n`,
/// then write+flush to the retained stdin WITHOUT closing (the persistent
/// process stays alive for the next turn / control exchange). Shared by
/// `deliver_prompt` (user lines) and `write_control_response` (control frames).
async fn write_ndjson_line(stdin: &mut BoxedStdin, value: &Value) -> Result<(), ProcessError> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|e| ProcessError::internal(format!("serialize stdin line: {e}")))?;
    bytes.push(b'\n');
    // DIAGNOSTIC (env-gated, default OFF via AIONUI_CLAUDE_WIRE_DUMP): log the RAW
    // stdin bytes we hand the CLI, so a "send accepted but no output" can be split
    // into "prompt was/wasn't actually written" vs "CLI received it but went silent".
    // OFF by default — it logs full prompt content (AGENTS.md sensitive-payload rule),
    // a deliberate debugging switch, never on in normal production.
    if std::env::var("AIONUI_CLAUDE_WIRE_DUMP").is_ok_and(|v| v != "0" && !v.is_empty()) {
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        tracing::info!(
            target: "aionui_session::claude_wire",
            direction = "stdin",
            byte_len = bytes.len(),
            preview = %preview,
            "claude wire bytes"
        );
    }
    stdin
        .write_all(&bytes)
        .await
        .map_err(|e| ProcessError::internal(format!("write stdin: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| ProcessError::internal(format!("flush stdin: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::BackendAdapter;
    use crate::event::{StopReason, TruncationKind, TurnOutcome};

    /// A `BoxedStdin` that captures every byte into a shared buffer, so a test can
    /// assert the exact NDJSON frame `deliver_prompt` writes.
    struct CaptureStdin(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl tokio::io::AsyncWrite for CaptureStdin {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Run `deliver_prompt` over a capturing stdin and return the written bytes as
    /// a UTF-8 string (the NDJSON line(s)).
    async fn captured_frame(content: &[crate::ContentBlock]) -> String {
        captured_frame_with_id(content, None).await
    }

    /// As `captured_frame`, but stamps a `client_msg_id` so a test can assert the
    /// `uuid` correlation field (B / §3.3) is written onto the user frame.
    async fn captured_frame_with_id(content: &[crate::ContentBlock], client_msg_id: Option<&str>) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut stdin: aionui_process::BoxedStdin = Box::new(CaptureStdin(buf.clone()));
        ClaudeAdapter::new()
            .deliver_prompt(&mut stdin, content, client_msg_id)
            .await
            .unwrap();
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// The multimodal `deliver_prompt` collapses a Text + Image + ResourceLink
    /// slice into EXACTLY ONE user NDJSON frame whose content[] carries: a text
    /// block, the NATIVE base64 image block (source-wrapped — the pinned shape in
    /// protocols/samples/claude-cli/2.1.177/image_input_frame.OK.json), and a
    /// `[Attached file: <uri>]` text reference for the file. Regression guard for
    /// the load-bearing image wire shape.
    #[tokio::test]
    async fn deliver_prompt_builds_multimodal_user_frame() {
        let frame = captured_frame(&[
            crate::ContentBlock::Text("hi".into()),
            crate::ContentBlock::Image {
                data: vec![1, 2, 3],
                media_type: "image/png".into(),
            },
            crate::ContentBlock::ResourceLink {
                uri: "/tmp/doc.pdf".into(),
                mime_type: None,
            },
        ])
        .await;
        // Exactly ONE NDJSON line.
        assert_eq!(
            frame.matches('\n').count(),
            1,
            "exactly one frame written, got {frame:?}"
        );
        let v: serde_json::Value = serde_json::from_str(frame.trim_end()).expect("valid JSON frame");
        assert_eq!(v["type"], "user");
        let content = v["message"]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 3, "text + image + file-ref, got {content:?}");
        // [0] text
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hi");
        // [1] NATIVE base64 image block — source-wrapped (the REQUIRED shape).
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AQID"); // base64([1,2,3])
        // [2] file delivered by reference as a Read-tool text hint.
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "[Attached file: /tmp/doc.pdf]");
    }

    /// B / §3.3: a `client_msg_id` is stamped as the user frame's top-level `uuid`
    /// (the correlation key claude echoes verbatim in its replay frame, so the
    /// ClaudeConnection reader can attribute which message a turn consumed). When
    /// `None`, NO `uuid` key is written (wire byte-identical to pre-correlation).
    #[tokio::test]
    async fn deliver_prompt_stamps_client_msg_id_as_uuid() {
        let with = captured_frame_with_id(&[crate::ContentBlock::Text("hi".into())], Some("cm-42")).await;
        let v: serde_json::Value = serde_json::from_str(with.trim_end()).expect("valid JSON");
        assert_eq!(v["uuid"], "cm-42", "client_msg_id is written as the frame uuid");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "hi", "content is unaffected");

        let without = captured_frame_with_id(&[crate::ContentBlock::Text("hi".into())], None).await;
        let v2: serde_json::Value = serde_json::from_str(without.trim_end()).expect("valid JSON");
        assert!(
            v2.get("uuid").is_none(),
            "no client_msg_id → no uuid key (byte-identical wire)"
        );
    }

    /// S16 one-frame invariant survives the &str→&[ContentBlock] widening: a text
    /// element containing a raw newline + quotes must NOT split the NDJSON frame.
    #[tokio::test]
    async fn deliver_prompt_text_with_newline_stays_one_frame() {
        let frame = captured_frame(&[crate::ContentBlock::Text("a\nb\"c".into())]).await;
        assert_eq!(
            frame.matches('\n').count(),
            1,
            "newline in text must not split the frame: {frame:?}"
        );
        let v: serde_json::Value = serde_json::from_str(frame.trim_end()).expect("valid JSON");
        assert_eq!(v["message"]["content"][0]["text"], "a\nb\"c");
    }

    /// 009 R8: a STRING tool_result content → one Text part (e.g. Bash stdout).
    /// Verified backend matrix (task-1 brief): claude MUST advertise
    /// `supports_midturn_delivery` so mid-turn UI can gate on it.
    #[test]
    fn capabilities_advertise_midturn_delivery() {
        let a = ClaudeAdapter::new();
        assert!(a.capabilities().supports_midturn_delivery);
        // Lock the backend-static table against the real constructor (claude
        // has no pub capability constructor, so the lock lives here).
        assert_eq!(
            crate::capability::backend_supports_midturn_delivery("claude"),
            a.capabilities().supports_midturn_delivery,
        );
    }

    /// Task 2: claude echoes back the uuid WE minted on a user frame via a
    /// `command_lifecycle` frame (verified 2.1.226, design spec §6.1) — the
    /// adapter must normalize it into `SessionEvent::MessageLifecycle`.
    #[test]
    fn parses_command_lifecycle_into_message_lifecycle_events() {
        let a = ClaudeAdapter::new();
        let frame = r#"{"type":"command_lifecycle","command_uuid":"u-1","state":"queued"}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        match a.parse_command_lifecycle(&v).as_slice() {
            [SessionEvent::MessageLifecycle { client_msg_id, phase }] => {
                assert_eq!(client_msg_id, "u-1");
                assert_eq!(*phase, crate::event::MessageLifecyclePhase::Queued);
            }
            other => panic!("expected one MessageLifecycle, got {other:?}"),
        }
    }

    /// An unrecognised `state` degrades to NO event rather than a guess — this
    /// frame is additive and claude may grow states we have not probed.
    #[test]
    fn unknown_command_lifecycle_state_is_ignored_not_panicked() {
        let a = ClaudeAdapter::new();
        let frame = r#"{"type":"command_lifecycle","command_uuid":"u-1","state":"future_state"}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        assert!(
            a.parse_command_lifecycle(&v).is_empty(),
            "an unknown phase must degrade to no event"
        );
    }

    #[test]
    fn parse_user_tool_result_string_content_to_text() {
        let a = ClaudeAdapter::new();
        let frame = r#"{"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tu1","content":"hello stdout"}]}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        let events = a.parse_user(&v);
        match events.as_slice() {
            [SessionEvent::ToolResult { content, .. }] => {
                assert_eq!(
                    content.as_slice(),
                    &[crate::event::ToolResultContent::Text("hello stdout".into())]
                );
            }
            other => panic!("expected one ToolResult, got {other:?}"),
        }
    }

    /// #486 (P4, LIVE-found 2026-06-22): a claude `tool_use` block with a
    /// missing/blank `name` is DROPPED — no `ToolCall` emitted — so it never
    /// persists as a ghost `tool_step{name:""}` row. Mirrors aionrs's empty-name
    /// guard. Other blocks in the same frame are unaffected.
    #[test]
    fn parse_assistant_drops_empty_name_tool_use_keeps_other_blocks() {
        let mut a = ClaudeAdapter::new();
        // A frame mixing a text block, a malformed tool_use (no `name`), and a
        // whitespace-only-name tool_use. Only the text must survive.
        let frame = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"working"},
            {"type":"tool_use","id":"t-missing","input":{}},
            {"type":"tool_use","id":"t-blank","name":"   ","input":{}}]}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        let events = a.parse_assistant(&v);
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::ToolCall { .. })),
            "empty/blank-name tool_use must NOT emit a ToolCall, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::MessageDelta { text, .. } if text == "working")),
            "a valid sibling block in the same frame still emits, got {events:?}"
        );
    }

    /// Regression guard for the other direction: a well-formed `tool_use` (real
    /// name) still emits its `ToolCall` — the #486 guard must not over-drop.
    #[test]
    fn parse_assistant_keeps_named_tool_use() {
        let mut a = ClaudeAdapter::new();
        let frame = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"t-ok","name":"Read","input":{"file_path":"/x"}}]}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        let events = a.parse_assistant(&v);
        match events.as_slice() {
            [
                SessionEvent::ToolCall {
                    tool_use_id,
                    name,
                    input,
                    ..
                },
            ] => {
                assert_eq!(tool_use_id, "t-ok");
                assert_eq!(name, "Read");
                // The tool ARGUMENTS (Gap #4 / H2) are load-bearing — the conversation
                // layer renders/persists them. A regression that dropped `input` would
                // pass a name-only assertion, so pin the full payload here.
                assert_eq!(
                    input,
                    &serde_json::json!({"file_path": "/x"}),
                    "tool_use input (arguments) must survive parse, not be dropped"
                );
            }
            other => panic!("expected one ToolCall(Read), got {other:?}"),
        }

        // Boundary: a tool_use with NO `input` key defaults to Value::Null (not a panic,
        // not a dropped ToolCall) — older frames / argument-less tools.
        let mut a = ClaudeAdapter::new();
        let frame = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"t-bare","name":"NoArgs"}]}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        match a.parse_assistant(&v).as_slice() {
            [SessionEvent::ToolCall { name, input, .. }] => {
                assert_eq!(name, "NoArgs");
                assert!(input.is_null(), "missing input → Value::Null (additive default)");
            }
            other => panic!("expected one ToolCall(NoArgs), got {other:?}"),
        }
    }

    /// PROPERTY (input field-value invariant, generalizes #486 / bug 201f999d):
    /// for ANY assistant frame whose `tool_use` blocks carry an arbitrary `name`
    /// (absent / "" / whitespace / valid) and arbitrary `id`, `parse_assistant`:
    ///   1. NEVER panics (malformed input is data, not a crash — I4);
    ///   2. NEVER emits a `ToolCall` with a blank/whitespace name (the exact ghost
    ///      tool-line defect 201f999d fixed — pin it across the whole value space,
    ///      not just the two hand-picked cases above);
    ///   3. emits EXACTLY one `ToolCall` per non-blank-name block (no over-drop).
    ///
    /// This is the §F.3 "input field-value boundary" face the unit tests only
    /// sampled — proptest sweeps the value space mechanically.
    #[test]
    fn prop_parse_assistant_never_emits_blank_name_toolcall() {
        use proptest::prelude::*;
        let name_strat = prop_oneof![
            Just(None),
            Just(Some(String::new())),
            Just(Some("   ".to_string())),
            Just(Some("\t\n".to_string())),
            "[a-zA-Z_][a-zA-Z0-9_]{0,8}".prop_map(Some),
        ];
        let blocks_strat = prop::collection::vec((name_strat, "[a-z0-9-]{0,6}"), 0..6);

        proptest!(|(blocks in blocks_strat)| {
            let content: Vec<serde_json::Value> = blocks
                .iter()
                .map(|(name, id)| {
                    let mut b = serde_json::json!({"type":"tool_use","id":id,"input":{}});
                    if let Some(n) = name {
                        b["name"] = serde_json::Value::String(n.clone());
                    }
                    b
                })
                .collect();
            let frame = serde_json::json!({
                "type":"assistant",
                "message":{"role":"assistant","content":content}
            });
            let mut a = ClaudeAdapter::new();
            let events = a.parse_assistant(&frame); // (1) must not panic

            for e in &events {
                if let SessionEvent::ToolCall { name, .. } = e {
                    prop_assert!(
                        !name.trim().is_empty(),
                        "emitted a ToolCall with blank name {name:?} (ghost tool line, 201f999d)"
                    );
                }
            }
            let expected = blocks
                .iter()
                .filter(|(name, _)| name.as_deref().map(|n| !n.trim().is_empty()).unwrap_or(false))
                .count();
            let got = events.iter().filter(|e| matches!(e, SessionEvent::ToolCall { .. })).count();
            prop_assert_eq!(got, expected, "one ToolCall per non-blank-name tool_use; over/under-drop");
        });
    }

    /// 009 R8 (LOAD-BEARING image regression guard): an ARRAY tool_result content
    /// with a text block + a base64 `image` block (the shape a Read-tool image
    /// arrives in) → [Text, Image{bytes decoded}]. Previously the whole content was
    /// dropped, so a claude Read-tool image was invisible at the session layer.
    #[test]
    fn parse_user_tool_result_array_with_image_to_text_and_image() {
        use base64::Engine as _;
        let a = ClaudeAdapter::new();
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        let frame = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[
                {{"type":"tool_result","tool_use_id":"tu2","content":[
                    {{"type":"text","text":"here is the image"}},
                    {{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{b64}"}}}}
                ]}}]}}}}"#
        );
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let events = a.parse_user(&v);
        match events.as_slice() {
            [SessionEvent::ToolResult { content, .. }] => {
                assert_eq!(content.len(), 2, "text + image, got {content:?}");
                assert_eq!(
                    content[0],
                    crate::event::ToolResultContent::Text("here is the image".into())
                );
                assert_eq!(
                    content[1],
                    crate::event::ToolResultContent::Image {
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    },
                    "the base64 image was decoded into bytes (not dropped)"
                );
            }
            other => panic!("expected one ToolResult, got {other:?}"),
        }
    }

    /// 009 R7/H3 (codex·ACP symmetry — claude leg): a FAILED tool_result carries
    /// `is_error:true` onto the `ToolResult` so a failed tool is NOT rendered as a
    /// success (a red tool stays red). The wire sets `is_error` on a rejected /
    /// errored tool; absent = success. ACP had this asserted; claude had the parse
    /// code (parse_user) but no failing-tool test. Pairs with the success case
    /// (default-false) so the routing bit is pinned on both edges.
    #[test]
    fn parse_user_failed_tool_result_carries_is_error_true() {
        let a = ClaudeAdapter::new();
        // A failed tool: is_error:true + the error text as content.
        let frame = r#"{"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tf1","is_error":true,
             "content":"Error: command not found: frobnicate"}]}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        match a.parse_user(&v).as_slice() {
            [
                SessionEvent::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                    ..
                },
            ] => {
                assert_eq!(tool_use_id, "tf1");
                assert!(
                    *is_error,
                    "H3: a failed tool_result must carry is_error:true (not render as success)"
                );
                assert_eq!(
                    content.as_slice(),
                    &[crate::event::ToolResultContent::Text(
                        "Error: command not found: frobnicate".into()
                    )],
                    "the error output is carried as content"
                );
            }
            other => panic!("expected one failed ToolResult, got {other:?}"),
        }

        // Control: a tool_result WITHOUT is_error defaults to success (false), so the
        // failing case above is a genuine signal, not a constant.
        let ok = r#"{"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"ts1","content":"ok"}]}}"#;
        let v2: serde_json::Value = serde_json::from_str(ok).unwrap();
        match a.parse_user(&v2).as_slice() {
            [SessionEvent::ToolResult { is_error, .. }] => {
                assert!(!*is_error, "no is_error field → success (default false)");
            }
            other => panic!("expected one ToolResult, got {other:?}"),
        }
    }

    /// Feed a single `result` frame through the public `parse_chunk` seam and
    /// return the `TurnResult`'s outcome (the rider the frontend's turn-end badge
    /// reads). Panics if no TurnResult is produced.
    fn outcome_of(frame: &str) -> TurnOutcome {
        let mut a = ClaudeAdapter::new();
        let line = format!("{frame}\n");
        let events = a.parse_chunk(line.as_bytes());
        events
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::TurnResult { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("result frame must produce a TurnResult")
    }

    /// 🖥️ UI-4 — claude `result_outcome()` mapping (terminal_reason / stop_reason →
    /// TurnOutcome) backs the truncated/refused/cancelled turn-end badge. This was
    /// untested (claude.rs had no test module — the legacy parse tests never reached
    /// main). A wrong mapping silently mislabels the badge (e.g. a real max_tokens
    /// cutoff shown as a clean completion). Pins every distinct mapping branch.
    #[test]
    fn ui4_claude_result_outcome_maps_terminal_reason() {
        // success result frames carry a terminal_reason/stop_reason rider.
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","terminal_reason":"max_tokens"}"#
            ),
            TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTokens)
            },
            "max_tokens → Truncated(MaxTokens)"
        );
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","terminal_reason":"max_turns"}"#
            ),
            TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTurns)
            },
            "max_turns → Truncated(MaxTurns)"
        );
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"","terminal_reason":"refusal"}"#
            ),
            TurnOutcome::Completed {
                stop_reason: StopReason::Refused { category: None }
            },
            "refusal → Refused"
        );
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","terminal_reason":"cancelled"}"#
            ),
            TurnOutcome::Cancelled {
                reason: crate::event::CancelReason::UserCancel
            },
            "cancelled → Cancelled(UserCancel)"
        );
        // end_turn / unknown / absent → EndTurn (errors route via is_error, not outcome).
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","terminal_reason":"end_turn"}"#
            ),
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn
            },
            "end_turn → EndTurn"
        );
        assert_eq!(
            outcome_of(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#),
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn
            },
            "absent terminal_reason → EndTurn (default)"
        );
        // stop_reason fallback when terminal_reason absent.
        assert_eq!(
            outcome_of(
                r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","stop_reason":"max_tokens"}"#
            ),
            TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTokens)
            },
            "stop_reason fallback: max_tokens → Truncated(MaxTokens)"
        );
    }

    // ── 009 R5: real-time turn boundary from --include-partial-messages ──────

    /// Parse a `stream_event` frame, returning its SessionEvents.
    fn stream_events(frame: &str) -> Vec<SessionEvent> {
        ClaudeAdapter::new().parse_chunk(format!("{frame}\n").as_bytes())
    }

    #[test]
    fn message_delta_tool_use_does_not_fold_the_turn() {
        // stop_reason:tool_use ⇒ a tool round follows, the turn is NOT over. Must
        // emit NO TurnResult (folding here would unlock mid-turn).
        let evs = stream_events(
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_details":null}}}"#,
        );
        assert!(
            !evs.iter().any(|e| matches!(e, SessionEvent::TurnResult { .. })),
            "tool_use must NOT produce a TurnResult, got {evs:?}"
        );
    }

    #[test]
    fn message_delta_end_turn_emits_realtime_turn_result() {
        // stop_reason:end_turn ⇒ the turn's reply finished in REAL TIME (before
        // the lagged `result`). Emit a clean TurnResult so the FSM folds Idle now.
        let evs = stream_events(
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_details":null}}}"#,
        );
        let tr = evs.iter().find_map(|e| match e {
            SessionEvent::TurnResult { is_error, outcome, .. } => Some((*is_error, outcome.clone())),
            _ => None,
        });
        assert_eq!(
            tr,
            Some((
                false,
                TurnOutcome::Completed {
                    stop_reason: StopReason::EndTurn
                }
            )),
            "end_turn → real-time TurnResult{{is_error:false, EndTurn}}, got {evs:?}"
        );
    }

    #[test]
    fn message_delta_terminal_variants_map_outcome() {
        // refusal / max_tokens carry through to the typed outcome on the real-time
        // TurnResult (same mapping as the result frame).
        let refusal = stream_events(
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"refusal"}}}"#,
        );
        assert!(
            refusal.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    outcome: TurnOutcome::Completed {
                        stop_reason: StopReason::Refused { .. }
                    },
                    ..
                }
            )),
            "refusal → Refused outcome, got {refusal:?}"
        );
    }

    #[test]
    fn known_non_delta_stream_events_are_inert_unknown_stays_opaque() {
        // P3: content_block_stop / message_stop are now EXPECTED frames (they bound
        // the stream we parse) → emit nothing, NOT AdapterSpecific.
        for frame in [
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ] {
            assert!(
                stream_events(frame).is_empty(),
                "expected-but-signalless stream sub-event emits nothing, got events for {frame}"
            );
        }
        // A genuinely UNKNOWN sub-type still falls to the opaque escape hatch.
        let evs = stream_events(r#"{"type":"stream_event","event":{"type":"some_future_subtype"}}"#);
        assert!(
            evs.iter().all(|e| matches!(e, SessionEvent::AdapterSpecific { .. })),
            "unknown stream_event sub-type stays opaque, got {evs:?}"
        );
    }

    #[test]
    fn content_block_delta_text_streams_incremental_message_deltas() {
        // The typewriter path: message_start sets the item_id, content_block_start
        // declares index 0 as text, then each text_delta emits one MessageDelta
        // keyed `<message.id>:text`. (One adapter instance across the frames —
        // the stream state is per-message, not per-line.)
        let mut a = ClaudeAdapter::new();
        let feed = |a: &mut ClaudeAdapter, frame: &str| -> Vec<SessionEvent> {
            a.parse_chunk(format!("{frame}\n").as_bytes())
        };
        assert!(
            feed(
                &mut a,
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_x"}}}"#
            )
            .is_empty()
        );
        assert!(
            feed(
                &mut a,
                r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#
            )
            .is_empty()
        );
        let d1 = feed(
            &mut a,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}"#,
        );
        let d2 = feed(
            &mut a,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}}"#,
        );
        for (evs, frag) in [(&d1, "Hel"), (&d2, "lo")] {
            match evs.as_slice() {
                [SessionEvent::MessageDelta { item_id, text }] => {
                    assert_eq!(item_id, "msg_x:text", "per-kind text item_id");
                    assert_eq!(text, frag);
                }
                other => panic!("expected one MessageDelta({frag}), got {other:?}"),
            }
        }

        // The consolidated assistant frame for the SAME message must NOT re-emit the
        // text (it already streamed) — else the finalizer (which appends) doubles it.
        // tool_use in the same frame is still emitted (dedup never touches tools).
        let consolidated = feed(
            &mut a,
            r#"{"type":"assistant","message":{"id":"msg_x","role":"assistant","content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
        );
        assert!(
            !consolidated
                .iter()
                .any(|e| matches!(e, SessionEvent::MessageDelta { .. })),
            "streamed text must be deduped on the consolidated frame, got {consolidated:?}"
        );
        assert!(
            consolidated
                .iter()
                .any(|e| matches!(e, SessionEvent::ToolCall { name, .. } if name == "Bash")),
            "tool_use is never deduped, got {consolidated:?}"
        );
    }

    #[test]
    fn consolidated_assistant_without_prior_stream_still_emits_text() {
        // Resume / no-partial-stream path: an assistant frame with NO preceding
        // content_block_delta must still emit its text (keyed per-kind) so nothing is
        // lost. Different message.id than any stream state → not deduped.
        let mut a = ClaudeAdapter::new();
        let evs = a.parse_chunk(
            concat!(
                r#"{"type":"assistant","message":{"id":"msg_y","role":"assistant","content":[{"type":"text","text":"full reply"}]}}"#,
                "\n"
            )
            .as_bytes(),
        );
        match evs.as_slice() {
            [SessionEvent::MessageDelta { item_id, text }] => {
                assert_eq!(item_id, "msg_y:text");
                assert_eq!(text, "full reply");
            }
            other => panic!("expected one MessageDelta(full reply), got {other:?}"),
        }
    }

    #[test]
    fn thinking_and_text_same_message_get_distinct_item_ids() {
        // claude shares ONE message.id across thinking + text blocks. The per-kind
        // suffix routes them to SEPARATE finalizer buffers so thinking never leaks
        // into the answer block. (Consolidated-frame path, the Bedrock reality where
        // thinking does not stream as thinking_delta.)
        let mut a = ClaudeAdapter::new();
        let evs = a.parse_chunk(
            concat!(
                r#"{"type":"assistant","message":{"id":"msg_z","role":"assistant","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"answer"}]}}"#,
                "\n"
            )
            .as_bytes(),
        );
        let think = evs.iter().find_map(|e| match e {
            SessionEvent::ThoughtDelta { item_id, text } => Some((item_id.clone(), text.clone())),
            _ => None,
        });
        let msg = evs.iter().find_map(|e| match e {
            SessionEvent::MessageDelta { item_id, text } => Some((item_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(think, Some(("msg_z:think".to_string(), "pondering".to_string())));
        assert_eq!(msg, Some(("msg_z:text".to_string(), "answer".to_string())));
    }

    /// A consolidated `thinking` block with NO plaintext (only its encrypted
    /// `signature`) must emit NO ThoughtDelta. That is the exact shape the CLI
    /// sends whenever thinking display resolves to `omitted` (live-probed on
    /// claude 2.1.220: `thinking` is "" while `signature` is ~650 chars).
    /// Emitting an empty delta produced a thinking card that expanded to nothing
    /// — a whole column of them once the empty segments started being persisted.
    /// The sibling text block must still come through untouched.
    #[test]
    fn consolidated_thinking_block_without_plaintext_emits_no_thought() {
        let mut a = ClaudeAdapter::new();
        let evs = a.parse_chunk(
            concat!(
                r#"{"type":"assistant","message":{"id":"msg_e","role":"assistant","content":[{"type":"thinking","thinking":"","signature":"ErUBCkYIBxgCKkC0"},{"type":"text","text":"answer"}]}}"#,
                "\n"
            )
            .as_bytes(),
        );
        assert!(
            !evs.iter().any(|e| matches!(e, SessionEvent::ThoughtDelta { .. })),
            "an empty thinking block must not produce a ThoughtDelta, got: {evs:?}"
        );
        let msg = evs.iter().find_map(|e| match e {
            SessionEvent::MessageDelta { item_id, text } => Some((item_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(msg, Some(("msg_e:text".to_string(), "answer".to_string())));
    }

    /// End-to-end on CAPTURED wire bytes: the frames a real `--thinking-display
    /// summarized` turn emits must parse into a NON-empty ThoughtDelta. Frames are
    /// verbatim from a live claude 2.1.220 capture (same flags the adapter spawns
    /// with), trimmed to the thinking block. Guards the whole reason the flag is
    /// passed — a parser that dropped this text would put us back to blank cards.
    #[test]
    fn captured_summarized_thinking_frames_yield_plaintext_thought() {
        let mut a = ClaudeAdapter::new();
        let evs = a.parse_chunk(
            concat!(
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_011CdZNE8pQQoWm1S5ytUj9a","role":"assistant"}}}"#,
                "\n",
                r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
                "\n",
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"17是质数，我只需检查它能","estimated_tokens":null}}}"#,
                "\n",
            )
            .as_bytes(),
        );
        let think = evs.iter().find_map(|e| match e {
            SessionEvent::ThoughtDelta { item_id, text } => Some((item_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(
            think,
            Some((
                "msg_011CdZNE8pQQoWm1S5ytUj9a:think".to_string(),
                "17是质数，我只需检查它能".to_string()
            ))
        );
        // The `content_block_start` carries an empty `thinking` too — it must not
        // leak an extra blank delta ahead of the real text.
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, SessionEvent::ThoughtDelta { .. }))
                .count(),
            1
        );
    }

    /// `--thinking-display` is version-gated and therefore manager-supplied, NOT
    /// built here (an older PATH-resolved claude rejects it at parse time and the
    /// spawn dies). Two things must hold: this crate never hardcodes it, and a
    /// manager that DOES pass it gets it through to the wire verbatim.
    #[tokio::test]
    async fn thinking_display_is_manager_supplied_not_hardcoded() {
        use crate::testing::FakeSpawner;

        let spawn_with = |extra: Vec<String>| async move {
            let spawner = FakeSpawner::new();
            let adapter = ClaudeAdapter::new();
            // FakeSpawner records the CommandSpec then Errs (it cannot make a real
            // process) — the spec we assert on was already captured.
            let _ = adapter
                .start_turn(
                    &spawner,
                    &SessionSpec::Fresh("11111111-1111-4111-8111-111111111111".into()),
                    None,
                    &extra,
                    &[],
                    None,
                )
                .await;
            spawner.last_command().await.expect("a spawn was recorded").args
        };

        let bare = spawn_with(Vec::new()).await;
        assert!(
            !bare.iter().any(|a| a == "--thinking-display"),
            "the adapter must not hardcode a version-gated flag, got: {bare:?}"
        );

        let gated = spawn_with(vec!["--thinking-display".to_string(), "summarized".to_string()]).await;
        let at = gated
            .iter()
            .position(|a| a == "--thinking-display")
            .expect("a manager-supplied flag must reach the spawn");
        assert_eq!(gated.get(at + 1).map(String::as_str), Some("summarized"));
    }

    /// Fork spawn: `ForkFrom(parent)` resumes the PARENT session with
    /// `--fork-session` so claude copies it into a NEW session — and never
    /// passes `--session-id` (unsupported combination, live help 2.1.221; the
    /// new id is backend-minted and learned from `system/init`).
    #[tokio::test]
    async fn fork_from_spawns_resume_with_fork_session_flag() {
        use crate::testing::FakeSpawner;
        let spawner = FakeSpawner::new();
        let adapter = ClaudeAdapter::new();
        let _ = adapter
            .start_turn(
                &spawner,
                &SessionSpec::ForkFrom("22222222-2222-4222-8222-222222222222".into()),
                None,
                &[],
                &[],
                None,
            )
            .await;
        let args = spawner.last_command().await.expect("a spawn was recorded").args;
        let at = args
            .iter()
            .position(|a| a == "--resume")
            .expect("fork resumes the parent");
        assert_eq!(
            args.get(at + 1).map(String::as_str),
            Some("22222222-2222-4222-8222-222222222222"),
            "--resume targets the PARENT session id"
        );
        assert!(args.iter().any(|a| a == "--fork-session"), "fork adds --fork-session");
        assert!(
            !args.iter().any(|a| a == "--session-id"),
            "--session-id must not be combined with --fork-session, got: {args:?}"
        );
    }

    /// Helper: drive one `result` NDJSON line through the adapter and return the
    /// `context_window` the emitted `UsageDelta` carries.
    #[cfg(test)]
    fn window_of(line: &str) -> Option<Option<u64>> {
        let mut a = ClaudeAdapter::new();
        a.parse_chunk(format!("{line}\n").as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta { context_window, .. } => Some(context_window),
                _ => None,
            })
    }

    /// The context-window size the usage indicator renders as `size` rides
    /// `result.modelUsage.<model>.contextWindow`. Verbatim shape from a live
    /// claude 2.1.220 turn (model `claude-opus-5`, window 1_000_000).
    #[test]
    fn result_carries_context_window_from_single_model_usage_entry() {
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":2,"cache_creation_input_tokens":10920,"#,
            r#""cache_read_input_tokens":15493,"output_tokens":5},"model":null,"#,
            r#""modelUsage":{"claude-opus-5":{"inputTokens":2,"outputTokens":5,"#,
            r#""contextWindow":1000000,"maxOutputTokens":64000}}}"#
        );
        assert_eq!(window_of(line), Some(Some(1_000_000)));
    }

    /// `result.model` is null on the wire, so the entry cannot be picked by model
    /// id. With several entries (a subagent ran on a different model) take the
    /// DOMINANT one by token volume — the main conversation model. Heuristic: only
    /// the single-entry shape has been captured live.
    #[test]
    fn multi_entry_model_usage_picks_the_dominant_model() {
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":10,"output_tokens":5},"#,
            r#""modelUsage":{"claude-haiku-4-5":{"inputTokens":12,"outputTokens":3,"contextWindow":200000},"#,
            r#""claude-opus-5":{"inputTokens":900,"outputTokens":100,"contextWindow":1000000}}}"#
        );
        assert_eq!(window_of(line), Some(Some(1_000_000)));
    }

    /// A usage-bearing result with no `modelUsage` (or an unparseable one) must
    /// still emit the UsageDelta — just without a window. The frontend guards on
    /// `size > 0`, so `None` degrades to "counter without a percentage".
    #[test]
    fn missing_or_malformed_model_usage_yields_no_window() {
        let bare = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":10,"output_tokens":5}}"#
        );
        assert_eq!(window_of(bare), Some(None));

        let malformed = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":10,"output_tokens":5},"modelUsage":{"m":{"contextWindow":"big"}}}"#
        );
        assert_eq!(window_of(malformed), Some(None));
    }

    /// A turn with tool rounds makes several API calls, and `result.usage`
    /// AGGREGATES them — feeding that to the indicator drove it past 100% of the
    /// window (reported in the wild at 340%). Occupancy is the LAST call's own
    /// usage. Frames verbatim from a live four-call turn (2.1.220): the per-call
    /// `message_delta.usage` values, then the aggregate `result.usage` whose
    /// `cache_read_input_tokens` 95_709 is exactly 15_493+26_345+26_485+27_386.
    #[test]
    fn multi_call_turn_reports_last_call_occupancy_not_the_turn_aggregate() {
        let mut a = ClaudeAdapter::new();
        let deltas = [
            (2, 10852, 15493, 134),
            (2, 140, 26345, 73),
            (2, 901, 26485, 73),
            (2, 79, 27386, 5),
        ];
        for (i, cc, cr, o) in deltas {
            let line = format!(
                r#"{{"type":"stream_event","event":{{"type":"message_delta","delta":{{"stop_reason":"tool_use"}},"usage":{{"input_tokens":{i},"cache_creation_input_tokens":{cc},"cache_read_input_tokens":{cr},"output_tokens":{o}}}}}}}"#
            );
            a.parse_chunk(format!("{line}\n").as_bytes());
        }
        let result = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":8,"cache_creation_input_tokens":11972,"#,
            r#""cache_read_input_tokens":95709,"output_tokens":285},"total_cost_usd":0.5}"#,
            "\n"
        );
        let total = a
            .parse_chunk(result.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta { total_tokens, .. } => Some(total_tokens),
                _ => None,
            })
            .expect("result emits a UsageDelta");
        assert_eq!(
            total, 27_472,
            "occupancy is the last call (2+79+27386+5), NOT the 107_974 aggregate"
        );
    }

    /// The detail line's counters are PER-TURN, so they ride the aggregate
    /// `result.usage` even though `total_tokens` must not. Thinking tokens are the
    /// exception: `result.usage.output_tokens_details` is null (live-captured), so
    /// they are summed off each `message_delta` — 60 + 12 across two calls here.
    #[test]
    fn breakdown_sums_thinking_across_calls_and_reads_caches_from_the_aggregate() {
        let mut a = ClaudeAdapter::new();
        for (cr, think) in [(15493u64, 60u64), (27386, 12)] {
            let line = format!(
                r#"{{"type":"stream_event","event":{{"type":"message_delta","delta":{{"stop_reason":"tool_use"}},"usage":{{"input_tokens":2,"cache_creation_input_tokens":79,"cache_read_input_tokens":{cr},"output_tokens":5,"output_tokens_details":{{"thinking_tokens":{think}}}}}}}}}"#
            );
            a.parse_chunk(format!("{line}\n").as_bytes());
        }
        let result = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":8,"cache_creation_input_tokens":11972,"#,
            r#""cache_read_input_tokens":95709,"output_tokens":285}}"#,
            "\n"
        );
        let b = a
            .parse_chunk(result.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta { breakdown, .. } => Some(breakdown),
                _ => None,
            })
            .expect("result emits a UsageDelta");
        assert_eq!(b.cached_read_tokens, 95_709, "per-turn cache reads");
        assert_eq!(b.cached_write_tokens, 11_972, "per-turn cache writes");
        assert_eq!(b.thought_tokens, 72, "thinking summed across the turn's calls");

        // The accumulator resets, or the next turn would inherit this turn's total.
        let next = a
            .parse_chunk(result.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta { breakdown, .. } => Some(breakdown),
                _ => None,
            })
            .expect("second result emits a UsageDelta");
        assert_eq!(next.thought_tokens, 0, "thinking must not carry into the next turn");
    }

    /// Without any streamed `message_delta` the aggregate is the only figure
    /// available, and for a single-call turn it IS the occupancy — so the fallback
    /// must still produce a number rather than nothing.
    #[test]
    fn without_streamed_calls_the_result_aggregate_is_used() {
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":2,"cache_creation_input_tokens":79,"#,
            r#""cache_read_input_tokens":27386,"output_tokens":5}}"#,
            "\n"
        );
        let mut a = ClaudeAdapter::new();
        let total = a
            .parse_chunk(line.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta { total_tokens, .. } => Some(total_tokens),
                _ => None,
            })
            .expect("result emits a UsageDelta");
        assert_eq!(total, 27_472);
    }

    /// Replays a live-captured compaction: five turns climbing to 27_349, the
    /// boundary, the compaction's own `result` (`num_turns:0`, zero aggregate, and
    /// crucially NO `message_delta`), then the next real turn at 23_779.
    ///
    /// The compaction must publish NOTHING. Its turn cannot measure the new
    /// context, and the latched pre-compaction figure would otherwise slip past
    /// the zero-usage guard and be re-published as current — the reported "used
    /// tokens never update after a compaction".
    #[test]
    fn compaction_invalidates_the_latched_occupancy() {
        let mut a = ClaudeAdapter::new();
        let occupancy_of = |evs: Vec<SessionEvent>| {
            evs.into_iter().find_map(|e| match e {
                SessionEvent::UsageDelta { total_tokens, .. } => Some(total_tokens),
                _ => None,
            })
        };
        let delta = |cr: u64| {
            format!(
                r#"{{"type":"stream_event","event":{{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":{cr},"output_tokens":0}}}}}}"#
            )
        };
        let result_with = |agg: u64| {
            format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":{agg},"output_tokens":0}}}}"#
            )
        };

        // A normal turn establishes the latch.
        a.parse_chunk(format!("{}\n", delta(27_347)).as_bytes());
        assert_eq!(
            occupancy_of(a.parse_chunk(format!("{}\n", result_with(27_347)).as_bytes())),
            Some(27_349)
        );

        // The compaction: boundary, then a zero-aggregate result with no delta.
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":27349,"post_tokens":3158}}"#;
        a.parse_chunk(format!("{boundary}\n").as_bytes());
        let zero_result = r#"{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}"#;
        assert_eq!(
            occupancy_of(a.parse_chunk(format!("{zero_result}\n").as_bytes())),
            Some(0),
            "the compaction must report zero (dropped downstream), NOT the stale 27_349"
        );

        // The next real turn measures the compacted context.
        a.parse_chunk(format!("{}\n", delta(23_777)).as_bytes());
        assert_eq!(
            occupancy_of(a.parse_chunk(format!("{}\n", result_with(23_777)).as_bytes())),
            Some(23_779)
        );
    }

    /// Switching models must move the window. `modelUsage` is session-CUMULATIVE
    /// (live-measured: `cacheReadInputTokens` 18_384 after turn 1, 44_791 after
    /// turn 2), so the model just switched AWAY from keeps the larger volume and a
    /// volume ranking pins the window to it — the reported bug, where opus -> haiku
    /// still displayed 1M instead of 200k. `system{subtype:"init"}` announces the
    /// live model, so that selects the entry.
    #[test]
    fn model_switch_moves_the_window_despite_cumulative_model_usage() {
        let result = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":10,"output_tokens":5},"#,
            // opus carries the whole accumulated history; haiku has just started.
            r#""modelUsage":{"claude-opus-5":{"inputTokens":4,"outputTokens":10,"#,
            r#""cacheReadInputTokens":44791,"contextWindow":1000000},"#,
            r#""claude-haiku-4-5":{"inputTokens":2,"outputTokens":5,"#,
            r#""cacheReadInputTokens":120,"contextWindow":200000}}}"#,
            "\n"
        );
        let window_after_init = |model: &str| {
            let mut a = ClaudeAdapter::new();
            let init = format!(r#"{{"type":"system","subtype":"init","model":"{model}"}}"#);
            a.parse_chunk(format!("{init}\n").as_bytes());
            a.parse_chunk(result.as_bytes()).into_iter().find_map(|e| match e {
                SessionEvent::UsageDelta { context_window, .. } => Some(context_window),
                _ => None,
            })
        };
        assert_eq!(
            window_after_init("claude-haiku-4-5"),
            Some(Some(200_000)),
            "after switching to haiku the window must follow, not stay on opus's 1M"
        );
        assert_eq!(window_after_init("claude-opus-5"), Some(Some(1_000_000)));
    }

    /// The dominant-model pick must weigh CACHE too. A `modelUsage` entry's
    /// in/out exclude cache, and the main model is the one holding the history as
    /// cache (live-captured: `inputTokens:10, outputTokens:34,
    /// cacheReadInputTokens:27953`). Ranking on in+out alone handed the window to
    /// a Task subagent doing uncached work.
    #[test]
    fn dominant_model_is_ranked_by_cache_inclusive_volume() {
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":10,"output_tokens":34},"#,
            // Main model: tiny in/out, huge cache — must win.
            r#""modelUsage":{"claude-opus-5":{"inputTokens":10,"outputTokens":34,"#,
            r#""cacheReadInputTokens":27953,"contextWindow":1000000},"#,
            // Subagent: larger uncached volume, smaller window — must NOT win.
            r#""claude-haiku-4-5":{"inputTokens":1445,"outputTokens":1767,"contextWindow":200000}}}"#
        );
        assert_eq!(
            window_of(line),
            Some(Some(1_000_000)),
            "the cache-heavy main model owns the window, not the busier subagent"
        );
    }

    /// H4 (design-vs-code gap audit, §5): `UsageDelta.total_tokens` MUST count the
    /// cache buckets (`cache_read_input_tokens` + `cache_creation_input_tokens`),
    /// which ARE billed input tokens — omitting them under-reported a cache-heavy
    /// turn ~10x and disagreed with codex (whose native `last.totalTokens` already
    /// includes cache). `input_tokens`/`output_tokens` stay the wire's BASE counts;
    /// only `total_tokens` is the genuine total. (Was a tripwire pinning the inert
    /// under-report; flipped to assert the fix.) `cost_usd` MUST carry through from
    /// `total_cost_usd` (previously never value-asserted).
    #[test]
    fn usage_delta_total_includes_cache_tokens() {
        let mut a = ClaudeAdapter::new();
        // The single_tool_turn fixture's result usage: input=3601, output=394,
        // cache_creation=3973, cache_read=33256. True total = 3601+394+3973+33256
        // = 41224 (NOT 3995). (One NDJSON line — frame_lines splits on '\n'.)
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":3601,"cache_creation_input_tokens":3973,"#,
            r#""cache_read_input_tokens":33256,"output_tokens":394},"total_cost_usd":0.1173}"#,
            "\n"
        )
        .to_string();
        let (input, output, total, cost) = a
            .parse_chunk(line.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta {
                    input_tokens,
                    output_tokens,
                    total_tokens,
                    cost_usd,
                    ..
                } => Some((input_tokens, output_tokens, total_tokens, cost_usd)),
                _ => None,
            })
            .expect("result frame with usage emits a UsageDelta");
        assert_eq!(
            (input, output),
            (3601, 394),
            "input/output stay the wire's BASE counts (cache is NOT folded into them)"
        );
        assert_eq!(
            total,
            3601 + 394 + 3973 + 33256,
            "H4: total_tokens counts base input + output + BOTH cache buckets (41224), \
             not input+output only (the prior ~10x under-report)"
        );
        assert_eq!(cost, Some(0.1173), "cost_usd carries through from total_cost_usd");
    }

    /// H4 boundary: when the cache buckets are absent (a non-cached turn), the
    /// computed total is just base input + output — the fix must not over-count
    /// missing fields (unwrap_or(0)).
    #[test]
    fn usage_delta_total_is_base_when_no_cache_fields() {
        let mut a = ClaudeAdapter::new();
        let line = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","#,
            r#""usage":{"input_tokens":100,"output_tokens":20}}"#,
            "\n"
        )
        .to_string();
        let (total, cost) = a
            .parse_chunk(line.as_bytes())
            .into_iter()
            .find_map(|e| match e {
                SessionEvent::UsageDelta {
                    total_tokens, cost_usd, ..
                } => Some((total_tokens, cost_usd)),
                _ => None,
            })
            .expect("UsageDelta emitted");
        assert_eq!(total, 120, "no cache buckets → total is base input+output");
        assert_eq!(cost, None, "no total_cost_usd → cost_usd is None");
    }

    /// AskUserQuestion projection (forward path): a `can_use_tool` for
    /// `AskUserQuestion` carries its `{questions:[…]}` input through on the
    /// `Permission` event, so the conversation layer can render a question card.
    #[test]
    fn ask_user_question_permission_carries_tool_name_and_questions() {
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-q1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_use_id": "toolu-q1",
                "input": {
                    "questions": [
                        { "question": "Pick a color", "header": "Color",
                          "options": [{"label":"blue","description":"the sky"}], "multiSelect": false }
                    ]
                }
            }
        });
        // 2026-08-04 spec: AskUserQuestion is its OWN Ask event now — asking is
        // not authorizing. The adapter must route it away from Permission, carry
        // the bare questions[] array, and remember the request_id so a later
        // control_cancel_request resolves the QUESTION counter.
        let mut adapter = ClaudeAdapter::new();
        let events = adapter.parse_control_request(&frame);
        match events.as_slice() {
            [SessionEvent::Ask { request_id, questions }] => {
                assert_eq!(request_id, "req-q1");
                assert_eq!(
                    questions[0]["question"], "Pick a color",
                    "the question text is projected, not dropped"
                );
                assert_eq!(
                    questions[0]["multiSelect"], false,
                    "multiSelect survives verbatim (the ws relay snake_cases later)"
                );
            }
            other => panic!("expected one Ask, got {other:?}"),
        }
        // The retract path must resolve the question counter, not the approval one.
        let cancel = serde_json::json!({ "type": "control_cancel_request", "request_id": "req-q1" });
        match adapter.parse_control_cancel_request(&cancel).as_slice() {
            [SessionEvent::AskResolved { request_id }] => assert_eq!(request_id, "req-q1"),
            other => panic!("expected AskResolved for a retracted ask, got {other:?}"),
        }
    }

    /// An ORDINARY tool permission (Bash) carries `tool_name` AND `input` — the
    /// permission card must show the approver what command they are approving
    /// (AionUi issue #3779: with input dropped the card read "命令: Bash" and the
    /// user approved blind). Display-to-the-approver is not logging, so TIO-13
    /// (never log tool input at info) is unaffected.
    #[test]
    fn ordinary_tool_permission_carries_input_and_tool_name() {
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": "req-b1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "toolu-b1",
                "input": { "command": "rm -rf /" }
            }
        });
        let events = ClaudeAdapter::new().parse_control_request(&frame);
        match events.as_slice() {
            [SessionEvent::Permission { tool_name, input, .. }] => {
                assert_eq!(tool_name.as_deref(), Some("Bash"));
                let input = input.as_ref().expect("ordinary tool input is projected for the card");
                assert_eq!(
                    input["command"], "rm -rf /",
                    "the command body reaches the permission card so the user reviews it"
                );
            }
            other => panic!("expected one Permission, got {other:?}"),
        }
    }

    /// ede_diagnostic filter: claude's internal `[ede_diagnostic] …` debug template
    /// in `errors[]` is dropped from `result_text` (it is NOT a user-readable error).
    #[test]
    fn parse_result_strips_ede_diagnostic_from_errors() {
        // ONLY the diagnostic → after stripping, errors is empty + is_error → falls
        // back to the structural `subtype` token, never the diagnostic string.
        //
        // The frame is the LIVE-CAPTURED result claude 2.1.185 emits on the
        // cancel-before-output edge (fixture pins the verbatim shape so a future
        // claude tag-change is caught here, not in production). The live oracle that
        // proves this fixture is faithful is `live_cli_e2e.rs
        // ::claude_live_cancel_before_output_never_leaks_ede_diagnostic`.
        let raw = include_str!("../../tests/fixtures/claude_2.1.185_cancel_before_output_result.ndjson");
        let frame: serde_json::Value = serde_json::from_str(raw.trim()).expect("fixture is valid result JSON");
        let events = ClaudeAdapter::new().parse_result(&frame);
        match events.first() {
            Some(SessionEvent::TurnResult {
                result_text, is_error, ..
            }) => {
                assert!(*is_error);
                assert!(
                    !result_text.contains("ede_diagnostic"),
                    "the internal diagnostic must never reach result_text, got: {result_text}"
                );
                assert_eq!(
                    result_text, "error_during_execution",
                    "stripped-empty errors + is_error falls back to the subtype token"
                );
            }
            other => panic!("expected a TurnResult, got {other:?}"),
        }
    }

    /// A genuine user-readable `errors[]` entry alongside the diagnostic survives —
    /// only the diagnostic is filtered, not the real error.
    #[test]
    fn parse_result_keeps_real_error_drops_only_diagnostic() {
        let frame = serde_json::json!({
            "type": "result",
            "is_error": true,
            "errors": ["[ede_diagnostic] result_type=user stop_reason=null", "rate limit exceeded"],
        });
        let events = ClaudeAdapter::new().parse_result(&frame);
        match events.first() {
            Some(SessionEvent::TurnResult { result_text, .. }) => {
                assert_eq!(
                    result_text, "rate limit exceeded",
                    "real error kept, diagnostic dropped"
                );
            }
            other => panic!("expected a TurnResult, got {other:?}"),
        }
    }

    /// ENUMERATION (parse_system subtype family, I8). `init` is silent; the network/
    /// compaction milestones normalize to the backend-neutral `Heartbeat` (liveness);
    /// anything else is opaque `AdapterSpecific`. A typo in a match arm (e.g.
    /// `api_retry`→`api_retrry`) would silently demote a Heartbeat to AdapterSpecific —
    /// this table pins every documented subtype + the unknown fallthrough.
    #[test]
    fn parse_system_subtypes_map_to_heartbeat_or_opaque() {
        let mut a = ClaudeAdapter::new();
        let mut sys = |subtype: &str| {
            let v = serde_json::json!({ "type": "system", "subtype": subtype });
            a.parse_system(&v)
        };

        // init → no state signal (empty).
        assert!(sys("init").is_empty(), "init carries no signal");

        // network backoff + compaction window → Heartbeat (liveness).
        for subtype in ["api_retry", "compact_boundary", "compacting"] {
            assert!(
                matches!(sys(subtype).as_slice(), [SessionEvent::Heartbeat]),
                "`{subtype}` must normalize to a single Heartbeat"
            );
        }

        // unknown subtype → opaque AdapterSpecific tagged `system/<subtype>` (never lost,
        // never a Heartbeat). Also the absent-subtype boundary → `system/`.
        match sys("brand_new_milestone").as_slice() {
            [SessionEvent::AdapterSpecific { tag, .. }] => {
                assert_eq!(
                    tag, "system/brand_new_milestone",
                    "unknown subtype stays opaque with its tag"
                );
            }
            other => panic!("expected one AdapterSpecific, got {other:?}"),
        }
        let no_subtype = sys("");
        assert!(
            matches!(no_subtype.as_slice(), [SessionEvent::AdapterSpecific { tag, .. }] if tag == "system/"),
            "absent subtype → opaque `system/` (no panic), got {no_subtype:?}"
        );
    }

    /// `control_cancel_request` (claude retracts a pending permission, §9.17) → a
    /// `PermissionResolved{request_id, Tool}` so the requires-action sub-state clears
    /// WITHOUT a user answer. This is a LIVE path (claude can withdraw a permission
    /// mid-decision) that had no test. A missing/empty request_id degrades to opaque
    /// AdapterSpecific (never a malformed PermissionResolved, never a panic).
    #[test]
    fn parse_control_cancel_request_resolves_permission_or_degrades() {
        // Valid retraction → PermissionResolved.
        let frame = serde_json::json!({ "type": "control_cancel_request", "request_id": "req-9" });
        match ClaudeAdapter::new().parse_control_cancel_request(&frame).as_slice() {
            [SessionEvent::PermissionResolved { request_id, kind }] => {
                assert_eq!(request_id, "req-9");
                assert_eq!(
                    *kind,
                    crate::event::PermissionKind::Tool,
                    "retraction is a TOOL approval"
                );
            }
            other => panic!("expected one PermissionResolved, got {other:?}"),
        }

        // Missing AND empty request_id both degrade to opaque AdapterSpecific (the
        // ref-count must NOT be decremented for an unidentifiable cancel).
        for frame in [
            serde_json::json!({ "type": "control_cancel_request" }),
            serde_json::json!({ "type": "control_cancel_request", "request_id": "" }),
        ] {
            match ClaudeAdapter::new().parse_control_cancel_request(&frame).as_slice() {
                [SessionEvent::AdapterSpecific { tag, .. }] => {
                    assert_eq!(tag, "control_cancel_request", "unidentifiable cancel stays opaque");
                }
                other => panic!("expected one AdapterSpecific, got {other:?}"),
            }
        }
    }

    /// ENUMERATION (parse_result `result_text` fallback chain, §R10/self-heal). The
    /// message text is chosen by a 4-arm precedence the ede + real-error tests only
    /// drove partially. Pin EACH arm in isolation so a reorder/regression is caught:
    /// arm1 `result` non-empty → use it verbatim (highest precedence); arm2 `result`
    /// empty + `errors` set → join the (ede-filtered) errors; arm3 both empty +
    /// is_error:true → fall back to the structural `subtype` token (so self-heal can
    /// detect a stderr-only fail); arm4 both empty + is_error:false → STAY EMPTY (never
    /// leak "success"; an empty success turn must read as EmptyTurn upstream).
    #[test]
    fn parse_result_text_fallback_chain_each_arm() {
        let text_of = |v: &Value| match ClaudeAdapter::new().parse_result(v).into_iter().next() {
            Some(SessionEvent::TurnResult { result_text, .. }) => result_text,
            other => panic!("expected a TurnResult, got {other:?}"),
        };

        // Arm 1: `result` wins even when errors[] also present.
        assert_eq!(
            text_of(&serde_json::json!({
                "type": "result", "is_error": false,
                "result": "the answer", "errors": ["ignored"]
            })),
            "the answer",
            "arm1: non-empty result takes precedence over errors"
        );
        // Arm 2: empty result, errors present → joined errors.
        assert_eq!(
            text_of(&serde_json::json!({
                "type": "result", "is_error": true, "result": "",
                "errors": ["rate limit", "retry later"]
            })),
            "rate limit; retry later",
            "arm2: errors joined when result empty"
        );
        // Arm 3: both empty + is_error → subtype token (self-heal needs a non-empty msg).
        assert_eq!(
            text_of(&serde_json::json!({
                "type": "result", "is_error": true, "subtype": "error_during_execution"
            })),
            "error_during_execution",
            "arm3: empty result+errors + is_error → subtype fallback"
        );
        // Arm 4: both empty + SUCCESS → stays empty (never the subtype, never "success").
        assert_eq!(
            text_of(&serde_json::json!({
                "type": "result", "is_error": false, "subtype": "success"
            })),
            "",
            "arm4: success terminal keeps result_text empty (EmptyTurn needs it empty)"
        );
    }

    /// PROPERTY (parse_result totality + is_error mirror — sibling of the
    /// parse_assistant proptest). For ANY result frame with arbitrary is_error /
    /// result / subtype / api_error_status, parse_result:
    ///   1. NEVER panics (malformed input is data, not a crash — I4);
    ///   2. emits exactly one TurnResult whose `is_error` MIRRORS the wire bit (default
    ///      false when absent) — the routing bit a SUCCESS-vs-error projection rides;
    ///   3. NEVER leaks the diagnostic tag into result_text (the ede invariant, swept).
    #[test]
    fn prop_parse_result_mirrors_is_error_and_never_leaks_diagnostic() {
        use proptest::prelude::*;
        let is_err = prop_oneof![Just(None), Just(Some(true)), Just(Some(false))];
        let result = prop_oneof![Just(None), Just(Some(String::new())), "[a-z ]{0,12}".prop_map(Some)];
        let errors = prop::collection::vec(
            prop_oneof![
                "[a-z ]{1,8}".prop_map(|s| s),
                Just("[ede_diagnostic] result_type=user".to_string()),
            ],
            0..4,
        );
        proptest!(|(is_err in is_err, result in result, errors in errors)| {
            let mut frame = serde_json::json!({ "type": "result", "subtype": "error_during_execution" });
            if let Some(e) = is_err { frame["is_error"] = serde_json::json!(e); }
            if let Some(r) = result { frame["result"] = serde_json::json!(r); }
            frame["errors"] = serde_json::json!(errors);

            let events = ClaudeAdapter::new().parse_result(&frame); // (1) must not panic
            let tr = events.iter().find(|e| matches!(e, SessionEvent::TurnResult { .. }));
            prop_assert!(tr.is_some(), "parse_result must emit a TurnResult");
            if let Some(SessionEvent::TurnResult { is_error, result_text, .. }) = tr {
                // (2) is_error mirrors the wire bit (absent → false).
                prop_assert_eq!(*is_error, is_err.unwrap_or(false), "is_error must mirror the wire");
                // (3) the diagnostic tag never reaches result_text.
                prop_assert!(
                    !result_text.contains("[ede_diagnostic]"),
                    "result_text leaked the internal diagnostic: {:?}", result_text
                );
            }
        });
    }

    /// PROPERTY (parse_user totality + ToolResult is_error default — sibling proptest).
    /// For ANY user frame whose `tool_result` blocks carry arbitrary is_error / content,
    /// parse_user:
    ///   1. NEVER panics;
    ///   2. emits one ToolResult per block whose `is_error` MIRRORS the wire (absent →
    ///      false: a tool with no is_error is a SUCCESS, never silently failed).
    #[test]
    fn prop_parse_user_tool_result_mirrors_is_error_default_false() {
        use proptest::prelude::*;
        let is_err = prop_oneof![Just(None), Just(Some(true)), Just(Some(false))];
        let blocks = prop::collection::vec((is_err, "[a-z ]{0,10}"), 0..5);
        proptest!(|(blocks in blocks)| {
            let content: Vec<serde_json::Value> = blocks
                .iter()
                .enumerate()
                .map(|(i, (is_error, text))| {
                    let mut b = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": format!("tu-{i}"),
                        "content": text,
                    });
                    if let Some(e) = is_error { b["is_error"] = serde_json::json!(e); }
                    b
                })
                .collect();
            let frame = serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": content }
            });
            let a = ClaudeAdapter::new();
            let events = a.parse_user(&frame); // (1) must not panic

            let results: Vec<&SessionEvent> =
                events.iter().filter(|e| matches!(e, SessionEvent::ToolResult { .. })).collect();
            prop_assert_eq!(results.len(), blocks.len(), "one ToolResult per tool_result block");
            for (ev, (wire_err, _)) in results.iter().zip(blocks.iter()) {
                if let SessionEvent::ToolResult { is_error, .. } = ev {
                    prop_assert_eq!(*is_error, wire_err.unwrap_or(false), "is_error mirrors wire (absent→false)");
                }
            }
        });
    }

    #[test]
    fn parse_tool_result_caps_oversized_text_on_char_boundary() {
        // Regression: a tool_result whose text exceeds TOOL_TEXT_CAP where the byte
        // at the cap splits a multi-byte UTF-8 char used to panic in `cap_text`
        // (`String::truncate` requires a char boundary). The panic killed the reader
        // task WITHOUT a terminal Detached, wedging the conversation at `pending`.
        // Build a body of multi-byte CJK chars ('世' = 3 bytes) so the TOOL_TEXT_CAP-th
        // byte is guaranteed mid-char (16384 % 3 != 0), then wrap it in a real frame.
        let big = "世".repeat(TOOL_TEXT_CAP); // 3 * 16384 bytes ≫ cap, cap lands mid-char
        let frame = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "big1", "content": big}
            ]}
        });
        let a = ClaudeAdapter::new();
        match a.parse_user(&frame).as_slice() {
            [SessionEvent::ToolResult { content, .. }] => match content.as_slice() {
                [crate::event::ToolResultContent::Text(t)] => {
                    assert!(t.ends_with("…[truncated]"), "carries the truncation marker");
                    let body = t.strip_suffix("…[truncated]").unwrap();
                    assert!(body.len() <= TOOL_TEXT_CAP, "capped at or below TOOL_TEXT_CAP");
                    // The real assertion is simply that we got here without panicking,
                    // AND that the cut landed on a char boundary (no partial '世').
                    assert!(
                        body.chars().all(|c| c == '世'),
                        "truncation preserved whole chars, no split multi-byte char"
                    );
                }
                other => panic!("expected one Text part, got {other:?}"),
            },
            other => panic!("expected one ToolResult, got {other:?}"),
        }
    }
}
