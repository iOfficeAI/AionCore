#![allow(clippy::disallowed_types)] // HTTP boundary maps crate-owned errors to the shared API response.

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, DevelopmentConfirmationRequest, DevelopmentDeploymentRequest, DevelopmentRunControlRequest,
    DevelopmentRunControlState, DevelopmentRunTimeline, DevelopmentTagRequest, DevelopmentTimelineEvent,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::{IApprovalRepository, IDevelopmentOperationsRepository, IDevelopmentRepository};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::delivery::{CreatePullRequestInput, CreateTagInput, DeliveryService, PrepareDeliveryInput};
use crate::deployment::{DeploymentRequestInput, DeploymentService};
use crate::error::DevelopmentError;
use crate::operations::{
    DevelopmentOperationsService, DevelopmentOperationsSnapshot, DevelopmentPolicyInput, RecoveryDecisionInput,
};
use crate::pricing::PricingService;
use crate::secrets::{SecretCreateInput, SecretGrantInput, SecretService};
use crate::service::{CompletionEvaluation, DevelopmentService};
use crate::types::{
    AppendPlanRevisionInput, AppendRequirementRevisionInput, AssignDevelopmentRoleInput, CompletionEvidenceInput,
    CreateArtifactInput, CreateDevelopmentRunInput, CreateDevelopmentTaskInput, ExecuteQualityGateInput,
    ResolveFindingInput, SubmitReviewInput, TransitionDevelopmentTaskInput,
};

impl From<DevelopmentError> for ApiError {
    fn from(value: DevelopmentError) -> Self {
        match value {
            DevelopmentError::BadRequest(message) => Self::BadRequest(message),
            DevelopmentError::NotFound(message) => Self::NotFound(message),
            DevelopmentError::Conflict(message) => Self::Conflict(message),
            DevelopmentError::Internal(_) => Self::Internal("Development operation failed".into()),
        }
    }
}

#[derive(Clone)]
pub struct DevelopmentRouterState {
    pub service: Arc<DevelopmentService>,
    pub delivery_service: Arc<DeliveryService>,
    pub deployment_service: Arc<DeploymentService>,
    pub operations_service: Arc<DevelopmentOperationsService>,
    pub secret_service: Arc<SecretService>,
    pub pricing_service: Arc<PricingService>,
    pub development_repo: Arc<dyn IDevelopmentRepository>,
    pub operations_repo: Arc<dyn IDevelopmentOperationsRepository>,
    pub approval_repo: Arc<dyn IApprovalRepository>,
}

