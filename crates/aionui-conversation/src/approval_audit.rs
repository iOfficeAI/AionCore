//! Host-only approval audit events and projection.
//! `approval/asked` + `approval/decided`.
//!
//! These events never enter the model transcript. The grant itself is the
//! derived tool result plus the current policy snapshot.

use aionui_ai_agent::shared_kernel::{ApprovalOutcome, ApprovalPolicy};
use serde_json::json;

use crate::stream_persistence::{CanonicalEventJournal, CanonicalJournalEvent, canonical_event_id};

pub(crate) const KIND_APPROVAL_ASKED: &str = "ApprovalAsked";
pub(crate) const KIND_APPROVAL_DECIDED: &str = "ApprovalDecided";
pub(crate) const KIND_APPROVAL_POLICY: &str = "ApprovalPolicy";

pub(crate) fn fold_approval_policy(events: &[CanonicalJournalEvent]) -> ApprovalPolicy {
    events
        .iter()
        .rev()
        .find(|event| event.kind == KIND_APPROVAL_POLICY)
        .and_then(|event| {
            event
                .payload
                .pointer("/data/policy")
                .or_else(|| event.payload.pointer("/policy"))
                .and_then(serde_json::Value::as_str)
                .and_then(ApprovalPolicy::parse)
        })
        .unwrap_or(ApprovalPolicy::Ask)
}

pub(crate) async fn append_approval_asked(
    journal: &CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
    request_id: &str,
    call_id: &str,
    tool_name: Option<&str>,
) -> Result<CanonicalJournalEvent, std::io::Error> {
    let payload = json!({
        "type": "approval_asked",
        "data": {
            "request_id": request_id,
            "call_id": call_id,
            "tool_name": tool_name,
        }
    });
    let seed = format!("approval_asked:{conversation_id}:{request_id}");
    let event_id = canonical_event_id(&seed, &payload);
    journal
        .append(
            user_id,
            conversation_id,
            event_id,
            KIND_APPROVAL_ASKED.to_owned(),
            payload,
        )
        .await
}

pub(crate) async fn append_approval_decided(
    journal: &CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
    request_id: &str,
    call_id: &str,
    outcome: ApprovalOutcome,
) -> Result<CanonicalJournalEvent, std::io::Error> {
    let payload = json!({
        "type": "approval_decided",
        "data": {
            "request_id": request_id,
            "call_id": call_id,
            "outcome": outcome.as_str(),
        }
    });
    let seed = format!(
        "approval_decided:{conversation_id}:{request_id}:{outcome}",
        outcome = outcome.as_str()
    );
    let event_id = canonical_event_id(&seed, &payload);
    journal
        .append(
            user_id,
            conversation_id,
            event_id,
            KIND_APPROVAL_DECIDED.to_owned(),
            payload,
        )
        .await
}

/// Journal the session approval policy. Production callers arrive from
/// `set_host_policy`; tests already write this event to exercise `never`.
pub(crate) async fn append_approval_policy(
    journal: &CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
    policy: ApprovalPolicy,
) -> Result<CanonicalJournalEvent, std::io::Error> {
    let payload = json!({
        "type": "approval_policy",
        "data": { "policy": policy.as_str() }
    });
    let seed = format!("approval_policy:{conversation_id}:{}", policy.as_str());
    let event_id = canonical_event_id(&seed, &payload);
    journal
        .append(
            user_id,
            conversation_id,
            event_id,
            KIND_APPROVAL_POLICY.to_owned(),
            payload,
        )
        .await
}

pub(crate) fn picked_option_id(data: &serde_json::Value) -> Option<String> {
    match data {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Object(map) => map
            .get("option_id")
            .or_else(|| map.get("optionId"))
            .or_else(|| map.get("value"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        _ => None,
    }
}

pub(crate) fn extract_permission_call_id(kind: &str, payload: &serde_json::Value) -> Option<String> {
    if !matches!(kind, "Permission" | "AcpPermission") {
        return None;
    }
    let candidates = [
        payload.pointer("/call_id"),
        payload.pointer("/data/call_id"),
        payload.pointer("/tool_call/tool_call_id"),
        payload.pointer("/data/tool_call/tool_call_id"),
        payload.pointer("/id"),
        payload.pointer("/data/id"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64, kind: &str, payload: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            event_id: format!("event-{sequence}"),
            conversation_id: "conv".into(),
            sequence,
            timestamp: sequence as i64,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn default_policy_is_ask() {
        assert_eq!(fold_approval_policy(&[]), ApprovalPolicy::Ask);
    }

    #[test]
    fn last_policy_event_wins() {
        let events = vec![
            event(1, KIND_APPROVAL_POLICY, json!({"data":{"policy":"never"}})),
            event(2, KIND_APPROVAL_POLICY, json!({"data":{"policy":"ask"}})),
        ];
        assert_eq!(fold_approval_policy(&events), ApprovalPolicy::Ask);
        let never = vec![event(1, KIND_APPROVAL_POLICY, json!({"data":{"policy":"never"}}))];
        assert_eq!(fold_approval_policy(&never), ApprovalPolicy::Never);
    }

    #[test]
    fn unknown_policy_falls_back_to_ask() {
        let events = vec![event(1, KIND_APPROVAL_POLICY, json!({"data":{"policy":"maybe"}}))];
        assert_eq!(fold_approval_policy(&events), ApprovalPolicy::Ask);
    }

    #[test]
    fn permission_call_id_reads_known_shapes() {
        assert_eq!(
            extract_permission_call_id("AcpPermission", &json!({"tool_call":{"tool_call_id":"c1"}})).as_deref(),
            Some("c1")
        );
        assert_eq!(
            extract_permission_call_id("Permission", &json!({"call_id":"c2"})).as_deref(),
            Some("c2")
        );
        assert!(extract_permission_call_id("Text", &json!({"call_id":"nope"})).is_none());
    }

    #[tokio::test]
    async fn asked_and_decided_are_durable_and_paired() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        append_approval_asked(&journal, "user", "conv", "req-1", "call-1", Some("Bash"))
            .await
            .unwrap();
        append_approval_decided(
            &journal,
            "user",
            "conv",
            "req-1",
            "call-1",
            ApprovalOutcome::Unavailable,
        )
        .await
        .unwrap();
        let events = journal.replay("user", "conv").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, KIND_APPROVAL_ASKED);
        assert_eq!(events[1].kind, KIND_APPROVAL_DECIDED);
        assert_eq!(events[1].payload["data"]["outcome"], "unavailable");
    }
}
