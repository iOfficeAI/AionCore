//! Repository traits for Skill Evolution tables.

use crate::error::DbError;
use crate::models::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, ExperienceArticleRow, SkillEvolutionProposalRow,
    SkillEvolutionSettingsRow, UpdateSkillEvolutionProposalParams, UpsertSkillEvolutionSettingsParams,
};

#[async_trait::async_trait]
pub trait IExperienceArticleRepository: Send + Sync {
    async fn create(&self, params: &CreateExperienceArticleParams<'_>) -> Result<ExperienceArticleRow, DbError>;
    async fn get(&self, id: &str) -> Result<Option<ExperienceArticleRow>, DbError>;
    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError>;

    /// List articles visible to a user: owned OR team-visible for given team ids.
    async fn list_visible(
        &self,
        owner_user_id: &str,
        team_ids: &[String],
        assistant_id: Option<&str>,
        visibility: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError>;
}

#[async_trait::async_trait]
pub trait ISkillEvolutionProposalRepository: Send + Sync {
    async fn create(
        &self,
        params: &CreateSkillEvolutionProposalParams<'_>,
    ) -> Result<SkillEvolutionProposalRow, DbError>;
    async fn get(&self, id: &str) -> Result<Option<SkillEvolutionProposalRow>, DbError>;
    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        status: Option<&str>,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SkillEvolutionProposalRow>, DbError>;
    async fn update(
        &self,
        id: &str,
        params: &UpdateSkillEvolutionProposalParams<'_>,
    ) -> Result<Option<SkillEvolutionProposalRow>, DbError>;
}

#[async_trait::async_trait]
pub trait ISkillEvolutionSettingsRepository: Send + Sync {
    async fn get(&self, user_id: &str) -> Result<Option<SkillEvolutionSettingsRow>, DbError>;
    async fn upsert(
        &self,
        params: &UpsertSkillEvolutionSettingsParams<'_>,
    ) -> Result<SkillEvolutionSettingsRow, DbError>;
}
