//! Deterministic reconciliation of validated Memory task candidates.

use std::collections::{HashMap, HashSet};

use aionui_api_types::{MemoryEntryKind, MemoryUpdateInput};
use aionui_common::generate_prefixed_id;
use aionui_db::models::MemoryEntryRow;
use aionui_db::{CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    MemoryError,
    validation::{ValidatedCandidate, ValidatedCandidateAction, normalize_stable_key},
};

pub(crate) struct Reconciler;

impl Reconciler {
    pub(crate) fn reconcile(
        user_id: &str,
        conversation_id: &str,
        evidence: &MemoryUpdateInput,
        stored_entries: &[MemoryEntryRow],
        candidates: Vec<ValidatedCandidate>,
    ) -> Result<Vec<CommitMemoryEntryRow>, MemoryError> {
        let supplied_ids = evidence
            .existing_entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>();
        let mut existing_by_id = HashMap::new();
        let mut existing_by_fingerprint = HashMap::new();
        for entry in stored_entries {
            if !supplied_ids.contains(entry.id.as_str()) {
                continue;
            }
            if entry.user_id != user_id || entry.state != "active" {
                return Err(MemoryError::InvalidInput);
            }
            let kind = entry_kind(&entry.kind)?;
            let fingerprint = memory_fingerprint(
                user_id,
                entry.project_id.as_deref(),
                entry.workspace_key.as_deref(),
                &kind,
                &normalize_stable_key(&entry.stable_key)?,
            )?;
            existing_by_id.insert(entry.id.as_str(), entry);
            if entry.project_id == evidence.conversation.project_id
                && entry.workspace_key == evidence.conversation.workspace_key
            {
                existing_by_fingerprint.insert(fingerprint, entry);
            }
        }
        if existing_by_id.len() != supplied_ids.len() {
            return Err(MemoryError::InvalidInput);
        }

        let mut reconciled = Vec::with_capacity(candidates.len());
        let mut targeted_entries = HashSet::new();
        let mut candidate_fingerprints = HashSet::new();
        for candidate in candidates {
            let kind = kind_name(&candidate.kind);
            let fingerprint = memory_fingerprint(
                user_id,
                evidence.conversation.project_id.as_deref(),
                evidence.conversation.workspace_key.as_deref(),
                &candidate.kind,
                &candidate.stable_key,
            )?;
            if !candidate_fingerprints.insert(fingerprint.clone()) {
                return Err(MemoryError::InvalidInput);
            }
            let (transition, target_id) = match &candidate.action {
                ValidatedCandidateAction::Create => match existing_by_fingerprint.get(&fingerprint) {
                    Some(target) if target.pinned || target.user_edited => {
                        let group = conflict_group_id(user_id, &target.id, &fingerprint)?;
                        (
                            CommitMemoryEntryTransition::Conflict {
                                target_entry_id: target.id.clone(),
                                conflict_group_id: group,
                            },
                            Some(target.id.as_str()),
                        )
                    }
                    Some(target) => (
                        CommitMemoryEntryTransition::Refine {
                            target_entry_id: target.id.clone(),
                        },
                        Some(target.id.as_str()),
                    ),
                    None => (CommitMemoryEntryTransition::Create, None),
                },
                ValidatedCandidateAction::Refine { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &existing_by_id,
                    ExplicitAction::Refine,
                )?,
                ValidatedCandidateAction::Supersede { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &existing_by_id,
                    ExplicitAction::Supersede,
                )?,
                ValidatedCandidateAction::Conflict { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &existing_by_id,
                    ExplicitAction::Conflict,
                )?,
            };
            if target_id.is_some_and(|target| !targeted_entries.insert(target.to_owned())) {
                return Err(MemoryError::InvalidInput);
            }
            reconciled.push(CommitMemoryEntryRow {
                id: generate_prefixed_id("memory-entry"),
                project_id: evidence.conversation.project_id.clone(),
                workspace_key: evidence.conversation.workspace_key.clone(),
                kind: kind.into(),
                stable_key: candidate.stable_key,
                fingerprint,
                content: candidate.content,
                transition,
                sources: candidate
                    .sources
                    .into_iter()
                    .map(|source| CommitMemorySourceRow {
                        conversation_id: conversation_id.into(),
                        turn_id: source.turn_id,
                        message_ids_json: source.message_ids_json,
                    })
                    .collect(),
            });
        }
        Ok(reconciled)
    }
}