#[derive(Debug, Default, Deserialize)]
struct RunListQuery {
    project_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EvidenceQuery {
    task_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OperationsQuery {
    run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReconcileInput {
    #[serde(default = "default_stale_after_ms")]
    stale_after_ms: i64,
}

#[derive(Debug, Deserialize)]
struct RecoverDeploymentsInput {
    #[serde(default = "default_stale_after_ms")]
    stale_after_ms: i64,
}

fn default_stale_after_ms() -> i64 {
    30 * 60 * 1000
}

#[derive(Debug, Deserialize)]
struct ConfirmedInput {
    #[serde(default)]
    confirmation_count: u8,
}

pub fn development_routes(state: DevelopmentRouterState) -> Router {
    Router::new()
        .route("/api/development-runs", post(create_run).get(list_runs))
        .route("/api/development-runs/{run_id}", get(get_run))
        .route("/api/development-runs/{run_id}/timeline", get(get_timeline))
        .route("/api/development-runs/{run_id}/control", post(control_run))
        .route(
            "/api/development-runs/{run_id}/requirements",
            get(get_requirements).post(append_requirement_revision),
        )
        .route("/api/development-runs/{run_id}/plans", post(append_plan_revision))
        .route(
            "/api/development-runs/{run_id}/completion-evidence/{task_id}",
            post(record_completion_evidence),
        )
        .route("/api/development-runs/{run_id}/completion", post(complete_run))
        .route(
            "/api/development-runs/{run_id}/workspace",
            get(get_single_workspace).post(prepare_single_workspace),
        )
        .route(
            "/api/development-runs/{run_id}/workspace/cancel",
            post(cancel_single_workspace),
        )
        .route(
            "/api/development-runs/{run_id}/roles",
            post(assign_role).get(list_roles),
        )
        .route(
            "/api/development-runs/{run_id}/tasks",
            post(create_task).get(list_tasks),
        )
        .route(
            "/api/development-runs/{run_id}/tasks/{task_id}/completion",
            get(evaluate_completion).post(complete_task),
        )
        .route(
            "/api/development-runs/{run_id}/tasks/{task_id}/transition",
            post(transition_task),
        )
        .route(
            "/api/development-runs/{run_id}/artifacts",
            post(create_artifact).get(list_artifacts),
        )
        .route(
            "/api/development-runs/{run_id}/quality-gates",
            post(execute_gate).get(list_gates),
        )
        .route(
            "/api/development-runs/{run_id}/reviews",
            post(submit_review).get(list_findings),
        )
        .route(
            "/api/development-runs/{run_id}/findings/{finding_id}",
            post(resolve_finding),
        )
        .route("/api/development-runs/{run_id}/delivery", get(get_delivery))
        .route(
            "/api/development-runs/{run_id}/delivery/prepare",
            post(prepare_delivery),
        )
        .route("/api/development-runs/{run_id}/delivery/push", post(push_delivery))
        .route(
            "/api/development-runs/{run_id}/delivery/pull-request",
            post(create_pull_request),
        )
        .route("/api/development-runs/{run_id}/delivery/sync", post(sync_delivery))
        .route("/api/development-runs/{run_id}/delivery/merge", post(merge_delivery))
        .route("/api/development-runs/{run_id}/delivery/report", get(delivery_report))
        .route(
            "/api/development-runs/{run_id}/delivery/tags",
            get(list_delivery_tags).post(create_delivery_tag),
        )
        .route(
            "/api/development-runs/{run_id}/deployments",
            get(list_deployments).post(request_deployment),
        )
        .route(
            "/api/development-runs/{run_id}/deployments/recover",
            post(recover_deployments),
        )
        .route(
            "/api/development-runs/{run_id}/deployments/{deployment_id}/approve",
            post(approve_deployment),
        )
        .route(
            "/api/development-runs/{run_id}/deployments/{deployment_id}/execute",
            post(execute_deployment),
        )
        .route(
            "/api/development-runs/{run_id}/deployments/{deployment_id}/cancel",
            post(cancel_deployment),
        )
        .route(
            "/api/development-projects/{project_id}/operations/policy",
            get(get_operations_policy).put(update_operations_policy),
        )
        .route(
            "/api/development-projects/{project_id}/secrets",
            get(list_secrets).post(create_secret),
        )
        .route(
            "/api/development-projects/{project_id}/secrets/{secret_id}/grants",
            post(grant_secret),
        )
        .route(
            "/api/development-projects/{project_id}/secrets/{secret_id}/revoke",
            post(revoke_secret),
        )
        .route(
            "/api/development-projects/{project_id}/operations",
            get(get_operations_snapshot),
        )
        .route(
            "/api/development-projects/{project_id}/operations/alerts/{alert_id}/ack",
            post(acknowledge_operations_alert),
        )
        .route("/api/development-operations/reconcile", post(reconcile_operations))
        .route("/api/development-runs/{run_id}/recovery", post(decide_recovery))
        .with_state(state)
}

async fn get_timeline(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<DevelopmentRunTimeline>>, ApiError> {
    let run = state.service.get_run(&user.id, &run_id).await.map_err(ApiError::from)?;
    let tasks = state
        .service
        .list_tasks(&user.id, &run_id)
        .await
        .map_err(ApiError::from)?;
    let mut events = vec![DevelopmentTimelineEvent {
        id: format!("run:{}", run.id),
        kind: "run".into(),
        correlation_id: run.id.clone(),
        task_id: None,
        title: run.request_summary.clone(),
        status: run.status.clone(),
        actor_id: run.source_user_id.clone(),
        occurred_at: run.created_at,
        metadata: json!({"execution_mode": run.execution_mode}),
    }];
    for task in &tasks {
        events.push(timeline_event(
            format!("task:{}", task.id),
            "task",
            &run.id,
            Some(task.id.clone()),
            task.subject.clone(),
            task.status.clone(),
            task.owner.clone(),
            task.updated_at,
            json!({"blocked_by": task.blocked_by, "risk_level": task.risk_level}),
        ));
    }
    for artifact in state
        .development_repo
        .list_artifacts(&run_id, None)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("artifact:{}", artifact.id),
            "file",
            &run.id,
            artifact.task_id,
            artifact.artifact_type,
            "recorded".into(),
            artifact.producer_agent_id,
            artifact.created_at,
            json!({"path_or_uri": artifact.path_or_uri, "checksum": artifact.checksum}),
        ));
    }
    for gate in state
        .development_repo
        .list_gates(&run_id, None)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("gate:{}", gate.id),
            "gate",
            &run.id,
            gate.task_id,
            gate.gate_type,
            gate.status,
            None,
            gate.finished_at.or(gate.started_at).unwrap_or(gate.created_at),
            json!({"required": gate.required, "duration_ms": gate.duration_ms}),
        ));
    }
    for task in &tasks {
        for finding in state
            .development_repo
            .list_findings(&run_id, &task.id)
            .await
            .map_err(DevelopmentError::from)
            .map_err(ApiError::from)?
        {
            events.push(timeline_event(
                format!("finding:{}", finding.id),
                "finding",
                &run.id,
                Some(finding.task_id),
                finding.reason,
                finding.status,
                Some(finding.reviewer_agent_id),
                finding.updated_at,
                json!({"severity": finding.severity, "file_path": finding.file_path, "line_number": finding.line_number}),
            ));
        }
    }
    for approval in state
        .approval_repo
        .list_for_user(&user.id, Some(&run_id))
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("approval:{}", approval.id),
            "approval",
            &run.id,
            approval.task_id,
            approval.action_type,
            approval.status,
            approval.approver_user_id.or(Some(approval.requester_user_id)),
            approval.updated_at,
            json!({"risk_level": approval.risk_level, "expires_at": approval.expires_at}),
        ));
    }
    if let Some(delivery) = state
        .development_repo
        .get_delivery(&user.id, &run_id)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("commit:{}", delivery.id),
            "commit",
            &run.id,
            None,
            delivery.commit_sha.clone().unwrap_or_else(|| delivery.branch.clone()),
            delivery.status.clone(),
            None,
            delivery.updated_at,
            json!({"branch": delivery.branch, "base_branch": delivery.base_branch}),
        ));
        for check in state
            .development_repo
            .list_ci_checks(&delivery.id)
            .await
            .map_err(DevelopmentError::from)
            .map_err(ApiError::from)?
        {
            events.push(timeline_event(
                format!("ci:{}", check.id),
                "ci",
                &run.id,
                None,
                check.name,
                check.status,
                None,
                check.completed_at.or(check.started_at).unwrap_or(check.created_at),
                json!({"details_url": check.details_url, "rework_task_id": check.rework_task_id}),
            ));
        }
        for tag in state
            .development_repo
            .list_delivery_tags(&user.id, &delivery.id)
            .await
            .map_err(DevelopmentError::from)
            .map_err(ApiError::from)?
        {
            events.push(timeline_event(
                format!("tag:{}", tag.id),
                "commit",
                &run.id,
                None,
                tag.name,
                tag.status,
                None,
                tag.updated_at,
                json!({"commit_sha": tag.commit_sha, "remote_url": tag.remote_url}),
            ));
        }
    }
    for deployment in state
        .development_repo
        .list_deployments(&user.id, &run_id)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("deployment:{}", deployment.id),
            "deployment",
            &run.id,
            None,
            deployment.environment,
            deployment.status,
            deployment.approved_by.or(Some(deployment.requested_by)),
            deployment.updated_at,
            json!({"commit_sha": deployment.commit_sha, "remote_id": deployment.remote_id}),
        ));
    }
    for usage in state
        .operations_repo
        .list_usage(&user.id, &run.project_id, Some(&run_id), 200)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("usage:{}", usage.id),
            "usage",
            &run.id,
            usage.task_id,
            usage.usage_type,
            usage.confidence,
            Some(usage.source),
            usage.created_at,
            json!({"input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens, "cost_microunits": usage.cost_microunits, "duration_ms": usage.duration_ms}),
        ));
    }
    for audit in state
        .operations_repo
        .list_audit(&user.id, &run.project_id, Some(&run_id), 200)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        let kind = if audit.action.contains("turn") {
            "turn"
        } else if audit.action.contains("tool") || audit.action.contains("execute") {
            "tool"
        } else {
            "audit"
        };
        events.push(timeline_event(
            format!("audit:{}", audit.id),
            kind,
            &run.id,
            audit.task_id,
            audit.action,
            audit.result,
            Some(audit.actor_id),
            audit.created_at,
            serde_json::from_str(&audit.redacted_payload_json).unwrap_or(Value::Null),
        ));
    }
    for alert in state
        .operations_repo
        .list_alerts(&user.id, &run.project_id, Some(&run_id), false)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("alert:{}", alert.id),
            "alert",
            &run.id,
            None,
            alert.message,
            alert.status,
            None,
            alert.updated_at,
            json!({"alert_type": alert.alert_type, "severity": alert.severity}),
        ));
    }
    for recovery in state
        .operations_repo
        .list_recovery(&user.id, &run.project_id, Some(&run_id), 200)
        .await
        .map_err(DevelopmentError::from)
        .map_err(ApiError::from)?
    {
        events.push(timeline_event(
            format!("recovery:{}", recovery.id),
            "recovery",
            &run.id,
            None,
            recovery.finding,
            recovery.decision,
            None,
            recovery.created_at,
            serde_json::from_str(&recovery.details_json).unwrap_or(Value::Null),
        ));
    }
    events.sort_by(|left, right| {
        left.occurred_at
            .cmp(&right.occurred_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(Json(ApiResponse::ok(DevelopmentRunTimeline {
        run_id: run.id.clone(),
        controls: control_state(&run, &tasks),
        events,
    })))
}

async fn control_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Json(input): Json<DevelopmentRunControlRequest>,
) -> Result<Json<ApiResponse<DevelopmentRunControlState>>, ApiError> {
    let run = state.service.get_run(&user.id, &run_id).await.map_err(ApiError::from)?;
    let tasks = state
        .service
        .list_tasks(&user.id, &run_id)
        .await
        .map_err(ApiError::from)?;
    let controls = control_state(&run, &tasks);
    if let Some(task_id) = input.task_id.as_deref() {
        let allowed = controls.allowed_task_actions.get(task_id).cloned().unwrap_or_default();
        if !allowed.iter().any(|action| action == &input.action) {
            return Err(ApiError::Conflict(format!(
                "task action {} is not allowed",
                input.action
            )));
        }
        let task = tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| ApiError::NotFound(format!("task {task_id}")))?;
        if input.action == "reassign" {
            let target = input
                .target_slot_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| ApiError::BadRequest("target_slot_id is required".into()))?;
            let roles = state
                .service
                .list_roles(&user.id, &run_id)
                .await
                .map_err(ApiError::from)?;
            if !roles.iter().any(|role| role.slot_id == target) {
                return Err(ApiError::BadRequest(
                    "target_slot_id is not assigned to this run".into(),
                ));
            }
            state
                .development_repo
                .update_task_owner(&run_id, task_id, Some(target))
                .await
                .map_err(DevelopmentError::from)
                .map_err(ApiError::from)?;
        } else {
            let target = task_control_target(&input.action, &task.status)
                .ok_or_else(|| ApiError::Conflict("task action has no valid target state".into()))?;
            state
                .development_repo
                .update_task_state(&run_id, task_id, target, &task.review_status, &task.verification_status)
                .await
                .map_err(DevelopmentError::from)
                .map_err(ApiError::from)?;
        }
    } else {
        if !controls
            .allowed_run_actions
            .iter()
            .any(|action| action == &input.action)
        {
            return Err(ApiError::Conflict(format!(
                "run action {} is not allowed",
                input.action
            )));
        }
        let (status, finished_at) = match input.action.as_str() {
            "pause" => ("paused", None),
            "cancel" => ("cancelled", Some(aionui_common::now_ms())),
            "retry" | "takeover" => ("running", None),
            _ => return Err(ApiError::BadRequest("unsupported run action".into())),
        };
        state
            .development_repo
            .update_run_status(&run_id, &user.id, status, finished_at)
            .await
            .map_err(DevelopmentError::from)
            .map_err(ApiError::from)?;
    }
    let updated_run = state.service.get_run(&user.id, &run_id).await.map_err(ApiError::from)?;
    let updated_tasks = state
        .service
        .list_tasks(&user.id, &run_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(control_state(&updated_run, &updated_tasks))))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the helper mirrors the persisted timeline event schema so call sites keep every source field explicit"
)]
fn timeline_event(
    id: String,
    kind: &str,
    correlation_id: &str,
    task_id: Option<String>,
    title: String,
    status: String,
    actor_id: Option<String>,
    occurred_at: i64,
    metadata: Value,
) -> DevelopmentTimelineEvent {
    DevelopmentTimelineEvent {
        id,
        kind: kind.into(),
        correlation_id: correlation_id.into(),
        task_id,
        title,
        status,
        actor_id,
        occurred_at,
        metadata,
    }
}

