use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn test_local_mode_skips_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();

    let router = aionui_app::create_router(&services).await;

    // Health check should work
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // An authenticated endpoint should work WITHOUT a token in local mode
    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    services.database.close().await;
}

#[tokio::test]
async fn test_non_local_mode_requires_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let router = aionui_app::create_router(&services).await;

    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "UNAUTHORIZED");

    services.database.close().await;
}
