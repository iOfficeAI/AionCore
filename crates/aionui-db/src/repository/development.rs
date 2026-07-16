use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{
    DevelopmentRunRoleRow, DevelopmentRunRow, DevelopmentTaskRow, QualityGateRunRow, ReviewFindingRow, TaskArtifactRow,
};

#[async_trait::async_trait]
pub trait IDevelopmentRepository: Send + Sync {
    async fn create_run(&self, row: &DevelopmentRunRow) -> Result<(), DbError>;
    async fn get_run(&self, run_id: &str, user_id: &str) -> Result<Option<DevelopmentRunRow>, DbError>;
    async fn list_runs(&self, user_id: &str, project_id: Option<&str>) -> Result<Vec<DevelopmentRunRow>, DbError>;
    async fn update_run_status(
        &self,
        run_id: &str,
        user_id: &str,
        status: &str,
        finished_at: Option<TimestampMs>,
    ) -> Result<bool, DbError>;

    async fn assign_role(&self, row: &DevelopmentRunRoleRow) -> Result<(), DbError>;
    async fn list_roles(&self, run_id: &str) -> Result<Vec<DevelopmentRunRoleRow>, DbError>;

    async fn create_task(&self, row: &DevelopmentTaskRow) -> Result<(), DbError>;
    async fn get_task(&self, run_id: &str, task_id: &str) -> Result<Option<DevelopmentTaskRow>, DbError>;
    async fn list_tasks(&self, run_id: &str) -> Result<Vec<DevelopmentTaskRow>, DbError>;
    async fn update_task_state(
        &self,
        run_id: &str,
        task_id: &str,
        status: &str,
        review_status: &str,
        verification_status: &str,
    ) -> Result<bool, DbError>;

    async fn create_artifact(&self, row: &TaskArtifactRow) -> Result<(), DbError>;
    async fn list_artifacts(&self, run_id: &str, task_id: Option<&str>) -> Result<Vec<TaskArtifactRow>, DbError>;

    async fn create_gate(&self, row: &QualityGateRunRow) -> Result<(), DbError>;
    async fn update_gate(&self, row: &QualityGateRunRow) -> Result<bool, DbError>;
    async fn list_gates(&self, run_id: &str, task_id: Option<&str>) -> Result<Vec<QualityGateRunRow>, DbError>;

    async fn create_finding(&self, row: &ReviewFindingRow) -> Result<(), DbError>;
    async fn list_findings(&self, run_id: &str, task_id: &str) -> Result<Vec<ReviewFindingRow>, DbError>;
    async fn update_finding_status(&self, run_id: &str, finding_id: &str, status: &str) -> Result<bool, DbError>;
}
