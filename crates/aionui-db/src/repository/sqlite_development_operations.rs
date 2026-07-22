use aionui_common::{TimestampMs, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentModelPriceRow, DevelopmentPolicyRow,
    DevelopmentPricedUsageEventRow, DevelopmentRecoveryRecordRow, DevelopmentRunRow, DevelopmentSecretGrantRow,
    DevelopmentSecretRow, DevelopmentUsageDimensionSummary, DevelopmentUsageEventRow, DevelopmentUsageSummary,
    ExecutionResourceLeaseRow, UsageDimension,
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
             allowed_secret_keys_json, allowed_commands_json, protected_paths_json, allowed_network_hosts_json, \
             protected_branches_json, dangerous_confirmation_count, max_duration_ms, max_parallel_agents, \
             max_retries, max_cost_microunits, max_total_tokens, fallback_model, alert_percent, over_limit_action, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, project_id) DO UPDATE SET isolation_mode=excluded.isolation_mode, \
             container_image=excluded.container_image, devcontainer_config_path=excluded.devcontainer_config_path, \
             container_cpu_millis=excluded.container_cpu_millis, container_memory_mb=excluded.container_memory_mb, \
             container_pids_limit=excluded.container_pids_limit, network_mode=excluded.network_mode, \
             allowed_secret_keys_json=excluded.allowed_secret_keys_json, \
             allowed_commands_json=excluded.allowed_commands_json, protected_paths_json=excluded.protected_paths_json, \
             allowed_network_hosts_json=excluded.allowed_network_hosts_json, \
             protected_branches_json=excluded.protected_branches_json, \
             dangerous_confirmation_count=excluded.dangerous_confirmation_count, max_duration_ms=excluded.max_duration_ms, \
             max_parallel_agents=excluded.max_parallel_agents, max_retries=excluded.max_retries, \
             max_cost_microunits=excluded.max_cost_microunits, max_total_tokens=excluded.max_total_tokens, \
             fallback_model=excluded.fallback_model, alert_percent=excluded.alert_percent, \
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
        .bind(&row.allowed_commands_json)
        .bind(&row.protected_paths_json)
        .bind(&row.allowed_network_hosts_json)
        .bind(&row.protected_branches_json)
        .bind(row.dangerous_confirmation_count)
        .bind(row.max_duration_ms)
        .bind(row.max_parallel_agents)
        .bind(row.max_retries)
        .bind(row.max_cost_microunits)
        .bind(row.max_total_tokens)
        .bind(&row.fallback_model)
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

    async fn list_usage(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DevelopmentUsageEventRow>, DbError> {
        let limit = limit.clamp(1, 500);
        Ok(match run_id {
            Some(run_id) => {
                sqlx::query_as(
                    "SELECT * FROM development_usage_events WHERE user_id = ? AND project_id = ? AND run_id = ? \
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
                    "SELECT * FROM development_usage_events WHERE user_id = ? AND project_id = ? \
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

    async fn claim_recovery_and_update_run(
        &self,
        row: &DevelopmentRecoveryRecordRow,
        expected_status: &str,
        expected_updated_at: TimestampMs,
        target_status: &str,
        finished_at: Option<TimestampMs>,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let mut transaction = self.pool.begin().await?;
        let claim = sqlx::query(
            "INSERT INTO development_recovery_records (id, user_id, project_id, run_id, recovery_key, finding, \
             decision, status_before, status_after, details_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, recovery_key) DO UPDATE SET finding=excluded.finding, \
             decision=excluded.decision, status_before=excluded.status_before, status_after=excluded.status_after, \
             details_json=excluded.details_json \
             WHERE development_recovery_records.decision IN ('manual_required', 'interrupted') \
                OR development_recovery_records.decision = excluded.decision",
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
        .execute(&mut *transaction)
        .await?;
        if claim.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        let run_update = sqlx::query(
            "UPDATE development_runs SET status = ?, finished_at = ?, updated_at = MAX(?, updated_at + 1) \
             WHERE id = ? AND user_id = ? AND status = ? AND updated_at = ?",
        )
        .bind(target_status)
        .bind(finished_at)
        .bind(updated_at)
        .bind(row.run_id.as_deref())
        .bind(&row.user_id)
        .bind(expected_status)
        .bind(expected_updated_at)
        .execute(&mut *transaction)
        .await?;
        if run_update.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
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
            "SELECT * FROM development_runs WHERE status IN ('running', 'verifying', 'reviewing', 'integrating') \
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
             owner_instance_id=CASE WHEN execution_resource_leases.recovery_decision IS NULL \
                 THEN excluded.owner_instance_id ELSE execution_resource_leases.owner_instance_id END, \
             heartbeat_at=excluded.heartbeat_at, \
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

    async fn claim_resource_cleanup(
        &self,
        lease_id: &str,
        expected_owner_instance_id: &str,
        expected_status: &str,
        expected_updated_at: TimestampMs,
        cleanup_owner_instance_id: &str,
        claimed_at: TimestampMs,
    ) -> Result<Option<ExecutionResourceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "UPDATE execution_resource_leases \
             SET owner_instance_id = ?, status = 'stopping', accepts_work = 0, \
                 heartbeat_at = ?, expires_at = ? + MAX(expires_at - heartbeat_at, 1), \
                 cleanup_status = 'stopping', cleanup_result = NULL, terminal_at = NULL, \
                 updated_at = MAX(?, updated_at + 1) \
             WHERE id = ? AND owner_instance_id = ? AND status = ? AND updated_at = ? \
               AND status IN ('active', 'orphaned', 'cleanup_failed') \
               AND (terminal_at IS NULL OR status = 'cleanup_failed') \
             RETURNING *",
        )
        .bind(cleanup_owner_instance_id)
        .bind(claimed_at)
        .bind(claimed_at)
        .bind(claimed_at)
        .bind(lease_id)
        .bind(expected_owner_instance_id)
        .bind(expected_status)
        .bind(expected_updated_at)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn finish_resource_cleanup(
        &self,
        lease_id: &str,
        cleanup_owner_instance_id: &str,
        expected_updated_at: TimestampMs,
        succeeded: bool,
        cleanup_result: &str,
        completed_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let (status, cleanup_status) = if succeeded {
            ("released", "released")
        } else {
            ("cleanup_failed", "failed")
        };
        let result = sqlx::query(
            "UPDATE execution_resource_leases \
             SET status = ?, accepts_work = 0, cleanup_status = ?, cleanup_result = ?, \
                 updated_at = MAX(?, updated_at + 1), terminal_at = ? \
             WHERE id = ? AND owner_instance_id = ? AND status = 'stopping' \
               AND updated_at = ? AND terminal_at IS NULL",
        )
        .bind(status)
        .bind(cleanup_status)
        .bind(cleanup_result)
        .bind(completed_at)
        .bind(completed_at)
        .bind(lease_id)
        .bind(cleanup_owner_instance_id)
        .bind(expected_updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn claim_resource_recovery_decision(
        &self,
        lease_id: &str,
        decision: &str,
        takeover_owner_instance_id: Option<&str>,
        updated_at: TimestampMs,
    ) -> Result<Option<ExecutionResourceLeaseRow>, DbError> {
        let result = if decision == "takeover" {
            sqlx::query_as(
                "UPDATE execution_resource_leases \
                 SET recovery_decision = ?, owner_instance_id = ?, status = 'active', accepts_work = 1, \
                     heartbeat_at = ?, expires_at = ? + MAX(expires_at - heartbeat_at, 1), \
                     cleanup_status = NULL, cleanup_result = NULL, terminal_at = NULL, \
                     updated_at = MAX(?, updated_at + 1) \
                 WHERE id = ? AND status = 'orphaned' AND terminal_at IS NULL \
                    AND (recovery_decision IS NULL \
                    OR (recovery_decision = 'takeover' AND status = 'orphaned')) \
                 RETURNING *",
            )
            .bind(decision)
            .bind(takeover_owner_instance_id)
            .bind(updated_at)
            .bind(updated_at)
            .bind(updated_at)
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "UPDATE execution_resource_leases SET recovery_decision = ?, \
                 updated_at = MAX(?, updated_at + 1) \
                 WHERE id = ? AND recovery_decision IS NULL \
                 RETURNING *",
            )
            .bind(decision)
            .bind(updated_at)
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(result)
    }

    async fn heartbeat_resource_lease(
        &self,
        lease_id: &str,
        owner_instance_id: &str,
        expected_updated_at: TimestampMs,
        heartbeat_at: TimestampMs,
        expires_at: TimestampMs,
    ) -> Result<Option<ExecutionResourceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "UPDATE execution_resource_leases SET heartbeat_at = ?, expires_at = ?, \
             updated_at = MAX(?, updated_at + 1) \
             WHERE id = ? AND owner_instance_id = ? AND updated_at = ? \
               AND status = 'active' AND accepts_work = 1 AND terminal_at IS NULL \
             RETURNING *",
        )
        .bind(heartbeat_at)
        .bind(expires_at)
        .bind(heartbeat_at)
        .bind(lease_id)
        .bind(owner_instance_id)
        .bind(expected_updated_at)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn complete_resource_lease(
        &self,
        lease_id: &str,
        owner_instance_id: &str,
        expected_updated_at: TimestampMs,
        cleanup_result: &str,
        completed_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE execution_resource_leases SET accepts_work = 0, status = 'released', \
             cleanup_status = 'released', cleanup_result = ?, \
             updated_at = MAX(?, updated_at + 1), terminal_at = ? \
             WHERE id = ? AND owner_instance_id = ? AND status = 'active' AND accepts_work = 1 \
               AND updated_at = ? AND terminal_at IS NULL",
        )
        .bind(cleanup_result)
        .bind(completed_at)
        .bind(completed_at)
        .bind(lease_id)
        .bind(owner_instance_id)
        .bind(expected_updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn orphan_resource_lease(
        &self,
        lease_id: &str,
        owner_instance_id: &str,
        expected_expires_at: TimestampMs,
        orphaned_at: TimestampMs,
    ) -> Result<Option<ExecutionResourceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "UPDATE execution_resource_leases SET status = 'orphaned', accepts_work = 0, \
             recovery_decision = NULL, updated_at = MAX(?, updated_at + 1) \
             WHERE id = ? AND owner_instance_id = ? AND expires_at = ? \
               AND status IN ('active', 'stopping') AND terminal_at IS NULL \
             RETURNING *",
        )
        .bind(orphaned_at)
        .bind(lease_id)
        .bind(owner_instance_id)
        .bind(expected_expires_at)
        .fetch_optional(&self.pool)
        .await?)
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

    async fn insert_secret(&self, row: &DevelopmentSecretRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_secrets (id, user_id, project_id, name, encrypted_value, key_version, status, \
             expires_at, revoked_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.name)
        .bind(&row.encrypted_value)
        .bind(&row.key_version)
        .bind(&row.status)
        .bind(row.expires_at)
        .bind(row.revoked_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_secret(&self, user_id: &str, secret_id: &str) -> Result<Option<DevelopmentSecretRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM development_secrets WHERE user_id = ? AND id = ?")
                .bind(user_id)
                .bind(secret_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_secrets(&self, user_id: &str, project_id: &str) -> Result<Vec<DevelopmentSecretRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_secrets WHERE user_id = ? AND project_id = ? ORDER BY created_at DESC, id DESC",
        )
        .bind(user_id)
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn revoke_secret(&self, user_id: &str, secret_id: &str, revoked_at: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE development_secrets SET status = 'revoked', revoked_at = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND status = 'active'",
        )
        .bind(revoked_at)
        .bind(revoked_at)
        .bind(user_id)
        .bind(secret_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert_secret_grant(&self, row: &DevelopmentSecretGrantRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_secret_grants (id, user_id, project_id, secret_id, scope_type, scope_id, \
             environment_key, status, expires_at, revoked_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, secret_id, scope_type, scope_id, environment_key) DO UPDATE SET \
             status=excluded.status, expires_at=excluded.expires_at, revoked_at=excluded.revoked_at, \
             updated_at=excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.secret_id)
        .bind(&row.scope_type)
        .bind(&row.scope_id)
        .bind(&row.environment_key)
        .bind(&row.status)
        .bind(row.expires_at)
        .bind(row.revoked_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_secret_grants(
        &self,
        user_id: &str,
        secret_id: &str,
    ) -> Result<Vec<DevelopmentSecretGrantRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_secret_grants WHERE user_id = ? AND secret_id = ? \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(user_id)
        .bind(secret_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn upsert_model_price(&self, row: &DevelopmentModelPriceRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_model_prices (id, provider, model, input_per_million_microunits, \
             output_per_million_microunits, cache_read_per_million_microunits, \
             cache_write_per_million_microunits, source_id, version, effective_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider, model, source_id, version) DO UPDATE SET \
             input_per_million_microunits=excluded.input_per_million_microunits, \
             output_per_million_microunits=excluded.output_per_million_microunits, \
             cache_read_per_million_microunits=excluded.cache_read_per_million_microunits, \
             cache_write_per_million_microunits=excluded.cache_write_per_million_microunits, \
             effective_at=excluded.effective_at",
        )
        .bind(&row.id)
        .bind(&row.provider)
        .bind(&row.model)
        .bind(row.input_per_million_microunits)
        .bind(row.output_per_million_microunits)
        .bind(row.cache_read_per_million_microunits)
        .bind(row.cache_write_per_million_microunits)
        .bind(&row.source_id)
        .bind(&row.version)
        .bind(row.effective_at)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resolve_model_price(
        &self,
        provider: &str,
        model: &str,
        occurred_at: TimestampMs,
    ) -> Result<Option<DevelopmentModelPriceRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_model_prices WHERE provider = ? AND model = ? AND effective_at <= ? \
             ORDER BY effective_at DESC, created_at DESC, id DESC LIMIT 1",
        )
        .bind(provider)
        .bind(model)
        .bind(occurred_at)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn append_priced_usage(&self, row: &DevelopmentPricedUsageEventRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO development_usage_events (id, user_id, project_id, run_id, task_id, conversation_id, \
             agent_id, team_id, usage_type, source, confidence, provider, model, input_tokens, output_tokens, \
             cache_read_tokens, cache_write_tokens, cost_microunits, cost_status, cost_origin, price_source_id, \
             price_version, price_effective_at, duration_ms, retry_count, metadata_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.conversation_id)
        .bind(&row.agent_id)
        .bind(&row.team_id)
        .bind(&row.usage_type)
        .bind(&row.source)
        .bind(&row.confidence)
        .bind(&row.provider)
        .bind(&row.model)
        .bind(row.input_tokens)
        .bind(row.output_tokens)
        .bind(row.cache_read_tokens)
        .bind(row.cache_write_tokens)
        .bind(row.cost_microunits)
        .bind(&row.cost_status)
        .bind(&row.cost_origin)
        .bind(&row.price_source_id)
        .bind(&row.price_version)
        .bind(row.price_effective_at)
        .bind(row.duration_ms)
        .bind(row.retry_count)
        .bind(&row.metadata_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn append_priced_usage_once(&self, row: &DevelopmentPricedUsageEventRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO development_usage_events (id, user_id, project_id, run_id, task_id, conversation_id, \
             agent_id, team_id, usage_type, source, confidence, provider, model, input_tokens, output_tokens, \
             cache_read_tokens, cache_write_tokens, cost_microunits, cost_status, cost_origin, price_source_id, \
             price_version, price_effective_at, duration_ms, retry_count, metadata_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.conversation_id)
        .bind(&row.agent_id)
        .bind(&row.team_id)
        .bind(&row.usage_type)
        .bind(&row.source)
        .bind(&row.confidence)
        .bind(&row.provider)
        .bind(&row.model)
        .bind(row.input_tokens)
        .bind(row.output_tokens)
        .bind(row.cache_read_tokens)
        .bind(row.cache_write_tokens)
        .bind(row.cost_microunits)
        .bind(&row.cost_status)
        .bind(&row.cost_origin)
        .bind(&row.price_source_id)
        .bind(&row.price_version)
        .bind(row.price_effective_at)
        .bind(row.duration_ms)
        .bind(row.retry_count)
        .bind(&row.metadata_json)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn latest_priced_usage_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<DevelopmentPricedUsageEventRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM development_usage_events WHERE user_id = ? AND conversation_id = ? \
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn summarize_usage_dimension(
        &self,
        user_id: &str,
        dimension: &UsageDimension,
    ) -> Result<DevelopmentUsageDimensionSummary, DbError> {
        let (column, value) = match dimension {
            UsageDimension::Project(value) => ("project_id", value),
            UsageDimension::Run(value) => ("run_id", value),
            UsageDimension::Task(value) => ("task_id", value),
            UsageDimension::Conversation(value) => ("conversation_id", value),
            UsageDimension::Agent(value) => ("agent_id", value),
            UsageDimension::Team(value) => ("team_id", value),
        };
        let sql = format!(
            "SELECT COUNT(*) AS event_count, COALESCE(SUM(input_tokens), 0) AS input_tokens, \
             COALESCE(SUM(output_tokens), 0) AS output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, \
             COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, \
             COALESCE(SUM(CASE WHEN cost_status = 'known' THEN cost_microunits ELSE 0 END), 0) AS known_cost_microunits, \
             COALESCE(SUM(CASE WHEN cost_status = 'unknown' THEN 1 ELSE 0 END), 0) AS unknown_cost_events, \
             COALESCE(SUM(duration_ms), 0) AS duration_ms, COALESCE(SUM(retry_count), 0) AS retry_count \
             FROM development_usage_events WHERE user_id = ? AND {column} = ?"
        );
        Ok(sqlx::query_as(&sql)
            .bind(user_id)
            .bind(value)
            .fetch_one(&self.pool)
            .await?)
    }
}
