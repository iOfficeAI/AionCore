use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentModelPriceRow, DevelopmentPolicyRow,
    DevelopmentPricedUsageEventRow, DevelopmentRecoveryRecordRow, DevelopmentRunRow, DevelopmentSecretGrantRow,
    DevelopmentSecretRow, DevelopmentUsageDimensionSummary, DevelopmentUsageEventRow, DevelopmentUsageSummary,
    ExecutionResourceLeaseRow, UsageDimension,
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

    async fn upsert_resource_lease(&self, row: &ExecutionResourceLeaseRow) -> Result<(), DbError>;
    async fn get_resource_lease(&self, lease_id: &str) -> Result<Option<ExecutionResourceLeaseRow>, DbError>;
    async fn list_resource_leases(
        &self,
        user_id: &str,
        run_id: &str,
        active_only: bool,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DbError>;
    async fn list_stale_resource_leases(&self, now: TimestampMs) -> Result<Vec<ExecutionResourceLeaseRow>, DbError>;
    async fn bind_execution_environment(
        &self,
        entity_type: &str,
        entity_id: &str,
        environment_id: &str,
        environment_kind: &str,
        bound_at: TimestampMs,
    ) -> Result<(), DbError>;

    async fn insert_secret(&self, row: &DevelopmentSecretRow) -> Result<(), DbError>;
    async fn get_secret(&self, user_id: &str, secret_id: &str) -> Result<Option<DevelopmentSecretRow>, DbError>;
    async fn list_secrets(&self, user_id: &str, project_id: &str) -> Result<Vec<DevelopmentSecretRow>, DbError>;
    async fn revoke_secret(&self, user_id: &str, secret_id: &str, revoked_at: TimestampMs) -> Result<bool, DbError>;
    async fn upsert_secret_grant(&self, row: &DevelopmentSecretGrantRow) -> Result<(), DbError>;
    async fn list_secret_grants(
        &self,
        user_id: &str,
        secret_id: &str,
    ) -> Result<Vec<DevelopmentSecretGrantRow>, DbError>;

    async fn upsert_model_price(&self, row: &DevelopmentModelPriceRow) -> Result<(), DbError>;
    async fn resolve_model_price(
        &self,
        provider: &str,
        model: &str,
        occurred_at: TimestampMs,
    ) -> Result<Option<DevelopmentModelPriceRow>, DbError>;
    async fn append_priced_usage(&self, row: &DevelopmentPricedUsageEventRow) -> Result<(), DbError>;
    async fn summarize_usage_dimension(
        &self,
        user_id: &str,
        dimension: &UsageDimension,
    ) -> Result<DevelopmentUsageDimensionSummary, DbError>;
}
