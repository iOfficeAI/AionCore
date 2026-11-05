#![allow(clippy::disallowed_types)]

//! HTTP routes for `/api/agent-center/*` (CSBU WorkMate 智能体中心).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    AdvanceAgentWorkflowRunRequest, AgentCenterDetailResponse, AgentCenterListItem, AgentCenterListQuery,
    AgentCenterRevisionResponse, AgentCenterRunPlanResponse, AgentWorkflowRunResponse, ApiResponse,
    CreateAgentCenterRequest, DecideAgentWorkflowApprovalRequest, PublishAgentCenterRequest,
    StartAgentWorkflowRunRequest, UpdateAgentCenterRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::GovernanceService;

use crate::agent_center_service::AgentCenterService;

#[derive(Clone)]
pub struct AgentCenterRouterState {
    pub service: Arc<AgentCenterService>,
    pub agent_execution_canceller: Option<Arc<dyn AgentWorkflowAgentExecutionCancellationPort>>,
    /// Optional governance for M9 E12 member catalog filtering.
    pub governance: Option<Arc<GovernanceService>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWorkflowAgentCancellationOutcome {
    NotRunning,
    Requested,
}

#[async_trait::async_trait]
pub trait AgentWorkflowAgentExecutionCancellationPort: Send + Sync {
    async fn cancel_agent_execution(
        &self,
        user_id: &str,
        run_id: &str,
        execution_id: &str,
        conversation_id: &str,
    ) -> Result<AgentWorkflowAgentCancellationOutcome, String>;
}

pub fn agent_center_routes(state: AgentCenterRouterState) -> Router {
    Router::new()
        .route("/api/agent-center/agents", get(list_agents).post(create_agent))
        .route("/api/agent-center/agents/{id}", get(get_agent).put(update_agent))
        .route("/api/agent-center/agents/{id}/publish", post(publish_agent))
        .route("/api/agent-center/agents/{id}/unpublish", post(unpublish_agent))
        .route("/api/agent-center/agents/{id}/versions", get(list_versions))
        .route("/api/agent-center/agents/{id}/run", post(run_agent))
        .route(
            "/api/agent-center/agents/{id}/workflow-runs",
            get(list_workflow_runs).post(start_workflow_run),
        )
        .route("/api/agent-center/workflow-runs/{id}", get(get_workflow_run))
        .route(
            "/api/agent-center/workflow-runs/{id}/advance",
            post(advance_workflow_run),
        )
        .route(
            "/api/agent-center/workflow-runs/{id}/approval",
            post(decide_workflow_approval),
        )
        .route("/api/agent-center/workflow-runs/{id}/cancel", post(cancel_workflow_run))
        .route("/api/agent-center/workflow-runs/{id}/retry", post(retry_workflow_run))
        .with_state(state)
}

async fn filter_agent_center_items(
    governance: Option<&Arc<GovernanceService>>,
    user_id: &str,
    items: Vec<AgentCenterListItem>,
) -> Result<Vec<AgentCenterListItem>, ApiError> {
    let Some(gov) = governance else {
        return Ok(items);
    };
    let active = gov.catalog_filter_active(user_id).await.map_err(|err| {
        tracing::warn!(error = %err, "governance catalog filter failed; returning empty");
        ApiError::Internal("Governance policy could not be read; try again.".into())
    })?;
    if !active {
        return Ok(items);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let agent = item.assistant.agent.as_ref();
        let allowed = gov
            .may_see_assistant(
                user_id,
                &item.assistant.id,
                agent.map(|a| a.r#type),
                agent.and_then(|a| a.acp_backend.as_deref()),
            )
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "governance catalog filter failed; returning empty");
                ApiError::Internal("Governance policy could not be read; try again.".into())
            })?;
        if allowed {
            out.push(item);
        }
    }
    Ok(out)
}

async fn ensure_may_see_assistant(
    governance: Option<&Arc<GovernanceService>>,
    user_id: &str,
    assistant_id: &str,
    agent_type: Option<aionui_common::AgentType>,
    backend: Option<&str>,
) -> Result<(), ApiError> {
    let Some(gov) = governance else {
        return Ok(());
    };
    let allowed = gov
        .may_see_assistant(user_id, assistant_id, agent_type, backend)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "governance catalog check failed");
            ApiError::Internal("Governance policy could not be read; try again.".into())
        })?;
    if allowed {
        return Ok(());
    }
    let role = gov.role_of(user_id).await.map_err(|err| {
        tracing::warn!(error = %err, "governance role lookup failed");
        ApiError::Internal("Governance policy could not be read; try again.".into())
    })?;
    Err(ApiError::Forbidden(gov.catalog_deny_message(
        role,
        "assistant",
        assistant_id,
    )))
}

