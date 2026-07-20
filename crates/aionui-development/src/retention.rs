use aionui_api_types::{
    DevelopmentRetentionPolicy, RetentionCleanupReport, RetentionCleanupRequest, RetentionPolicyInput,
};
use aionui_common::now_ms;
use aionui_db::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};
use uuid::Uuid;

use crate::DevelopmentError;

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const DEFAULT_HISTORY_DAYS: i64 = 365;
const DEFAULT_ARTIFACT_DAYS: i64 = 90;
const DEFAULT_EVALUATION_DAYS: i64 = 365;
const MIN_RETENTION_DAYS: i64 = 1;
const MAX_RETENTION_DAYS: i64 = 3_650;

#[derive(Clone)]
pub struct RetentionService {
    pool: SqlitePool,
}

impl RetentionService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_policy(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<DevelopmentRetentionPolicy, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        let row = sqlx::query(
            "SELECT user_id,project_id,conversation_history_days,artifact_days,evaluation_days,\
             immutable_audit_log,updated_at FROM development_retention_policies \
             WHERE user_id=? AND project_id=?",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        match row {
            Some(row) => policy_from_row(&row),
            None => Ok(DevelopmentRetentionPolicy {
                user_id: user_id.into(),
                project_id: project_id.into(),
                conversation_history_days: DEFAULT_HISTORY_DAYS,
                artifact_days: DEFAULT_ARTIFACT_DAYS,
                evaluation_days: DEFAULT_EVALUATION_DAYS,
                immutable_audit_log: true,
                updated_at: 0,
            }),
        }
    }

    pub async fn update_policy(
        &self,
        user_id: &str,
        project_id: &str,
        input: RetentionPolicyInput,
    ) -> Result<DevelopmentRetentionPolicy, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        validate_days(input.conversation_history_days)?;
        validate_days(input.artifact_days)?;
        validate_days(input.evaluation_days)?;
        let updated_at = now_ms();
        sqlx::query(
            "INSERT INTO development_retention_policies \
             (user_id,project_id,conversation_history_days,artifact_days,evaluation_days,immutable_audit_log,updated_at) \
             VALUES (?,?,?,?,?,1,?) ON CONFLICT(user_id,project_id) DO UPDATE SET \
             conversation_history_days=excluded.conversation_history_days,artifact_days=excluded.artifact_days,\
             evaluation_days=excluded.evaluation_days,updated_at=excluded.updated_at",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(input.conversation_history_days)
        .bind(input.artifact_days)
        .bind(input.evaluation_days)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        self.get_policy(user_id, project_id).await
    }

    pub async fn cleanup(
        &self,
        user_id: &str,
        project_id: &str,
        request: RetentionCleanupRequest,
    ) -> Result<RetentionCleanupReport, DevelopmentError> {
        let policy = self.get_policy(user_id, project_id).await?;
        let now = now_ms();
        let history_cutoff = cutoff(now, policy.conversation_history_days);
        let artifact_cutoff = cutoff(now, policy.artifact_days);
        let evaluation_cutoff = cutoff(now, policy.evaluation_days);
        let counts = cleanup_counts(
            &self.pool,
            user_id,
            project_id,
            history_cutoff,
            artifact_cutoff,
            evaluation_cutoff,
        )
        .await?;
        if request.dry_run {
            return Ok(report(None, project_id, true, counts, now));
        }

        let required_confirmations: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(dangerous_confirmation_count),2) FROM development_policies \
             WHERE user_id=? AND project_id=?",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        if i64::from(request.confirmation_count) < required_confirmations.max(2) {
            return Err(DevelopmentError::BadRequest(format!(
                "retention cleanup requires {} confirmations",
                required_confirmations.max(2)
            )));
        }

        let execution_id = Uuid::now_v7().to_string();
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        let applied_counts = delete_expired(
            &mut transaction,
            user_id,
            project_id,
            history_cutoff,
            artifact_cutoff,
            evaluation_cutoff,
        )
        .await?;
        sqlx::query(
            "INSERT INTO development_retention_executions \
             (id,user_id,project_id,message_count,artifact_count,evaluation_count,audit_events_retained,created_at) \
             VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(&execution_id)
        .bind(user_id)
        .bind(project_id)
        .bind(applied_counts.0)
        .bind(applied_counts.1)
        .bind(applied_counts.2)
        .bind(applied_counts.3)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(internal)?;
        sqlx::query(
            "INSERT INTO development_audit_events \
             (id,user_id,actor_type,actor_id,action,target_type,target_id,project_id,result,redacted_payload_json,created_at) \
             VALUES (? ,?,'user',?,'retention.cleanup','project',?,?,'success',?,?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(user_id)
        .bind(user_id)
        .bind(project_id)
        .bind(project_id)
        .bind(
            serde_json::json!({
                "execution_id": execution_id,
                "messages": applied_counts.0,
                "artifacts": applied_counts.1,
                "evaluations": applied_counts.2,
                "immutable_audit_events_retained": applied_counts.3,
            })
            .to_string(),
        )
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(report(Some(execution_id), project_id, false, applied_counts, now))
    }

    async fn require_project(&self, user_id: &str, project_id: &str) -> Result<(), DevelopmentError> {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id=? AND user_id=?")
            .bind(project_id)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        if exists == 1 {
            Ok(())
        } else {
            Err(DevelopmentError::NotFound(format!("project {project_id}")))
        }
    }
}

