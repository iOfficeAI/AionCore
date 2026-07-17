use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentPolicyRow, DevelopmentRecoveryRecordRow,
    DevelopmentRunRow, DevelopmentUsageEventRow, DevelopmentUsageSummary,
};

#[async_trait::async_trait]
pub trait IDevelopmentOperationsRepository: Send + Sync {
    async fn get_policy(&self, user_id: &str, project_id: &str) -> Result<Option<DevelopmentPolicyRow>, DbError>;
    async fn upsert_policy(&self, row: &DevelopmentPolicyRow) -> Result<(), DbError>;

    async fn append_usage(&self, row: &DevelopmentUsageEventRow) -> Result<(), DbError>;
    async fn summarize_usage(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
    ) -> Result<DevelopmentUsageSummary, DbError>;

    async fn append_audit(&self, row: &DevelopmentAuditEventRow) -> Result<(), DbError>;
    async fn list_audit(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DevelopmentAuditEventRow>, DbError>;

    async fn upsert_alert(&self, row: &DevelopmentAlertRow) -> Result<(), DbError>;
    async fn list_alerts(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<DevelopmentAlertRow>, DbError>;
    async fn update_alert_status(
        &self,
        user_id: &str,
        alert_id: &str,
        status: &str,
        resolved_at: Option<TimestampMs>,
    ) -> Result<bool, DbError>;

    async fn append_recovery(&self, row: &DevelopmentRecoveryRecordRow) -> Result<(), DbError>;
    async fn list_recovery(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DevelopmentRecoveryRecordRow>, DbError>;
    async fn list_recovery_candidates(&self, updated_before: TimestampMs) -> Result<Vec<DevelopmentRunRow>, DbError>;
}