async fn list_agents(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Query(query): Query<AgentCenterListQuery>,
) -> Result<Json<ApiResponse<Vec<AgentCenterListItem>>>, ApiError> {
    let items = state
        .service
        .list_for_user(&current_user.id, &query.scope, query.team_id.as_deref())
        .await?;
    let items = match filter_agent_center_items(state.governance.as_ref(), &current_user.id, items).await {
        Ok(items) => items,
        Err(ApiError::Internal(_)) => {
            // D12-p2-4: fail closed to empty catalog for members on storage errors.
            Vec::new()
        }
        Err(other) => return Err(other),
    };
    Ok(Json(ApiResponse::ok(items)))
}

async fn create_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentCenterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentCenterDetailResponse>>), ApiError> {
    let Json(req) = body?;
    let created = state.service.create_for_user(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn get_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let detail = state.service.get_detail_for_user(&current_user.id, &id, None).await?;
    let agent = detail.assistant.engine.agent.as_ref();
    ensure_may_see_assistant(
        state.governance.as_ref(),
        &current_user.id,
        &detail.assistant.id,
        agent.map(|a| a.r#type),
        agent.and_then(|a| a.acp_backend.as_deref()),
    )
    .await?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn update_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let Json(req) = body?;
    let updated = state.service.update_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn publish_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PublishAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => PublishAgentCenterRequest::default(),
    };
    let published = state.service.publish_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(published)))
}

async fn unpublish_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let draft = state.service.unpublish_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(draft)))
}

async fn list_versions(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AgentCenterRevisionResponse>>>, ApiError> {
    let versions = state.service.list_versions_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(versions)))
}

async fn run_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterRunPlanResponse>>, ApiError> {
    let plan = state.service.run_plan_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(plan)))
}

async fn start_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<StartAgentWorkflowRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentWorkflowRunResponse>>), ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .start_workflow_run_for_user(&current_user.id, &id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(run))))
}

async fn list_workflow_runs(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AgentWorkflowRunResponse>>>, ApiError> {
    let runs = state.service.list_workflow_runs_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(runs)))
}

async fn get_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let run = state.service.get_workflow_run_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(run)))
}

async fn advance_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AdvanceAgentWorkflowRunRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .advance_workflow_run_for_user(&current_user.id, &id, req)
        .await?;
    Ok(Json(ApiResponse::ok(run)))
}

async fn decide_workflow_approval(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<DecideAgentWorkflowApprovalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .decide_workflow_approval_for_user(&current_user.id, &id, req)
        .await?;
    dispatch_pending_tool_execution(&state.service, &current_user.id, &run);
    Ok(Json(ApiResponse::ok(run)))
}

async fn cancel_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let mut run = state
        .service
        .cancel_workflow_run_for_user(&current_user.id, &id)
        .await?;
    let execution = run.nodes.get(run.current_node_index).and_then(|node| {
        if node.kind != "agent" {
            return None;
        }
        Some((node.execution_id.clone()?, node.conversation_id.clone()?))
    });
    if let Some((execution_id, conversation_id)) = execution {
        let outcome = match state.agent_execution_canceller.as_ref() {
            Some(canceller) => {
                canceller
                    .cancel_agent_execution(&current_user.id, &run.id, &execution_id, &conversation_id)
                    .await
            }
            None => Err("agent execution cancellation is unavailable".to_owned()),
        };
        run = match outcome {
            Ok(AgentWorkflowAgentCancellationOutcome::NotRunning) => {
                state
                    .service
                    .confirm_agent_cancellation_for_user(&current_user.id, &run.id, &execution_id, &conversation_id)
                    .await?
            }
            Ok(AgentWorkflowAgentCancellationOutcome::Requested) => run,
            Err(error) => {
                tracing::error!(
                    user_id = %current_user.id,
                    run_id = %run.id,
                    execution_id,
                    conversation_id,
                    error,
                    "agent-workflow: failed to cancel conversation-backed agent execution"
                );
                state
                    .service
                    .fail_agent_cancellation_for_user(&current_user.id, &run.id, &execution_id, &conversation_id)
                    .await?
            }
        };
    }
    Ok(Json(ApiResponse::ok(run)))
}

async fn retry_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let run = state.service.retry_workflow_run_for_user(&current_user.id, &id).await?;
    dispatch_pending_tool_execution(&state.service, &current_user.id, &run);
    Ok(Json(ApiResponse::ok(run)))
}

fn dispatch_pending_tool_execution(service: &Arc<AgentCenterService>, user_id: &str, run: &AgentWorkflowRunResponse) {
    if !matches!(
        run.next_action.as_ref(),
        Some(aionui_api_types::AgentWorkflowNextAction::InvokeTool { .. })
    ) {
        return;
    }
    let service = service.clone();
    let user_id = user_id.to_owned();
    let run_id = run.id.clone();
    tokio::spawn(async move {
        if let Err(error) = service.execute_pending_tools_for_user(&user_id, &run_id).await {
            tracing::warn!(user_id, run_id, error = %error, "agent-workflow: background MCP tool execution failed");
        }
    });
}
