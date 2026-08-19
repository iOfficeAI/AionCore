//! Transcript compaction lock, tool-pairing boundaries, and surface token
//! measurement. Projection-only: the journal stays append-only.

use crate::journal_transcript::{DraftItem, TranscriptVisibility};
use crate::stream_persistence::{CanonicalEventJournal, CanonicalJournalEvent, canonical_event_id};
use serde_json::json;

pub(crate) const DEFAULT_KEEP_RECENT_TOOL_RESULTS: usize = 3;
pub(crate) const MIN_KEEP_RECENT_TOOL_RESULTS: usize = 1;
pub(crate) const MAX_KEEP_RECENT_TOOL_RESULTS: usize = 20;
pub(crate) const KIND_COMPACTION_START: &str = "CompactionStart";
pub(crate) const KIND_COMPACTION_END: &str = "CompactionEnd";
pub(crate) const KIND_COMPACTION_POLICY: &str = "CompactionPolicy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionLock {
    None,
    Open,
    Closed,
}

impl CompactionLock {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptTokenMeasurement {
    pub log_revision: u64,
    pub surface_tokens: u64,
    pub nodes: Vec<TranscriptTokenNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptTokenNode {
    pub sequence: u64,
    pub tokens: u64,
}

pub(crate) fn parse_keep_n(value: u64) -> Option<usize> {
    let keep_n = usize::try_from(value).ok()?;
    (MIN_KEEP_RECENT_TOOL_RESULTS..=MAX_KEEP_RECENT_TOOL_RESULTS)
        .contains(&keep_n)
        .then_some(keep_n)
}

pub(crate) fn fold_compaction_keep_n(events: &[CanonicalJournalEvent]) -> usize {
    events
        .iter()
        .rev()
        .find(|event| event.kind == KIND_COMPACTION_POLICY)
        .and_then(|event| {
            event
                .payload
                .pointer("/data/keep_n")
                .or_else(|| event.payload.pointer("/keep_n"))
                .and_then(serde_json::Value::as_u64)
        })
        .and_then(parse_keep_n)
        .unwrap_or(DEFAULT_KEEP_RECENT_TOOL_RESULTS)
}

pub(crate) async fn append_compaction_policy(
    journal: &CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
    keep_n: usize,
) -> Result<CanonicalJournalEvent, std::io::Error> {
    let payload = json!({
        "type": "compaction_policy",
        "data": { "keep_n": keep_n as u64 }
    });
    let seed = format!("compaction_policy:{conversation_id}:{keep_n}");
    let event_id = canonical_event_id(&seed, &payload);
    journal
        .append(
            user_id,
            conversation_id,
            event_id,
            KIND_COMPACTION_POLICY.to_owned(),
            payload,
        )
        .await
}

pub(crate) fn compact_old_tool_results(items: &mut [DraftItem], keep_n: usize) {
    let keep_n = keep_n.clamp(MIN_KEEP_RECENT_TOOL_RESULTS, MAX_KEEP_RECENT_TOOL_RESULTS);
    let tool_indexes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.transcript_kind == "tool/call" && item.visibility == TranscriptVisibility::Model)
        .map(|(index, _)| index)
        .collect();
    let prune_end = tool_indexes.len().saturating_sub(keep_n);
    for &index in &tool_indexes[..prune_end] {
        let item = &mut items[index];
        item.content = item.summary.clone();
        item.compacted = true;
    }
}

pub(crate) fn compaction_lock(events: impl IntoIterator<Item = impl AsRef<str>>) -> CompactionLock {
    let mut lock = CompactionLock::None;
    for kind in events {
        match kind.as_ref() {
            KIND_COMPACTION_START => lock = CompactionLock::Open,
            KIND_COMPACTION_END if matches!(lock, CompactionLock::Open) => lock = CompactionLock::Closed,
            _ => {}
        }
    }
    lock
}

/// A tool call/result pair is balanced when every observed `call_id` has a
/// terminal status or output. Events without a call id that already carry
/// output count as self-paired.
pub(crate) fn tool_pairing_balanced(events: &[crate::stream_persistence::CanonicalJournalEvent]) -> bool {
    use std::collections::HashSet;
    let mut open: HashSet<String> = HashSet::new();
    for event in events {
        if !matches!(event.kind.as_str(), "ToolCall" | "AcpToolCall") {
            continue;
        }
        let call_id = tool_call_id(&event.payload);
        let terminal = is_terminal_tool(&event.payload);
        match (call_id, terminal) {
            (Some(id), true) => {
                open.remove(&id);
            }
            (Some(id), false) => {
                open.insert(id);
            }
            (None, false) => return false,
            (None, true) => {}
        }
    }
    open.is_empty()
}

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn tool_pairing_balanced_before(
    events: &[crate::stream_persistence::CanonicalJournalEvent],
    seq: u64,
) -> bool {
    let prefix: Vec<_> = events.iter().filter(|event| event.sequence < seq).cloned().collect();
    tool_pairing_balanced(&prefix)
}

pub(crate) fn measure_model_surface(items: &[DraftItem], log_revision: u64) -> TranscriptTokenMeasurement {
    let nodes: Vec<TranscriptTokenNode> = items
        .iter()
        .filter(|item| item.visibility == TranscriptVisibility::Model)
        .map(|item| TranscriptTokenNode {
            sequence: item.sequence,
            tokens: estimate_tokens(&item.content),
        })
        .collect();
    let surface_tokens = nodes.iter().map(|node| node.tokens).sum();
    TranscriptTokenMeasurement {
        log_revision,
        surface_tokens,
        nodes,
    }
}

pub(crate) fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    chars.div_ceil(4).max(if text.is_empty() { 0 } else { 1 })
}

