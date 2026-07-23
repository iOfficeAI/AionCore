use std::collections::BTreeMap;

use aionui_api_types::{
    AcceptanceCriterion, CriterionCoverage, PlanRevision, RequirementVersion, RequirementsSnapshot,
};
use aionui_db::models::{
    AcceptanceCriterionRow, CompletionEvidenceRow, PlanRevisionRow, RequirementVersionRow, TaskCriterionRow,
};

pub(crate) fn build_snapshot(
    run_id: &str,
    versions: Vec<RequirementVersionRow>,
    criteria: Vec<AcceptanceCriterionRow>,
    plans: Vec<PlanRevisionRow>,
    mappings: Vec<TaskCriterionRow>,
    evidence: Vec<CompletionEvidenceRow>,
) -> RequirementsSnapshot {
    let original_requirement = versions.first().map(|row| row.content.clone()).unwrap_or_default();
    let mut task_ids: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for mapping in &mappings {
        task_ids
            .entry(mapping.criterion_id.as_str())
            .or_default()
            .push(mapping.task_id.clone());
    }
    let mut evidence_ids: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut accepted: BTreeMap<&str, bool> = BTreeMap::new();
    for item in &evidence {
        evidence_ids
            .entry(item.criterion_id.as_str())
            .or_default()
            .push(item.id.clone());
        if item.accepted {
            accepted.insert(item.criterion_id.as_str(), true);
        }
    }
    let coverage = criteria
        .iter()
        .map(|criterion| CriterionCoverage {
            criterion_id: criterion.id.clone(),
            statement: criterion.statement.clone(),
            task_ids: task_ids.remove(criterion.id.as_str()).unwrap_or_default(),
            evidence_ids: evidence_ids.remove(criterion.id.as_str()).unwrap_or_default(),
            accepted: accepted.get(criterion.id.as_str()).copied().unwrap_or(false),
        })
        .collect();
    RequirementsSnapshot {
        run_id: run_id.into(),
        original_requirement,
        requirement_versions: versions
            .into_iter()
            .map(|row| RequirementVersion {
                id: row.id,
                version: row.version,
                content: row.content,
                change_summary: row.change_summary,
                created_at: row.created_at,
            })
            .collect(),
        active_criteria: criteria
            .into_iter()
            .map(|row| AcceptanceCriterion {
                id: row.id,
                requirement_version_id: row.requirement_version_id,
                ordinal: row.ordinal,
                statement: row.statement,
                required: row.required,
            })
            .collect(),
        plan_revisions: plans
            .into_iter()
            .map(|row| PlanRevision {
                id: row.id,
                revision: row.revision,
                summary: row.summary,
                content: row.content,
                created_at: row.created_at,
            })
            .collect(),
        coverage,
    }
}
