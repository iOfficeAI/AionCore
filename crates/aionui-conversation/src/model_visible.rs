//! Runtime invariant: model-visible input is reconstructible from the journal.
//!
//! The invariant is "model visible ⇒ recorded". Adding a
//! model-facing input requires a journal event that `derive_transcript` can
//! project. The check is diagnostic: production logs a contract violation
//! instead of failing the turn.

use crate::journal_transcript::{RequestedVisibility, derive_transcript, recorded_content};
use crate::stream_persistence::CanonicalJournalEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimedModelInput {
    pub transcript_kind: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelVisibleViolation {
    pub reason: String,
}

impl std::fmt::Display for ModelVisibleViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Kinds that must carry a reconstructible payload (not a kind-name fallback).
const REQUIRED_PAYLOAD_KINDS: &[&str] = &["UserPrompt", "Ask"];

/// Every claimed model-visible input must appear in the derived model surface.
pub(crate) fn check_claimed_inputs_recorded(
    conversation_id: &str,
    events: &[CanonicalJournalEvent],
    claimed: &[ClaimedModelInput],
) -> Result<(), ModelVisibleViolation> {
    let transcript = derive_transcript(conversation_id, events, RequestedVisibility::Model);
    for claim in claimed {
        let found = transcript
            .items
            .iter()
            .any(|item| item.transcript_kind == claim.transcript_kind && item.content == claim.content);
        if !found {
            return Err(ModelVisibleViolation {
                reason: format!(
                    "claimed {} input is not reconstructible from the journal",
                    claim.transcript_kind
                ),
            });
        }
    }
    Ok(())
}

/// Model-visible events that own user/ask text must have a real payload.
pub(crate) fn check_model_surface_reconstructible(
    events: &[CanonicalJournalEvent],
) -> Result<(), ModelVisibleViolation> {
    for event in events {
        if !REQUIRED_PAYLOAD_KINDS.contains(&event.kind.as_str()) {
            continue;
        }
        if recorded_content(&event.kind, &event.payload).is_none() {
            return Err(ModelVisibleViolation {
                reason: format!(
                    "model-visible {} event {} has no reconstructible payload",
                    event.kind, event.event_id
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn recorded_user_prompt_satisfies_the_claimed_input() {
        let events = vec![event(
            1,
            "UserPrompt",
            serde_json::json!({"data":{"content":"list files"}}),
        )];
        check_claimed_inputs_recorded(
            "conv",
            &events,
            &[ClaimedModelInput {
                transcript_kind: "user/message",
                content: "list files".into(),
            }],
        )
        .expect("journaled prompt must be reconstructible");
    }

    #[test]
    fn missing_claimed_prompt_violates_the_invariant() {
        let err = check_claimed_inputs_recorded(
            "conv",
            &[],
            &[ClaimedModelInput {
                transcript_kind: "user/message",
                content: "list files".into(),
            }],
        )
        .expect_err("empty journal cannot reconstruct a claimed prompt");
        assert!(err.reason.contains("user/message"));
    }

    #[test]
    fn permission_events_are_not_required_on_the_model_surface() {
        let events = vec![
            event(1, "Permission", serde_json::json!({"type":"permission"})),
            event(2, "UserPrompt", serde_json::json!({"data":{"content":"ok"}})),
        ];
        check_claimed_inputs_recorded(
            "conv",
            &events,
            &[ClaimedModelInput {
                transcript_kind: "user/message",
                content: "ok".into(),
            }],
        )
        .expect("host-only permission must not block a recorded prompt");
        check_model_surface_reconstructible(&events).expect("permission has no required payload");
    }

    #[test]
    fn user_prompt_without_payload_fails_reconstructible_check() {
        let events = vec![event(1, "UserPrompt", serde_json::json!({"type":"user_prompt"}))];
        let err = check_model_surface_reconstructible(&events).expect_err("empty prompt payload");
        assert!(err.reason.contains("UserPrompt"));
    }
}
