//! Live progress projection for a running claude Workflow.
//!
//! A Workflow is non-blocking: the `Workflow` tool call completes the instant the
//! workflow is launched, and everything that happens afterwards — for tens of
//! seconds or many minutes — arrives ONLY as `system/task_progress` frames. Those
//! became `SubagentUpdate` / `SubagentDetail` / `WorkflowPhase`, which the session
//! pump used purely to decide when a turn may end. Nothing reached the user, so
//! the conversation looked frozen for the whole flight.
//!
//! This module accumulates those events into a roster and renders it into the two
//! frames the frontend already knows how to draw. The pump owns the ledger; every
//! rendering decision lives here as a pure function over [`WorkflowCard`], so the
//! display format can be iterated without touching the pump.
//!
//! Two properties of the upstream data drive the whole design:
//!
//! 1. **`workflow_progress[]` is a delta, not a snapshot.** Most frames describe a
//!    single agent. The roster therefore has to be accumulated here; rendering a
//!    frame on its own would show one agent and hide the rest.
//! 2. **The emitted `tool_group` must always carry the FULL roster in a stable
//!    order.** `persist_tool_group` replaces the row's entire content and keys the
//!    row by the FIRST entry's `call_id`, so a partial list erases the agents left
//!    out and a reordered list starts a second row.

use std::collections::BTreeMap;

use aionui_session::WorkflowLoopState;

use crate::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus, ToolGroupEntry, ToolGroupStatus};

/// Minimum gap between two progress emissions for one workflow.
///
/// A real capture refreshed roughly once a second; a long workflow scales
/// linearly, and every emission costs a WebSocket broadcast plus two DB upserts.
/// State changes bypass this (see [`WorkflowCard::take_emission`]), so the visible
/// beats — an agent appearing, finishing, a phase turning over — are never
/// delayed, only the cosmetic token/tool-count churn between them is.
const THROTTLE_MS: i64 = 500;

/// Longest `lastToolSummary` rendered before it is elided in the MIDDLE.
///
/// The frontend truncates a group row's description from the TAIL at 100 chars
/// (`normalizeToolGroup`), and these summaries are usually file paths — whose
/// informative part is the tail. Eliding the middle keeps both ends.
const SUMMARY_MAX: usize = 48;

/// How a workflow container ended. Mirrors the terminal `SubagentStatus` values,
/// projected onto the two different status vocabularies the wire uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardStatus {
    Running,
    Completed,
    Errored,
    Cancelled,
}

impl CardStatus {
    /// Status vocabulary of the `tool_call` channel (lowercase on the wire).
    fn as_tool_call(self) -> ToolCallStatus {
        match self {
            Self::Running => ToolCallStatus::Running,
            Self::Completed => ToolCallStatus::Completed,
            Self::Errored => ToolCallStatus::Error,
            Self::Cancelled => ToolCallStatus::Canceled,
        }
    }
}

/// One workflow agent's accumulated display state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentEntry {
    pub label: Option<String>,
    pub phase_index: Option<u32>,
    pub phase_title: Option<String>,
    pub model: Option<String>,
    pub loop_state: Option<WorkflowLoopState>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub last_tool_name: Option<String>,
    pub last_tool_summary: Option<String>,
    pub duration_ms: Option<u64>,
}

impl AgentEntry {
    fn is_done(&self) -> bool {
        matches!(self.loop_state, Some(WorkflowLoopState::Done))
    }

    /// Merge one detail event. Every field is optional and only overwrites when
    /// present: claude omits fields it has no update for, so a blind assignment
    /// would blank out values the previous frame established.
    fn merge(&mut self, d: AgentDetail) {
        macro_rules! set {
            ($($f:ident),+) => { $( if d.$f.is_some() { self.$f = d.$f; } )+ };
        }
        set!(
            label,
            phase_index,
            phase_title,
            model,
            loop_state,
            tokens,
            tool_calls,
            last_tool_name,
            last_tool_summary,
            duration_ms
        );
    }
}

