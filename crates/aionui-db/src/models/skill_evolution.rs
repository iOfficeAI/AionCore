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
}

#[derive(Debug, Clone)]
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
}

impl Default for UpdateSkillEvolutionProposalParams<'static> {
    fn default() -> Self {
        Self {
            status: None,
            title: None,
            experience_summary: None,
            experience_article_ids: None,
            draft_skill_md: None,
            draft_diff_summary: None,
            target_skill_key: None,
            reviewer_user_id: None,
            review_comment: None,
            reviewed_at: None,
            applied_skill_key: None,
            applied_skill_version: None,
            previous_skill_md: None,
        }
    }
}