#[derive(Clone, Copy)]
enum ExplicitAction {
    Refine,
    Supersede,
    Conflict,
}

fn reconcile_explicit_target<'a>(
    user_id: &str,
    target_entry_id: &'a str,
    candidate_fingerprint: &str,
    existing_by_id: &HashMap<&str, &MemoryEntryRow>,
    action: ExplicitAction,
) -> Result<(CommitMemoryEntryTransition, Option<&'a str>), MemoryError> {
    let target = existing_by_id.get(target_entry_id).ok_or(MemoryError::InvalidInput)?;
    let protected = target.pinned || target.user_edited;
    let transition = match action {
        ExplicitAction::Refine if !protected => CommitMemoryEntryTransition::Refine {
            target_entry_id: target_entry_id.into(),
        },
        ExplicitAction::Supersede if !protected => CommitMemoryEntryTransition::Supersede {
            target_entry_id: target_entry_id.into(),
        },
        ExplicitAction::Refine | ExplicitAction::Supersede | ExplicitAction::Conflict => {
            CommitMemoryEntryTransition::Conflict {
                target_entry_id: target_entry_id.into(),
                conflict_group_id: conflict_group_id(user_id, target_entry_id, candidate_fingerprint)?,
            }
        }
    };
    Ok((transition, Some(target_entry_id)))
}

pub(crate) fn memory_fingerprint(
    user_id: &str,
    project_id: Option<&str>,
    workspace_key: Option<&str>,
    kind: &MemoryEntryKind,
    stable_key: &str,
) -> Result<String, MemoryError> {
    structured_hash(&(
        "memory-fingerprint-v1",
        user_id,
        project_id,
        workspace_key,
        kind_name(kind),
        stable_key,
    ))
}

fn conflict_group_id(user_id: &str, target_entry_id: &str, fingerprint: &str) -> Result<String, MemoryError> {
    Ok(format!(
        "memory-conflict-{}",
        structured_hash(&("memory-conflict-v1", user_id, target_entry_id, fingerprint))?,
    ))
}