fn control_state(
    run: &aionui_db::models::DevelopmentRunRow,
    tasks: &[aionui_db::models::DevelopmentTaskRow],
) -> DevelopmentRunControlState {
    let allowed_run_actions = match run.status.as_str() {
        "running" => vec!["pause", "cancel"],
        "paused" => vec!["retry", "takeover", "cancel"],
        "failed" => vec!["retry", "takeover"],
        _ => vec![],
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    let allowed_task_actions = tasks
        .iter()
        .map(|task| {
            let actions: Vec<&str> = match task.status.as_str() {
                "pending" | "ready" | "claimed" => vec!["advance", "reassign", "cancel"],
                "in_progress" => vec!["pause", "reassign", "cancel"],
                "waiting_approval" => vec!["retry", "reassign", "cancel"],
                "verifying" | "review" => vec!["rework", "reassign", "cancel"],
                "rework" | "failed" | "conflict" => vec!["retry", "rework", "reassign", "cancel"],
                _ => vec![],
            };
            (task.id.clone(), actions.into_iter().map(str::to_owned).collect())
        })
        .collect::<BTreeMap<_, _>>();
    DevelopmentRunControlState {
        run_id: run.id.clone(),
        run_status: run.status.clone(),
        allowed_run_actions,
        allowed_task_actions,
    }
}

fn task_control_target<'a>(action: &str, current: &'a str) -> Option<&'a str> {
    match action {
        "advance" => match current {
            "pending" => Some("ready"),
            "ready" => Some("claimed"),
            "claimed" => Some("in_progress"),
            _ => None,
        },
        "pause" => Some("waiting_approval"),
        "retry" => Some("in_progress"),
        "rework" => Some("rework"),
        "cancel" => Some("cancelled"),
        _ => None,
    }
}

