//! Row models for Skill Evolution (经验库 / 技能提案).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ExperienceArticleRow {
    pub id: String,
    pub owner_user_id: String,
    pub assistant_id: Option<String>,
    pub team_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub body_md: String,
    pub source_conversation_ids: String,
    pub tags: String,
    pub status: String,
    /// private | team | owner_editors (Phase 3 ACL)
    pub visibility: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct CreateExperienceArticleParams<'a> {
    pub id: &'a str,
    pub owner_user_id: &'a str,
    pub assistant_id: Option<&'a str>,
    pub team_id: Option<&'a str>,
    pub kind: &'a str,
    pub title: &'a str,
    pub body_md: &'a str,
    pub source_conversation_ids: &'a str,
    pub tags: &'a str,
    pub status: &'a str,
    pub visibility: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillEvolutionProposalRow {
    pub id: String,
    pub owner_user_id: String,
    pub assistant_id: Option<String>,
    pub conversation_id: Option<String>,
    pub status: String,
    pub title: String,
    pub experience_summary: String,
    pub experience_article_ids: String,
    pub action: String,
    pub target_skill_key: Option<String>,
    pub draft_skill_md: String,
    pub draft_diff_summary: Option<String>,
    pub reviewer_user_id: Option<String>,
    pub review_comment: Option<String>,
    pub reviewed_at: Option<TimestampMs>,
    pub applied_skill_key: Option<String>,
    pub applied_skill_version: Option<String>,
    pub previous_skill_md: Option<String>,
    pub team_id: Option<String>,
    pub visibility: String,
    pub gate_mode: String,
    pub gate_score: Option<i64>,
    pub gate_signals: String,
    pub gate_recommendation: Option<String>,
    pub try_run_ok: Option<i64>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct CreateSkillEvolutionProposalParams<'a> {
    pub id: &'a str,
    pub owner_user_id: &'a str,
    pub assistant_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub status: &'a str,
    pub title: &'a str,
    pub experience_summary: &'a str,
    pub experience_article_ids: &'a str,
    pub action: &'a str,
    pub target_skill_key: Option<&'a str>,
    pub draft_skill_md: &'a str,
    pub draft_diff_summary: Option<&'a str>,
    pub team_id: Option<&'a str>,
    pub visibility: &'a str,
    pub gate_mode: &'a str,
    pub gate_score: Option<i64>,
    pub gate_signals: &'a str,
    pub gate_recommendation: Option<&'a str>,
    pub try_run_ok: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateSkillEvolutionProposalParams<'a> {
    pub status: Option<&'a str>,
    pub title: Option<&'a str>,
    pub experience_summary: Option<&'a str>,
    pub experience_article_ids: Option<&'a str>,
    pub draft_skill_md: Option<&'a str>,
    pub draft_diff_summary: Option<&'a str>,
    pub target_skill_key: Option<&'a str>,
    pub reviewer_user_id: Option<&'a str>,
    pub review_comment: Option<&'a str>,
    pub reviewed_at: Option<i64>,
    pub applied_skill_key: Option<&'a str>,
    pub applied_skill_version: Option<&'a str>,
    pub previous_skill_md: Option<&'a str>,
    pub team_id: Option<&'a str>,
    pub visibility: Option<&'a str>,
    pub gate_mode: Option<&'a str>,
    pub gate_score: Option<i64>,
    pub gate_signals: Option<&'a str>,
    pub gate_recommendation: Option<&'a str>,
    pub try_run_ok: Option<i64>,
    /// When true, clear try_run_ok to NULL (distinct from Some/None merge).
    pub clear_try_run_ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillEvolutionSettingsRow {
    pub user_id: String,
    pub gate_mode: String,
    pub assist_threshold: i64,
    pub auto_threshold: i64,
    pub default_experience_visibility: String,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct UpsertSkillEvolutionSettingsParams<'a> {
    pub user_id: &'a str,
    pub gate_mode: &'a str,
    pub assist_threshold: i64,
    pub auto_threshold: i64,
    pub default_experience_visibility: &'a str,
}