/// The subset of `SessionEvent::SubagentDetail` this module consumes, so the
/// pump can hand over a plain value and keep this module free of event plumbing.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentDetail {
    pub label: Option<String>,
    pub phase_index: Option<u32>,
    pub phase_title: Option<String>,
    pub model: Option<String>,
    pub loop_state: Option<WorkflowLoopState>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub last_tool_name: Option<String>,
    pub last_tool_summary: Option<String>,
    pub duration_ms: Option<u64>,
}

/// One rendered snapshot, ready for the wire.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rendered {
    pub description: String,
    pub output: String,
    pub entries: Vec<ToolGroupEntry>,
}

/// What kind of background work this card tracks. All three arrive as the same
/// `task_started` shape, differing only in `task_type`:
///
/// - `local_workflow` → [`CardKind::Workflow`]: has a per-agent roster
///   (`workflow_progress[]`) to render.
/// - `local_bash` / `local_agent` → [`CardKind::BackgroundTask`]: no roster ever
///   arrives — the card is a single live row on the launching tool call.
///
/// (verified: local_agent + linkage in
/// `claude_2.1.169_single_tool_turn.ndjson`; local_bash + linkage in
/// `claude_2.1.220_background_bash_turn.ndjson`; local_workflow in
/// `claude_2.1.176_workflow_multiagent_3parallel_1fail.ndjson`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardKind {
    Workflow,
    BackgroundTask,
}

/// Live ledger for ONE workflow container.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowCard {
    kind: CardKind,
    /// Static headline prefix for a [`CardKind::BackgroundTask`] card, built at
    /// open from the launching call's own arguments (`description`/`command` for
    /// Bash, `description` for Task) plus the task id — the id is what the user
    /// quotes to ask the model to stop the task. Empty for workflow cards.
    bg_headline: String,
    /// The Workflow tool call's own id, so the emitted `tool_call` updates the
    /// card already on screen instead of adding a second one.
    call_id: String,
    /// Re-sent on every frame: the DB merge is a JSON merge-patch, so an absent
    /// `name` is fine but a stale one is not, and `stamp_tool_name`'s map is
    /// cleared at every turn settlement — a workflow outliving its turn cannot
    /// rely on it.
    tool_name: String,
    /// Likewise re-sent: `args` has no `skip_serializing_if`, so emitting it as
    /// null would DELETE the persisted arguments (merge-patch semantics).
    args: serde_json::Value,
    started_at_ms: i64,
    /// Declared up front on the first progress frame, so this is complete long
    /// before the agents are.
    phases: BTreeMap<u32, String>,
    /// Insertion-ordered: the first entry keys the persisted `tool_group` row, so
    /// the order must never change once established.
    agents: Vec<(String, AgentEntry)>,
    status: CardStatus,
    /// The agent this card last saw move — the headline names what is actually
    /// progressing rather than the first slot that happens to be unfinished.
    last_touched: Option<String>,
    last_render: Option<Rendered>,
    last_emit_ms: Option<i64>,
}

impl WorkflowCard {
    pub fn new(call_id: String, tool_name: String, args: serde_json::Value, now_ms: i64) -> Self {
        Self {
            kind: CardKind::Workflow,
            bg_headline: String::new(),
            call_id,
            tool_name,
            args,
            started_at_ms: now_ms,
            phases: BTreeMap::new(),
            agents: Vec::new(),
            status: CardStatus::Running,
            last_touched: None,
            last_render: None,
            last_emit_ms: None,
        }
    }

    /// A rosterless background task (`local_bash` / `local_agent`). `desc` is the
    /// launching call's own description of the work; `task_id` rides in the
    /// headline so the user can name the task when asking to stop it.
    ///
    /// `desc` is DROPPED when it merely repeats `tool_name`. The step row draws
    /// the name and the description side by side and dedupes only on exact
    /// equality, so once a backend derives its step label from the same argument
    /// the headline quotes — since #870 a claude Bash label IS that call's own
    /// `description` — keeping both printed the text twice: "Wait for instance
    /// readiness" + "Wait for instance readiness · bg task bdyspm202 · 00:09".
    /// Where the label is still a bare tool name, `desc` is the only thing
    /// saying what the task does, so it is kept.
    pub fn new_background(
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
        task_id: &str,
        desc: Option<String>,
        now_ms: i64,
    ) -> Self {
        let mut card = Self::new(call_id, tool_name, args, now_ms);
        card.kind = CardKind::BackgroundTask;
        card.bg_headline = match desc {
            Some(d) if d.trim() == card.tool_name.trim() => format!("bg task {task_id}"),
            Some(d) => format!("{} · bg task {task_id}", elide_middle(&d, SUMMARY_MAX)),
            None => format!("bg task {task_id}"),
        };
        card
    }

