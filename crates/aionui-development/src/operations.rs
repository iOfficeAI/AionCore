use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentPolicyRow, DevelopmentRecoveryRecordRow,
    DevelopmentRunRow, DevelopmentUsageDimensionSummary, DevelopmentUsageEventRow, DevelopmentUsageSummary,
    UsageDimension,
};
use aionui_db::{
    IAgentWorkspaceLeaseRepository, IDevelopmentOperationsRepository, IDevelopmentRepository, IProjectRepository,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::DevelopmentError;
use crate::policy::{DevelopmentPolicyRules, PolicyDecision, PolicyEngine, PolicyOperation};
use crate::resources::{DevelopmentResourceController, ResourceLeaseCoordinator};

const DEFAULT_MAX_DURATION_MS: i64 = 4 * 60 * 60 * 1000;
const MAX_AUDIT_PAYLOAD_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevelopmentPolicyInput {
    pub isolation_mode: String,
    pub container_image: Option<String>,
    pub devcontainer_config_path: Option<String>,
    pub container_cpu_millis: i64,
    pub container_memory_mb: i64,
    pub container_pids_limit: i64,
    pub network_mode: String,
    #[serde(default)]
    pub allowed_secret_keys: Vec<String>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub protected_paths: Vec<String>,
    #[serde(default)]
    pub allowed_network_hosts: Vec<String>,
    #[serde(default)]
    pub protected_branches: Vec<String>,
    #[serde(default = "default_confirmation_count")]
    pub dangerous_confirmation_count: i64,
    pub max_duration_ms: i64,
    pub max_parallel_agents: i64,
    pub max_retries: i64,
    pub max_cost_microunits: i64,
    #[serde(default)]
    pub max_total_tokens: i64,
    #[serde(default)]
    pub fallback_model: Option<String>,
    pub alert_percent: i64,
    pub over_limit_action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetEvaluation {
    pub allowed: bool,
    pub action: String,
    pub reasons: Vec<String>,
    pub usage: DevelopmentUsageSummary,
    pub replacement_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevelopmentOperationsSnapshot {
    pub policy: DevelopmentPolicyRow,
    pub usage: DevelopmentUsageSummary,
    pub priced_usage: DevelopmentUsageDimensionSummary,
    pub alerts: Vec<DevelopmentAlertRow>,
    pub audit: Vec<DevelopmentAuditEventRow>,
    pub recovery: Vec<DevelopmentRecoveryRecordRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryDecisionInput {
    pub action: String,
}

#[derive(Clone)]
pub struct DevelopmentOperationsService {
    operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
    development_repo: Arc<dyn IDevelopmentRepository>,
    project_repo: Arc<dyn IProjectRepository>,
    lease_repo: Arc<dyn IAgentWorkspaceLeaseRepository>,
    resource_coordinator: Option<ResourceLeaseCoordinator>,
    resource_controller: Option<Arc<dyn DevelopmentResourceController>>,
}

impl DevelopmentOperationsService {
    pub fn new(
        operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
        development_repo: Arc<dyn IDevelopmentRepository>,
        project_repo: Arc<dyn IProjectRepository>,
        lease_repo: Arc<dyn IAgentWorkspaceLeaseRepository>,
    ) -> Self {
        Self {
            operations_repo,
            development_repo,
            project_repo,
            lease_repo,
            resource_coordinator: None,
            resource_controller: None,
        }
    }

    pub fn with_resources(
        mut self,
        coordinator: ResourceLeaseCoordinator,
        controller: Arc<dyn DevelopmentResourceController>,
    ) -> Self {
        self.resource_coordinator = Some(coordinator);
        self.resource_controller = Some(controller);
        self
    }

    pub async fn get_policy(&self, user_id: &str, project_id: &str) -> Result<DevelopmentPolicyRow, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        Ok(self
            .operations_repo
            .get_policy(user_id, project_id)
            .await?
            .unwrap_or_else(|| default_policy(user_id, project_id)))
    }

    pub async fn upsert_policy(
        &self,
        user_id: &str,
        project_id: &str,
        input: DevelopmentPolicyInput,
    ) -> Result<DevelopmentPolicyRow, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        validate_policy(&input)?;
        let existing = self.operations_repo.get_policy(user_id, project_id).await?;
        let now = now_ms();
        let mut secret_keys: Vec<String> = input
            .allowed_secret_keys
            .into_iter()
            .map(|key| key.trim().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        secret_keys.sort();
        let allowed_commands = normalized_values(input.allowed_commands);
        let protected_paths = normalized_values(input.protected_paths);
        let allowed_network_hosts = normalized_values(input.allowed_network_hosts);
        let protected_branches = normalized_values(input.protected_branches);
        let row = DevelopmentPolicyRow {
            id: existing
                .as_ref()
                .map(|row| row.id.clone())
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            user_id: user_id.into(),
            project_id: project_id.into(),
            isolation_mode: input.isolation_mode,
            container_image: clean_optional(input.container_image),
            devcontainer_config_path: clean_optional(input.devcontainer_config_path),
            container_cpu_millis: input.container_cpu_millis,
            container_memory_mb: input.container_memory_mb,
            container_pids_limit: input.container_pids_limit,
            network_mode: input.network_mode,
            allowed_secret_keys_json: serde_json::to_string(&secret_keys)
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?,
            allowed_commands_json: json_array(&allowed_commands)?,
            protected_paths_json: json_array(&protected_paths)?,
            allowed_network_hosts_json: json_array(&allowed_network_hosts)?,
            protected_branches_json: json_array(&protected_branches)?,
            dangerous_confirmation_count: input.dangerous_confirmation_count,
            max_duration_ms: input.max_duration_ms,
            max_parallel_agents: input.max_parallel_agents,
            max_retries: input.max_retries,
            max_cost_microunits: input.max_cost_microunits,
            max_total_tokens: input.max_total_tokens,
            fallback_model: clean_optional(input.fallback_model),
            alert_percent: input.alert_percent,
            over_limit_action: input.over_limit_action,
            created_at: existing.map(|row| row.created_at).unwrap_or(now),
            updated_at: now,
        };
        self.operations_repo.upsert_policy(&row).await?;
        self.audit(
            user_id,
            "user",
            user_id,
            "policy.update",
            "project",
            project_id,
            project_id,
            None,
            None,
            "success",
            json!({
                "isolation_mode": row.isolation_mode,
                "network_mode": row.network_mode,
                "allowed_secret_keys": secret_keys,
                "allowed_commands": allowed_commands,
                "protected_paths": protected_paths,
                "allowed_network_hosts": allowed_network_hosts,
                "protected_branches": protected_branches,
                "dangerous_confirmation_count": row.dangerous_confirmation_count,
                "max_duration_ms": row.max_duration_ms,
                "max_parallel_agents": row.max_parallel_agents,
                "max_retries": row.max_retries,
                "max_cost_microunits": row.max_cost_microunits,
                "max_total_tokens": row.max_total_tokens,
                "fallback_model": row.fallback_model,
                "over_limit_action": row.over_limit_action,
            }),
            &[],
        )
        .await?;
        Ok(row)
    }

    pub async fn evaluate_policy(
        &self,
        user_id: &str,
        project_id: &str,
        target_id: &str,
        operation: &PolicyOperation,
        confirmations: u8,
    ) -> Result<PolicyDecision, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        if !target_id.is_empty() {
            let run = self.require_run(user_id, target_id).await?;
            if run.project_id != project_id {
                return Err(DevelopmentError::NotFound(format!("run {target_id}")));
            }
        }
        let policy = self.get_policy(user_id, project_id).await?;
        let rules = DevelopmentPolicyRules {
            allowed_commands: parse_string_array(&policy.allowed_commands_json)?,
            protected_paths: parse_string_array(&policy.protected_paths_json)?,
            allowed_network_hosts: parse_string_array(&policy.allowed_network_hosts_json)?,
            protected_branches: parse_string_array(&policy.protected_branches_json)?,
            dangerous_confirmation_count: u8::try_from(policy.dangerous_confirmation_count)
                .map_err(|_| DevelopmentError::Internal("invalid persisted confirmation count".into()))?,
        };
        let decision = PolicyEngine::evaluate(&rules, operation, confirmations);
        let result = match decision {
            PolicyDecision::Allowed => "success",
            PolicyDecision::Denied { .. } => "denied",
            PolicyDecision::ConfirmationRequired { .. } => "confirmation_required",
        };
        self.audit(
            user_id,
            "user",
            user_id,
            &format!("policy.{}", policy_operation_kind(operation)),
            "development_run",
            target_id,
            project_id,
            (!target_id.is_empty()).then_some(target_id),
            None,
            result,
            json!({"decision": decision, "confirmations": confirmations}),
            &[],
        )
        .await?;
        Ok(decision)
    }

    pub async fn evaluate_budget(
        &self,
        user_id: &str,
        run_id: &str,
        operation: &str,
        prospective_retry_count: i64,
    ) -> Result<BudgetEvaluation, DevelopmentError> {
        let run = self.require_run(user_id, run_id).await?;
        let policy = self.get_policy(user_id, &run.project_id).await?;
        let usage = self
            .operations_repo
            .summarize_usage(user_id, &run.project_id, Some(run_id))
            .await?;
        let mut reasons = Vec::new();
        let elapsed = run
            .started_at
            .map(|started| now_ms().saturating_sub(started))
            .unwrap_or(0);
        let consumed_duration = elapsed.max(usage.duration_ms);
        if consumed_duration > policy.max_duration_ms {
            reasons.push(format!(
                "duration budget exceeded: {consumed_duration}ms > {}ms",
                policy.max_duration_ms
            ));
        }
        if policy.max_cost_microunits > 0 && usage.cost_microunits > policy.max_cost_microunits {
            reasons.push(format!(
                "cost budget exceeded: {} > {} microunits",
                usage.cost_microunits, policy.max_cost_microunits
            ));
        }
        let total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        if policy.max_total_tokens > 0 && total_tokens > policy.max_total_tokens {
            reasons.push(format!(
                "token budget exceeded: {total_tokens} > {}",
                policy.max_total_tokens
            ));
        }
        if prospective_retry_count > policy.max_retries {
            reasons.push(format!(
                "retry budget exceeded: {prospective_retry_count} > {}",
                policy.max_retries
            ));
        }
        if operation == "assign_role" {
            let roles = self.development_repo.list_roles(run_id).await?;
            let slots = roles.iter().map(|role| role.slot_id.as_str()).collect::<BTreeSet<_>>();
            if slots.len() as i64 >= policy.max_parallel_agents {
                reasons.push(format!(
                    "parallel agent budget exceeded: {} >= {}",
                    slots.len(),
                    policy.max_parallel_agents
                ));
            }
        }
        reasons.sort();

        let threshold_reached = budget_threshold_reached(&policy, &usage, consumed_duration);
        if threshold_reached || !reasons.is_empty() {
            let severity = if reasons.is_empty() { "warning" } else { "critical" };
            let message = if reasons.is_empty() {
                format!("development budget reached {}% warning threshold", policy.alert_percent)
            } else {
                reasons.join("; ")
            };
            self.upsert_alert(&run, "budget", severity, &message, &format!("budget:{run_id}"))
                .await?;
        }

        let allowed = reasons.is_empty() || policy.over_limit_action == "notify";
        if !reasons.is_empty() {
            self.audit(
                user_id,
                "system",
                "development-policy-engine",
                &format!("budget.{operation}"),
                "development_run",
                run_id,
                &run.project_id,
                Some(run_id),
                None,
                if allowed { "success" } else { "denied" },
                json!({
                    "reasons": reasons,
                    "action": policy.over_limit_action,
                    "replacement_model": policy.fallback_model,
                    "usage": usage
                }),
                &[],
            )
            .await?;
            self.apply_budget_action(&run, &policy.over_limit_action).await?;
        }
        Ok(BudgetEvaluation {
            allowed,
            action: policy.over_limit_action,
            reasons,
            usage,
            replacement_model: policy.fallback_model,
        })
    }

    pub async fn require_budget(
        &self,
        user_id: &str,
        run_id: &str,
        operation: &str,
        prospective_retry_count: i64,
    ) -> Result<BudgetEvaluation, DevelopmentError> {
        let evaluation = self
            .evaluate_budget(user_id, run_id, operation, prospective_retry_count)
            .await?;
        if evaluation.allowed {
            Ok(evaluation)
        } else {
            Err(DevelopmentError::Conflict(format!(
                "development budget blocked {operation}: {}",
                evaluation.reasons.join("; ")
            )))
        }
    }

    pub async fn record_usage(&self, row: DevelopmentUsageEventRow) -> Result<(), DevelopmentError> {
        self.require_project(&row.user_id, &row.project_id).await?;
        if let Some(run_id) = row.run_id.as_deref() {
            let run = self.require_run(&row.user_id, run_id).await?;
            if run.project_id != row.project_id {
                return Err(DevelopmentError::BadRequest(
                    "usage run does not belong to the project".into(),
                ));
            }
        }
        let run_id = row.run_id.clone();
        let user_id = row.user_id.clone();
        let retry_count = row.retry_count;
        self.operations_repo.append_usage(&row).await?;
        if let Some(run_id) = run_id {
            self.evaluate_budget(&user_id, &run_id, "usage_recorded", retry_count)
                .await?;
        }
        Ok(())
    }

    async fn apply_budget_action(&self, run: &DevelopmentRunRow, action: &str) -> Result<(), DevelopmentError> {
        if !budget_action_can_transition(&run.status) {
            return Ok(());
        }
        match action {
            "pause" => {
                self.pause_run_if_snapshot_current(run).await?;
            }
            "terminate" => {
                let claimed = self
                    .development_repo
                    .update_run_status_if_current(
                        &run.id,
                        &run.user_id,
                        &run.status,
                        run.updated_at,
                        "integrating",
                        None,
                    )
                    .await?;
                if !claimed {
                    return Ok(());
                }
                let Some(claimed_run) = self.development_repo.get_run(&run.id, &run.user_id).await? else {
                    return Err(DevelopmentError::Internal(
                        "budget termination claim disappeared".into(),
                    ));
                };
                if let (Some(coordinator), Some(controller)) = (&self.resource_coordinator, &self.resource_controller)
                    && let Err(error) = coordinator.cancel_run(&run.user_id, &run.id, controller.as_ref()).await
                {
                    let _ = self
                        .development_repo
                        .update_run_status_if_current(
                            &run.id,
                            &run.user_id,
                            "integrating",
                            claimed_run.updated_at,
                            "paused",
                            None,
                        )
                        .await?;
                    return Err(error);
                }
                self.development_repo
                    .update_run_status_if_current(
                        &run.id,
                        &run.user_id,
                        "integrating",
                        claimed_run.updated_at,
                        "cancelled",
                        Some(now_ms()),
                    )
                    .await?;
            }
            "notify" | "downgrade_model" => {}
            _ => return Err(DevelopmentError::Internal("invalid persisted budget action".into())),
        }
        Ok(())
    }

    async fn pause_run_if_snapshot_current(&self, run: &DevelopmentRunRow) -> Result<(), DevelopmentError> {
        self.development_repo
            .update_run_status_if_current(&run.id, &run.user_id, &run.status, run.updated_at, "paused", None)
            .await?;
        Ok(())
    }

    pub async fn pause_after_runtime_action_failure(
        &self,
        user_id: &str,
        run_id: &str,
        message: &str,
    ) -> Result<(), DevelopmentError> {
        let run = self.require_run(user_id, run_id).await?;
        if budget_action_can_transition(&run.status) {
            self.pause_run_if_snapshot_current(&run).await?;
        }
        self.upsert_alert(
            &run,
            "budget",
            "critical",
            message,
            &format!("budget-runtime-action:{}", run.id),
        )
        .await?;
        self.audit(
            user_id,
            "system",
            "development-policy-engine",
            "budget.runtime_action_failed",
            "development_run",
            run_id,
            &run.project_id,
            Some(run_id),
            None,
            "denied",
            json!({"reason": message, "fallback_action": "pause"}),
            &[],
        )
        .await?;
        Ok(())
    }

    pub async fn snapshot(
        &self,
        user_id: &str,
        project_id: &str,
        run_id: Option<&str>,
    ) -> Result<DevelopmentOperationsSnapshot, DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        if let Some(run_id) = run_id {
            let run = self.require_run(user_id, run_id).await?;
            if run.project_id != project_id {
                return Err(DevelopmentError::NotFound(format!("run {run_id}")));
            }
        }
        let usage_dimension = run_id
            .map(|run_id| UsageDimension::Run(run_id.to_owned()))
            .unwrap_or_else(|| UsageDimension::Project(project_id.to_owned()));
        Ok(DevelopmentOperationsSnapshot {
            policy: self.get_policy(user_id, project_id).await?,
            usage: self
                .operations_repo
                .summarize_usage(user_id, project_id, run_id)
                .await?,
            priced_usage: self
                .operations_repo
                .summarize_usage_dimension(user_id, &usage_dimension)
                .await?,
            alerts: self
                .operations_repo
                .list_alerts(user_id, project_id, run_id, true)
                .await?,
            audit: self.operations_repo.list_audit(user_id, project_id, run_id, 50).await?,
            recovery: self
                .operations_repo
                .list_recovery(user_id, project_id, run_id, 50)
                .await?,
        })
    }

    pub async fn acknowledge_alert(
        &self,
        user_id: &str,
        project_id: &str,
        alert_id: &str,
    ) -> Result<(), DevelopmentError> {
        self.require_project(user_id, project_id).await?;
        let belongs_to_project = self
            .operations_repo
            .list_alerts(user_id, project_id, None, false)
            .await?
            .iter()
            .any(|alert| alert.id == alert_id);
        if !belongs_to_project {
            return Err(DevelopmentError::NotFound(format!("alert {alert_id}")));
        }
        if !self
            .operations_repo
            .update_alert_status(user_id, alert_id, "acknowledged", None)
            .await?
        {
            return Err(DevelopmentError::NotFound(format!("alert {alert_id}")));
        }
        self.audit(
            user_id,
            "user",
            user_id,
            "alert.acknowledge",
            "development_alert",
            alert_id,
            project_id,
            None,
            None,
            "success",
            json!({}),
            &[],
        )
        .await?;
        Ok(())
    }

    pub async fn reconcile_stale_runs(
        &self,
        stale_after_ms: i64,
    ) -> Result<Vec<DevelopmentRecoveryRecordRow>, DevelopmentError> {
        if let Some(resources) = &self.resource_coordinator {
            resources.reconcile_stale(now_ms()).await?;
        }
        self.reconcile_stale_runs_scoped(None, stale_after_ms).await
    }

    pub async fn reconcile_stale_runs_for_user(
        &self,
        user_id: &str,
        stale_after_ms: i64,
    ) -> Result<Vec<DevelopmentRecoveryRecordRow>, DevelopmentError> {
        if stale_after_ms <= 0 {
            return Err(DevelopmentError::BadRequest("stale_after_ms must be positive".into()));
        }
        if let Some(resources) = &self.resource_coordinator {
            resources.reconcile_stale_for_user(user_id, now_ms()).await?;
        }
        self.reconcile_stale_runs_scoped(Some(user_id), stale_after_ms).await
    }

    async fn reconcile_stale_runs_scoped(
        &self,
        user_id: Option<&str>,
        stale_after_ms: i64,
    ) -> Result<Vec<DevelopmentRecoveryRecordRow>, DevelopmentError> {
        if stale_after_ms <= 0 {
            return Err(DevelopmentError::BadRequest("stale_after_ms must be positive".into()));
        }
        let candidates = self
            .operations_repo
            .list_recovery_candidates(now_ms().saturating_sub(stale_after_ms))
            .await?;
        let mut records = Vec::with_capacity(candidates.len());
        for run in candidates {
            if user_id.is_some_and(|user_id| user_id != run.user_id) {
                continue;
            }
            let mut interrupted_gate_count = 0_usize;
            for mut gate in self.development_repo.list_gates(&run.id, None).await? {
                if gate.status == "running" {
                    gate.status = "interrupted".into();
                    gate.finished_at = Some(now_ms());
                    gate.duration_ms = gate
                        .started_at
                        .map(|started_at| now_ms().saturating_sub(started_at).max(0));
                    self.development_repo.update_gate(&gate).await?;
                    interrupted_gate_count += 1;
                }
            }
            let project = self.project_repo.get_for_user(&run.project_id, &run.user_id).await?;
            let mut findings = vec![match project {
                None => "project registration is missing".to_owned(),
                Some(project) if !Path::new(&project.local_path).is_dir() => {
                    "project working directory is missing".to_owned()
                }
                Some(project) if git2::Repository::discover(&project.local_path).is_err() => {
                    "project Git repository is unavailable".to_owned()
                }
                Some(_) => "run heartbeat is stale and requires an explicit recovery decision".to_owned(),
            }];
            if let Some(team_id) = run.team_id.as_deref() {
                let leases = self.lease_repo.list_for_team(team_id).await?;
                let relevant = leases
                    .iter()
                    .filter(|lease| lease.lease_status != "released")
                    .collect::<Vec<_>>();
                if relevant.is_empty() {
                    findings.push("team run has no active workspace lease registered".into());
                }
                for lease in relevant {
                    if lease.lease_status != "active" {
                        findings.push(format!(
                            "workspace lease {} is in {} state",
                            lease.id, lease.lease_status
                        ));
                    } else if !Path::new(&lease.worktree_path).is_dir() {
                        findings.push(format!("workspace lease {} worktree is missing", lease.id));
                    } else if git2::Repository::discover(&lease.worktree_path).is_err() {
                        findings.push(format!("workspace lease {} Git worktree is unavailable", lease.id));
                    }
                    if lease.cleanup_status != "not_started" && lease.cleanup_status != "completed" {
                        findings.push(format!(
                            "workspace lease {} cleanup is {}",
                            lease.id, lease.cleanup_status
                        ));
                    }
                }
            }
            if interrupted_gate_count > 0 {
                findings.push(format!(
                    "marked {interrupted_gate_count} unfinished quality gate(s) as interrupted"
                ));
            }
            let finding = findings.join("; ");
            let recovery_key = format!("run:{}:stale", run.id);
            let existing = self
                .operations_repo
                .list_recovery(&run.user_id, &run.project_id, Some(&run.id), 200)
                .await?
                .into_iter()
                .find(|row| row.recovery_key == recovery_key);
            let record = DevelopmentRecoveryRecordRow {
                id: existing
                    .as_ref()
                    .map(|row| row.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                user_id: run.user_id.clone(),
                project_id: run.project_id.clone(),
                run_id: Some(run.id.clone()),
                recovery_key: recovery_key.clone(),
                finding: finding.clone(),
                decision: "manual_required".into(),
                status_before: Some(run.status.clone()),
                status_after: Some(run.status.clone()),
                details_json: json!({"updated_at": run.updated_at}).to_string(),
                created_at: existing.map(|row| row.created_at).unwrap_or_else(now_ms),
            };
            self.operations_repo.append_recovery(&record).await?;
            self.audit(
                &run.user_id,
                "system",
                "development-recovery-reconciler",
                "recovery.detect",
                "development_run",
                &run.id,
                &run.project_id,
                Some(&run.id),
                None,
                "success",
                json!({"finding": finding, "decision": "manual_required"}),
                &[],
            )
            .await?;
            self.upsert_alert(
                &run,
                "recovery",
                "critical",
                &finding,
                &format!("recovery:{recovery_key}"),
            )
            .await?;
            records.push(record);
        }
        Ok(records)
    }

    pub async fn decide_recovery(
        &self,
        user_id: &str,
        run_id: &str,
        input: RecoveryDecisionInput,
    ) -> Result<DevelopmentRecoveryRecordRow, DevelopmentError> {
        if !matches!(
            input.action.as_str(),
            "resume" | "retry" | "rollback" | "takeover" | "terminate"
        ) {
            return Err(DevelopmentError::BadRequest(
                "recovery action must be resume, retry, rollback, takeover, or terminate".into(),
            ));
        }
        let run = self.require_run(user_id, run_id).await?;
        let recovery_key = format!("run:{run_id}:stale");
        let existing = self
            .operations_repo
            .list_recovery(user_id, &run.project_id, Some(run_id), 200)
            .await?
            .into_iter()
            .find(|row| row.recovery_key == recovery_key);
        let has_recovery_context = existing.as_ref().is_some_and(|row| {
            matches!(row.decision.as_str(), "manual_required" | "interrupted") || row.decision == input.action
        });
        if run.status == "succeeded"
            || (!matches!(run.status.as_str(), "paused" | "cancelled") && !has_recovery_context)
        {
            return Err(DevelopmentError::Conflict(format!(
                "run {} is not awaiting recovery",
                run.id
            )));
        }
        let target = if matches!(input.action.as_str(), "resume" | "retry" | "takeover") {
            "running"
        } else {
            "cancelled"
        };
        let pending_record = DevelopmentRecoveryRecordRow {
            id: existing
                .as_ref()
                .map(|row| row.id.clone())
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            user_id: user_id.into(),
            project_id: run.project_id.clone(),
            run_id: Some(run_id.into()),
            recovery_key,
            finding: existing
                .as_ref()
                .map(|row| row.finding.clone())
                .unwrap_or_else(|| "manual recovery decision".into()),
            decision: input.action.clone(),
            status_before: existing
                .as_ref()
                .and_then(|row| row.status_before.clone())
                .or_else(|| Some(run.status.clone())),
            status_after: None,
            details_json: json!({"state": "pending"}).to_string(),
            created_at: existing.as_ref().map(|row| row.created_at).unwrap_or_else(now_ms),
        };
        if !self
            .operations_repo
            .claim_recovery_and_update_run(&pending_record, &run.status, run.updated_at, "paused", None, now_ms())
            .await?
        {
            return Err(DevelopmentError::Conflict(format!(
                "run {} changed state or already has a different recovery decision",
                run.id
            )));
        }
        let recovering_run = self.require_run(user_id, run_id).await?;
        if recovering_run.status != "paused" {
            return Err(DevelopmentError::Conflict(
                "run left its safe recovery state before resources were reconciled".into(),
            ));
        }
        if let Some(resources) = &self.resource_coordinator {
            let decision = if input.action == "resume" {
                "retry"
            } else {
                input.action.as_str()
            };
            for lease in self.operations_repo.list_resource_leases(user_id, run_id, true).await? {
                resources.record_recovery_decision(&lease.id, decision).await?;
            }
            if matches!(decision, "rollback" | "terminate")
                && let Some(controller) = &self.resource_controller
            {
                resources.cancel_run(user_id, run_id, controller.as_ref()).await?;
            }
        }
        let finished_at = (target == "cancelled").then(now_ms);
        if !self
            .development_repo
            .update_run_status_if_current(
                run_id,
                user_id,
                &recovering_run.status,
                recovering_run.updated_at,
                target,
                finished_at,
            )
            .await?
        {
            return Err(DevelopmentError::Conflict(
                "run left its safe recovery state while resources were reconciled".into(),
            ));
        }
        let record = DevelopmentRecoveryRecordRow {
            status_after: Some(target.into()),
            details_json: "{}".into(),
            ..pending_record
        };
        self.operations_repo.append_recovery(&record).await?;
        for alert in self
            .operations_repo
            .list_alerts(user_id, &run.project_id, Some(run_id), true)
            .await?
        {
            if alert.alert_type == "recovery" {
                self.operations_repo
                    .update_alert_status(user_id, &alert.id, "resolved", Some(now_ms()))
                    .await?;
            }
        }
        self.audit(
            user_id,
            "user",
            user_id,
            &format!("recovery.{}", input.action),
            "development_run",
            run_id,
            &run.project_id,
            Some(run_id),
            None,
            "success",
            json!({"status_before": record.status_before, "status_after": record.status_after}),
            &[],
        )
        .await?;
        Ok(record)
    }

    // Keep the audit schema explicit at call sites so security-relevant actor, target,
    // ownership and scope fields cannot be silently inherited from ambient state.
    #[allow(clippy::too_many_arguments)]
    pub async fn audit(
        &self,
        user_id: &str,
        actor_type: &str,
        actor_id: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
        project_id: &str,
        run_id: Option<&str>,
        task_id: Option<&str>,
        result: &str,
        payload: Value,
        secret_values: &[String],
    ) -> Result<(), DevelopmentError> {
        let payload = serde_json::to_string(&payload).map_err(|error| DevelopmentError::Internal(error.to_string()))?;
        let mut payload = redact_sensitive(&payload, secret_values);
        if payload.len() > MAX_AUDIT_PAYLOAD_BYTES {
            payload.truncate(MAX_AUDIT_PAYLOAD_BYTES);
        }
        let payload = if serde_json::from_str::<Value>(&payload).is_ok() {
            payload
        } else {
            serde_json::to_string(&json!({"redacted_text": payload}))
                .map_err(|error| DevelopmentError::Internal(error.to_string()))?
        };
        self.operations_repo
            .append_audit(&DevelopmentAuditEventRow {
                id: uuid::Uuid::now_v7().to_string(),
                user_id: user_id.into(),
                actor_type: actor_type.into(),
                actor_id: actor_id.into(),
                action: action.into(),
                target_type: target_type.into(),
                target_id: target_id.into(),
                project_id: project_id.into(),
                run_id: run_id.map(str::to_owned),
                task_id: task_id.map(str::to_owned),
                result: result.into(),
                redacted_payload_json: payload,
                created_at: now_ms(),
            })
            .await?;
        Ok(())
    }

    async fn require_project(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<aionui_db::models::ProjectRow, DevelopmentError> {
        self.project_repo
            .get_for_user(project_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("project {project_id}")))
    }

    async fn require_run(&self, user_id: &str, run_id: &str) -> Result<DevelopmentRunRow, DevelopmentError> {
        self.development_repo
            .get_run(run_id, user_id)
            .await?
            .ok_or_else(|| DevelopmentError::NotFound(format!("run {run_id}")))
    }

    async fn upsert_alert(
        &self,
        run: &DevelopmentRunRow,
        alert_type: &str,
        severity: &str,
        message: &str,
        dedupe_key: &str,
    ) -> Result<(), DevelopmentError> {
        let existing = self
            .operations_repo
            .list_alerts(&run.user_id, &run.project_id, Some(&run.id), false)
            .await?
            .into_iter()
            .find(|row| row.dedupe_key == dedupe_key);
        let now = now_ms();
        self.operations_repo
            .upsert_alert(&DevelopmentAlertRow {
                id: existing
                    .as_ref()
                    .map(|row| row.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                user_id: run.user_id.clone(),
                project_id: run.project_id.clone(),
                run_id: Some(run.id.clone()),
                alert_type: alert_type.into(),
                severity: severity.into(),
                status: "open".into(),
                message: redact_sensitive(message, &[]),
                dedupe_key: dedupe_key.into(),
                created_at: existing.map(|row| row.created_at).unwrap_or(now),
                updated_at: now,
                resolved_at: None,
            })
            .await?;
        Ok(())
    }
}

pub fn default_policy(user_id: &str, project_id: &str) -> DevelopmentPolicyRow {
    DevelopmentPolicyRow {
        id: format!("default:{project_id}"),
        user_id: user_id.into(),
        project_id: project_id.into(),
        isolation_mode: "host".into(),
        container_image: None,
        devcontainer_config_path: None,
        container_cpu_millis: 1000,
        container_memory_mb: 2048,
        container_pids_limit: 256,
        network_mode: "none".into(),
        allowed_secret_keys_json: "[]".into(),
        allowed_commands_json: "[]".into(),
        protected_paths_json: "[\".env\",\".git\",\".github/workflows\"]".into(),
        allowed_network_hosts_json: "[]".into(),
        protected_branches_json: "[\"main\",\"master\"]".into(),
        dangerous_confirmation_count: 2,
        max_duration_ms: DEFAULT_MAX_DURATION_MS,
        max_parallel_agents: 4,
        max_retries: 3,
        max_cost_microunits: 0,
        max_total_tokens: 0,
        fallback_model: None,
        alert_percent: 80,
        over_limit_action: "pause".into(),
        created_at: 0,
        updated_at: 0,
    }
}

fn validate_policy(input: &DevelopmentPolicyInput) -> Result<(), DevelopmentError> {
    if !matches!(input.isolation_mode.as_str(), "host" | "docker" | "devcontainer") {
        return Err(DevelopmentError::BadRequest("unsupported isolation_mode".into()));
    }
    if input.isolation_mode == "docker" && clean_optional(input.container_image.clone()).is_none() {
        return Err(DevelopmentError::BadRequest(
            "docker isolation requires container_image".into(),
        ));
    }
    if input.isolation_mode == "devcontainer" {
        let path = clean_optional(input.devcontainer_config_path.clone())
            .ok_or_else(|| DevelopmentError::BadRequest("devcontainer isolation requires config path".into()))?;
        let candidate = Path::new(&path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|component| component == Component::ParentDir)
            || candidate.extension().and_then(|value| value.to_str()) != Some("json")
        {
            return Err(DevelopmentError::BadRequest(
                "devcontainer config path must be a relative JSON path inside the project".into(),
            ));
        }
    }
    if !matches!(input.network_mode.as_str(), "none" | "bridge") {
        return Err(DevelopmentError::BadRequest(
            "network_mode must be none or bridge".into(),
        ));
    }
    if !(100..=64_000).contains(&input.container_cpu_millis)
        || !(128..=262_144).contains(&input.container_memory_mb)
        || !(16..=32_768).contains(&input.container_pids_limit)
        || input.max_duration_ms <= 0
        || !(1..=64).contains(&input.max_parallel_agents)
        || !(0..=100).contains(&input.max_retries)
        || input.max_cost_microunits < 0
        || input.max_total_tokens < 0
        || !(1..=2).contains(&input.dangerous_confirmation_count)
        || !(1..=100).contains(&input.alert_percent)
    {
        return Err(DevelopmentError::BadRequest("policy limits are out of range".into()));
    }
    if !matches!(
        input.over_limit_action.as_str(),
        "notify" | "pause" | "downgrade_model" | "terminate"
    ) {
        return Err(DevelopmentError::BadRequest("unsupported over_limit_action".into()));
    }
    if input.over_limit_action == "downgrade_model" && clean_optional(input.fallback_model.clone()).is_none() {
        return Err(DevelopmentError::BadRequest(
            "downgrade_model requires fallback_model".into(),
        ));
    }
    for key in &input.allowed_secret_keys {
        if !valid_env_key(key.trim()) {
            return Err(DevelopmentError::BadRequest(format!(
                "invalid Secret environment key: {key}"
            )));
        }
    }
    Ok(())
}

fn default_confirmation_count() -> i64 {
    2
}

fn normalized_values(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn json_array(values: &[String]) -> Result<String, DevelopmentError> {
    serde_json::to_string(values).map_err(|error| DevelopmentError::Internal(error.to_string()))
}

fn parse_string_array(value: &str) -> Result<Vec<String>, DevelopmentError> {
    serde_json::from_str(value).map_err(|_| DevelopmentError::Internal("invalid persisted policy rules".into()))
}

fn policy_operation_kind(operation: &PolicyOperation) -> &'static str {
    match operation {
        PolicyOperation::Command { .. } => "command",
        PolicyOperation::Path { .. } => "path",
        PolicyOperation::Network { .. } => "network",
        PolicyOperation::Git { .. } => "git",
        PolicyOperation::Deploy { .. } => "deploy",
        PolicyOperation::Delete { .. } => "delete",
    }
}

fn valid_env_key(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('_' | 'A'..='Z'))
        && chars.all(|character| matches!(character, '_' | 'A'..='Z' | '0'..='9'))
        && value.len() <= 128
}

fn budget_threshold_reached(policy: &DevelopmentPolicyRow, usage: &DevelopmentUsageSummary, duration_ms: i64) -> bool {
    let percentage =
        |used: i64, limit: i64| limit > 0 && used.saturating_mul(100) >= limit.saturating_mul(policy.alert_percent);
    percentage(duration_ms, policy.max_duration_ms) || percentage(usage.cost_microunits, policy.max_cost_microunits)
}

fn budget_action_can_transition(status: &str) -> bool {
    matches!(
        status,
        "preflight" | "running" | "waiting_approval" | "verifying" | "reviewing" | "rework"
    )
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

pub fn redact_sensitive(value: &str, secret_values: &[String]) -> String {
    crate::secrets::redact_text(value, secret_values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{
        IDevelopmentRepository, SqliteAgentWorkspaceLeaseRepository, SqliteDevelopmentOperationsRepository,
        SqliteDevelopmentRepository, SqliteProjectRepository, init_database_memory,
    };

    async fn budget_action_fixture() -> (DevelopmentOperationsService, Arc<SqliteDevelopmentRepository>) {
        let db = init_database_memory().await.expect("database");
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('budget-user', 'budget', '', 1, 1)",
        )
        .execute(db.pool())
        .await
        .expect("user");
        sqlx::query(
            "INSERT INTO projects \
             (id, user_id, name, local_path, project_type, created_at, updated_at) \
             VALUES ('budget-project', 'budget-user', 'Budget', '/tmp/budget-project', 'single', 1, 1)",
        )
        .execute(db.pool())
        .await
        .expect("project");
        let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
        development_repo
            .create_run(&DevelopmentRunRow {
                id: "budget-run".into(),
                user_id: "budget-user".into(),
                project_id: "budget-project".into(),
                team_id: None,
                source_channel: None,
                source_user_id: None,
                execution_mode: "single".into(),
                status: "running".into(),
                request_summary: "Budget race".into(),
                acceptance_criteria: "[]".into(),
                baseline_commit: None,
                integration_branch: None,
                started_at: Some(1),
                finished_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .expect("run");
        let service = DevelopmentOperationsService::new(
            Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone())),
            development_repo.clone(),
            Arc::new(SqliteProjectRepository::new(db.pool().clone())),
            Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
        );
        (service, development_repo)
    }

    #[tokio::test]
    async fn stale_budget_actions_do_not_reopen_a_terminal_run() {
        for action in ["pause", "terminate"] {
            let (service, repository) = budget_action_fixture().await;
            let stale = repository
                .get_run("budget-run", "budget-user")
                .await
                .expect("read stale run")
                .expect("run exists");
            repository
                .update_run_status("budget-run", "budget-user", "succeeded", Some(10))
                .await
                .expect("finish run concurrently");

            service
                .apply_budget_action(&stale, action)
                .await
                .expect("stale action is ignored");

            let current = repository
                .get_run("budget-run", "budget-user")
                .await
                .expect("read current run")
                .expect("run exists");
            assert_eq!(current.status, "succeeded");
            assert_eq!(current.finished_at, Some(10));
        }
    }

    #[tokio::test]
    async fn stale_runtime_failure_pause_does_not_reopen_a_terminal_run() {
        let (service, repository) = budget_action_fixture().await;
        let stale = repository
            .get_run("budget-run", "budget-user")
            .await
            .expect("read stale run")
            .expect("run exists");
        repository
            .update_run_status("budget-run", "budget-user", "cancelled", Some(20))
            .await
            .expect("cancel run concurrently");

        service
            .pause_run_if_snapshot_current(&stale)
            .await
            .expect("stale pause is ignored");

        let current = repository
            .get_run("budget-run", "budget-user")
            .await
            .expect("read current run")
            .expect("run exists");
        assert_eq!(current.status, "cancelled");
        assert_eq!(current.finished_at, Some(20));
    }
}
