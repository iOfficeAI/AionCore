use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::CronJobRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronRunClaimResult {
    Claimed,
    Duplicate,
    QueueBusy,
}

#[derive(Debug, Clone)]
pub struct ClaimCronRunParams<'a> {
    pub job_id: &'a str,
    pub scheduled_at: TimestampMs,
    pub owner_id: &'a str,
    pub now: TimestampMs,
    pub lease_until: TimestampMs,
    pub queue_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct FinishCronRunParams<'a> {
    pub job_id: &'a str,
    pub scheduled_at: TimestampMs,
    pub owner_id: &'a str,
    pub status: &'a str,
    pub conversation_id: Option<&'a str>,
    pub error: Option<&'a str>,
    pub finished_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableCronRun {
    pub scheduled_at: TimestampMs,
    pub wake_at: TimestampMs,
}

/// Parameters for updating a cron job.
///
/// All fields are optional; `None` means "keep the current value".
#[derive(Debug, Clone, Default)]
pub struct UpdateCronJobParams {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub schedule_kind: Option<String>,
    pub schedule_value: Option<String>,
    pub schedule_tz: Option<Option<String>>,
    pub schedule_description: Option<Option<String>>,
    pub payload_message: Option<String>,
    pub execution_mode: Option<String>,
    pub agent_config: Option<Option<String>>,
    pub conversation_id: Option<String>,
    pub conversation_title: Option<Option<String>>,
    pub skill_content: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub next_run_at: Option<Option<TimestampMs>>,
    pub last_run_at: Option<Option<TimestampMs>>,
    pub last_status: Option<Option<String>>,
    pub last_error: Option<Option<String>>,
    pub run_count: Option<i64>,
    pub retry_count: Option<i64>,
    pub queue_enabled: Option<bool>,
}

/// Data access abstraction for the `cron_jobs` table.
#[async_trait::async_trait]
pub trait ICronRepository: Send + Sync {
    /// Inserts a new cron job row.
    async fn insert(&self, row: &CronJobRow) -> Result<(), DbError>;

    /// Updates a cron job whose conversation is owned by `user_id`.
    async fn update_for_user(&self, user_id: &str, id: &str, params: &UpdateCronJobParams) -> Result<(), DbError>;

    /// Deletes a cron job whose conversation is owned by `user_id`.
    async fn delete_for_user(&self, user_id: &str, id: &str) -> Result<(), DbError>;

    /// System-only lookup path for scheduler/recovery code. User-facing code
    /// must use `get_by_id_for_user`.
    async fn get_by_id_system(&self, id: &str) -> Result<Option<CronJobRow>, DbError>;

    /// Returns a single cron job whose conversation is owned by `user_id`.
    async fn get_by_id_for_user(&self, user_id: &str, id: &str) -> Result<Option<CronJobRow>, DbError>;

    /// System-only full scan. User-facing code must use `list_all_for_user`.
    async fn list_all_system(&self) -> Result<Vec<CronJobRow>, DbError>;

    /// Returns all cron jobs whose conversations are owned by `user_id`.
    async fn list_all_for_user(&self, user_id: &str) -> Result<Vec<CronJobRow>, DbError>;

    /// System-only enabled-job scan used by the scheduler.
    async fn list_enabled_system(&self) -> Result<Vec<CronJobRow>, DbError>;

    /// System-only conversation lookup. User-facing code must use
    /// `list_by_conversation_for_user`.
    async fn list_by_conversation_system(&self, conversation_id: &str) -> Result<Vec<CronJobRow>, DbError>;

    /// Returns all cron jobs for a given conversation owned by `user_id`.
    async fn list_by_conversation_for_user(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<CronJobRow>, DbError>;

    /// Atomically claims one scheduled occurrence across all backend processes.
    async fn claim_run(&self, params: &ClaimCronRunParams<'_>) -> Result<CronRunClaimResult, DbError>;

    /// Extends an active run lease owned by this backend instance.
    async fn renew_run_lease(
        &self,
        job_id: &str,
        scheduled_at: TimestampMs,
        owner_id: &str,
        lease_until: TimestampMs,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;

    /// Releases a claimed occurrence until its scheduled retry time.
    async fn defer_run(
        &self,
        job_id: &str,
        scheduled_at: TimestampMs,
        owner_id: &str,
        retry_at: TimestampMs,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;

    /// Completes a claimed occurrence and releases its lease.
    async fn finish_run(&self, params: &FinishCronRunParams<'_>) -> Result<bool, DbError>;

    /// Deletes terminal run records older than the retention cutoff.
    async fn cleanup_runs_before(&self, cutoff: TimestampMs) -> Result<u64, DbError>;

    /// Returns the oldest unfinished occurrence that should be resumed for a job.
    async fn get_recoverable_run(&self, job_id: &str, now: TimestampMs) -> Result<Option<RecoverableCronRun>, DbError>;
}
