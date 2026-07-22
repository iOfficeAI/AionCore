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
const LEASE_TOKEN_HEADER: &str = "x-memory-lease-token";

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
    let claimed = state
        .service
        .claim_job(&user.id, &request.worker_id, request.lease_duration_ms)
        .await?;
    let (job, lease_token) = claimed
        .map(|claimed| (Some(claimed.job), Some(claimed.lease_token)))
        .unwrap_or((None, None));
    Ok(Json(ApiResponse::ok(ClaimMemoryJobResponse { job, lease_token })))
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
        .renew_job_lease(
            &user.id,
            &id,
            &request.worker_id,
            &request.lease_token,
            request.lease_duration_ms,
        )
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
    let released = state
        .service
        .release_job(&user.id, &id, &request.worker_id, &request.lease_token)
        .await?;
    Ok(Json(ApiResponse::ok(ReleaseMemoryJobLeaseResponse { released })))
}

async fn evidence(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<MemoryJobEvidenceResponse>>, ApiError> {
    let worker_id = worker_id(&headers)?;
    let lease_token = lease_token(&headers)?;
    let job = state.service.get_job(&user.id, &id).await?;
    if job.lease_owner.as_deref() != Some(worker_id) {
        return Err(MemoryError::LeaseLost.into());
    }
    let input = state.service.load_job_evidence(&user.id, &id, lease_token).await?;
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
        .record_job_failure(&user.id, &id, worker_id, &request.lease_token, request.failure)
        .await?;
    Ok(Json(ApiResponse::ok(RecordMemoryJobFailureResponse { job })))
}

fn lease_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(LEASE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid memory lease token".into()))
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
    use aionui_db::models::{ConversationRow, MessageRow};
    use aionui_db::{
        IConversationRepository, IMemoryRepository, SqliteConversationRepository, SqliteMemoryRepository,
        UpdateMemorySettingsRow, init_database_memory,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::memory_routes;
    use crate::service::MAX_LEASE_DURATION_MS;
    use crate::{AppOperationsReadinessPort, MemoryError, MemoryRouterState, MemoryService, MemoryTurnOutcome};

    struct UsableReadiness;

    #[async_trait::async_trait]
    impl AppOperationsReadinessPort for UsableReadiness {
        async fn is_usable(&self) -> Result<bool, MemoryError> {
            Ok(true)
        }
    }

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

    #[tokio::test]
    async fn internal_routes_reject_zero_and_overlong_leases_before_accessing_dependencies() {
        let router = memory_routes(MemoryRouterState {
            service: Arc::new(MemoryService::new()),
        });
        for lease_duration_ms in [0, MAX_LEASE_DURATION_MS + 1] {
            let mut claim = Request::post("/api/memory/internal/jobs/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "worker_id": "worker-1",
                        "lease_duration_ms": lease_duration_ms,
                    })
                    .to_string(),
                ))
                .unwrap();
            claim.extensions_mut().insert(current_user());
            assert_eq!(
                router.clone().oneshot(claim).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
            );

            let mut renew = Request::post("/api/memory/internal/jobs/job-1/lease")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "worker_id": "worker-1",
                        "lease_token": "lease-1",
                        "lease_duration_ms": lease_duration_ms,
                    })
                    .to_string(),
                ))
                .unwrap();
            renew.extensions_mut().insert(current_user());
            assert_eq!(
                router.clone().oneshot(renew).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
            );
        }
    }

    #[tokio::test]
    async fn internal_routes_accept_the_current_token_and_reject_spoofed_or_cross_user_access() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "conversation-1".into(),
                user_id: "system_default_user".into(),
                name: "Conversation".into(),
                r#type: "gemini".into(),
                extra: "{}".into(),
                model: None,
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .await
            .unwrap();
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: "system_default_user".into(),
                enabled: Some(true),
                default_capture: Some(true),
                default_recall: None,
                consent_version: Some(1),
                now: 1,
            })
            .await
            .unwrap();
        for (id, position, content, created_at) in [
            ("user", "right", "Do the work", 10),
            ("assistant", "left", "Work completed", 11),
        ] {
            conversations
                .insert_message(&MessageRow {
                    id: id.into(),
                    conversation_id: "conversation-1".into(),
                    turn_id: Some("turn-1".into()),
                    msg_id: None,
                    r#type: "text".into(),
                    content: serde_json::json!({ "content": content }).to_string(),
                    position: Some(position.into()),
                    status: Some("finish".into()),
                    hidden: false,
                    created_at,
                })
                .await
                .unwrap();
        }
        let service = Arc::new(MemoryService::with_job_dependencies(
            memory,
            conversations,
            Arc::new(UsableReadiness),
        ));
        service
            .on_turn_completed(
                "system_default_user",
                "conversation-1",
                "turn-1",
                MemoryTurnOutcome::Completed,
            )
            .await;
        let claimed = service
            .claim_job("system_default_user", "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        let router = memory_routes(MemoryRouterState { service });

        let evidence_request = |user_id: &str, token: &str| {
            let mut request = Request::get(format!("/api/memory/internal/jobs/{}/evidence", claimed.id))
                .header("x-memory-worker-id", "worker-1")
                .header("x-memory-lease-token", token)
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(CurrentUser {
                id: user_id.into(),
                username: "user".into(),
            });
            request
        };
        assert_eq!(
            router
                .clone()
                .oneshot(evidence_request("system_default_user", &claimed.lease_token))
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
        );
        assert_eq!(
            router
                .clone()
                .oneshot(evidence_request("system_default_user", "spoofed-token"))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT,
        );
        assert_eq!(
            router
                .oneshot(evidence_request("another-user", &claimed.lease_token))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
        );
    }

    fn current_user() -> CurrentUser {
        CurrentUser {
            id: "system_default_user".into(),
            username: "user".into(),
        }
    }
}