fn validate_days(days: i64) -> Result<(), DevelopmentError> {
    if (MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&days) {
        Ok(())
    } else {
        Err(DevelopmentError::BadRequest(format!(
            "retention days must be between {MIN_RETENTION_DAYS} and {MAX_RETENTION_DAYS}"
        )))
    }
}

fn cutoff(now: i64, days: i64) -> i64 {
    now.saturating_sub(days.saturating_mul(DAY_MS))
}

fn policy_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<DevelopmentRetentionPolicy, DevelopmentError> {
    Ok(DevelopmentRetentionPolicy {
        user_id: row.try_get("user_id").map_err(internal)?,
        project_id: row.try_get("project_id").map_err(internal)?,
        conversation_history_days: row.try_get("conversation_history_days").map_err(internal)?,
        artifact_days: row.try_get("artifact_days").map_err(internal)?,
        evaluation_days: row.try_get("evaluation_days").map_err(internal)?,
        immutable_audit_log: row.try_get::<i64, _>("immutable_audit_log").map_err(internal)? != 0,
        updated_at: row.try_get("updated_at").map_err(internal)?,
    })
}

async fn cleanup_counts(
    pool: &SqlitePool,
    user_id: &str,
    project_id: &str,
    history_cutoff: i64,
    artifact_cutoff: i64,
    evaluation_cutoff: i64,
) -> Result<(i64, i64, i64, i64), DevelopmentError> {
    let messages = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages m JOIN project_resource_links l \
         ON l.resource_type='conversation' AND l.resource_id=m.conversation_id \
         WHERE l.user_id=? AND l.project_id=? AND m.created_at<?",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(history_cutoff)
    .fetch_one(pool)
    .await
    .map_err(internal)?;
    let artifacts = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_artifacts a JOIN development_runs r ON r.id=a.run_id \
         WHERE r.user_id=? AND r.project_id=? AND a.created_at<?",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(artifact_cutoff)
    .fetch_one(pool)
    .await
    .map_err(internal)?;
    let evaluations = sqlx::query_scalar(
        "SELECT COUNT(*) FROM development_evaluations WHERE user_id=? AND project_id=? \
         AND accepted_baseline=0 AND created_at<?",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(evaluation_cutoff)
    .fetch_one(pool)
    .await
    .map_err(internal)?;
    let audit = sqlx::query_scalar("SELECT COUNT(*) FROM development_audit_events WHERE user_id=? AND project_id=?")
        .bind(user_id)
        .bind(project_id)
        .fetch_one(pool)
        .await
        .map_err(internal)?;
    Ok((messages, artifacts, evaluations, audit))
}

async fn delete_expired(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    project_id: &str,
    history_cutoff: i64,
    artifact_cutoff: i64,
    evaluation_cutoff: i64,
) -> Result<(i64, i64, i64, i64), DevelopmentError> {
    let messages = sqlx::query(
        "DELETE FROM messages WHERE created_at<? AND conversation_id IN \
         (SELECT resource_id FROM project_resource_links WHERE user_id=? AND project_id=? AND resource_type='conversation')",
    )
    .bind(history_cutoff)
    .bind(user_id)
    .bind(project_id)
    .execute(&mut **transaction)
    .await
    .map_err(internal)?
    .rows_affected();
    let artifacts = sqlx::query(
        "DELETE FROM task_artifacts WHERE created_at<? AND run_id IN \
         (SELECT id FROM development_runs WHERE user_id=? AND project_id=?)",
    )
    .bind(artifact_cutoff)
    .bind(user_id)
    .bind(project_id)
    .execute(&mut **transaction)
    .await
    .map_err(internal)?
    .rows_affected();
    let evaluations = sqlx::query(
        "DELETE FROM development_evaluations WHERE user_id=? AND project_id=? \
         AND accepted_baseline=0 AND created_at<?",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(evaluation_cutoff)
    .execute(&mut **transaction)
    .await
    .map_err(internal)?
    .rows_affected();
    let audit = sqlx::query_scalar("SELECT COUNT(*) FROM development_audit_events WHERE user_id=? AND project_id=?")
        .bind(user_id)
        .bind(project_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(internal)?;
    Ok((
        i64::try_from(messages).unwrap_or(i64::MAX),
        i64::try_from(artifacts).unwrap_or(i64::MAX),
        i64::try_from(evaluations).unwrap_or(i64::MAX),
        audit,
    ))
}

fn report(
    execution_id: Option<String>,
    project_id: &str,
    dry_run: bool,
    counts: (i64, i64, i64, i64),
    completed_at: i64,
) -> RetentionCleanupReport {
    RetentionCleanupReport {
        execution_id,
        project_id: project_id.into(),
        dry_run,
        message_count: counts.0,
        artifact_count: counts.1,
        evaluation_count: counts.2,
        audit_events_retained: counts.3,
        completed_at,
    }
}

fn internal(error: impl std::fmt::Display) -> DevelopmentError {
    DevelopmentError::Internal(error.to_string())
}
