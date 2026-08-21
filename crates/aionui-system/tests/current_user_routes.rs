//! Black-box tests for `GET /api/system/current-user`.
//!
//! The route exists so a client can learn WHICH user id the backend attributes
//! its requests to. That cannot be answered by `/api/auth/user`: the auth
//! router builds its own `AuthState` whose `identity_mode` is only ever
//! `AionPro` or `UserSession` (`aionui-auth/src/routes.rs`), so in local
//! identity mode that endpoint returns 401 while every ordinary route happily
//! serves the injected default user. A client that needs to compare a
//! broadcast payload's `user_id` against "me" therefore had no usable source,
//! and the cross-session loop warning silently matched nothing in the desktop
//! app.
//!
//! Hence the contract pinned here: this route echoes whatever `CurrentUser` the
//! ordinary auth middleware put on the request — no lookups, no second source
//! of truth.

use std::sync::Arc;

use aionui_realtime::BroadcastEventBus;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_auth::CurrentUser;
use aionui_db::{
    SqliteClientPreferenceRepository, SqliteFeedbackDiagnosticsRepository, SqliteProviderRepository,
    SqliteSettingsRepository, UserStatus, UserType, init_database_memory,
};
use aionui_system::{
    ClientPrefService, FeedbackDiagnosticsService, ModelFetchService, ProtocolDetectionService, ProviderService,
    RuntimePrepareService, SettingsService, SystemRouterState, VersionCheckService, system_routes,
};

const TEST_KEY: [u8; 32] = [0x42; 32];

async fn setup() -> axum::Router {
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    let state = SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "1.0.0".to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(Arc::new(BroadcastEventBus::new(16))),
        feedback_diagnostics_service: FeedbackDiagnosticsService::new(Arc::new(
            SqliteFeedbackDiagnosticsRepository::new(db.pool().clone()),
        )),
    };
    system_routes(state)
}

fn request_for_user(id: &str, username: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method("GET")
        .uri("/api/system/current-user")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(CurrentUser {
        id: id.to_owned(),
        username: username.to_owned(),
        user_type: UserType::Local,
        status: UserStatus::Active,
    });
    req
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn echoes_the_identity_the_middleware_injected() {
    let app = setup().await;
    let resp = app.oneshot(request_for_user("user_42", "someone")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["id"], "user_42");
    assert_eq!(body["data"]["username"], "someone");
}

/// Whatever the middleware injected is the answer — including the fixed
/// identity local mode uses, which is the case that made this route necessary.
#[tokio::test]
async fn reports_the_local_default_identity_verbatim() {
    let app = setup().await;
    let resp = app
        .oneshot(request_for_user("system_default_user", "system_default_user"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["id"], "system_default_user");
}

/// No password hash, no session generation, no email: this answers "who am I",
/// and anything beyond that would be a new place for user data to leak.
#[tokio::test]
async fn exposes_only_id_and_username() {
    let app = setup().await;
    let resp = app.oneshot(request_for_user("user_42", "someone")).await.unwrap();

    let body = body_json(resp).await;
    let fields = body["data"].as_object().expect("data must be an object");
    let mut keys: Vec<&str> = fields.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["id", "username"]);
}
