//! Deterministic reconciliation of validated Memory task candidates.

use std::collections::{HashMap, HashSet};

use aionui_api_types::{MemoryEntryKind, MemoryUpdateInput};
use aionui_common::generate_prefixed_id;
use aionui_db::models::MemoryEntryRow;
use aionui_db::{CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow, ExpectedMemoryEntryRow};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    MemoryError,
    validation::{ValidatedCandidate, ValidatedCandidateAction, normalize_stable_key},
};

pub(crate) struct Reconciler;

pub(crate) struct ReconciliationLookup {
    pub fingerprints: Vec<String>,
    pub target_ids: Vec<String>,
}

impl Reconciler {
    pub(crate) fn lookup(
        user_id: &str,
        evidence: &MemoryUpdateInput,
        candidates: &[ValidatedCandidate],
    ) -> Result<ReconciliationLookup, MemoryError> {
        let mut fingerprints = Vec::with_capacity(candidates.len());
        let mut target_ids = Vec::new();
        for candidate in candidates {
            fingerprints.push(memory_fingerprint(
                user_id,
                evidence.conversation.project_id.as_deref(),
                evidence.conversation.workspace_key.as_deref(),
                &candidate.kind,
                &candidate.stable_key,
            )?);
            let target = match &candidate.action {
                ValidatedCandidateAction::Create => None,
                ValidatedCandidateAction::Refine { target_entry_id }
                | ValidatedCandidateAction::Supersede { target_entry_id }
                | ValidatedCandidateAction::Conflict { target_entry_id } => Some(target_entry_id.clone()),
            };
            if let Some(target) = target {
                target_ids.push(target);
            }
        }
        Ok(ReconciliationLookup {
            fingerprints,
            target_ids,
        })
    }

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
        let mut tombstoned_fingerprints = HashSet::new();
        for entry in stored_entries {
            if entry.user_id != user_id {
                return Err(MemoryError::InvalidInput);
            }
            if supplied_ids.contains(entry.id.as_str()) {
                existing_by_id.insert(entry.id.as_str(), entry);
            }
            if entry.state == "deleted" {
                tombstoned_fingerprints.insert(entry.fingerprint.clone());
                continue;
            }
            if entry.state != "active" {
                continue;
            }
            let kind = entry_kind(&entry.kind)?;
            let fingerprint = memory_fingerprint(
                user_id,
                entry.project_id.as_deref(),
                entry.workspace_key.as_deref(),
                &kind,
                &normalize_stable_key(&entry.stable_key)?,
            )?;
            if entry.project_id == evidence.conversation.project_id
                && entry.workspace_key == evidence.conversation.workspace_key
            {
                existing_by_fingerprint.insert(fingerprint, entry);
            }
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
            if tombstoned_fingerprints.contains(&fingerprint) {
                continue;
            }
            if let Some(target_id) = match &candidate.action {
                ValidatedCandidateAction::Create => None,
                ValidatedCandidateAction::Refine { target_entry_id }
                | ValidatedCandidateAction::Supersede { target_entry_id }
                | ValidatedCandidateAction::Conflict { target_entry_id } => Some(target_entry_id),
            } && existing_by_fingerprint
                .get(&fingerprint)
                .is_some_and(|existing| existing.id != *target_id)
            {
                return Err(MemoryError::InvalidInput);
            }
            let (transition, target_id, target_scope) = match &candidate.action {
                ValidatedCandidateAction::Create => match existing_by_fingerprint.get(&fingerprint) {
                    Some(target)
                        if (target.pinned || target.user_edited)
                            && target.content.as_deref() == Some(candidate.content.as_str()) =>
                    {
                        (
                            CommitMemoryEntryTransition::AttachSource {
                                target: expected_entry(target),
                            },
                            Some(target.id.as_str()),
                            Some((target.project_id.clone(), target.workspace_key.clone())),
                        )
                    }
                    Some(target) if target.pinned || target.user_edited => {
                        let group = conflict_group_id(user_id, &target.id, &fingerprint)?;
                        (
                            CommitMemoryEntryTransition::Conflict {
                                target: expected_entry(target),
                                conflict_group_id: group,
                            },
                            Some(target.id.as_str()),
                            None,
                        )
                    }
                    Some(target) => (
                        CommitMemoryEntryTransition::Refine {
                            target: expected_entry(target),
                        },
                        Some(target.id.as_str()),
                        Some((target.project_id.clone(), target.workspace_key.clone())),
                    ),
                    None => (CommitMemoryEntryTransition::Create, None, None),
                },
                ValidatedCandidateAction::Refine { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &candidate.content,
                    evidence,
                    &existing_by_id,
                    ExplicitAction::Refine,
                )?,
                ValidatedCandidateAction::Supersede { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &candidate.content,
                    evidence,
                    &existing_by_id,
                    ExplicitAction::Supersede,
                )?,
                ValidatedCandidateAction::Conflict { target_entry_id } => reconcile_explicit_target(
                    user_id,
                    target_entry_id,
                    &fingerprint,
                    &candidate.content,
                    evidence,
                    &existing_by_id,
                    ExplicitAction::Conflict,
                )?,
            };
            if target_id.is_some_and(|target| !targeted_entries.insert(target.to_owned())) {
                return Err(MemoryError::InvalidInput);
            }
            let (project_id, workspace_key) = target_scope.unwrap_or_else(|| {
                (
                    evidence.conversation.project_id.clone(),
                    evidence.conversation.workspace_key.clone(),
                )
            });
            reconciled.push(CommitMemoryEntryRow {
                id: generate_prefixed_id("memory-entry"),
                project_id,
                workspace_key,
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

type MemoryScope = (Option<String>, Option<String>);
type ReconciledTarget<'a> = (CommitMemoryEntryTransition, Option<&'a str>, Option<MemoryScope>);

fn reconcile_explicit_target<'a>(
    user_id: &str,
    target_entry_id: &'a str,
    candidate_fingerprint: &str,
    candidate_content: &str,
    evidence: &MemoryUpdateInput,
    existing_by_id: &HashMap<&str, &MemoryEntryRow>,
    action: ExplicitAction,
) -> Result<ReconciledTarget<'a>, MemoryError> {
    let target = existing_by_id.get(target_entry_id).ok_or(MemoryError::InvalidInput)?;
    if target.project_id != evidence.conversation.project_id
        || target.workspace_key != evidence.conversation.workspace_key
    {
        return Err(MemoryError::InvalidInput);
    }
    let protected = target.pinned || target.user_edited;
    let transition = match action {
        ExplicitAction::Refine
            if protected
                && target.fingerprint == candidate_fingerprint
                && target.content.as_deref() == Some(candidate_content) =>
        {
            CommitMemoryEntryTransition::AttachSource {
                target: expected_entry(target),
            }
        }
        ExplicitAction::Refine if !protected => CommitMemoryEntryTransition::Refine {
            target: expected_entry(target),
        },
        ExplicitAction::Supersede if !protected => CommitMemoryEntryTransition::Supersede {
            target: expected_entry(target),
        },
        ExplicitAction::Refine | ExplicitAction::Supersede | ExplicitAction::Conflict => {
            CommitMemoryEntryTransition::Conflict {
                target: expected_entry(target),
                conflict_group_id: conflict_group_id(user_id, target_entry_id, candidate_fingerprint)?,
            }
        }
    };
    Ok((
        transition,
        Some(target_entry_id),
        Some((target.project_id.clone(), target.workspace_key.clone())),
    ))
}

fn expected_entry(entry: &MemoryEntryRow) -> ExpectedMemoryEntryRow {
    ExpectedMemoryEntryRow {
        id: entry.id.clone(),
        revision: entry.revision,
        state: entry.state.clone(),
        fingerprint: entry.fingerprint.clone(),
        project_id: entry.project_id.clone(),
        workspace_key: entry.workspace_key.clone(),
        content: entry.content.clone(),
    }
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
            CommitMemoryEntryTransition::Refine { ref target } if target.id == "entry-1"
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
            CommitMemoryEntryTransition::Conflict { ref target, .. } if target.id == "entry-1"
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
            CommitMemoryEntryTransition::Supersede { ref target } if target.id == "entry-1"
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
            CommitMemoryEntryTransition::Conflict { ref target, .. } if target.id == "entry-1"
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

    #[test]
    fn full_store_matches_outside_the_evidence_window_and_respects_tombstones() {
        let evidence = MemoryUpdateInput {
            existing_entries: Vec::new(),
            ..evidence(false)
        };
        let active = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &stored(false, Some("project-1")),
            vec![candidate(ValidatedCandidateAction::Create)],
        )
        .unwrap();
        assert!(matches!(
            active[0].transition,
            CommitMemoryEntryTransition::Refine { ref target } if target.id == "entry-1"
        ));

        let mut deleted = stored(false, Some("project-1"));
        deleted[0].state = "deleted".into();
        deleted[0].content = None;
        assert!(
            Reconciler::reconcile(
                "user-1",
                "conversation-1",
                &evidence,
                &deleted,
                vec![candidate(ValidatedCandidateAction::Create)],
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn protected_identical_content_attaches_provenance_without_mutating_the_entry() {
        let evidence = evidence(true);
        let mut entries = stored(true, Some("project-1"));
        entries[0].content = Some("Ship".into());
        let reconciled = Reconciler::reconcile(
            "user-1",
            "conversation-1",
            &evidence,
            &entries,
            vec![candidate(ValidatedCandidateAction::Create)],
        )
        .unwrap();
        assert!(matches!(
            reconciled[0].transition,
            CommitMemoryEntryTransition::AttachSource { ref target }
                if target.id == "entry-1" && target.content.as_deref() == Some("Ship")
        ));
    }

    #[test]
    fn protected_identical_content_with_a_different_identity_conflicts() {
        let evidence = evidence(true);
        let mut entries = stored(true, Some("project-1"));
        entries[0].content = Some("Ship".into());
        let mut proposal = candidate(ValidatedCandidateAction::Refine {
            target_entry_id: "entry-1".into(),
        });
        proposal.stable_key = "different release identity".into();
        let reconciled =
            Reconciler::reconcile("user-1", "conversation-1", &evidence, &entries, vec![proposal]).unwrap();
        assert!(matches!(
            reconciled[0].transition,
            CommitMemoryEntryTransition::Conflict { ref target, .. } if target.id == "entry-1"
        ));
    }

    #[test]
    fn explicit_targets_must_match_the_conversation_scope_exactly() {
        let evidence = evidence(false);
        assert_eq!(
            Reconciler::reconcile(
                "user-1",
                "conversation-1",
                &evidence,
                &stored(false, None),
                vec![candidate(ValidatedCandidateAction::Refine {
                    target_entry_id: "entry-1".into(),
                })],
            ),
            Err(crate::MemoryError::InvalidInput),
        );
    }

    #[test]
    fn conflict_group_is_deterministic_for_repeated_identical_input() {
        let evidence = evidence(true);
        let run = || {
            let reconciled = Reconciler::reconcile(
                "user-1",
                "conversation-1",
                &evidence,
                &stored(true, Some("project-1")),
                vec![candidate(ValidatedCandidateAction::Conflict {
                    target_entry_id: "entry-1".into(),
                })],
            )
            .unwrap();
            match &reconciled[0].transition {
                CommitMemoryEntryTransition::Conflict { conflict_group_id, .. } => conflict_group_id.clone(),
                _ => panic!("expected conflict transition"),
            }
        };
        assert_eq!(run(), run());
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
        let fingerprint =
            memory_fingerprint("user-1", project_id, None, &MemoryEntryKind::Decision, "release plan").unwrap();
        vec![MemoryEntryRow {
            id: "entry-1".into(),
            revision: 0,
            user_id: "user-1".into(),
            project_id: project_id.map(str::to_owned),
            workspace_key: None,
            kind: "decision".into(),
            stable_key: "release plan".into(),
            fingerprint,
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
