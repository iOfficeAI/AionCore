use aionui_common::{TimestampMs, generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::CronJobRow;
use crate::repository::cron::{
    ClaimCronRunParams, CronRunClaimResult, FinishCronRunParams, ICronRepository, RecoverableCronRun,
    UpdateCronJobParams,
};

#[derive(Clone, Debug)]
pub struct SqliteCronRepository {
    pool: SqlitePool,
}

impl SqliteCronRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ICronRepository for SqliteCronRepository {
    async fn insert(&self, row: &CronJobRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO cron_jobs (\
                id, name, enabled, schedule_kind, schedule_value, schedule_tz, \
                schedule_description, payload_message, execution_mode, agent_config, \
                conversation_id, conversation_title, created_by, \
                skill_content, description, created_at, updated_at, next_run_at, last_run_at, \
                last_status, last_error, run_count, retry_count, max_retries, queue_enabled\
            ) VALUES (\
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?\
            )",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(row.enabled)
        .bind(&row.schedule_kind)
        .bind(&row.schedule_value)
        .bind(&row.schedule_tz)
        .bind(&row.schedule_description)
        .bind(&row.payload_message)
        .bind(&row.execution_mode)
        .bind(&row.agent_config)
        .bind(&row.conversation_id)
        .bind(&row.conversation_title)
        .bind(&row.created_by)
        .bind(&row.skill_content)
        .bind(&row.description)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.next_run_at)
        .bind(row.last_run_at)
        .bind(&row.last_status)
        .bind(&row.last_error)
        .bind(row.run_count)
        .bind(row.retry_count)
        .bind(row.max_retries)
        .bind(row.queue_enabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update(&self, id: &str, params: &UpdateCronJobParams) -> Result<(), DbError> {
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        macro_rules! push_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::Str(v.clone()));
                }
            };
        }

        macro_rules! push_opt_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptStr(v.clone()));
                }
            };
        }

        macro_rules! push_opt_i64 {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptI64(*v));
                }
            };
        }

        macro_rules! push_i64 {
            ($field:ident) => {
                if let Some(v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::I64(v));
                }
            };
        }

        if let Some(v) = params.enabled {
            set_parts.push("enabled = ?".to_string());
            binds.push(BindValue::Bool(v));
        }
        if let Some(v) = params.queue_enabled {
            set_parts.push("queue_enabled = ?".to_string());
            binds.push(BindValue::Bool(v));
        }

        push_str!(name);
        push_str!(schedule_kind);
        push_str!(schedule_value);
        push_opt_str!(schedule_tz);
        push_opt_str!(schedule_description);
        push_str!(payload_message);
        push_str!(execution_mode);
        push_opt_str!(agent_config);
        push_str!(conversation_id);
        push_opt_str!(conversation_title);
        push_opt_str!(skill_content);
        push_opt_str!(description);
        push_opt_i64!(next_run_at);
        push_opt_i64!(last_run_at);
        push_opt_str!(last_status);
        push_opt_str!(last_error);
        push_i64!(run_count);
        push_i64!(retry_count);

        if set_parts.is_empty() {
            return Ok(());
        }

        set_parts.push("updated_at = ?".to_string());
        binds.push(BindValue::I64(now_ms()));

        let sql = format!("UPDATE cron_jobs SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("cron job '{id}'")));
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM cron_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("cron job '{id}'")));
        }
        Ok(())
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<CronJobRow>, DbError> {
        let row = sqlx::query_as::<_, CronJobRow>("SELECT * FROM cron_jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_all(&self) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>("SELECT * FROM cron_jobs ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn list_enabled(&self) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>("SELECT * FROM cron_jobs WHERE enabled = 1 ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn list_by_conversation(&self, conversation_id: &str) -> Result<Vec<CronJobRow>, DbError> {
        let rows = sqlx::query_as::<_, CronJobRow>(
            "SELECT * FROM cron_jobs WHERE conversation_id = ? ORDER BY created_at ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete_by_conversation(&self, conversation_id: &str) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM cron_jobs WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn claim_run(&self, params: &ClaimCronRunParams<'_>) -> Result<CronRunClaimResult, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result = async {
            let existing = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
                "SELECT status, owner_id, lease_until FROM cron_job_runs WHERE job_id = ? AND scheduled_at = ?",
            )
            .bind(params.job_id)
            .bind(params.scheduled_at)
            .fetch_optional(&mut *connection)
            .await?;

            if let Some((status, _owner_id, lease_until)) = existing {
                if matches!(status.as_str(), "running" | "retrying")
                    && lease_until.is_some_and(|lease| lease <= params.now)
                {
                    let updated = sqlx::query(
                        "UPDATE cron_job_runs SET status = 'running', owner_id = ?, lease_until = ?, \
                         started_at = COALESCE(started_at, ?), finished_at = NULL, error = NULL, updated_at = ? \
                         WHERE job_id = ? AND scheduled_at = ? AND status IN ('running', 'retrying') AND lease_until <= ?",
                    )
                    .bind(params.owner_id)
                    .bind(params.lease_until)
                    .bind(params.now)
                    .bind(params.now)
                    .bind(params.job_id)
                    .bind(params.scheduled_at)
                    .bind(params.now)
                    .execute(&mut *connection)
                    .await?;
                    return Ok(if updated.rows_affected() == 1 {
                        CronRunClaimResult::Claimed
                    } else {
                        CronRunClaimResult::Duplicate
                    });
                }
                return Ok(CronRunClaimResult::Duplicate);
            }

            if params.queue_enabled {
                let has_active_run: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM cron_job_runs \
                     WHERE job_id = ? AND status IN ('running', 'retrying') AND lease_until > ?)",
                )
                .bind(params.job_id)
                .bind(params.now)
                .fetch_one(&mut *connection)
                .await?;

                if has_active_run {
                    sqlx::query(
                        "INSERT INTO cron_job_runs (id, job_id, scheduled_at, status, created_at, updated_at, finished_at) \
                         VALUES (?, ?, ?, 'skipped', ?, ?, ?)",
                    )
                    .bind(generate_prefixed_id("cron_run"))
                    .bind(params.job_id)
                    .bind(params.scheduled_at)
                    .bind(params.now)
                    .bind(params.now)
                    .bind(params.now)
                    .execute(&mut *connection)
                    .await?;
                    return Ok(CronRunClaimResult::QueueBusy);
                }
            }

            sqlx::query(
                "INSERT INTO cron_job_runs (id, job_id, scheduled_at, status, owner_id, lease_until, started_at, created_at, updated_at) \
                 VALUES (?, ?, ?, 'running', ?, ?, ?, ?, ?)",
            )
            .bind(generate_prefixed_id("cron_run"))
            .bind(params.job_id)
            .bind(params.scheduled_at)
            .bind(params.owner_id)
            .bind(params.lease_until)
            .bind(params.now)
            .bind(params.now)
            .bind(params.now)
            .execute(&mut *connection)
            .await?;

            Ok(CronRunClaimResult::Claimed)
        }
        .await;

        match result {
            Ok(result) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(result)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn renew_run_lease(
        &self,
        job_id: &str,
        scheduled_at: TimestampMs,
        owner_id: &str,
        lease_until: TimestampMs,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE cron_job_runs SET lease_until = ?, updated_at = ? \
             WHERE job_id = ? AND scheduled_at = ? AND status = 'running' AND owner_id = ?",
        )
        .bind(lease_until)
        .bind(updated_at)
        .bind(job_id)
        .bind(scheduled_at)
        .bind(owner_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn defer_run(
        &self,
        job_id: &str,
        scheduled_at: TimestampMs,
        owner_id: &str,
        retry_at: TimestampMs,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE cron_job_runs SET status = 'retrying', owner_id = NULL, lease_until = ?, updated_at = ? \
             WHERE job_id = ? AND scheduled_at = ? AND status = 'running' AND owner_id = ?",
        )
        .bind(retry_at)
        .bind(updated_at)
        .bind(job_id)
        .bind(scheduled_at)
        .bind(owner_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn finish_run(&self, params: &FinishCronRunParams<'_>) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE cron_job_runs SET status = ?, conversation_id = ?, error = ?, lease_until = NULL, \
             finished_at = ?, updated_at = ? \
             WHERE job_id = ? AND scheduled_at = ? AND status = 'running' AND owner_id = ?",
        )
        .bind(params.status)
        .bind(params.conversation_id)
        .bind(params.error)
        .bind(params.finished_at)
        .bind(params.finished_at)
        .bind(params.job_id)
        .bind(params.scheduled_at)
        .bind(params.owner_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn cleanup_runs_before(&self, cutoff: TimestampMs) -> Result<u64, DbError> {
        let result =
            sqlx::query("DELETE FROM cron_job_runs WHERE status IN ('ok', 'error', 'skipped') AND finished_at < ?")
                .bind(cutoff)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }

    async fn get_recoverable_run(&self, job_id: &str, now: TimestampMs) -> Result<Option<RecoverableCronRun>, DbError> {
        let row = sqlx::query_as::<_, (TimestampMs, TimestampMs)>(
            "SELECT scheduled_at, MAX(lease_until, ?) AS wake_at FROM cron_job_runs \
             WHERE job_id = ? AND status IN ('running', 'retrying') AND lease_until IS NOT NULL \
             ORDER BY lease_until ASC LIMIT 1",
        )
        .bind(now)
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(scheduled_at, wake_at)| RecoverableCronRun { scheduled_at, wake_at }))
    }
}

// ── Dynamic bind helpers ────────────────────────────────────────────

#[derive(Debug, Clone)]
enum BindValue {
    Str(String),
    OptStr(Option<String>),
    Bool(bool),
    I64(i64),
    OptI64(Option<i64>),
}

fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    val: &'q BindValue,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match val {
        BindValue::Str(s) => query.bind(s.as_str()),
        BindValue::OptStr(s) => query.bind(s.as_deref()),
        BindValue::Bool(b) => query.bind(*b),
        BindValue::I64(n) => query.bind(*n),
        BindValue::OptI64(n) => query.bind(*n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteCronRepository, crate::Database) {
        let db = init_database_memory().await.expect("init db");
        let repo = SqliteCronRepository::new(db.pool().clone());

        // Insert a user + conversation so FK-like constraints hold logically
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user_1', 'tester', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conv_1', 'user_1', 'Test Conv', 'normal', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        (repo, db)
    }

    fn make_row(id: &str) -> CronJobRow {
        let now = now_ms();
        CronJobRow {
            id: id.into(),
            name: "Test Job".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("Every minute".into()),
            payload_message: "ping".into(),
            execution_mode: "existing".into(),
            agent_config: None,
            conversation_id: "conv_1".into(),
            conversation_title: Some("Test Conv".into()),
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now,
            updated_at: now,
            next_run_at: Some(now + 60_000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        }
    }

    #[tokio::test]
    async fn insert_and_get_by_id() {
        let (repo, _db) = setup().await;
        let row = make_row("cron_1");
        repo.insert(&row).await.unwrap();

        let found = repo.get_by_id("cron_1").await.unwrap().expect("found");
        assert_eq!(found.id, "cron_1");
        assert_eq!(found.name, "Test Job");
        assert!(found.enabled);
        assert_eq!(found.schedule_kind, "every");
        assert_eq!(found.run_count, 0);
    }

    #[tokio::test]
    async fn get_by_id_returns_none_for_missing() {
        let (repo, _db) = setup().await;
        let result = repo.get_by_id("cron_missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_all_returns_all_rows() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_a")).await.unwrap();
        repo.insert(&make_row("cron_b")).await.unwrap();

        let all = repo.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_e1")).await.unwrap();

        let mut disabled = make_row("cron_e2");
        disabled.enabled = false;
        repo.insert(&disabled).await.unwrap();

        let enabled = repo.list_enabled().await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "cron_e1");
    }

    #[tokio::test]
    async fn list_by_conversation_filters_correctly() {
        let (repo, db) = setup().await;
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conv_2', 'user_1', 'Other', 'normal', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        repo.insert(&make_row("cron_c1")).await.unwrap();
        let mut other = make_row("cron_c2");
        other.conversation_id = "conv_2".into();
        repo.insert(&other).await.unwrap();

        let conv1_jobs = repo.list_by_conversation("conv_1").await.unwrap();
        assert_eq!(conv1_jobs.len(), 1);
        assert_eq!(conv1_jobs[0].id, "cron_c1");

        let conv2_jobs = repo.list_by_conversation("conv_2").await.unwrap();
        assert_eq!(conv2_jobs.len(), 1);
        assert_eq!(conv2_jobs[0].id, "cron_c2");
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_u1")).await.unwrap();

        let params = UpdateCronJobParams {
            name: Some("Renamed".into()),
            enabled: Some(false),
            run_count: Some(42),
            ..Default::default()
        };
        repo.update("cron_u1", &params).await.unwrap();

        let updated = repo.get_by_id("cron_u1").await.unwrap().unwrap();
        assert_eq!(updated.name, "Renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.run_count, 42);
        assert!(updated.updated_at >= updated.created_at);
    }

    #[tokio::test]
    async fn update_queue_enabled() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_queue")).await.unwrap();

        repo.update(
            "cron_queue",
            &UpdateCronJobParams {
                queue_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(repo.get_by_id("cron_queue").await.unwrap().unwrap().queue_enabled);
    }

    #[tokio::test]
    async fn claim_run_deduplicates_same_occurrence_across_repository_instances() {
        let (repo, db) = setup().await;
        repo.insert(&make_row("cron_claim")).await.unwrap();
        let other_repo = SqliteCronRepository::new(db.pool().clone());

        let first = ClaimCronRunParams {
            job_id: "cron_claim",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 70_000,
            queue_enabled: false,
        };
        let second = ClaimCronRunParams {
            owner_id: "owner-b",
            ..first.clone()
        };
        let (first, second) = tokio::join!(repo.claim_run(&first), other_repo.claim_run(&second));
        let results = [first.unwrap(), second.unwrap()];

        assert_eq!(
            results
                .iter()
                .filter(|result| **result == CronRunClaimResult::Claimed)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == CronRunClaimResult::Duplicate)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn claim_run_queue_busy_records_skipped_occurrence() {
        let (repo, db) = setup().await;
        repo.insert(&make_row("cron_queue_claim")).await.unwrap();

        assert_eq!(
            repo.claim_run(&ClaimCronRunParams {
                job_id: "cron_queue_claim",
                scheduled_at: 10_000,
                owner_id: "owner-a",
                now: 10_000,
                lease_until: 70_000,
                queue_enabled: true,
            })
            .await
            .unwrap(),
            CronRunClaimResult::Claimed
        );
        assert_eq!(
            repo.claim_run(&ClaimCronRunParams {
                job_id: "cron_queue_claim",
                scheduled_at: 20_000,
                owner_id: "owner-b",
                now: 20_000,
                lease_until: 80_000,
                queue_enabled: true,
            })
            .await
            .unwrap(),
            CronRunClaimResult::QueueBusy
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM cron_job_runs WHERE job_id = 'cron_queue_claim' AND scheduled_at = 20000",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(status, "skipped");
    }

    #[tokio::test]
    async fn expired_run_lease_can_be_reclaimed() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_expired")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_expired",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 20_000,
            queue_enabled: false,
        })
        .await
        .unwrap();

        assert_eq!(
            repo.claim_run(&ClaimCronRunParams {
                job_id: "cron_expired",
                scheduled_at: 10_000,
                owner_id: "owner-b",
                now: 20_001,
                lease_until: 80_001,
                queue_enabled: false,
            })
            .await
            .unwrap(),
            CronRunClaimResult::Claimed
        );
    }

    #[tokio::test]
    async fn deferred_run_reuses_original_occurrence_key() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_retry_claim")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_retry_claim",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 70_000,
            queue_enabled: false,
        })
        .await
        .unwrap();
        assert!(
            repo.defer_run("cron_retry_claim", 10_000, "owner-a", 20_000, 11_000)
                .await
                .unwrap()
        );

        assert_eq!(
            repo.claim_run(&ClaimCronRunParams {
                job_id: "cron_retry_claim",
                scheduled_at: 10_000,
                owner_id: "owner-b",
                now: 20_000,
                lease_until: 80_000,
                queue_enabled: false,
            })
            .await
            .unwrap(),
            CronRunClaimResult::Claimed
        );
    }

    #[tokio::test]
    async fn queue_enabled_treats_deferred_run_as_active() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_retry_queue")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_retry_queue",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 70_000,
            queue_enabled: true,
        })
        .await
        .unwrap();
        repo.defer_run("cron_retry_queue", 10_000, "owner-a", 40_000, 11_000)
            .await
            .unwrap();

        assert_eq!(
            repo.claim_run(&ClaimCronRunParams {
                job_id: "cron_retry_queue",
                scheduled_at: 20_000,
                owner_id: "owner-b",
                now: 20_000,
                lease_until: 80_000,
                queue_enabled: true,
            })
            .await
            .unwrap(),
            CronRunClaimResult::QueueBusy
        );
    }

    #[tokio::test]
    async fn recoverable_running_run_waits_for_unexpired_lease() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_recover_running")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_recover_running",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 70_000,
            queue_enabled: false,
        })
        .await
        .unwrap();

        assert_eq!(
            repo.get_recoverable_run("cron_recover_running", 20_000).await.unwrap(),
            Some(RecoverableCronRun {
                scheduled_at: 10_000,
                wake_at: 70_000,
            })
        );
    }

    #[tokio::test]
    async fn recoverable_running_run_wakes_immediately_after_expiry() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_recover_expired")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_recover_expired",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 20_000,
            queue_enabled: false,
        })
        .await
        .unwrap();

        assert_eq!(
            repo.get_recoverable_run("cron_recover_expired", 30_000).await.unwrap(),
            Some(RecoverableCronRun {
                scheduled_at: 10_000,
                wake_at: 30_000,
            })
        );
    }

    #[tokio::test]
    async fn recoverable_retry_preserves_original_occurrence() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_recover_retry")).await.unwrap();
        repo.claim_run(&ClaimCronRunParams {
            job_id: "cron_recover_retry",
            scheduled_at: 10_000,
            owner_id: "owner-a",
            now: 10_000,
            lease_until: 70_000,
            queue_enabled: false,
        })
        .await
        .unwrap();
        repo.defer_run("cron_recover_retry", 10_000, "owner-a", 40_000, 11_000)
            .await
            .unwrap();

        assert_eq!(
            repo.get_recoverable_run("cron_recover_retry", 20_000).await.unwrap(),
            Some(RecoverableCronRun {
                scheduled_at: 10_000,
                wake_at: 40_000,
            })
        );
    }

    #[tokio::test]
    async fn update_optional_nullable_fields() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_u2")).await.unwrap();

        let params = UpdateCronJobParams {
            last_status: Some(Some("ok".into())),
            last_error: Some(Some("timeout".into())),
            skill_content: Some(Some("---\nname: skill\n---\nDo it".into())),
            ..Default::default()
        };
        repo.update("cron_u2", &params).await.unwrap();

        let updated = repo.get_by_id("cron_u2").await.unwrap().unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert_eq!(updated.last_error.as_deref(), Some("timeout"));
        assert!(updated.skill_content.is_some());

        let clear_params = UpdateCronJobParams {
            last_status: Some(None),
            last_error: Some(None),
            skill_content: Some(None),
            ..Default::default()
        };
        repo.update("cron_u2", &clear_params).await.unwrap();

        let cleared = repo.get_by_id("cron_u2").await.unwrap().unwrap();
        assert!(cleared.last_status.is_none());
        assert!(cleared.last_error.is_none());
        assert!(cleared.skill_content.is_none());
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let params = UpdateCronJobParams {
            name: Some("x".into()),
            ..Default::default()
        };
        let err = repo.update("cron_nope", &params).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_empty_params_is_noop() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_noop")).await.unwrap();

        let before = repo.get_by_id("cron_noop").await.unwrap().unwrap();
        repo.update("cron_noop", &UpdateCronJobParams::default()).await.unwrap();
        let after = repo.get_by_id("cron_noop").await.unwrap().unwrap();

        assert_eq!(before.updated_at, after.updated_at);
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_d1")).await.unwrap();

        repo.delete("cron_d1").await.unwrap();
        let result = repo.get_by_id("cron_d1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete("cron_nope").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_by_conversation_removes_all_related() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_dc1")).await.unwrap();
        repo.insert(&make_row("cron_dc2")).await.unwrap();

        let deleted = repo.delete_by_conversation("conv_1").await.unwrap();
        assert_eq!(deleted, 2);

        let remaining = repo.list_all().await.unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn delete_by_conversation_returns_zero_for_no_match() {
        let (repo, _db) = setup().await;
        let deleted = repo.delete_by_conversation("conv_none").await.unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn update_schedule_fields() {
        let (repo, _db) = setup().await;
        repo.insert(&make_row("cron_s1")).await.unwrap();

        let params = UpdateCronJobParams {
            schedule_kind: Some("cron".into()),
            schedule_value: Some("0 0 9 * * *".into()),
            schedule_tz: Some(Some("Asia/Shanghai".into())),
            schedule_description: Some(Some("Daily at 9am".into())),
            next_run_at: Some(Some(9999999)),
            ..Default::default()
        };
        repo.update("cron_s1", &params).await.unwrap();

        let updated = repo.get_by_id("cron_s1").await.unwrap().unwrap();
        assert_eq!(updated.schedule_kind, "cron");
        assert_eq!(updated.schedule_value, "0 0 9 * * *");
        assert_eq!(updated.schedule_tz.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(updated.next_run_at, Some(9999999));
    }

    #[tokio::test]
    async fn insert_all_schedule_kinds() {
        let (repo, _db) = setup().await;

        let mut at_job = make_row("cron_at");
        at_job.schedule_kind = "at".into();
        at_job.schedule_value = "1700000000000".into();
        repo.insert(&at_job).await.unwrap();

        let mut cron_job = make_row("cron_cron");
        cron_job.schedule_kind = "cron".into();
        cron_job.schedule_value = "0 */5 * * * *".into();
        cron_job.schedule_tz = Some("UTC".into());
        repo.insert(&cron_job).await.unwrap();

        let all = repo.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn insert_with_skill_content() {
        let (repo, _db) = setup().await;
        let mut row = make_row("cron_sk");
        row.skill_content = Some("---\nname: My Skill\ndescription: A test\n---\nDo X".into());
        repo.insert(&row).await.unwrap();

        let found = repo.get_by_id("cron_sk").await.unwrap().unwrap();
        assert!(found.skill_content.unwrap().contains("My Skill"));
    }

    #[tokio::test]
    async fn insert_with_agent_config_json() {
        let (repo, _db) = setup().await;
        let mut row = make_row("cron_ac");
        row.agent_config = Some(r#"{"name":"GPT","model_id":"gpt-4"}"#.into());
        repo.insert(&row).await.unwrap();

        let found = repo.get_by_id("cron_ac").await.unwrap().unwrap();
        let config = found.agent_config.unwrap();
        assert!(config.contains("gpt-4"));
    }
}
