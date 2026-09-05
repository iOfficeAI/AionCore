//! HTTP contract for `/api/skill-evolution/*` (CSBU WorkMate 技能进化).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionStatus {
    Draft,
    PendingReview,
    Approved,
    Rejected,
    Applied,
    RolledBack,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionAction {
    Create,
    Patch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionGateMode {
    HumanOnly,
    HeuristicAssist,
    AutoApplyOnPass,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvolutionGateRecommendation {
    Approve,
    Reject,
    NeedsReview,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceVisibility {
    Private,
    Team,
    OwnerEditors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvolutionGateSignal {
    pub id: String,
    pub passed: bool,
    pub weight: u32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvolutionProposalResponse {
    pub id: String,
    pub assistant_id: Option<String>,
    pub conversation_id: Option<String>,
    pub status: SkillEvolutionStatus,
    pub title: String,
    pub experience_summary: String,
    pub experience_article_ids: Vec<String>,
    pub action: SkillEvolutionAction,
    pub target_skill_key: Option<String>,
    pub draft_skill_md: String,
    pub draft_diff_summary: Option<String>,
    pub reviewer_user_id: Option<String>,
    pub review_comment: Option<String>,
    pub reviewed_at: Option<i64>,
    pub applied_skill_key: Option<String>,
    pub applied_skill_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub gate_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_score: Option<u32>,
    #[serde(default)]
    pub gate_signals: Vec<SkillEvolutionGateSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub try_run_ok: Option<bool>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateSkillEvolutionProposalRequest {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub experience_summary: Option<String>,
    #[serde(default)]
    pub action: Option<SkillEvolutionAction>,
    #[serde(default)]
    pub target_skill_key: Option<String>,
    #[serde(default)]
    pub draft_skill_md: Option<String>,
    #[serde(default)]
    pub draft_diff_summary: Option<String>,
    /// When true and draft_skill_md is empty, generate a SKILL.md stub template.
    #[serde(default = "default_true")]
    pub auto_stub: bool,
    /// When true, create as pending_review instead of draft.
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub try_run_ok: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SkillEvolutionListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReviewSkillEvolutionRequest {
    #[serde(default)]
    pub comment: Option<String>,
    /// On approve: skill key to apply/export (overrides target_skill_key).
    #[serde(default)]
    pub applied_skill_key: Option<String>,
    #[serde(default)]
    pub applied_skill_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvolutionExportPayload {
    pub skill_key: String,
    pub skill_md: String,
    pub suggested_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveSkillEvolutionResponse {
    pub proposal: SkillEvolutionProposalResponse,
    pub export: SkillEvolutionExportPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceArticleResponse {
    pub id: String,
    pub assistant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub body_md: String,
    pub source_conversation_ids: Vec<String>,
    pub tags: Vec<String>,
    pub status: String,
    #[serde(default)]
    pub visibility: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateExperienceArticleRequest {
    pub title: String,
    #[serde(default)]
    pub body_md: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub source_conversation_ids: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ExperienceListQuery {
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Request body for evolve endpoints (Maintainer + Proposer LLM passes).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EvolveSkillEvolutionRequest {
    #[serde(default)]
    pub assistant_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub target_skill_key: Option<String>,
    #[serde(default)]
    pub action: Option<SkillEvolutionAction>,
    /// When true, create/update as pending_review; otherwise draft for editing.
    #[serde(default)]
    pub submit: bool,
    /// Optional model id override (must exist on an enabled provider).
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    /// Optional override of user settings gate_mode for this evolve.
    #[serde(default)]
    pub gate_mode: Option<String>,
    #[serde(default)]
    pub try_run_ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillEvolutionTrajectoryOverview {
    pub turns: u64,
    pub steps: u64,
    pub tools: u64,
    pub errors: u64,
    pub record_count: usize,
    pub digest_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolveSkillEvolutionResponse {
    pub proposal: SkillEvolutionProposalResponse,
    pub experience_articles: Vec<ExperienceArticleResponse>,
    pub trajectory_overview: SkillEvolutionTrajectoryOverview,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_note: Option<String>,
}

/// Request body for apply — write Skills Hub / workspace + optional pin.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplySkillEvolutionRequest {
    #[serde(default = "default_true")]
    pub write_to_skills_hub: bool,
    #[serde(default = "default_true")]
    pub pin_on_assistant: bool,
    /// Override workspace root for `.csbu-workmate/skills/<key>/SKILL.md`.
    #[serde(default)]
    pub workspace_root: Option<String>,
}

impl Default for ApplySkillEvolutionRequest {
    fn default() -> Self {
        Self {
            write_to_skills_hub: true,
            pin_on_assistant: true,
            workspace_root: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvolutionSkillRefPayload {
    pub skill_key: String,
    pub version_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplySkillEvolutionResponse {
    pub proposal: SkillEvolutionProposalResponse,
    pub export: SkillEvolutionExportPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_hub_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_ref: Option<SkillEvolutionSkillRefPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvolutionSettingsResponse {
    pub gate_mode: String,
    pub assist_threshold: u32,
    pub auto_threshold: u32,
    pub default_experience_visibility: String,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UpdateSkillEvolutionSettingsRequest {
    #[serde(default)]
    pub gate_mode: Option<String>,
    #[serde(default)]
    pub assist_threshold: Option<u32>,
    #[serde(default)]
    pub auto_threshold: Option<u32>,
    #[serde(default)]
    pub default_experience_visibility: Option<String>,
}

/// Light cross-model transfer note (Phase 3 optional; not a full experiment bench).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossModelTransferNoteResponse {
    pub title: String,
    pub body_md: String,
}