fn tool_call_id(payload: &serde_json::Value) -> Option<String> {
    let candidates = [
        payload.pointer("/data/call_id"),
        payload.pointer("/call_id"),
        payload.pointer("/data/update/tool_call_id"),
        payload.pointer("/update/tool_call_id"),
        payload.pointer("/data/tool_call_id"),
        payload.pointer("/tool_call_id"),
    ];
    for candidate in candidates {
        if let Some(id) = candidate
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_owned());
        }
    }
    None
}

fn is_terminal_tool(payload: &serde_json::Value) -> bool {
    let status = payload
        .pointer("/data/status")
        .or_else(|| payload.pointer("/status"))
        .or_else(|| payload.pointer("/data/update/status"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if matches!(
        status,
        "completed" | "failed" | "error" | "cancelled" | "canceled" | "success"
    ) {
        return true;
    }
    payload
        .pointer("/data/output")
        .or_else(|| payload.pointer("/output"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|output| !output.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_persistence::CanonicalJournalEvent;

    fn event(sequence: u64, kind: &str, payload: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            runtime_epoch: "test-runtime".into(),
            event_id: format!("event-{sequence}"),
            conversation_id: "conv".into(),
            sequence,
            timestamp: sequence as i64,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn last_keep_n_policy_wins_and_rejects_out_of_range() {
        assert_eq!(fold_compaction_keep_n(&[]), DEFAULT_KEEP_RECENT_TOOL_RESULTS);
        let events = vec![
            event(1, KIND_COMPACTION_POLICY, serde_json::json!({"data":{"keep_n":10}})),
            event(2, KIND_COMPACTION_POLICY, serde_json::json!({"data":{"keep_n":1}})),
        ];
        assert_eq!(fold_compaction_keep_n(&events), 1);
        let invalid = vec![event(
            1,
            KIND_COMPACTION_POLICY,
            serde_json::json!({"data":{"keep_n":99}}),
        )];
        assert_eq!(fold_compaction_keep_n(&invalid), DEFAULT_KEEP_RECENT_TOOL_RESULTS);
        assert!(parse_keep_n(0).is_none());
        assert!(parse_keep_n(21).is_none());
    }

    #[test]
    fn unpaired_start_leaves_the_lock_open() {
        assert_eq!(compaction_lock(["CompactionStart"]), CompactionLock::Open);
        assert_eq!(
            compaction_lock(["CompactionStart", "CompactionEnd"]),
            CompactionLock::Closed
        );
        assert_eq!(compaction_lock(["Text"]), CompactionLock::None);
    }

    #[test]
    fn tool_pairing_requires_a_terminal_result_for_each_call() {
        let events = vec![
            event(
                1,
                "ToolCall",
                serde_json::json!({"data":{"call_id":"a","status":"pending"}}),
            ),
            event(
                2,
                "ToolCall",
                serde_json::json!({"data":{"call_id":"a","status":"completed","output":"ok"}}),
            ),
        ];
        assert!(tool_pairing_balanced(&events));
        assert!(!tool_pairing_balanced_before(&events, 2));
        assert!(tool_pairing_balanced_before(&events, 3));
    }

    #[test]
    fn dangling_call_is_unbalanced() {
        let events = vec![event(
            1,
            "ToolCall",
            serde_json::json!({"data":{"call_id":"open","status":"in_progress"}}),
        )];
        assert!(!tool_pairing_balanced(&events));
    }

    #[test]
    fn token_estimate_is_stable_and_nonzero_for_text() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }
}