    pub fn is_background(&self) -> bool {
        self.kind == CardKind::BackgroundTask
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    pub fn elapsed_ms(&self, now_ms: i64) -> u64 {
        // Wall-clock, never the frame's own `usage.duration_ms`: that value goes
        // BACKWARDS at settlement (the container's terminal frame reports a
        // smaller elapsed than the progress frame before it), which would make the
        // card's timer tick backwards.
        now_ms.saturating_sub(self.started_at_ms).max(0) as u64
    }

    /// Record a phase declaration. Returns whether this changed anything.
    pub fn declare_phase(&mut self, index: u32, title: String) -> bool {
        self.phases.insert(index, title).is_none()
    }

    /// Merge one agent detail. Returns true when this is a STATE change (a new
    /// agent, or one reaching a new loop state) — those bypass throttling.
    pub fn upsert_agent(&mut self, r#ref: &str, detail: AgentDetail) -> bool {
        self.last_touched = Some(r#ref.to_owned());
        let incoming_state = detail.loop_state;
        match self.agents.iter_mut().find(|(k, _)| k == r#ref) {
            Some((_, entry)) => {
                let before = entry.loop_state;
                entry.merge(detail);
                before != entry.loop_state
            }
            None => {
                let mut entry = AgentEntry::default();
                entry.merge(detail);
                self.agents.push((r#ref.to_owned(), entry));
                let _ = incoming_state;
                true
            }
        }
    }

    /// Settle the container. Every still-running agent is forced to `Done`: the
    /// workflow is over, so leaving them mid-loop would strand their rows on a
    /// spinner with nothing left to ever clear it.
    pub fn settle(&mut self, status: CardStatus) {
        self.status = status;
        for (_, entry) in &mut self.agents {
            if !entry.is_done() {
                entry.loop_state = Some(WorkflowLoopState::Done);
            }
        }
    }

    /// Decide whether to emit now, and if so return the frames.
    ///
    /// `forced` covers state changes and settlement; otherwise an emission needs
    /// both a content change and the throttle window to have elapsed. Returns
    /// `None` when nothing should go out.
    pub fn take_emission(&mut self, now_ms: i64, forced: bool) -> Option<(ToolCallEventData, Vec<ToolGroupEntry>)> {
        // A WORKFLOW card with an empty roster renders to a contentless card;
        // wait for the first agent. A background task never has a roster — its
        // headline alone is the content, so it emits from the moment it opens.
        if self.kind == CardKind::Workflow && self.agents.is_empty() {
            return None;
        }
        let rendered = self.render(now_ms);
        let changed = self.last_render.as_ref() != Some(&rendered);
        let due = self
            .last_emit_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= THROTTLE_MS);
        if !(forced || changed && due) {
            return None;
        }
        let card = ToolCallEventData {
            call_id: self.call_id.clone(),
            name: self.tool_name.clone(),
            args: self.args.clone(),
            status: self.status.as_tool_call(),
            input: None,
            // A background card must NOT send output: the launching call's real
            // tool_result (e.g. the task-id text) is already persisted there, and
            // the DB merge-patch would clobber it with an empty tree.
            output: match self.kind {
                CardKind::Workflow => Some(rendered.output.clone()),
                CardKind::BackgroundTask => None,
            },
            description: Some(rendered.description.clone()),
        };
        let entries = rendered.entries.clone();
        self.last_render = Some(rendered);
        self.last_emit_ms = Some(now_ms);
        Some((card, entries))
    }

    // ─────────────────────────── rendering ───────────────────────────
    // Pure functions of the ledger. The display format is expected to be tuned
    // against the real UI; keeping it all here means that tuning touches one
    // place and only these tests.

    fn render(&self, now_ms: i64) -> Rendered {
        if self.kind == CardKind::BackgroundTask {
            // One live row: headline + elapsed. No roster, no expandable tree.
            return Rendered {
                description: format!("{} · {}", self.bg_headline, fmt_clock(self.elapsed_ms(now_ms))),
                output: String::new(),
                entries: Vec::new(),
            };
        }
        Rendered {
            description: self.render_description(now_ms),
            output: self.render_output(),
            entries: self.render_entries(),
        }
    }

    /// Always visible on the card's title row.
    fn render_description(&self, now_ms: i64) -> String {
        let focus = self
            .last_touched
            .as_ref()
            .and_then(|r| self.agents.iter().find(|(k, _)| k == r))
            .or_else(|| self.agents.last());
        let mut parts: Vec<String> = Vec::new();
        if let Some((_, agent)) = focus {
            if let Some(title) = agent
                .phase_title
                .clone()
                .or_else(|| agent.phase_index.and_then(|i| self.phases.get(&i).cloned()))
            {
                let (done, known) = self.phase_progress(agent.phase_index);
                parts.push(format!("{title} [{done}/{known}]"));
            }
            if let Some(label) = &agent.label {
                parts.push(label.clone());
            }
        }
        parts.push(format!("{} agents", self.agents.len()));
        let tokens: u64 = self.agents.iter().filter_map(|(_, a)| a.tokens).sum();
        parts.push(format!("{} tok", fmt_tokens(Some(tokens))));
        parts.push(fmt_clock(self.elapsed_ms(now_ms)));
        parts.join(" · ")
    }

    /// Shown when the card is expanded: the whole roster grouped by phase.
    fn render_output(&self) -> String {
        let mut by_phase: BTreeMap<Option<u32>, Vec<&AgentEntry>> = BTreeMap::new();
        for (_, agent) in &self.agents {
            by_phase.entry(agent.phase_index).or_default().push(agent);
        }
        let mut lines: Vec<String> = Vec::new();
        for (phase, agents) in &by_phase {
            if let Some(idx) = phase {
                let title = self
                    .phases
                    .get(idx)
                    .cloned()
                    .or_else(|| agents.first().and_then(|a| a.phase_title.clone()));
                if let Some(title) = title {
                    let (done, known) = self.phase_progress(Some(*idx));
                    lines.push(format!("Phase {idx}  {title}   [{done}/{known}]"));
                }
            }
            for agent in agents {
                lines.push(self.render_agent_line(agent));
            }
        }
        lines.join("\n")
    }

    fn render_agent_line(&self, agent: &AgentEntry) -> String {
        let mark = if agent.is_done() { "✔" } else { "⋯" };
        let label = agent.label.clone().unwrap_or_else(|| "?".into());
        let mut line = format!(
            "  {mark} {:<14} {:<10} {:>7} tok",
            label,
            short_model(agent.model.as_deref()),
            fmt_tokens(agent.tokens)
        );
        if let Some(tail) = self.render_agent_tail(agent) {
            line.push_str("   ");
            line.push_str(&tail);
        }
        line
    }

    /// The trailing "what is it doing" column, shared by both renderings.
    fn render_agent_tail(&self, agent: &AgentEntry) -> Option<String> {
        let mut bits: Vec<String> = Vec::new();
        if let Some(name) = &agent.last_tool_name {
            match &agent.last_tool_summary {
                Some(summary) => bits.push(format!("{name}: {}", elide_middle(summary, SUMMARY_MAX))),
                None => bits.push(name.clone()),
            }
        } else if let Some(n) = agent.tool_calls.filter(|n| *n > 0) {
            bits.push(format!("{n} tools"));
        }
        if let Some(ms) = agent.duration_ms {
            bits.push(format!("{:.1}s", ms as f64 / 1000.0));
        }
        if bits.is_empty() { None } else { Some(bits.join("   ")) }
    }

    /// One `tool_group` entry per agent, in stable insertion order.
    fn render_entries(&self) -> Vec<ToolGroupEntry> {
        self.agents
            .iter()
            .map(|(r#ref, agent)| {
                let mut bits: Vec<String> = Vec::new();
                if let Some(idx) = agent.phase_index {
                    let title = agent
                        .phase_title
                        .clone()
                        .or_else(|| self.phases.get(&idx).cloned())
                        .unwrap_or_default();
                    bits.push(format!("[P{idx} {title}]").replace(" ]", "]"));
                }
                bits.push(short_model(agent.model.as_deref()));
                bits.push(format!("{} tok", fmt_tokens(agent.tokens)));
                if let Some(n) = agent.tool_calls.filter(|n| *n > 0) {
                    bits.push(format!("{n} tools"));
                }
                if let Some(name) = &agent.last_tool_name {
                    match &agent.last_tool_summary {
                        Some(s) => bits.push(format!("{name}: {}", elide_middle(s, SUMMARY_MAX))),
                        None => bits.push(name.clone()),
                    }
                }
                if let Some(ms) = agent.duration_ms {
                    bits.push(format!("{:.1}s", ms as f64 / 1000.0));
                }
                ToolGroupEntry {
                    // MUST be globally unique, not the bare roster key. The roster
                    // keys agents by `index` ("1", "2", …) — fine for dedup, fatal
                    // as a `call_id`: `persist_tool_group` uses the FIRST entry's
                    // call_id as `messages.id`, so a bare index collides with every
                    // other conversation's workflow ("UNIQUE constraint failed:
                    // messages.id", seen 77× in a live run — the agent rows then
                    // never persist and vanish on reload, while the live WebSocket
                    // still showed them, so only the DB layer noticed).
                    // The container's tool_use_id is globally unique, so prefixing
                    // with it is enough.
                    call_id: format!("{}:{}", self.call_id, r#ref),
                    name: agent.label.clone().unwrap_or_else(|| r#ref.clone()),
                    // start/progress → Executing, done → Success. There is NO
                    // agent-level failure signal: a workflow agent whose command
                    // exits non-zero still reports `done` (the failure lives in
                    // `resultPreview`, which is deliberately not surfaced). Only
                    // the container carries an error outcome.
                    status: if agent.is_done() {
                        ToolGroupStatus::Success
                    } else {
                        ToolGroupStatus::Executing
                    },
                    description: Some(bits.join(" · ")),
                }
            })
            .collect()
    }

    /// `(done, known)` for a phase. `known` is what has been DISPATCHED so far,
    /// not a static total — agents appear as the workflow dispatches them, so
    /// this denominator legitimately grows.
    fn phase_progress(&self, phase: Option<u32>) -> (usize, usize) {
        let members = self.agents.iter().filter(|(_, a)| a.phase_index == phase);
        let known = members.clone().count();
        let done = members.filter(|(_, a)| a.is_done()).count();
        (done, known)
    }
}

/// `global.anthropic.claude-opus-4-8` → `opus-4-8`.
fn short_model(model: Option<&str>) -> String {
    let Some(model) = model else { return "?".into() };
    model
        .rsplit('.')
        .next()
        .unwrap_or(model)
        .trim_start_matches("claude-")
        .to_owned()
}

fn fmt_tokens(tokens: Option<u64>) -> String {
    match tokens {
        None => "—".into(),
        Some(t) if t >= 1000 => format!("{:.1}k", t as f64 / 1000.0),
        Some(t) => t.to_string(),
    }
}

fn fmt_clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Elide the MIDDLE, keeping both ends. These strings are usually paths or shell
/// commands, where the tail carries as much meaning as the head — and the
/// frontend's own truncation already cuts the tail off.
fn elide_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_owned();
    }
    let keep = max.saturating_sub(1) / 2;
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(label: &str, state: WorkflowLoopState) -> AgentDetail {
        AgentDetail {
            label: Some(label.into()),
            phase_index: Some(1),
            phase_title: Some("Run".into()),
            model: Some("global.anthropic.claude-opus-4-8".into()),
            loop_state: Some(state),
            ..Default::default()
        }
    }

    fn card() -> WorkflowCard {
        WorkflowCard::new("toolu_wf".into(), "Workflow".into(), serde_json::json!({"x": 1}), 0)
    }

    #[test]
    fn entries_carry_the_full_roster_in_insertion_order() {
        // persist_tool_group replaces the whole row and keys it by the FIRST
        // entry, so a partial or reordered list corrupts the persisted group.
        let mut c = card();
        for (i, label) in ["run:A", "run:B", "run:C"].iter().enumerate() {
            c.upsert_agent(&(i + 1).to_string(), detail(label, WorkflowLoopState::Progress));
        }
        let (_, entries) = c.take_emission(10, true).expect("first emission");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["run:A", "run:B", "run:C"], "full roster, stable order");
        // The first entry keys the persisted row, so its call_id must be globally
        // unique — a bare roster index would collide across conversations.
        assert_eq!(entries[0].call_id, "toolu_wf:1");
        assert!(
            entries.iter().all(|e| e.call_id.starts_with("toolu_wf:")),
            "every entry is namespaced by its container: {entries:?}"
        );
    }

    #[test]
    fn a_later_delta_updates_in_place_and_keeps_the_others() {
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        c.upsert_agent("2", detail("run:B", WorkflowLoopState::Progress));
        let _ = c.take_emission(10, true);
        // Only run:B moves; run:A must survive with its label intact.
        c.upsert_agent(
            "2",
            AgentDetail {
                tokens: Some(2000),
                ..Default::default()
            },
        );
        let (_, entries) = c.take_emission(1000, true).unwrap();
        assert_eq!(entries.len(), 2, "a single-agent delta must not drop the roster");
        assert_eq!(entries[0].name, "run:A");
        assert!(
            entries[1].description.as_deref().unwrap().contains("2.0k tok"),
            "the moved agent picked up its new tokens: {:?}",
            entries[1].description
        );
    }

    /// Two workflows (different conversations, different Workflow tool calls)
    /// must not produce colliding entry ids.
    ///
    /// `persist_tool_group` writes the FIRST entry's `call_id` straight into
    /// `messages.id`, which is globally unique. The roster keys agents by `index`,
    /// so emitting that bare key made every workflow's first row claim id "1" —
    /// the second conversation to run one then failed with "UNIQUE constraint
    /// failed: messages.id" and its agent rows never persisted (observed 77× in a
    /// live run; the live WebSocket still rendered them, so only the DB noticed).
    #[test]
    fn entry_ids_do_not_collide_across_workflows() {
        let mut a = WorkflowCard::new("toolu_aaa".into(), "Workflow".into(), serde_json::Value::Null, 0);
        let mut b = WorkflowCard::new("toolu_bbb".into(), "Workflow".into(), serde_json::Value::Null, 0);
        // Same roster keys on both — this is the norm, `index` restarts at 1 for
        // every workflow.
        a.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        b.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));

        let (_, ea) = a.take_emission(10, true).unwrap();
        let (_, eb) = b.take_emission(10, true).unwrap();
        assert_ne!(
            ea[0].call_id, eb[0].call_id,
            "two workflows must not claim the same messages.id"
        );
    }

