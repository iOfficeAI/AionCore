#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, ClaimMemoryJobRequest, ClaimMemoryJobResponse, CompleteMemoryJobRequest, MemoryJobEvidenceResponse,
    RecordMemoryJobFailureRequest, RecordMemoryJobFailureResponse, ReleaseMemoryJobLeaseRequest,
    ReleaseMemoryJobLeaseResponse, RenewMemoryJobLeaseRequest, RenewMemoryJobLeaseResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::MemoryError;
pub use crate::state::MemoryRouterState;

const WORKER_ID_HEADER: &str = "x-memory-worker-id";

pub fn memory_routes(state: MemoryRouterState) -> Router {
    Router::new()
        .route("/api/memory/internal/jobs/claim", post(claim))
        .route("/api/memory/internal/jobs/{id}/lease", post(renew_lease))
        .route("/api/memory/internal/jobs/{id}/release", post(release))
        .route("/api/memory/internal/jobs/{id}/evidence", get(evidence))
        .route("/api/memory/internal/jobs/{id}/complete", post(complete))
        .route("/api/memory/internal/jobs/{id}/fail", post(fail))
        .with_state(state)
}

async fn claim(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ClaimMemoryJobRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClaimMemoryJobResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let job = state
        .service
        .claim_job(&user.id, &request.worker_id, request.lease_duration_ms)
        .await?;
    Ok(Json(ApiResponse::ok(ClaimMemoryJobResponse { job })))
}

async fn renew_lease(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<RenewMemoryJobLeaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RenewMemoryJobLeaseResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let lease_expires_at = state
        .service
        .renew_job_lease(&user.id, &id, &request.worker_id, request.lease_duration_ms)
        .await?;
    Ok(Json(ApiResponse::ok(RenewMemoryJobLeaseResponse { lease_expires_at })))
}

async fn release(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ReleaseMemoryJobLeaseRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ReleaseMemoryJobLeaseResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let released = state.service.release_job(&user.id, &id, &request.worker_id).await?;
    Ok(Json(ApiResponse::ok(ReleaseMemoryJobLeaseResponse { released })))
}

async fn evidence(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<MemoryJobEvidenceResponse>>, ApiError> {
    let worker_id = worker_id(&headers)?;
    let input = state.service.load_job_evidence(&user.id, &id, worker_id).await?;
    let job = state.service.get_job(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(MemoryJobEvidenceResponse { job, input })))
}

async fn complete(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CompleteMemoryJobRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let worker_id = worker_id(&headers)?;
    let Json(request) = body.map_err(ApiError::from)?;
    state.service.complete_job(&user.id, &id, worker_id, request).await?;
    Ok(Json(ApiResponse::success()))
}

async fn fail(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<RecordMemoryJobFailureRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RecordMemoryJobFailureResponse>>, ApiError> {
    let worker_id = worker_id(&headers)?;
    let Json(request) = body.map_err(ApiError::from)?;
    let job = state
        .service
        .record_job_failure(&user.id, &id, worker_id, request.failure)
        .await?;
    Ok(Json(ApiResponse::ok(RecordMemoryJobFailureResponse { job })))
}

fn worker_id(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(WORKER_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid memory worker identity".into()))
}

impl From<MemoryError> for ApiError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::NotFound => Self::NotFound(error.to_string()),
            MemoryError::Forbidden => Self::Forbidden(error.to_string()),
            MemoryError::InvalidInput => Self::BadRequest(error.to_string()),
            MemoryError::LeaseLost | MemoryError::StaleRevision | MemoryError::Conflict => {
                Self::Conflict(error.to_string())
            }
            MemoryError::Internal => Self::Internal(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_auth::CurrentUser;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::memory_routes;
    use crate::{MemoryRouterState, MemoryService};

    #[tokio::test]
    async fn internal_routes_require_normalized_failure_codes_and_worker_identity() {
        let router = memory_routes(MemoryRouterState {
            service: Arc::new(MemoryService::new()),
        });

        let mut evidence = Request::get("/api/memory/internal/jobs/job-1/evidence")
            .body(Body::empty())
            .unwrap();
        evidence.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(evidence).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let mut failure = Request::post("/api/memory/internal/jobs/job-1/fail")
            .header("content-type", "application/json")
            .header("x-memory-worker-id", "worker-1")
            .body(Body::from(r#"{"failure":{"code":"provider_error","message":"raw"}}"#))
            .unwrap();
        failure.extensions_mut().insert(current_user());
        assert_eq!(router.oneshot(failure).await.unwrap().status(), StatusCode::BAD_REQUEST);
    }

    fn current_user() -> CurrentUser {
        CurrentUser {
            id: "system_default_user".into(),
            username: "user".into(),
        }
    }
}
