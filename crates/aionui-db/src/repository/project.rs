use crate::error::DbError;
use crate::models::{
    ProjectCommandProfileRow, ProjectRepositoryFactsRow, ProjectResourceLinkRow, ProjectRow, ProjectRuntimeProfileRow,
};

#[derive(Debug, Clone, Default)]
pub struct UpdateProjectParams {
    pub name: Option<String>,
    pub local_path: Option<String>,
    pub repository_url: Option<Option<String>>,
    pub default_branch: Option<Option<String>>,
    pub project_type: Option<String>,
}

#[async_trait::async_trait]
pub trait IProjectRepository: Send + Sync {
    async fn create(&self, row: &ProjectRow) -> Result<(), DbError>;
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<ProjectRow>, DbError>;
    async fn get_for_user(&self, project_id: &str, user_id: &str) -> Result<Option<ProjectRow>, DbError>;
    async fn update_for_user(
        &self,
        project_id: &str,
        user_id: &str,
        params: &UpdateProjectParams,
    ) -> Result<ProjectRow, DbError>;
    async fn delete_for_user(&self, project_id: &str, user_id: &str) -> Result<bool, DbError>;

    async fn upsert_command_profile(&self, row: &ProjectCommandProfileRow) -> Result<(), DbError>;
    async fn get_command_profile(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectCommandProfileRow>, DbError>;
    async fn upsert_runtime_profile(&self, row: &ProjectRuntimeProfileRow) -> Result<(), DbError>;
    async fn get_runtime_profile(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectRuntimeProfileRow>, DbError>;
    async fn upsert_repository_facts(&self, row: &ProjectRepositoryFactsRow) -> Result<(), DbError>;
    async fn get_repository_facts(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Option<ProjectRepositoryFactsRow>, DbError>;

    async fn bind_resource(
        &self,
        project_id: &str,
        user_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ProjectResourceLinkRow, DbError>;
    async fn resource_is_owned(&self, user_id: &str, resource_type: &str, resource_id: &str) -> Result<bool, DbError>;
    async fn get_for_resource(
        &self,
        user_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<Option<ProjectRow>, DbError>;
    async fn list_resource_links(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> Result<Vec<ProjectResourceLinkRow>, DbError>;
}
