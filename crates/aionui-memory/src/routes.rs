#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};

use aionui_api_types::{
    ApiResponse, ClaimMemoryJobRequest, ClaimMemoryJobResponse, CompleteMemoryJobRequest, ConversationMemoryPolicy,
    CreateMemoryRetrievalRequest, DeleteMemoryEntryResponse, ListMemoryChangeSetsQuery, ListMemoryEntriesQuery,
    MemoryChangeSetListResponse, MemoryEntryListResponse, MemoryEntryResponse, MemoryEntryState,
    MemoryJobEvidenceResponse, MemoryRetrievalPreview, MemorySettings, MemoryStatus, RecordMemoryJobFailureRequest,
    RecordMemoryJobFailureResponse, ReleaseMemoryJobLeaseRequest, ReleaseMemoryJobLeaseResponse,
    RenewMemoryJobLeaseRequest, RenewMemoryJobLeaseResponse, ResolveMemoryEntryConflictRequest,
    ResolveMemoryEntryConflictResponse, RetryMemoryJobResponse, UpdateConversationMemoryPolicyRequest,
    UpdateMemoryEntryRequest, UpdateMemorySettingsRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::MemoryError;
pub use crate::state::MemoryRouterState;

const WORKER_ID_HEADER: &str = "x-memory-worker-id";
const LEASE_TOKEN_HEADER: &str = "x-memory-lease-token";

pub fn memory_routes(state: MemoryRouterState) -> Router {
    Router::new()
        .route("/api/memory/settings", get(get_settings).put(update_settings))
        .route("/api/memory/status", get(status))
        .route("/api/memory/entries", get(list_entries))
        .route(
            "/api/memory/entries/{id}",
            axum::routing::patch(update_entry).delete(delete_entry),
        )
        .route("/api/memory/entries/{id}/resolve-conflict", post(resolve_conflict))
        .route("/api/memory/change-sets", get(list_change_sets))
        .route("/api/memory/retrievals", post(create_retrieval))
        .route(
            "/api/conversations/{id}/memory-policy",
            get(get_conversation_policy).put(update_conversation_policy),
        )
        .route("/api/memory/conversations/{id}", delete(forget_conversation))
        .route("/api/memory", delete(clear_memory))
        .route("/api/memory/jobs/{id}/retry", post(retry_job))
        .route("/api/memory/internal/jobs/claim", post(claim))
        .route("/api/memory/internal/jobs/{id}/lease", post(renew_lease))
        .route("/api/memory/internal/jobs/{id}/release", post(release))
        .route("/api/memory/internal/jobs/{id}/evidence", get(evidence))
        .route("/api/memory/internal/jobs/{id}/complete", post(complete))
        .route("/api/memory/internal/jobs/{id}/fail", post(fail))
        .with_state(state)
}

async fn create_retrieval(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateMemoryRetrievalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MemoryRetrievalPreview>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .create_retrieval(&user.id, &request.conversation_id, &request.prompt)
            .await?,
    )))
}

async fn get_settings(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<MemorySettings>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.get_settings(&user.id).await?)))
}

async fn update_settings(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UpdateMemorySettingsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MemorySettings>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.update_settings(&user.id, request).await?,
    )))
}

async fn status(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<MemoryStatus>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.status(&user.id).await?)))
}

async fn list_entries(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    query: Result<Query<ListMemoryEntriesQuery>, QueryRejection>,
) -> Result<Json<ApiResponse<MemoryEntryListResponse>>, ApiError> {
    let Query(query) = query.map_err(|_| MemoryError::InvalidInput)?;
    Ok(Json(ApiResponse::ok(
        state.service.list_entries(&user.id, query).await?,
    )))
}

async fn update_entry(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateMemoryEntryRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MemoryEntryResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.update_entry(&user.id, &id, request).await?,
    )))
}

async fn delete_entry(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteMemoryEntryResponse>>, ApiError> {
    state.service.delete_entry(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(DeleteMemoryEntryResponse {
        id,
        state: MemoryEntryState::Deleted,
    })))
}

