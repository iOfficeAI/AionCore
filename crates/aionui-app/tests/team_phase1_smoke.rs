//! D11 — Wave-2 app assembly smoke test.
//!
//! Minimum guarantee: after D7/D8/D9/D10 merged, `AppServices` composes into
//! a router that actually exposes the `/api/teams` endpoints. Anything beyond
//! compile-check is validated by `team_e2e.rs`; this file is kept intentionally
//! tiny so assembly regressions surface first.

mod common;

use axum::http::StatusCode;
use tower::ServiceExt;

use common::{build_app, get_with_token, json_with_token, setup_and_login};

/// Router boots and `/api/teams` is wired through `build_team_state` into
/// `aionui_team::team_routes`. If the team module failed to assemble, the
/// route would 404 (or compile would have failed earlier).
#[tokio::test]
async fn phase1_router_assembles_with_team_module() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams", &token);
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /api/teams must be wired through build_team_state"
    );
}

/// The custom Team preset and ad-hoc static routes must remain reachable after
/// composing `TeamRouterState`; this also guards their ordering ahead of the
/// dynamic `/api/teams/{id}` route.
#[tokio::test]
async fn custom_team_routes_are_wired_before_dynamic_team_route() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let presets = app
        .clone()
        .oneshot(get_with_token("/api/team-presets", &token))
        .await
        .unwrap();
    assert_eq!(
        presets.status(),
        StatusCode::OK,
        "GET /api/team-presets must be reachable"
    );

    let association = app
        .clone()
        .oneshot(get_with_token(
            "/api/teams/by-conversation?conversation_id=missing-conversation",
            &token,
        ))
        .await
        .unwrap();
    assert_ne!(
        association.status(),
        StatusCode::NOT_FOUND,
        "GET /api/teams/by-conversation must resolve to the static route"
    );

    let create = app
        .oneshot(json_with_token(
            "POST",
            "/api/teams/from-conversation",
            serde_json::json!({ "conversation_id": "missing-conversation" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_ne!(
        create.status(),
        StatusCode::NOT_FOUND,
        "POST /api/teams/from-conversation must resolve to the static route"
    );
}