fn structured_hash(value: &impl Serialize) -> Result<String, MemoryError> {
    let material = serde_json::to_vec(value).map_err(|_| MemoryError::Internal)?;
    Ok(Sha256::digest(material)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn kind_name(kind: &MemoryEntryKind) -> &'static str {
    match kind {
        MemoryEntryKind::Decision => "decision",
        MemoryEntryKind::Outcome => "outcome",
        MemoryEntryKind::Artifact => "artifact",
        MemoryEntryKind::Issue => "issue",
        MemoryEntryKind::NextStep => "next_step",
        MemoryEntryKind::WorkConstraint => "work_constraint",
    }
}

fn entry_kind(kind: &str) -> Result<MemoryEntryKind, MemoryError> {
    match kind {
        "decision" => Ok(MemoryEntryKind::Decision),
        "outcome" => Ok(MemoryEntryKind::Outcome),
        "artifact" => Ok(MemoryEntryKind::Artifact),
        "issue" => Ok(MemoryEntryKind::Issue),
        "next_step" => Ok(MemoryEntryKind::NextStep),
        "work_constraint" => Ok(MemoryEntryKind::WorkConstraint),
        _ => Err(MemoryError::InvalidInput),
    }
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{
        ExistingMemoryEntryInput, MemoryEntryKind, MemoryUpdateConversationInput, MemoryUpdateInput,
    };
    use aionui_db::CommitMemoryEntryTransition;
    use aionui_db::models::MemoryEntryRow;

    use super::{Reconciler, memory_fingerprint};
    use crate::validation::{ValidatedCandidate, ValidatedCandidateAction, ValidatedSource};

    #[test]
    fn fingerprint_is_stable_for_normalized_identity_and_bound_to_owner_and_scope() {
        let first = memory_fingerprint(
            "user-1",
            Some("project-1"),
            None,
            &MemoryEntryKind::Decision,
            "release plan",
        )
        .unwrap();
        assert_eq!(
            first,
            memory_fingerprint(
                "user-1",
                Some("project-1"),
                None,
                &MemoryEntryKind::Decision,
                "release plan"
            )
            .unwrap(),
        );
        assert_ne!(
            first,
            memory_fingerprint(
                "user-2",
                Some("project-1"),
                None,
                &MemoryEntryKind::Decision,
                "release plan"
            )
            .unwrap(),
        );
        assert_ne!(
            first,
            memory_fingerprint(
                "user-1",
                Some("project-2"),
                None,
                &MemoryEntryKind::Decision,
                "release plan"
            )
            .unwrap(),
        );
    }

    #[test]
    fn matching_create_refines_but_protected_matching_create_conflicts() {
        let evidence = evidence(false);
        let refined = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(false, Some("project-1")),
            vec![candidate(ValidatedCandidateAction::Create)],
        )
        .unwrap();
        assert!(matches!(
            refined[0].transition,
            CommitMemoryEntryTransition::Refine { ref target_entry_id } if target_entry_id == "entry-1"
        ));

        let protected = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(true, Some("project-1")),
            vec![candidate(ValidatedCandidateAction::Create)],
        )
        .unwrap();
        assert!(matches!(
            protected[0].transition,
            CommitMemoryEntryTransition::Conflict { ref target_entry_id, .. } if target_entry_id == "entry-1"
        ));
    }

    #[test]
    fn explicit_replacement_and_ambiguity_map_without_model_calls() {
        let evidence = evidence(false);
        let reconciled = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(false, Some("project-1")),
            vec![candidate(ValidatedCandidateAction::Supersede {
                target_entry_id: "entry-1".into(),
            })],
        )
        .unwrap();
        assert!(matches!(
            reconciled[0].transition,
            CommitMemoryEntryTransition::Supersede { ref target_entry_id } if target_entry_id == "entry-1"
        ));

        let reconciled = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(false, Some("project-1")),
            vec![candidate(ValidatedCandidateAction::Conflict {
                target_entry_id: "entry-1".into(),
            })],
        )
        .unwrap();
        assert!(matches!(
            reconciled[0].transition,
            CommitMemoryEntryTransition::Conflict { ref target_entry_id, .. } if target_entry_id == "entry-1"
        ));
    }

    #[test]
    fn duplicate_normalized_candidate_identity_is_rejected() {
        let evidence = MemoryUpdateInput {
            existing_entries: Vec::new(),
            ..evidence(false)
        };
        assert!(matches!(
            Reconciler::reconcile(
                "user-1",
                "conversation-1",
                &evidence,
                &[],
                vec![
                    candidate(ValidatedCandidateAction::Create),
                    candidate(ValidatedCandidateAction::Create),
                ],
            ),
            Err(crate::MemoryError::InvalidInput),
        ));
    }

    #[test]
    fn matching_key_in_a_different_scope_remains_a_create() {
        let evidence = evidence(false);
        let reconciled = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(false, None),
            vec![candidate(ValidatedCandidateAction::Create)],
        )
        .unwrap();
        assert_eq!(reconciled[0].transition, CommitMemoryEntryTransition::Create);
    }

    fn candidate(action: ValidatedCandidateAction) -> ValidatedCandidate {
        ValidatedCandidate {
            action,
            kind: MemoryEntryKind::Decision,
            stable_key: "release plan".into(),
            content: "Ship".into(),
            sources: vec![ValidatedSource {
                turn_id: "turn-1".into(),
                message_ids_json: r#"["message-1"]"#.into(),
            }],
        }
    }

    fn evidence(pinned: bool) -> MemoryUpdateInput {
        MemoryUpdateInput {
            conversation: MemoryUpdateConversationInput {
                id: "conversation-1".into(),
                project_id: Some("project-1".into()),
                workspace_key: None,
            },
            previous_summary: None,
            existing_entries: vec![ExistingMemoryEntryInput {
                id: "entry-1".into(),
                kind: MemoryEntryKind::Decision,
                stable_key: "release plan".into(),
                content: "Existing".into(),
                pinned,
                user_edited: false,
            }],
            source_turns: Vec::new(),
        }
    }

    fn stored(pinned: bool, project_id: Option<&str>) -> Vec<MemoryEntryRow> {
        vec![MemoryEntryRow {
            id: "entry-1".into(),
            user_id: "user-1".into(),
            project_id: project_id.map(str::to_owned),
            workspace_key: None,
            kind: "decision".into(),
            stable_key: "release plan".into(),
            fingerprint: "stored-fingerprint-is-not-trusted".into(),
            content: Some("Existing".into()),
            state: "active".into(),
            pinned,
            user_edited: false,
            supersedes_id: None,
            conflict_group_id: None,
            schema_version: 1,
            deleted_at: None,
            created_at: 1,
            updated_at: 1,
            sources: Vec::new(),
        }]
    }
}