async fn resolve_conflict(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ResolveMemoryEntryConflictRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ResolveMemoryEntryConflictResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.resolve_conflict(&user.id, &id, request).await?,
    )))
}

async fn list_change_sets(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    query: Result<Query<ListMemoryChangeSetsQuery>, QueryRejection>,
) -> Result<Json<ApiResponse<MemoryChangeSetListResponse>>, ApiError> {
    let Query(query) = query.map_err(|_| MemoryError::InvalidInput)?;
    Ok(Json(ApiResponse::ok(
        state.service.list_change_sets(&user.id, query).await?,
    )))
}

async fn get_conversation_policy(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationMemoryPolicy>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_conversation_policy(&user.id, &id).await?,
    )))
}

async fn update_conversation_policy(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationMemoryPolicyRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationMemoryPolicy>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.update_conversation_policy(&user.id, &id, request).await?,
    )))
}

async fn forget_conversation(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.forget_conversation(&user.id, &id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn clear_memory(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.clear_all_memory(&user.id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn retry_job(
    State(state): State<MemoryRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RetryMemoryJobResponse>>, ApiError> {
    let job = state.service.retry_failed_job(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(RetryMemoryJobResponse { job })))
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
    let Json(request) = match body {
        Ok(request) => request,
        Err(_) => {
            let lease_token = lease_token(&headers)?;
            state
                .service
                .record_malformed_completion(&user.id, &id, worker_id, lease_token)
                .await?;
            return Err(MemoryError::InvalidInput.into());
        }
    };
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
    async fn retrieval_route_uses_authenticated_owner_and_rejects_invalid_input() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "conversation-retrieval".into(),
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
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: "system_default_user".into(),
                enabled: Some(true),
                default_capture: None,
                default_recall: Some(true),
                consent_version: Some(1),
                now: 1,
            })
            .await
            .unwrap();
        let router = memory_routes(MemoryRouterState {
            service: Arc::new(MemoryService::with_job_dependencies(
                memory,
                conversations,
                Arc::new(UsableReadiness),
            )),
        });
        let request = |user_id: &str, body: &'static str| {
            let mut request = Request::post("/api/memory/retrievals")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            request.extensions_mut().insert(CurrentUser {
                id: user_id.into(),
                username: "user".into(),
            });
            request
        };
        let response = router
            .clone()
            .oneshot(request(
                "system_default_user",
                r#"{"conversation_id":"conversation-retrieval","prompt":"current work"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["conversation_id"], "conversation-retrieval");
        assert_eq!(json["data"]["entries"], serde_json::json!([]));

        assert_eq!(
            router
                .clone()
                .oneshot(request(
                    "system_default_user",
                    r#"{"conversation_id":"conversation-retrieval","prompt":" "}"#,
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            router
                .oneshot(request(
                    "another-user",
                    r#"{"conversation_id":"conversation-retrieval","prompt":"current work"}"#,
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
        );
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
            memory.clone(),
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

    #[tokio::test]
    async fn malformed_completion_envelopes_consume_the_invalid_output_retry_budget() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "conversation-malformed".into(),
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
            ("malformed-user", "right", "Do the work", 10),
            ("malformed-assistant", "left", "Work completed", 11),
        ] {
            conversations
                .insert_message(&MessageRow {
                    id: id.into(),
                    conversation_id: "conversation-malformed".into(),
                    turn_id: Some("turn-malformed".into()),
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
            memory.clone(),
            conversations,
            Arc::new(UsableReadiness),
        ));
        service
            .on_turn_completed(
                "system_default_user",
                "conversation-malformed",
                "turn-malformed",
                MemoryTurnOutcome::Completed,
            )
            .await;
        let first = service
            .claim_job("system_default_user", "worker-malformed", 30_000)
            .await
            .unwrap()
            .unwrap();
        let router = memory_routes(MemoryRouterState {
            service: service.clone(),
        });

        let malformed_request = |lease_token: &str, body: &'static str| {
            let mut request = Request::post(format!("/api/memory/internal/jobs/{}/complete", first.id))
                .header("content-type", "application/json")
                .header("x-memory-worker-id", "worker-malformed")
                .header("x-memory-lease-token", lease_token)
                .body(Body::from(body))
                .unwrap();
            request.extensions_mut().insert(current_user());
            request
        };
        assert_eq!(
            router
                .clone()
                .oneshot(malformed_request(&first.lease_token, "{"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
        );
        let retry = memory.get_job("system_default_user", &first.id).await.unwrap().unwrap();
        assert_eq!(retry.state, "retry_wait");
        assert_eq!(retry.attempt_count, 1);
        assert_eq!(retry.invalid_output_count, 1);

        sqlx::query("UPDATE memory_jobs SET next_attempt_at = 0 WHERE id = ?")
            .bind(&first.id)
            .execute(db.pool())
            .await
            .unwrap();
        let second = service
            .claim_job("system_default_user", "worker-malformed", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            router
                .oneshot(malformed_request(
                    &second.lease_token,
                    r#"{"expected_revision":0,"lease_token":"unused","output":{"summary":{}}}"#,
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
        );
        let failed = memory.get_job("system_default_user", &first.id).await.unwrap().unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.attempt_count, 2);
        assert_eq!(failed.invalid_output_count, 2);
    }

    #[tokio::test]
    async fn public_settings_and_policy_routes_enforce_ownership_and_validate_filters() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "owned-conversation".into(),
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
        let service = Arc::new(MemoryService::with_job_dependencies(
            memory.clone(),
            conversations,
            Arc::new(UsableReadiness),
        ));
        let router = memory_routes(MemoryRouterState { service });

        let mut settings = Request::get("/api/memory/settings").body(Body::empty()).unwrap();
        settings.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(settings).await.unwrap().status(), StatusCode::OK);

        let mut invalid_consent = Request::put("/api/memory/settings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"consent_version":999}"#))
            .unwrap();
        invalid_consent.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(invalid_consent).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
        );
        let mut enable_without_consent = Request::put("/api/memory/settings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"enabled":true}"#))
            .unwrap();
        enable_without_consent.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(enable_without_consent).await.unwrap().status(),
            StatusCode::OK,
        );
        assert_eq!(
            memory
                .get_settings("system_default_user")
                .await
                .unwrap()
                .consent_version,
            None,
        );
        let mut accept_disclosure = Request::put("/api/memory/settings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"consent_version":1}"#))
            .unwrap();
        accept_disclosure.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(accept_disclosure).await.unwrap().status(),
            StatusCode::OK,
        );
        assert!(
            memory
                .get_settings("system_default_user")
                .await
                .unwrap()
                .consented_at
                .is_some(),
        );

        let mut status = Request::get("/api/memory/status").body(Body::empty()).unwrap();
        status.extensions_mut().insert(current_user());
        let status_response = router.clone().oneshot(status).await.unwrap();
        assert_eq!(status_response.status(), StatusCode::OK);
        let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
        assert_eq!(status_json["data"]["app_operations_readiness"]["health"], "ready");

        let mut malformed_filter = Request::get("/api/memory/entries?created_after=20&created_before=10")
            .body(Body::empty())
            .unwrap();
        malformed_filter.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(malformed_filter).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
        );

        let mut cross_user = Request::get("/api/conversations/owned-conversation/memory-policy")
            .body(Body::empty())
            .unwrap();
        cross_user.extensions_mut().insert(CurrentUser {
            id: "another-user".into(),
            username: "other".into(),
        });
        assert_eq!(
            router.clone().oneshot(cross_user).await.unwrap().status(),
            StatusCode::NOT_FOUND,
        );

        let mut policy = Request::put("/api/conversations/owned-conversation/memory-policy")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"capture_enabled":false,"recall_enabled":false}"#))
            .unwrap();
        policy.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(policy).await.unwrap().status(), StatusCode::OK);
        let mut get_policy = Request::get("/api/conversations/owned-conversation/memory-policy")
            .body(Body::empty())
            .unwrap();
        get_policy.extensions_mut().insert(current_user());
        let policy_response = router.clone().oneshot(get_policy).await.unwrap();
        assert_eq!(policy_response.status(), StatusCode::OK);
        let policy_body = axum::body::to_bytes(policy_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let policy_json: serde_json::Value = serde_json::from_slice(&policy_body).unwrap();
        assert_eq!(policy_json["data"]["capture_enabled"], false);
        assert_eq!(policy_json["data"]["recall_enabled"], false);

        let mut inherit = Request::put("/api/conversations/owned-conversation/memory-policy")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        inherit.extensions_mut().insert(current_user());
        let inherit_response = router.clone().oneshot(inherit).await.unwrap();
        assert_eq!(inherit_response.status(), StatusCode::OK);
        let inherit_body = axum::body::to_bytes(inherit_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let inherit_json: serde_json::Value = serde_json::from_slice(&inherit_body).unwrap();
        assert!(inherit_json["data"].get("capture_enabled").is_none());
        assert!(inherit_json["data"].get("recall_enabled").is_none());
    }

    #[tokio::test]
    async fn public_library_and_lifecycle_routes_preserve_protection_tombstones_and_reset_fences() {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations
            .create(&ConversationRow {
                id: "conversation-public".into(),
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
                default_recall: Some(true),
                consent_version: Some(1),
                now: 2,
            })
            .await
            .unwrap();
        for (id, stable_key, fingerprint, content, state, group) in [
            ("entry-edit", "edit", "fp-edit", "Original", "active", None),
            (
                "forget-exclusive",
                "exclusive",
                "fp-exclusive",
                "Exclusive",
                "active",
                None,
            ),
            (
                "forget-protected",
                "protected",
                "fp-protected",
                "Protected",
                "active",
                None,
            ),
            (
                "conflict-a",
                "decision",
                "fp-conflict",
                "Version A",
                "conflict",
                Some("group-1"),
            ),
            (
                "conflict-b",
                "decision",
                "fp-conflict",
                "Version B",
                "conflict",
                Some("group-1"),
            ),
            (
                "select-a",
                "select",
                "fp-select",
                "Select A",
                "conflict",
                Some("group-2"),
            ),
            (
                "select-b",
                "select",
                "fp-select",
                "Select B",
                "conflict",
                Some("group-2"),
            ),
            (
                "separate-a",
                "separate",
                "fp-separate",
                "Separate A",
                "conflict",
                Some("group-3"),
            ),
            (
                "separate-b",
                "separate",
                "fp-separate",
                "Separate B",
                "conflict",
                Some("group-3"),
            ),
        ] {
            sqlx::query(
                "INSERT INTO memory_entries
                 (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,revision,
                  conflict_group_id,schema_version,created_at,updated_at)
                 VALUES (?,'system_default_user','decision',?,?,?,?,0,0,0,?,1,10,10)",
            )
            .bind(id)
            .bind(stable_key)
            .bind(fingerprint)
            .bind(content)
            .bind(state)
            .bind(group)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO memory_sources
             (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
             VALUES ('entry-edit','conversation-public','turn-1','[\"message-1\"]',10,10)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE memory_entries SET pinned = 1 WHERE id = 'forget-protected'")
            .execute(db.pool())
            .await
            .unwrap();
        for entry_id in ["forget-exclusive", "forget-protected"] {
            sqlx::query(
                "INSERT INTO memory_sources
                 (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
                 VALUES (?,'conversation-public','turn-2','[\"message-2\"]',10,10)",
            )
            .bind(entry_id)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO memory_jobs
             (id,user_id,conversation_id,through_turn_id,operation_version,queue_digest,input_hash,
              expected_revision,state,attempt_count,invalid_output_count,created_at,updated_at)
             VALUES ('job-failed','system_default_user','conversation-public','turn-1','v1','digest','hash',
                     0,'failed',1,0,10,10)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO memory_change_sets
             (id,user_id,conversation_id,through_turn_id,job_id,added_ids_json,refined_ids_json,
              superseded_ids_json,conflict_ids_json,created_at)
             VALUES ('change-1','system_default_user','conversation-public','turn-1','job-failed',
                     '[\"entry-edit\"]','[]','[]','[]',10)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let service = Arc::new(MemoryService::with_job_dependencies(
            memory.clone(),
            conversations,
            Arc::new(UsableReadiness),
        ));
        let router = memory_routes(MemoryRouterState { service });

        for mut request in [
            Request::patch("/api/memory/entries/entry-edit")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"pinned":true}"#))
                .unwrap(),
            Request::post("/api/memory/entries/conflict-a/resolve-conflict")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"keep_separate"}"#))
                .unwrap(),
            Request::post("/api/memory/jobs/job-failed/retry")
                .body(Body::empty())
                .unwrap(),
        ] {
            request.extensions_mut().insert(CurrentUser {
                id: "another-user".into(),
                username: "other".into(),
            });
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND,
            );
        }

        let mut list = Request::get("/api/memory/entries?source_conversation_id=conversation-public")
            .body(Body::empty())
            .unwrap();
        list.extensions_mut().insert(current_user());
        let list_response = router.clone().oneshot(list).await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
        let edited_item = list_json["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "entry-edit")
            .unwrap();
        assert_eq!(edited_item["sources"][0]["message_ids"][0], "message-1");

        let mut edit = Request::patch("/api/memory/entries/entry-edit")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"content":"Edited","pinned":true,"project_id":"project-1"}"#,
            ))
            .unwrap();
        edit.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(edit).await.unwrap().status(), StatusCode::OK);
        let edited = memory
            .get_entry("system_default_user", "entry-edit")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.content.as_deref(), Some("Edited"));
        assert!(edited.pinned && edited.user_edited);
        assert_eq!(edited.project_id.as_deref(), Some("project-1"));

        let mut cross_user_delete = Request::delete("/api/memory/entries/entry-edit")
            .body(Body::empty())
            .unwrap();
        cross_user_delete.extensions_mut().insert(CurrentUser {
            id: "another-user".into(),
            username: "other".into(),
        });
        assert_eq!(
            router.clone().oneshot(cross_user_delete).await.unwrap().status(),
            StatusCode::NOT_FOUND,
        );

        let mut resolve = Request::post("/api/memory/entries/conflict-a/resolve-conflict")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"action":"merge","content":"Merged version"}"#))
            .unwrap();
        resolve.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(resolve).await.unwrap().status(), StatusCode::OK);
        let merged = memory
            .get_entry("system_default_user", "conflict-a")
            .await
            .unwrap()
            .unwrap();
        let superseded = memory
            .get_entry("system_default_user", "conflict-b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.state, "active");
        assert!(merged.user_edited);
        assert_eq!(superseded.state, "superseded");

        let mut select = Request::post("/api/memory/entries/select-a/resolve-conflict")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"action":"select","selected_entry_id":"select-b"}"#))
            .unwrap();
        select.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(select).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            memory
                .get_entry("system_default_user", "select-a")
                .await
                .unwrap()
                .unwrap()
                .state,
            "superseded",
        );
        let selected = memory
            .get_entry("system_default_user", "select-b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.state, "active");
        assert!(selected.user_edited);

        let mut keep_separate = Request::post("/api/memory/entries/separate-a/resolve-conflict")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"action":"keep_separate"}"#))
            .unwrap();
        keep_separate.extensions_mut().insert(current_user());
        assert_eq!(
            router.clone().oneshot(keep_separate).await.unwrap().status(),
            StatusCode::OK,
        );
        let separate_a = memory
            .get_entry("system_default_user", "separate-a")
            .await
            .unwrap()
            .unwrap();
        let separate_b = memory
            .get_entry("system_default_user", "separate-b")
            .await
            .unwrap()
            .unwrap();
        assert!(separate_a.state == "active" && separate_a.user_edited);
        assert!(separate_b.state == "active" && separate_b.user_edited);
        assert_ne!(separate_a.fingerprint, separate_b.fingerprint);
        let original_identity_tombstones: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entries WHERE user_id = 'system_default_user'
             AND fingerprint = 'fp-separate' AND state = 'deleted' AND content IS NULL",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(original_identity_tombstones, 1);

        let mut changes = Request::get("/api/memory/change-sets?conversation_id=conversation-public")
            .body(Body::empty())
            .unwrap();
        changes.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(changes).await.unwrap().status(), StatusCode::OK);

        let mut retry = Request::post("/api/memory/jobs/job-failed/retry")
            .body(Body::empty())
            .unwrap();
        retry.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(retry).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            memory
                .get_job("system_default_user", "job-failed")
                .await
                .unwrap()
                .unwrap()
                .state,
            "pending",
        );

        let mut disable = Request::put("/api/memory/settings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"enabled":false}"#))
            .unwrap();
        disable.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(disable).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            memory
                .get_job("system_default_user", "job-failed")
                .await
                .unwrap()
                .unwrap()
                .state,
            "canceled",
        );
        assert!(
            memory
                .get_entry("system_default_user", "entry-edit")
                .await
                .unwrap()
                .is_some()
        );

        let mut delete = Request::delete("/api/memory/entries/entry-edit")
            .body(Body::empty())
            .unwrap();
        delete.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(delete).await.unwrap().status(), StatusCode::OK);
        let tombstone = memory
            .get_entry("system_default_user", "entry-edit")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tombstone.state, "deleted");
        assert_eq!(tombstone.content, None);
        assert!(tombstone.sources.is_empty());
        let mut deleted_entries = Request::get("/api/memory/entries?state=deleted")
            .body(Body::empty())
            .unwrap();
        deleted_entries.extensions_mut().insert(current_user());
        let deleted_response = router.clone().oneshot(deleted_entries).await.unwrap();
        assert_eq!(deleted_response.status(), StatusCode::OK);
        let deleted_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(deleted_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let deleted_item = &deleted_json["data"]["items"][0];
        assert_eq!(deleted_item["id"], "entry-edit");
        assert_eq!(deleted_item["state"], "deleted");
        assert!(deleted_item["deleted_at"].is_number());
        for scrubbed in ["stable_key", "content", "sources"] {
            assert!(deleted_item.get(scrubbed).is_none(), "{scrubbed} crossed the API");
        }

        let mut cross_user_forget = Request::delete("/api/memory/conversations/conversation-public")
            .body(Body::empty())
            .unwrap();
        cross_user_forget.extensions_mut().insert(CurrentUser {
            id: "another-user".into(),
            username: "other".into(),
        });
        assert_eq!(
            router.clone().oneshot(cross_user_forget).await.unwrap().status(),
            StatusCode::NOT_FOUND,
        );
        let mut forget = Request::delete("/api/memory/conversations/conversation-public")
            .body(Body::empty())
            .unwrap();
        forget.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(forget).await.unwrap().status(), StatusCode::OK);
        assert!(
            memory
                .get_entry("system_default_user", "forget-exclusive")
                .await
                .unwrap()
                .is_none(),
        );
        let protected = memory
            .get_entry("system_default_user", "forget-protected")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(protected.state, "deleted");
        assert_eq!(protected.content, None);
        assert!(protected.sources.is_empty());
        assert!(!protected.pinned && !protected.user_edited);
        let mut protected_tombstones = Request::get("/api/memory/entries?state=deleted")
            .body(Body::empty())
            .unwrap();
        protected_tombstones.extensions_mut().insert(current_user());
        let protected_response = router.clone().oneshot(protected_tombstones).await.unwrap();
        assert_eq!(protected_response.status(), StatusCode::OK);
        let protected_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(protected_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            protected_json["data"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["id"] == "forget-protected" && entry["state"] == "deleted"),
        );
        assert!(
            memory
                .effective_policy("system_default_user", "conversation-public")
                .await
                .unwrap()
                .reset_at
                .is_some(),
        );

        let mut clear = Request::delete("/api/memory").body(Body::empty()).unwrap();
        clear.extensions_mut().insert(current_user());
        assert_eq!(router.clone().oneshot(clear).await.unwrap().status(), StatusCode::OK);
        assert!(memory.list_entries("system_default_user").await.unwrap().is_empty());
        assert!(
            memory
                .get_settings("system_default_user")
                .await
                .unwrap()
                .reset_at
                .is_some()
        );
    }

    fn current_user() -> CurrentUser {
        CurrentUser {
            id: "system_default_user".into(),
            username: "user".into(),
        }
    }
}
