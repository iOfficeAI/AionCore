//! Validation for untrusted Memory task proposals.

use std::collections::{HashMap, HashSet};

use aionui_api_types::{
    MemoryCandidateMutation, MemoryEntryKind, MemorySummary, MemoryTaskResultProvenance, MemoryUpdateInput,
    MemoryUpdateOutput,
};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{
    MemoryError,
    sanitizer::{
        MAX_EVIDENCE_TURNS, MAX_MUTATION_COUNT, MAX_STRING_LENGTH, MAX_SUMMARY_BYTES, MAX_SUMMARY_ITEMS, sanitize_text,
        strip_user_context_sentences,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidatedCandidateAction {
    Create,
    Refine { target_entry_id: String },
    Supersede { target_entry_id: String },
    Conflict { target_entry_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedCandidate {
    pub action: ValidatedCandidateAction,
    pub kind: MemoryEntryKind,
    pub stable_key: String,
    pub content: String,
    pub sources: Vec<ValidatedSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedSource {
    pub turn_id: String,
    pub message_ids_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedProposal {
    pub summary_json: String,
    pub candidates: Vec<ValidatedCandidate>,
    pub provenance: MemoryTaskResultProvenance,
}

pub(crate) struct ProposalValidator;

impl ProposalValidator {
    pub(crate) fn validate(
        output: MemoryUpdateOutput,
        provenance: MemoryTaskResultProvenance,
        evidence: &MemoryUpdateInput,
    ) -> Result<ValidatedProposal, MemoryError> {
        validate_metadata(&provenance)?;
        if output.mutations.len() > MAX_MUTATION_COUNT {
            return Err(MemoryError::InvalidInput);
        }
        let summary = sanitize_summary(output.summary)?;
        let summary_json = serde_json::to_string(&summary).map_err(|_| MemoryError::InvalidInput)?;
        if summary_json.len() > MAX_SUMMARY_BYTES {
            return Err(MemoryError::InvalidInput);
        }

        let valid_targets = evidence
            .existing_entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>();
        let turns = evidence
            .source_turns
            .iter()
            .map(|turn| (turn.turn_id.as_str(), turn))
            .collect::<HashMap<_, _>>();
        let mut targeted_entries = HashSet::new();
        let mut candidates = Vec::with_capacity(output.mutations.len());

        for mutation in output.mutations {
            let (action, kind, raw_stable_key, raw_content, source_turn_ids) = match mutation {
                MemoryCandidateMutation::Create {
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => (
                    ValidatedCandidateAction::Create,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                ),
                MemoryCandidateMutation::Refine {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => (
                    validate_target(
                        target_entry_id,
                        &valid_targets,
                        &mut targeted_entries,
                        |target_entry_id| ValidatedCandidateAction::Refine { target_entry_id },
                    )?,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                ),
                MemoryCandidateMutation::Supersede {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => (
                    validate_target(
                        target_entry_id,
                        &valid_targets,
                        &mut targeted_entries,
                        |target_entry_id| ValidatedCandidateAction::Supersede { target_entry_id },
                    )?,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                ),
                MemoryCandidateMutation::Conflict {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => (
                    validate_target(
                        target_entry_id,
                        &valid_targets,
                        &mut targeted_entries,
                        |target_entry_id| ValidatedCandidateAction::Conflict { target_entry_id },
                    )?,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                ),
            };

            if raw_stable_key.len() > MAX_STRING_LENGTH || raw_content.len() > MAX_STRING_LENGTH {
                return Err(MemoryError::InvalidInput);
            }
            let stable_key = normalize_stable_key(&raw_stable_key)?;
            let content = strip_user_context_sentences(&sanitize_text(&raw_content));
            if content.trim().is_empty() || content.len() > MAX_STRING_LENGTH {
                return Err(MemoryError::InvalidInput);
            }
            if source_turn_ids.is_empty() || source_turn_ids.len() > MAX_EVIDENCE_TURNS {
                return Err(MemoryError::InvalidInput);
            }
            let mut unique_turns = HashSet::new();
            let mut sources = Vec::with_capacity(source_turn_ids.len());
            for turn_id in source_turn_ids {
                if !unique_turns.insert(turn_id.clone()) {
                    return Err(MemoryError::InvalidInput);
                }
                let turn = turns.get(turn_id.as_str()).ok_or(MemoryError::InvalidInput)?;
                sources.push(ValidatedSource {
                    turn_id,
                    message_ids_json: serde_json::to_string(
                        &turn
                            .messages
                            .iter()
                            .map(|message| &message.message_id)
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|_| MemoryError::Internal)?,
                });
            }
            candidates.push(ValidatedCandidate {
                action,
                kind,
                stable_key,
                content,
                sources,
            });
        }

        Ok(ValidatedProposal {
            summary_json,
            candidates,
            provenance,
        })
    }
}

fn validate_target<F>(
    target_entry_id: String,
    valid_targets: &HashSet<&str>,
    targeted_entries: &mut HashSet<String>,
    action: F,
) -> Result<ValidatedCandidateAction, MemoryError>
where
    F: FnOnce(String) -> ValidatedCandidateAction,
{
    if !valid_targets.contains(target_entry_id.as_str()) || !targeted_entries.insert(target_entry_id.clone()) {
        return Err(MemoryError::InvalidInput);
    }
    Ok(action(target_entry_id))
}

pub(crate) fn normalize_stable_key(value: &str) -> Result<String, MemoryError> {
    if value.is_empty() || value.len() > MAX_STRING_LENGTH {
        return Err(MemoryError::InvalidInput);
    }
    let normalized = value.nfkc().flat_map(char::to_lowercase);
    let mut output = String::with_capacity(value.len());
    let mut pending_separator = false;
    for character in normalized {
        if character.is_alphanumeric() || is_combining_mark(character) {
            if pending_separator && !output.is_empty() {
                output.push(' ');
            }
            output.push(character);
            pending_separator = false;
        } else if !output.is_empty() {
            pending_separator = true;
        }
    }
    let output = output.nfc().collect::<String>();
    if output.is_empty() || output.len() > MAX_STRING_LENGTH {
        return Err(MemoryError::InvalidInput);
    }
    Ok(output)
}

pub(crate) fn sanitize_summary(summary: MemorySummary) -> Result<MemorySummary, MemoryError> {
    let item_count = usize::from(!summary.goal.is_empty())
        + summary.current_state.len()
        + summary.decisions.len()
        + summary.artifacts.len()
        + summary.issues.len()
        + summary.next_steps.len()
        + summary.work_constraints.len();
    if summary.goal.len() > MAX_STRING_LENGTH || item_count > MAX_SUMMARY_ITEMS {
        return Err(MemoryError::InvalidInput);
    }
    let goal = strip_user_context_sentences(&sanitize_text(&summary.goal));
    let sanitize_values = |values: Vec<String>| -> Result<Vec<String>, MemoryError> {
        values
            .into_iter()
            .map(|value| {
                if value.len() > MAX_STRING_LENGTH {
                    return Err(MemoryError::InvalidInput);
                }
                let value = strip_user_context_sentences(&sanitize_text(&value));
                (!value.trim().is_empty() && value.len() <= MAX_STRING_LENGTH)
                    .then_some(value)
                    .ok_or(MemoryError::InvalidInput)
            })
            .collect()
    };
    let summary = MemorySummary {
        goal,
        current_state: sanitize_values(summary.current_state)?,
        decisions: sanitize_values(summary.decisions)?,
        artifacts: sanitize_values(summary.artifacts)?,
        issues: sanitize_values(summary.issues)?,
        next_steps: sanitize_values(summary.next_steps)?,
        work_constraints: sanitize_values(summary.work_constraints)?,
    };
    (summary.goal.len() <= MAX_STRING_LENGTH)
        .then_some(summary)
        .ok_or(MemoryError::InvalidInput)
}

fn validate_metadata(provenance: &MemoryTaskResultProvenance) -> Result<(), MemoryError> {
    for value in [
        &provenance.provider_id,
        &provenance.model_id,
        &provenance.prompt_version,
    ] {
        if value.trim().is_empty() || value.len() > MAX_STRING_LENGTH {
            return Err(MemoryError::InvalidInput);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{
        ExistingMemoryEntryInput, MemoryEntryKind, MemorySourceMessageInput, MemorySourceMessageRole,
        MemorySourceTurnInput, MemoryTaskResultProvenance, MemoryUpdateConversationInput,
    };
    use serde_json::json;

    use super::{ProposalValidator, normalize_stable_key};
    use crate::{MemoryError, sanitizer::MAX_MUTATION_COUNT};

    #[test]
    fn model_contract_rejects_unknown_invalid_and_missing_fields() {
        let base = json!({
            "summary": {
                "goal": "Ship",
                "current_state": [],
                "decisions": [],
                "artifacts": [],
                "issues": [],
                "next_steps": [],
                "work_constraints": []
            },
            "mutations": []
        });
        let mut unknown = base.clone();
        unknown["model_fingerprint"] = json!("untrusted");
        assert!(serde_json::from_value::<aionui_api_types::MemoryUpdateOutput>(unknown).is_err());

        let mut missing_summary_field = base.clone();
        missing_summary_field["summary"]
            .as_object_mut()
            .unwrap()
            .remove("issues");
        assert!(serde_json::from_value::<aionui_api_types::MemoryUpdateOutput>(missing_summary_field).is_err());

        for (field, value) in [("action", "delete"), ("kind", "preference")] {
            let mut invalid = base.clone();
            invalid["mutations"] = json!([{
                "action": "create",
                "kind": "decision",
                "stable_key": "release",
                "content": "Ship",
                "source_turn_ids": ["turn-1"]
            }]);
            invalid["mutations"][0][field] = json!(value);
            assert!(serde_json::from_value::<aionui_api_types::MemoryUpdateOutput>(invalid).is_err());
        }
    }

    #[test]
    fn unicode_normalization_collapses_case_whitespace_and_punctuation() {
        assert_eq!(
            normalize_stable_key("  CAF\u{c9}\u{2014}Release...Plan  ").unwrap(),
            "caf\u{e9} release plan",
        );
        assert_eq!(
            normalize_stable_key("cafe\u{301}\tRELEASE_plan").unwrap(),
            "caf\u{e9} release plan",
        );
    }

    #[test]
    fn validator_rejects_duplicate_omitted_and_out_of_evidence_targets() {
        let evidence = evidence();
        let duplicate = output(json!([
            mutation("refine", "entry-1", "turn-1"),
            mutation("conflict", "entry-1", "turn-1")
        ]));
        assert_eq!(
            ProposalValidator::validate(duplicate, provenance(), &evidence),
            Err(MemoryError::InvalidInput),
        );

        for invalid in [
            output(json!([mutation("refine", "omitted-entry", "turn-1")])),
            output(json!([mutation("refine", "entry-1", "turn-outside-evidence")])),
        ] {
            assert_eq!(
                ProposalValidator::validate(invalid, provenance(), &evidence),
                Err(MemoryError::InvalidInput),
            );
        }
    }

    #[test]
    fn validator_rejects_oversized_values_and_excess_mutations() {
        let evidence = evidence();
        let oversized = output(json!([{
            "action": "create",
            "kind": "decision",
            "stable_key": "release",
            "content": "x".repeat(crate::sanitizer::MAX_STRING_LENGTH + 1),
            "source_turn_ids": ["turn-1"]
        }]));
        assert_eq!(
            ProposalValidator::validate(oversized, provenance(), &evidence),
            Err(MemoryError::InvalidInput),
        );

        let mutations = (0..=MAX_MUTATION_COUNT)
            .map(|index| {
                json!({
                    "action": "create",
                    "kind": "decision",
                    "stable_key": format!("release-{index}"),
                    "content": "Ship",
                    "source_turn_ids": ["turn-1"]
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ProposalValidator::validate(output(json!(mutations)), provenance(), &evidence),
            Err(MemoryError::InvalidInput),
        );
    }

    fn output(mutations: serde_json::Value) -> aionui_api_types::MemoryUpdateOutput {
        serde_json::from_value(json!({
            "summary": {
                "goal": "Ship",
                "current_state": [],
                "decisions": [],
                "artifacts": [],
                "issues": [],
                "next_steps": [],
                "work_constraints": []
            },
            "mutations": mutations
        }))
        .unwrap()
    }

    fn mutation(action: &str, target: &str, turn: &str) -> serde_json::Value {
        json!({
            "action": action,
            "target_entry_id": target,
            "kind": "decision",
            "stable_key": "release",
            "content": "Ship",
            "source_turn_ids": [turn]
        })
    }

    fn provenance() -> MemoryTaskResultProvenance {
        MemoryTaskResultProvenance {
            provider_id: "provider-result".into(),
            model_id: "model-result".into(),
            prompt_version: "memory-prompt-v1".into(),
        }
    }

    fn evidence() -> aionui_api_types::MemoryUpdateInput {
        aionui_api_types::MemoryUpdateInput {
            conversation: MemoryUpdateConversationInput {
                id: "conversation-1".into(),
                project_id: Some("project-1".into()),
                workspace_key: None,
            },
            previous_summary: None,
            existing_entries: vec![ExistingMemoryEntryInput {
                id: "entry-1".into(),
                kind: MemoryEntryKind::Decision,
                stable_key: "release".into(),
                content: "Existing".into(),
                pinned: false,
                user_edited: false,
            }],
            source_turns: vec![MemorySourceTurnInput {
                turn_id: "turn-1".into(),
                messages: vec![MemorySourceMessageInput {
                    message_id: "message-1".into(),
                    role: MemorySourceMessageRole::User,
                    content: "Ship".into(),
                }],
            }],
        }
    }
}
