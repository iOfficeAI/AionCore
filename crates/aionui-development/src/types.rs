use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDevelopmentRunInput {
    pub project_id: String,
    pub team_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_user_id: Option<String>,
    pub execution_mode: String,
    pub request_summary: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDevelopmentTaskInput {
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default = "default_task_type")]
    pub task_type: String,
    #[serde(default = "default_risk_level")]
    pub risk_level: String,
    pub assigned_workspace_lease_id: Option<String>,
}

fn default_task_type() -> String {
    "implementation".into()
}

fn default_risk_level() -> String {
    "medium".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteQualityGateInput {
    pub task_id: Option<String>,
    pub gate_type: String,
    pub workspace_lease_id: Option<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateArtifactInput {
    pub task_id: Option<String>,
    pub artifact_type: String,
    pub path_or_uri: String,
    pub checksum: String,
    pub producer_agent_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitReviewInput {
    pub task_id: String,
    pub reviewer_agent_id: String,
    pub producer_agent_id: Option<String>,
    #[serde(default)]
    pub findings: Vec<ReviewFindingInput>,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFindingInput {
    pub severity: String,
    pub file_path: Option<String>,
    pub line_number: Option<i64>,
    pub reason: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveFindingInput {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignDevelopmentRoleInput {
    pub slot_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDevelopmentTaskInput {
    pub status: String,
}