    #[test]
    fn background_headline_does_not_repeat_the_step_label() {
        // The step row draws `name` then `description`, deduping only on EXACT
        // equality. Since #870 a claude Bash step's label IS that call's own
        // `description`, so a headline that also quoted it printed the text
        // twice: "Wait for instance readiness" + "Wait for instance readiness ·
        // bg task bdyspm202 · 00:09".
        let label = "Wait for instance readiness";
        let mut c = WorkflowCard::new_background(
            "toolu_bg".into(),
            label.into(),
            serde_json::json!({"command": "until grep -q READY log; do sleep 1; done", "description": label}),
            "bdyspm202",
            Some(label.to_string()),
            0,
        );
        let (f, _) = c.take_emission(9_000, true).expect("background cards open immediately");
        let description = f.description.as_deref().unwrap();

        assert!(
            !description.contains(label),
            "headline must not repeat the label the row already shows: {description:?}"
        );
        // It still has to say WHICH task, so the user can ask to stop it.
        assert!(
            description.contains("bg task bdyspm202"),
            "lost the task id: {description:?}"
        );
        assert!(description.ends_with("00:09"), "lost the clock: {description:?}");
    }

    /// The other direction: where the label is still a BARE tool name, the desc
    /// is the only thing saying what the task does, so it must be kept.
    #[test]
    fn background_headline_keeps_a_desc_that_adds_information() {
        let mut c = WorkflowCard::new_background(
            "toolu_bg".into(),
            "Bash".into(),
            serde_json::Value::Null,
            "b7",
            Some("Sleep 15 seconds".into()),
            0,
        );
        let (f, _) = c.take_emission(0, true).unwrap();
        let description = f.description.as_deref().unwrap();
        assert!(
            description.contains("Sleep 15 seconds") && description.contains("bg task b7"),
            "a desc distinct from the label carries the only description of the work: {description:?}"
        );
    }

