//! Canonical journal boundaries used when forking a conversation.

use crate::journal_compaction::tool_pairing_balanced;
use crate::model_visible::check_model_surface_reconstructible;
use crate::stream_persistence::CanonicalJournalEvent;

const TRANSIENT_LIFECYCLE_KINDS: &[&str] = &[
    "InputHeld",
    "InputDispatching",
    "InputAccepted",
    "InputApplied",
    "InputCanceled",
    "InputFailed",
    "CancellationRequested",
    "CancellationCancelling",
    "CancellationConvergedIdle",
    "CancellationForceTerminated",
    "CancellationFailed",
];

/// Select a replayable journal prefix for a fork.
///
/// A HEAD fork includes the committed journal. A turn-anchored fork stops
/// immediately before the next backend turn. Queue lifecycle records are
/// deliberately omitted: only their applied `UserPrompt` is model-visible.
pub(crate) fn select_fork_prefix(
    events: &[CanonicalJournalEvent],
    is_head: bool,
    last_turn_id: Option<&str>,
) -> Result<Vec<CanonicalJournalEvent>, &'static str> {
    if events.is_empty() {
        return Ok(Vec::new());
    }

    let end = if is_head {
        events.len()
    } else {
        let turn_id = last_turn_id.ok_or("fork journal turn anchor is missing")?;
        let start = events
            .iter()
            .position(|event| backend_turn_id(event) == Some(turn_id))
            .ok_or("fork journal turn anchor was not recorded")?;
        events[start + 1..]
            .iter()
            .position(|event| event.kind == "BackendTurnBound")
            .map_or(events.len(), |offset| start + 1 + offset)
    };

    let prefix = &events[..end];
    if !tool_pairing_balanced(prefix) {
        return Err("fork journal boundary splits a tool call/result pair");
    }
    if check_model_surface_reconstructible(prefix).is_err() {
        return Err("fork journal model surface is not reconstructible");
    }

    Ok(prefix
        .iter()
        .filter(|event| !TRANSIENT_LIFECYCLE_KINDS.contains(&event.kind.as_str()))
        .cloned()
        .collect())
}

fn backend_turn_id(event: &CanonicalJournalEvent) -> Option<&str> {
    (event.kind == "BackendTurnBound")
        .then(|| event.payload.pointer("/data").and_then(serde_json::Value::as_str))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64, kind: &str, payload: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            runtime_epoch: "test-runtime".into(),
            event_id: format!("event-{sequence}"),
            conversation_id: "parent".into(),
            sequence,
            timestamp: sequence as i64,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn anchored_fork_stops_before_the_next_turn() {
        let events = vec![
            event(1, "BackendTurnBound", serde_json::json!({"data":"turn-1"})),
            event(2, "UserPrompt", serde_json::json!({"data":{"content":"one"}})),
            event(3, "Finish", serde_json::json!({})),
            event(4, "BackendTurnBound", serde_json::json!({"data":"turn-2"})),
            event(5, "UserPrompt", serde_json::json!({"data":{"content":"two"}})),
        ];

        let prefix = select_fork_prefix(&events, false, Some("turn-1")).unwrap();
        assert_eq!(prefix.len(), 3);
        assert!(prefix.iter().all(|event| event.sequence <= 3));
    }

    #[test]
    fn fork_rejects_an_open_tool_pair() {
        let events = vec![
            event(1, "BackendTurnBound", serde_json::json!({"data":"turn-1"})),
            event(
                2,
                "ToolCall",
                serde_json::json!({"data":{"call_id":"call-1","status":"running"}}),
            ),
        ];

        assert_eq!(
            select_fork_prefix(&events, true, None).unwrap_err(),
            "fork journal boundary splits a tool call/result pair"
        );
    }

    #[test]
    fn fork_omits_unapplied_input_lifecycle() {
        let events = vec![
            event(1, "InputHeld", serde_json::json!({"data":{"input_id":"input-1"}})),
            event(
                2,
                "UserPrompt",
                serde_json::json!({"data":{"msg_id":"input-1","content":"applied"}}),
            ),
            event(3, "InputApplied", serde_json::json!({"data":{"input_id":"input-1"}})),
        ];

        let prefix = select_fork_prefix(&events, true, None).unwrap();
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].kind, "UserPrompt");
    }
}
