//! Repository traits for Skill Evolution tables.

use crate::error::DbError;
use crate::models::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, ExperienceArticleRow,
    SkillEvolutionProposalRow, UpdateSkillEvolutionProposalParams,
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
