use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentTagRequest {
    pub name: String,
    pub commit_sha: String,
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default)]
    pub confirmation_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentDeploymentRequest {
    pub environment: String,
    pub deployment_key: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentConfirmationRequest {
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default)]
    pub confirmation_count: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevelopmentTimelineEvent {
    pub id: String,
    pub kind: String,
    pub correlation_id: String,
    pub task_id: Option<String>,
    pub title: String,
    pub status: String,
    pub actor_id: Option<String>,
    pub occurred_at: i64,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevelopmentRunControlState {
    pub run_id: String,
    pub run_status: String,
    pub allowed_run_actions: Vec<String>,
    pub allowed_task_actions: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevelopmentRunTimeline {
    pub run_id: String,
    pub events: Vec<DevelopmentTimelineEvent>,
    pub controls: DevelopmentRunControlState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentRunControlRequest {
    pub action: String,
    pub task_id: Option<String>,
    pub target_slot_id: Option<String>,
}
