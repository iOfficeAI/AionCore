use aionui_common::{TimestampMs, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentPolicyRow, DevelopmentRecoveryRecordRow,
    DevelopmentRunRow, DevelopmentUsageEventRow, DevelopmentUsageSummary, ExecutionResourceLeaseRow,
};
use crate::repository::development_operations::IDevelopmentOperationsRepository;

#[derive(Clone, Debug)]
pub struct SqliteDevelopmentOperationsRepository {
    pool: SqlitePool,
}

impl SqliteDevelopmentOperationsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IDevelopmentOperationsRepository for SqliteDevelopmentOperationsRepository {
    async fn get_policy(&self, user_id: &str, project_id: &str) -> Result<Option<DevelopmentPolicyRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_policies WHERE user_id = ? AND project_id = ?")
                .bind(user_id)
                .bind(project_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn upsert_policy(&self, row: &DevelopmentPolicyRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_policies (id, user_id, project_id, isolation_mode, container_image, \
             devcontainer_config_path, container_cpu_millis, container_memory_mb, container_pids_limit, network_mode, \
             allowed_secret_keys_json, max_duration_ms, max_parallel_agents, max_retries, max_cost_microunits, \
             alert_percent, over_limit_action, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, project_id) DO UPDATE SET isolation_mode=excluded.isolation_mode, \
             container_image=excluded.container_image, devcontainer_config_path=excluded.devcontainer_config_path, \
             container_cpu_millis=excluded.container_cpu_millis, container_memory_mb=excluded.container_memory_mb, \
             container_pids_limit=excluded.container_pids_limit, network_mode=excluded.network_mode, \
             allowed_secret_keys_json=excluded.allowed_secret_keys_json, max_duration_ms=excluded.max_duration_ms, \
             max_parallel_agents=excluded.max_parallel_agents, max_retries=excluded.max_retries, \
             max_cost_microunits=excluded.max_cost_microunits, alert_percent=excluded.alert_percent, \
             over_limit_action=excluded.over_limit_action, updated_at=excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.isolation_mode)
        .bind(&row.container_image)
        .bind(&row.devcontainer_config_path)
        .bind(row.container_cpu_millis)
        .bind(row.container_memory_mb)
        .bind(row.container_pids_limit)
        .bind(&row.network_mode)
        .bind(&row.allowed_secret_keys_json)
        .bind(row.max_duration_ms)
        .bind(row.max_parallel_agents)
        .bind(row.max_retries)
        .bind(row.max_cost_microunits)
        .bind(row.alert_percent)
        .bind(&row.over_limit_action)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn append_usage(&self, row: &DevelopmentUsageEventRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_usage_events (id, user_id, project_id, run_id, task_id, usage_type, source, \
             confidence, input_tokens, output_tokens, cost_microunits, duration_ms, retry_count, metadata_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.usage_type)
        .bind(&row.source)
        .bind(&row.confidence)
        .bind(row.input_tokens)
        .bind(row.output_tokens)
        .bind(row.cost_microunits)
        .bind(row.duration_ms)
        .bind(row.retry_count)
        .bind(&row.metadata_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn summarize_usage(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
    ) -> Result<DevelopmentUsageSummary, DbError> {
        let base = "SELECT COUNT(*) AS event_count, COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost_microunits), 0) AS cost_microunits, \
                    COALESCE(SUM(duration_ms), 0) AS duration_ms, COALESCE(SUM(retry_count), 0) AS retry_count \
                    FROM development_usage_events WHERE user_id = ? AND project_id = ?";
        let row = match run_id {
            Some(run_id) => {
                sqlx::query_as::<_, DevelopmentUsageSummary>(&format!("{base} AND run_id = ?"))
                    .bind(user_id)
                    .bind(project_id)
                    .bind(run_id)
                    .fetch_one(&self.pool)
                    .await?
            }
            None => {
                sqlx::query_as::<_, DevelopmentUsageSummary>(base)
                    .bind(user_id)
                    .bind(project_id)
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(row)
    }

    async fn append_audit(&self, row: &DevelopmentAuditEventRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_audit_events (id, user_id, actor_type, actor_id, action, target_type, \
             target_id, project_id, run_id, task_id, result, redacted_payload_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.actor_type)
        .bind(&row.actor_id)
        .bind(&row.action)
        .bind(&row.target_type)
        .bind(&row.target_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.result)
        .bind(&row.redacted_payload_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_audit(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DevelopmentAuditEventRow>, DbError> {
        let limit = limit.clamp(1, 200);
        Ok(match run_id {
            Some(run_id) => {
                sqlx::query_as(
                    "SELECT * FROM development_audit_events WHERE user_id = ? AND project_id = ? AND run_id = ? \
                 ORDER BY created_at DESC, id DESC LIMIT ?",
                )
                .bind(user_id)
                .bind(project_id)
                .bind(run_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT * FROM development_audit_events WHERE user_id = ? AND project_id = ? \
                 ORDER BY created_at DESC, id DESC LIMIT ?",
                )
                .bind(user_id)
                .bind(project_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        })
    }

    async fn upsert_alert(&self, row: &DevelopmentAlertRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_alerts (id, user_id, project_id, run_id, alert_type, severity, status, message, \
             dedupe_key, created_at, updated_at, resolved_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, dedupe_key) DO UPDATE SET run_id=excluded.run_id, alert_type=excluded.alert_type, \
             severity=excluded.severity, status=excluded.status, message=excluded.message, \
             updated_at=excluded.updated_at, resolved_at=excluded.resolved_at",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.alert_type)
        .bind(&row.severity)
        .bind(&row.status)
        .bind(&row.message)
        .bind(&row.dedupe_key)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.resolved_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_alerts(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<DevelopmentAlertRow>, DbError> {
        let status_clause = if active_only { " AND status != 'resolved'" } else { "" };
        Ok(match run_id {
            Some(run_id) => sqlx::query_as(&format!(
                "SELECT * FROM development_alerts WHERE user_id = ? AND project_id = ? AND run_id = ?{status_clause} \
                 ORDER BY updated_at DESC, id ASC"
            ))
            .bind(user_id)
            .bind(project_id)
            .bind(run_id)
            .fetch_all(&self.pool)
            .await?,
            None => {
                sqlx::query_as(&format!(
                    "SELECT * FROM development_alerts WHERE user_id = ? AND project_id = ?{status_clause} \
                 ORDER BY updated_at DESC, id ASC"
                ))
                .bind(user_id)
                .bind(project_id)
                .fetch_all(&self.pool)
                .await?
            }
        })
    }

    async fn update_alert_status(
        &self,
        user_id: &str,
        alert_id: &str,
        status: &str,
        resolved_at: Option<TimestampMs>,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_alerts SET status = ?, resolved_at = ?, updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(status)
        .bind(resolved_at)
        .bind(now_ms())
        .bind(alert_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn append_recovery(&self, row: &DevelopmentRecoveryRecordRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_recovery_records (id, user_id, project_id, run_id, recovery_key, finding, \
             decision, status_before, status_after, details_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, recovery_key) DO UPDATE SET finding=excluded.finding, decision=excluded.decision, \
             status_before=excluded.status_before, status_after=excluded.status_after, \
             details_json=excluded.details_json",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.recovery_key)
        .bind(&row.finding)
        .bind(&row.decision)
        .bind(&row.status_before)
        .bind(&row.status_after)
        .bind(&row.details_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_recovery(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DevelopmentRecoveryRecordRow>, DbError> {
        let limit = limit.clamp(1, 200);
        Ok(match run_id {
            Some(run_id) => {
                sqlx::query_as(
                    "SELECT * FROM development_recovery_records WHERE user_id = ? AND project_id = ? AND run_id = ? \
                 ORDER BY created_at DESC, id DESC LIMIT ?",
                )
                .bind(user_id)
                .bind(project_id)
                .bind(run_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT * FROM development_recovery_records WHERE user_id = ? AND project_id = ? \
                 ORDER BY created_at DESC, id DESC LIMIT ?",
                )
                .bind(user_id)
                .bind(project_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        })
    }

    async fn list_recovery_candidates(&self, updated_before: TimestampMs) -> Result<Vec<DevelopmentRunRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_runs WHERE status IN ('running', 'verifying', 'reviewing') \
             AND updated_at <= ? ORDER BY updated_at ASC, id ASC",
        )
        .bind(updated_before)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn upsert_resource_lease(&self, row: &ExecutionResourceLeaseRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO execution_resource_leases (id, user_id, project_id, run_id, task_id, turn_id, gate_id, \
             environment_id, environment_kind, resource_kind, resource_identifier, status, accepts_work, \
             owner_instance_id, heartbeat_at, expires_at, cleanup_order, cleanup_status, cleanup_result, \
             recovery_decision, created_at, updated_at, terminal_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, accepts_work=excluded.accepts_work, \
             owner_instance_id=excluded.owner_instance_id, heartbeat_at=excluded.heartbeat_at, \
             expires_at=excluded.expires_at, cleanup_status=excluded.cleanup_status, \
             cleanup_result=excluded.cleanup_result, \
             recovery_decision=COALESCE(execution_resource_leases.recovery_decision, excluded.recovery_decision), \
             updated_at=excluded.updated_at, terminal_at=excluded.terminal_at",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.turn_id)
        .bind(&row.gate_id)
        .bind(&row.environment_id)
        .bind(&row.environment_kind)
        .bind(&row.resource_kind)
        .bind(&row.resource_identifier)
        .bind(&row.status)
        .bind(row.accepts_work)
        .bind(&row.owner_instance_id)
        .bind(row.heartbeat_at)
        .bind(row.expires_at)
        .bind(row.cleanup_order)
        .bind(&row.cleanup_status)
        .bind(&row.cleanup_result)
        .bind(&row.recovery_decision)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.terminal_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_resource_lease(&self, lease_id: &str) -> Result<Option<ExecutionResourceLeaseRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM execution_resource_leases WHERE id = ?")
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_resource_leases(
        &self,
        user_id: &str,
        run_id: &str,
        active_only: bool,
    ) -> Result<Vec<ExecutionResourceLeaseRow>, DbError> {
        let status = if active_only {
            " AND status IN ('active', 'stopping', 'orphaned', 'cleanup_failed')"
        } else {
            ""
        };
        Ok(sqlx::query_as(&format!(
            "SELECT * FROM execution_resource_leases WHERE user_id = ? AND run_id = ?{status} \
             ORDER BY cleanup_order ASC, created_at ASC, id ASC"
        ))
        .bind(user_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_stale_resource_leases(&self, now: TimestampMs) -> Result<Vec<ExecutionResourceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM execution_resource_leases WHERE status IN ('active', 'stopping') \
             AND expires_at <= ? ORDER BY expires_at ASC, id ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn bind_execution_environment(
        &self,
        entity_type: &str,
        entity_id: &str,
        environment_id: &str,
        environment_kind: &str,
        bound_at: TimestampMs,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO execution_environment_bindings \
             (entity_type, entity_id, environment_id, environment_kind, bound_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(entity_type, entity_id, environment_id) DO UPDATE SET \
             environment_kind=excluded.environment_kind, bound_at=excluded.bound_at",
        )
        .bind(entity_type)
        .bind(entity_id)
        .bind(environment_id)
        .bind(environment_kind)
        .bind(bound_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
