#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, State};
use axum::routing::post;

use aionui_api_types::{ApiResponse, TestBedrockConnectionRequest};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::SiteRole;
#[cfg(test)]
use aionui_db::UserType;

use super::service::ConnectionTestService;

/// Router state for connection test routes.
#[derive(Clone)]
pub struct ConnectionTestRouterState {
    pub service: ConnectionTestService,
}

/// Build the connection test router.
///
/// Routes:
/// - `POST /api/bedrock/test-connection` — test AWS Bedrock credentials
///
/// All routes require authentication (applied by the caller).
pub fn connection_test_routes(state: ConnectionTestRouterState) -> Router {
    Router::new()
        .route("/api/bedrock/test-connection", post(test_bedrock))
        .with_state(state)
}

/// POST /api/bedrock/test-connection
///
/// Test AWS Bedrock credentials with a lightweight API call.
/// Returns 200 on success, 400 for validation errors, 422-equivalent for
/// invalid credentials (mapped to 400 with descriptive message).
async fn test_bedrock(
    State(state): State<ConnectionTestRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<TestBedrockConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if user.site_role != SiteRole::Admin {
        return Err(ApiError::Forbidden(
            "Bedrock connection testing is available only to site administrators".into(),
        ));
    }
    state
        .service
        .test_bedrock_connection(req.bedrock_config)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::message("Connection successful")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn test_router_state_clone() {
        let state = ConnectionTestRouterState {
            service: ConnectionTestService::new(reqwest::Client::new()),
        };
        let _cloned = state.clone();
    }

    #[test]
    fn test_router_construction() {
        let state = ConnectionTestRouterState {
            service: ConnectionTestService::new(reqwest::Client::new()),
        };
        let _router = connection_test_routes(state);
    }

    #[tokio::test]
    async fn member_cannot_test_bedrock_credentials() {
        let state = ConnectionTestRouterState {
            service: ConnectionTestService::new(reqwest::Client::new()),
        };
        let router = connection_test_routes(state);
        for config in [
            serde_json::json!({
                "auth_method": "profile",
                "region": "us-east-1",
                "profile": "default"
            }),
            serde_json::json!({
                "auth_method": "accessKey",
                "region": "us-east-1",
                "access_key_id": "AKIA_TEST",
                "secret_access_key": "secret"
            }),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/bedrock/test-connection")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"bedrock_config": config}).to_string()))
                .unwrap();
            request.extensions_mut().insert(CurrentUser {
                id: "member-user".into(),
                username: "member-user".into(),
                user_type: UserType::Local,
                status: aionui_db::UserStatus::Active,
                site_role: SiteRole::Member,
                must_change_password: false,
            });

            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
}
