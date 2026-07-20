use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementVersion {
    pub id: String,
    pub version: i64,
    pub content: String,
    pub change_summary: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub requirement_version_id: String,
    pub ordinal: i64,
    pub statement: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRevision {
    pub id: String,
    pub revision: i64,
    pub summary: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionCoverage {
    pub criterion_id: String,
    pub statement: String,
    pub task_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementsSnapshot {
    pub run_id: String,
    pub original_requirement: String,
    pub requirement_versions: Vec<RequirementVersion>,
    pub active_criteria: Vec<AcceptanceCriterion>,
    pub plan_revisions: Vec<PlanRevision>,
    pub coverage: Vec<CriterionCoverage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleRunWorkspace {
    pub run_id: String,
    pub baseline_commit: String,
    pub initial_diff_checksum: String,
    pub initial_diff_path: String,
    pub workspace_lease_id: Option<String>,
    pub workspace_path: Option<String>,
    pub branch: Option<String>,
    pub candidate_commit: Option<String>,
    pub safe_point: String,
    pub cleanup_status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentSecretCreateRequest {
    pub name: String,
    pub value: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentSecretReference {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub status: String,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentSecretGrantRequest {
    pub secret_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub environment_key: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentModelPriceRequest {
    pub provider: String,
    pub model: String,
    pub input_per_million_microunits: i64,
    pub output_per_million_microunits: i64,
    pub cache_read_per_million_microunits: i64,
    pub cache_write_per_million_microunits: i64,
    pub source_id: String,
    pub version: String,
    pub effective_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentUsageCost {
    pub cost_microunits: Option<i64>,
    pub origin: String,
    pub price_source_id: Option<String>,
    pub price_version: Option<String>,
    pub price_effective_at: Option<i64>,
}
