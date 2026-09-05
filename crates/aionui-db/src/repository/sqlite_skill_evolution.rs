//! SQLite repositories for Skill Evolution.

use sqlx::SqlitePool;

use super::skill_evolution::{
    IExperienceArticleRepository, ISkillEvolutionProposalRepository, ISkillEvolutionSettingsRepository,
};
use crate::error::DbError;
use crate::models::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, ExperienceArticleRow, SkillEvolutionProposalRow,
    SkillEvolutionSettingsRow, UpdateSkillEvolutionProposalParams, UpsertSkillEvolutionSettingsParams,
};

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct SqliteExperienceArticleRepository {
    pool: SqlitePool,
}

impl SqliteExperienceArticleRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IExperienceArticleRepository for SqliteExperienceArticleRepository {
    async fn create(&self, params: &CreateExperienceArticleParams<'_>) -> Result<ExperienceArticleRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO experience_articles (
                id, owner_user_id, assistant_id, team_id, kind, title, body_md,
                source_conversation_ids, tags, status, visibility, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.owner_user_id)
        .bind(params.assistant_id)
        .bind(params.team_id)
        .bind(params.kind)
        .bind(params.title)
        .bind(params.body_md)
        .bind(params.source_conversation_ids)
        .bind(params.tags)
        .bind(params.status)
        .bind(params.visibility)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.id)
            .await?
            .ok_or_else(|| DbError::Init("experience_articles create missing row".into()))
    }

    async fn get(&self, id: &str) -> Result<Option<ExperienceArticleRow>, DbError> {
        let row = sqlx::query_as::<_, ExperienceArticleRow>("SELECT * FROM experience_articles WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError> {
        self.list_visible(owner_user_id, &[], assistant_id, None, limit).await
    }

    async fn list_visible(
        &self,
        owner_user_id: &str,
        team_ids: &[String],
        assistant_id: Option<&str>,
        visibility: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError> {
        let limit = limit.clamp(1, 200);
        // Fetch a broader owned set then filter in Rust for team ACL (SQLite IN with dyn list is awkward).
        let owned = if let Some(aid) = assistant_id {
            sqlx::query_as::<_, ExperienceArticleRow>(
                "SELECT * FROM experience_articles
                 WHERE owner_user_id = ? AND assistant_id = ? AND status = 'active'
                 ORDER BY updated_at DESC LIMIT ?",
            )
            .bind(owner_user_id)
            .bind(aid)
            .bind(limit * 2)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, ExperienceArticleRow>(
                "SELECT * FROM experience_articles
                 WHERE owner_user_id = ? AND status = 'active'
                 ORDER BY updated_at DESC LIMIT ?",
            )
            .bind(owner_user_id)
            .bind(limit * 2)
            .fetch_all(&self.pool)
            .await?
        };

        let mut team_rows = Vec::new();
        for tid in team_ids {
            let rows = if let Some(aid) = assistant_id {
                sqlx::query_as::<_, ExperienceArticleRow>(
                    "SELECT * FROM experience_articles
                     WHERE team_id = ? AND visibility = 'team' AND status = 'active'
                       AND owner_user_id != ? AND assistant_id = ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(tid)
                .bind(owner_user_id)
                .bind(aid)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            } else {
                sqlx::query_as::<_, ExperienceArticleRow>(
                    "SELECT * FROM experience_articles
                     WHERE team_id = ? AND visibility = 'team' AND status = 'active'
                       AND owner_user_id != ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(tid)
                .bind(owner_user_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            };
            team_rows.extend(rows);
        }

        let mut merged = owned;
        merged.extend(team_rows);
        merged.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        if let Some(vis) = visibility {
            merged.retain(|r| r.visibility == vis);
        }
        merged.truncate(limit as usize);
        Ok(merged)
    }
}

pub struct SqliteSkillEvolutionProposalRepository {
    pool: SqlitePool,
}

impl SqliteSkillEvolutionProposalRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISkillEvolutionProposalRepository for SqliteSkillEvolutionProposalRepository {
    async fn create(
        &self,
        params: &CreateSkillEvolutionProposalParams<'_>,
    ) -> Result<SkillEvolutionProposalRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO skill_evolution_proposals (
                id, owner_user_id, assistant_id, conversation_id, status, title,
                experience_summary, experience_article_ids, action, target_skill_key,
                draft_skill_md, draft_diff_summary, team_id, visibility, gate_mode,
                gate_score, gate_signals, gate_recommendation, try_run_ok, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.owner_user_id)
        .bind(params.assistant_id)
        .bind(params.conversation_id)
        .bind(params.status)
        .bind(params.title)
        .bind(params.experience_summary)
        .bind(params.experience_article_ids)
        .bind(params.action)
        .bind(params.target_skill_key)
        .bind(params.draft_skill_md)
        .bind(params.draft_diff_summary)
        .bind(params.team_id)
        .bind(params.visibility)
        .bind(params.gate_mode)
        .bind(params.gate_score)
        .bind(params.gate_signals)
        .bind(params.gate_recommendation)
        .bind(params.try_run_ok)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.id)
            .await?
            .ok_or_else(|| DbError::Init("skill_evolution_proposals create missing row".into()))
    }

    async fn get(&self, id: &str) -> Result<Option<SkillEvolutionProposalRow>, DbError> {
        let row =
            sqlx::query_as::<_, SkillEvolutionProposalRow>("SELECT * FROM skill_evolution_proposals WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        status: Option<&str>,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SkillEvolutionProposalRow>, DbError> {
        let limit = limit.clamp(1, 200);
        match (status, assistant_id) {
            (Some(st), Some(aid)) => {
                let rows = sqlx::query_as::<_, SkillEvolutionProposalRow>(
                    "SELECT * FROM skill_evolution_proposals
                     WHERE owner_user_id = ? AND status = ? AND assistant_id = ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(owner_user_id)
                .bind(st)
                .bind(aid)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
                Ok(rows)
            }
            (Some(st), None) => {
                let rows = sqlx::query_as::<_, SkillEvolutionProposalRow>(
                    "SELECT * FROM skill_evolution_proposals
                     WHERE owner_user_id = ? AND status = ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(owner_user_id)
                .bind(st)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
                Ok(rows)
            }
            (None, Some(aid)) => {
                let rows = sqlx::query_as::<_, SkillEvolutionProposalRow>(
                    "SELECT * FROM skill_evolution_proposals
                     WHERE owner_user_id = ? AND assistant_id = ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(owner_user_id)
                .bind(aid)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
                Ok(rows)
            }
            (None, None) => {
                let rows = sqlx::query_as::<_, SkillEvolutionProposalRow>(
                    "SELECT * FROM skill_evolution_proposals
                     WHERE owner_user_id = ?
                     ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(owner_user_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
                Ok(rows)
            }
        }
    }

    async fn update(
        &self,
        id: &str,
        params: &UpdateSkillEvolutionProposalParams<'_>,
    ) -> Result<Option<SkillEvolutionProposalRow>, DbError> {
        let existing = match self.get(id).await? {
            Some(row) => row,
            None => return Ok(None),
        };
        let now = now_ms();
        let status = params.status.unwrap_or(existing.status.as_str());
        let title = params.title.unwrap_or(existing.title.as_str());
        let experience_summary = params
            .experience_summary
            .unwrap_or(existing.experience_summary.as_str());
        let experience_article_ids = params
            .experience_article_ids
            .unwrap_or(existing.experience_article_ids.as_str());
        let draft_skill_md = params.draft_skill_md.unwrap_or(existing.draft_skill_md.as_str());
        let draft_diff_summary = params.draft_diff_summary.or(existing.draft_diff_summary.as_deref());
        let target_skill_key = params.target_skill_key.or(existing.target_skill_key.as_deref());
        let reviewer_user_id = params.reviewer_user_id.or(existing.reviewer_user_id.as_deref());
        let review_comment = params.review_comment.or(existing.review_comment.as_deref());
        let reviewed_at = params.reviewed_at.or(existing.reviewed_at);
        let applied_skill_key = params.applied_skill_key.or(existing.applied_skill_key.as_deref());
        let applied_skill_version = params
            .applied_skill_version
            .or(existing.applied_skill_version.as_deref());
        let previous_skill_md = params.previous_skill_md.or(existing.previous_skill_md.as_deref());
        let team_id = params.team_id.or(existing.team_id.as_deref());
        let visibility = params.visibility.unwrap_or(existing.visibility.as_str());
        let gate_mode = params.gate_mode.unwrap_or(existing.gate_mode.as_str());
        let gate_score = params.gate_score.or(existing.gate_score);
        let gate_signals = params.gate_signals.unwrap_or(existing.gate_signals.as_str());
        let gate_recommendation = params.gate_recommendation.or(existing.gate_recommendation.as_deref());
        let try_run_ok = if params.clear_try_run_ok {
            None
        } else {
            params.try_run_ok.or(existing.try_run_ok)
        };

        sqlx::query(
            "UPDATE skill_evolution_proposals SET
                status = ?, title = ?, experience_summary = ?, experience_article_ids = ?,
                draft_skill_md = ?, draft_diff_summary = ?, target_skill_key = ?,
                reviewer_user_id = ?, review_comment = ?, reviewed_at = ?,
                applied_skill_key = ?, applied_skill_version = ?, previous_skill_md = ?,
                team_id = ?, visibility = ?, gate_mode = ?, gate_score = ?, gate_signals = ?,
                gate_recommendation = ?, try_run_ok = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(status)
        .bind(title)
        .bind(experience_summary)
        .bind(experience_article_ids)
        .bind(draft_skill_md)
        .bind(draft_diff_summary)
        .bind(target_skill_key)
        .bind(reviewer_user_id)
        .bind(review_comment)
        .bind(reviewed_at)
        .bind(applied_skill_key)
        .bind(applied_skill_version)
        .bind(previous_skill_md)
        .bind(team_id)
        .bind(visibility)
        .bind(gate_mode)
        .bind(gate_score)
        .bind(gate_signals)
        .bind(gate_recommendation)
        .bind(try_run_ok)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        self.get(id).await
    }
}

pub struct SqliteSkillEvolutionSettingsRepository {
    pool: SqlitePool,
}

impl SqliteSkillEvolutionSettingsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISkillEvolutionSettingsRepository for SqliteSkillEvolutionSettingsRepository {
    async fn get(&self, user_id: &str) -> Result<Option<SkillEvolutionSettingsRow>, DbError> {
        let row =
            sqlx::query_as::<_, SkillEvolutionSettingsRow>("SELECT * FROM skill_evolution_settings WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn upsert(
        &self,
        params: &UpsertSkillEvolutionSettingsParams<'_>,
    ) -> Result<SkillEvolutionSettingsRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO skill_evolution_settings (
                user_id, gate_mode, assist_threshold, auto_threshold,
                default_experience_visibility, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET
                gate_mode = excluded.gate_mode,
                assist_threshold = excluded.assist_threshold,
                auto_threshold = excluded.auto_threshold,
                default_experience_visibility = excluded.default_experience_visibility,
                updated_at = excluded.updated_at",
        )
        .bind(params.user_id)
        .bind(params.gate_mode)
        .bind(params.assist_threshold)
        .bind(params.auto_threshold)
        .bind(params.default_experience_visibility)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get(params.user_id)
            .await?
            .ok_or_else(|| DbError::Init("skill_evolution_settings upsert missing row".into()))
    }
}
