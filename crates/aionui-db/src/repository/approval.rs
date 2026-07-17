use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::ApprovalRequestRow;

#[async_trait::async_trait]
pub trait IApprovalRepository: Send + Sync {
    async fn create(&self, row: &ApprovalRequestRow) -> Result<(), DbError>;
    async fn get(&self, id: &str) -> Result<Option<ApprovalRequestRow>, DbError>;
    async fn get_by_conversation_call(
        &self,
        conversation_id: &str,
        call_id: &str,
    ) -> Result<Option<ApprovalRequestRow>, DbError>;
    async fn list_for_user(
        &self,
        requester_user_id: &str,
        run_id: Option<&str>,
    ) -> Result<Vec<ApprovalRequestRow>, DbError>;
    async fn consume(&self, id: &str, approver_user_id: &str, status: &str, now: TimestampMs) -> Result<bool, DbError>;
    async fn mark_expired(&self, now: TimestampMs) -> Result<u64, DbError>;
}