    #[test]
    fn clock_advance_alone_re_emits_a_background_card() {
        // A background task has no frames between task_started and its terminal,
        // so the pump's 1s tick is the only thing that moves the clock — and each
        // tick must also re-assert Running (the launching call's tool_result
        // repaints the row completed otherwise).
        let mut c = WorkflowCard::new_background(
            "toolu_bg".into(),
            "Bash".into(),
            serde_json::Value::Null,
            "b1",
            Some("sleep 30".into()),
            0,
        );
        let (f, entries) = c.take_emission(0, true).expect("opens immediately, roster or not");
        assert!(entries.is_empty());
        assert!(f.description.as_deref().unwrap().ends_with("00:00"));
        assert!(f.output.is_none(), "no output — it would clobber the task-id text");
        assert!(
            c.take_emission(300, false).is_none(),
            "same displayed second, inside throttle: nothing"
        );
        let (f2, _) = c.take_emission(1000, false).expect("a new displayed second re-emits");
        assert!(f2.description.as_deref().unwrap().ends_with("00:01"));
        assert_eq!(f2.status, ToolCallStatus::Running, "each tick re-asserts running");
    }

    #[test]
    fn done_agents_report_success_and_running_ones_execute() {
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Done));
        c.upsert_agent("2", detail("run:B", WorkflowLoopState::Progress));
        let (_, entries) = c.take_emission(10, true).unwrap();
        assert_eq!(entries[0].status, ToolGroupStatus::Success);
        assert_eq!(entries[1].status, ToolGroupStatus::Executing);
    }

    #[test]
    fn settle_forces_every_agent_terminal() {
        // Otherwise a killed workflow strands its rows on a spinner forever —
        // and hasRunningToolMessages keeps the conversation indicator lit.
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        c.upsert_agent("2", detail("run:B", WorkflowLoopState::Progress));
        c.settle(CardStatus::Cancelled);
        let (card_frame, entries) = c.take_emission(10, true).unwrap();
        assert_eq!(card_frame.status, ToolCallStatus::Canceled);
        assert!(
            entries.iter().all(|e| e.status.is_terminal()),
            "no entry may stay Executing after settlement: {entries:?}"
        );
    }

    #[test]
    fn throttles_cosmetic_churn_but_never_a_state_change() {
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        assert!(c.take_emission(0, true).is_some(), "first emission goes out");

        // Same content, inside the window → nothing.
        assert!(c.take_emission(100, false).is_none());
        // Changed content but still inside the window → still nothing.
        c.upsert_agent(
            "1",
            AgentDetail {
                tokens: Some(10),
                ..Default::default()
            },
        );
        assert!(c.take_emission(200, false).is_none(), "throttled");
        // Past the window → goes out.
        c.upsert_agent(
            "1",
            AgentDetail {
                tokens: Some(20),
                ..Default::default()
            },
        );
        assert!(c.take_emission(THROTTLE_MS, false).is_some());

        // A state change ignores the window entirely.
        let forced = c.upsert_agent("1", detail("run:A", WorkflowLoopState::Done));
        assert!(forced, "reaching a new loop state is a state change");
        assert!(c.take_emission(THROTTLE_MS + 1, forced).is_some());
    }

    #[test]
    fn identical_content_is_not_re_emitted() {
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        let _ = c.take_emission(0, true);
        // Well past the throttle window, but nothing changed except the clock —
        // and the clock is part of the description, so it DOES change. Freeze it.
        assert!(
            c.take_emission(0, false).is_none(),
            "no content change at the same instant → no frame"
        );
    }

    #[test]
    fn empty_roster_emits_nothing() {
        let mut c = card();
        assert!(c.take_emission(10, true).is_none(), "a card with no agents is blank");
    }

    #[test]
    fn card_frame_always_carries_name_and_args() {
        // The DB merge is a JSON merge-patch: `args` has no skip_serializing_if,
        // so emitting null would DELETE the persisted arguments.
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Progress));
        let (frame, _) = c.take_emission(10, true).unwrap();
        assert_eq!(frame.name, "Workflow");
        assert_eq!(frame.args, serde_json::json!({"x": 1}));
        assert!(frame.description.is_some());
        assert!(frame.output.is_some());
    }

    #[test]
    fn output_groups_agents_under_their_phase() {
        let mut c = card();
        c.declare_phase(1, "Run".into());
        c.declare_phase(2, "Summarize".into());
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Done));
        c.upsert_agent(
            "4",
            AgentDetail {
                label: Some("summarize:D".into()),
                phase_index: Some(2),
                phase_title: Some("Summarize".into()),
                loop_state: Some(WorkflowLoopState::Progress),
                ..Default::default()
            },
        );
        let (frame, _) = c.take_emission(10, true).unwrap();
        let out = frame.output.unwrap();
        assert!(out.contains("Phase 1  Run   [1/1]"), "got:\n{out}");
        assert!(out.contains("Phase 2  Summarize   [0/1]"), "got:\n{out}");
        assert!(out.find("run:A") < out.find("Phase 2"), "agents sit under their phase");
    }

    #[test]
    fn phase_denominator_grows_as_agents_are_dispatched() {
        // `workflow_progress[]` is a delta stream: later agents of a phase appear
        // only once dispatched, so [m/n] is progress-so-far, not a static total.
        let mut c = card();
        c.upsert_agent("1", detail("run:A", WorkflowLoopState::Done));
        assert_eq!(c.phase_progress(Some(1)), (1, 1));
        c.upsert_agent("2", detail("run:B", WorkflowLoopState::Progress));
        assert_eq!(c.phase_progress(Some(1)), (1, 2));
    }

    #[test]
    fn long_tool_summaries_are_elided_in_the_middle() {
        // The frontend cuts a row's description at 100 chars FROM THE TAIL, and
        // these are paths — so the tail must be preserved here.
        let long = "/private/tmp/claude-501/-private-tmp/8e57e34a-bc88-4d0c-a66f-deadbeefcafe/scratchpad/notes.md";
        let mut c = card();
        c.upsert_agent(
            "1",
            AgentDetail {
                label: Some("run:B".into()),
                last_tool_name: Some("Read".into()),
                last_tool_summary: Some(long.into()),
                ..Default::default()
            },
        );
        let (_, entries) = c.take_emission(10, true).unwrap();
        let desc = entries[0].description.clone().unwrap();
        assert!(desc.contains('…'), "must be elided: {desc}");
        assert!(desc.ends_with("notes.md"), "the tail must survive: {desc}");
        assert!(desc.chars().count() <= 100, "row must fit the frontend cap: {desc}");
    }

    #[test]
    fn model_ids_are_shortened() {
        assert_eq!(short_model(Some("global.anthropic.claude-opus-4-8")), "opus-4-8");
        assert_eq!(short_model(None), "?");
    }

    #[test]
    fn clock_and_tokens_format() {
        assert_eq!(fmt_clock(42_500), "00:42");
        assert_eq!(fmt_clock(125_000), "02:05");
        assert_eq!(fmt_tokens(Some(30_678)), "30.7k");
        assert_eq!(fmt_tokens(Some(842)), "842");
        assert_eq!(fmt_tokens(None), "—");
    }
}