async fn list_secrets(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<crate::secrets::SecretMetadata>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .secret_service
            .list(&user.id, &project_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_secret(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    body: Result<Json<aionui_api_types::DevelopmentSecretCreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<crate::secrets::SecretMetadata>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let secret = state
        .secret_service
        .create(
            &user.id,
            &project_id,
            SecretCreateInput {
                name: input.name,
                value: input.value,
                expires_at: input.expires_at,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(secret))))
}

async fn grant_secret(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, secret_id)): Path<(String, String)>,
    body: Result<Json<aionui_api_types::DevelopmentSecretGrantRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<crate::secrets::SecretGrantMetadata>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    if input.secret_id != secret_id {
        return Err(ApiError::BadRequest("Secret ID does not match route".into()));
    }
    let secrets = state
        .secret_service
        .list(&user.id, &project_id)
        .await
        .map_err(ApiError::from)?;
    if !secrets.iter().any(|secret| secret.id == secret_id) {
        return Err(ApiError::NotFound("Secret".into()));
    }
    Ok(Json(ApiResponse::ok(
        state
            .secret_service
            .grant(
                &user.id,
                SecretGrantInput {
                    secret_id,
                    scope_type: input.scope_type,
                    scope_id: input.scope_id,
                    environment_key: input.environment_key,
                    expires_at: input.expires_at,
                },
            )
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn revoke_secret(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, secret_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let secrets = state
        .secret_service
        .list(&user.id, &project_id)
        .await
        .map_err(ApiError::from)?;
    if !secrets.iter().any(|secret| secret.id == secret_id) {
        return Err(ApiError::NotFound("Secret".into()));
    }
    state
        .secret_service
        .revoke(&user.id, &secret_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(serde_json::json!({"revoked": true}))))
}

async fn get_operations_policy(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentPolicyRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .get_policy(&user.id, &project_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn update_operations_policy(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    body: Result<Json<DevelopmentPolicyInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentPolicyRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .upsert_policy(&user.id, &project_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_operations_snapshot(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    Query(query): Query<OperationsQuery>,
) -> Result<Json<ApiResponse<DevelopmentOperationsSnapshot>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .snapshot(&user.id, &project_id, query.run_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn acknowledge_operations_alert(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((project_id, alert_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    state
        .operations_service
        .get_policy(&user.id, &project_id)
        .await
        .map_err(ApiError::from)?;
    state
        .operations_service
        .acknowledge_alert(&user.id, &project_id, &alert_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(serde_json::json!({"acknowledged": true}))))
}

async fn reconcile_operations(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ReconcileInput>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRecoveryRecordRow>>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .reconcile_stale_runs_for_user(&user.id, input.stale_after_ms)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn decide_recovery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<RecoveryDecisionInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentRecoveryRecordRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .operations_service
            .decide_recovery(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .get(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn prepare_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<PrepareDeliveryInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .prepare(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn push_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ConfirmedInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .push(&user.id, &run_id, input.confirmation_count)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_pull_request(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreatePullRequestInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .create_pull_request(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn sync_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .sync(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn merge_delivery(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ConfirmedInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeliveryRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .merge(&user.id, &run_id, input.confirmation_count)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn delivery_report(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .report(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_delivery_tag(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<DevelopmentTagRequest>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<ApiResponse<aionui_db::models::DevelopmentDeliveryTagRow>>,
    ),
    ApiError,
> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .delivery_service
        .create_tag(
            &user.id,
            &run_id,
            CreateTagInput {
                name: input.name,
                commit_sha: input.commit_sha,
                confirmed: input.confirmed,
                confirmation_count: input.confirmation_count,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_delivery_tags(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentDeliveryTagRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .delivery_service
            .list_tags(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn request_deployment(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<DevelopmentDeploymentRequest>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<ApiResponse<aionui_db::models::DevelopmentDeploymentRow>>,
    ),
    ApiError,
> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .deployment_service
        .request(
            &user.id,
            &run_id,
            DeploymentRequestInput {
                environment: input.environment,
                deployment_key: input.deployment_key,
                commit_sha: input.commit_sha,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_deployments(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentDeploymentRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .deployment_service
            .list(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn approve_deployment(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, deployment_id)): Path<(String, String)>,
    body: Result<Json<DevelopmentConfirmationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeploymentRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .deployment_service
            .approve(&user.id, &run_id, &deployment_id, input.confirmation_count)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn execute_deployment(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, deployment_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeploymentRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .deployment_service
            .execute(&user.id, &run_id, &deployment_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn cancel_deployment(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, deployment_id)): Path<(String, String)>,
    body: Result<Json<DevelopmentConfirmationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentDeploymentRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .deployment_service
            .cancel(&user.id, &run_id, &deployment_id, input.confirmed)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn recover_deployments(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<RecoverDeploymentsInput>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentDeploymentRow>>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .deployment_service
            .recover_stale(&user.id, &run_id, input.stale_after_ms)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn assign_role(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<AssignDevelopmentRoleInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentRunRoleRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .assign_role(&user.id, &run_id, &input.slot_id, &input.role)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_roles(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRunRoleRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_roles(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateDevelopmentRunInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentRunRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_run(&user.id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_runs(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<RunListQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentRunRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_runs(&user.id, query.project_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentRunRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_run(&user.id, &run_id).await.map_err(ApiError::from)?,
    )))
}

async fn get_requirements(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_api_types::RequirementsSnapshot>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .requirements_snapshot(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn append_requirement_revision(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<AppendRequirementRevisionInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::RequirementVersion>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .append_requirement_revision(
            &user.id,
            &run_id,
            &input.content,
            &input.change_summary,
            input.acceptance_criteria,
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn append_plan_revision(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<AppendPlanRevisionInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::PlanRevision>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .append_plan_revision(&user.id, &run_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn record_completion_evidence(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<CompletionEvidenceInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::CompletionEvidenceRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .record_completion_evidence(&user.id, &run_id, &task_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn complete_run(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentRunRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .complete_run(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_single_workspace(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Option<aionui_api_types::SingleRunWorkspace>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_single_workspace(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn prepare_single_workspace(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::SingleRunWorkspace>>), ApiError> {
    let row = state
        .service
        .prepare_single_workspace(&user.id, &run_id)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn cancel_single_workspace(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<aionui_api_types::SingleRunWorkspace>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .cancel_single_workspace(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreateDevelopmentTaskInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_task(&user.id, &run_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_tasks(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::DevelopmentTaskRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_tasks(&user.id, &run_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn evaluate_completion(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<CompletionEvaluation>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .evaluate_completion(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn complete_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .complete_task(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn transition_task(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, task_id)): Path<(String, String)>,
    body: Result<Json<TransitionDevelopmentTaskInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .transition_task(&user.id, &run_id, &task_id, &input.status)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn create_artifact(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<CreateArtifactInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::TaskArtifactRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .create_artifact(&user.id, &run_id, input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_artifacts(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::TaskArtifactRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_artifacts(&user.id, &run_id, query.task_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn execute_gate(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<ExecuteQualityGateInput>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_db::models::QualityGateRunRow>>), ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    let row = state
        .service
        .execute_gate(
            &user.id,
            &run_id,
            input.task_id.as_deref(),
            &input.gate_type,
            input.workspace_lease_id.as_deref(),
            input.required,
        )
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(row))))
}

async fn list_gates(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::QualityGateRunRow>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_gates(&user.id, &run_id, query.task_id.as_deref())
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn submit_review(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    body: Result<Json<SubmitReviewInput>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_db::models::DevelopmentTaskRow>>, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .submit_review(&user.id, &run_id, input)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn list_findings(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
    Query(query): Query<EvidenceQuery>,
) -> Result<Json<ApiResponse<Vec<aionui_db::models::ReviewFindingRow>>>, ApiError> {
    let task_id = query
        .task_id
        .ok_or_else(|| ApiError::BadRequest("task_id is required".into()))?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .list_findings(&user.id, &run_id, &task_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn resolve_finding(
    State(state): State<DevelopmentRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((run_id, finding_id)): Path<(String, String)>,
    body: Result<Json<ResolveFindingInput>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(input) = body.map_err(ApiError::from)?;
    state
        .service
        .resolve_finding(&user.id, &run_id, &finding_id, &input.status)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}
