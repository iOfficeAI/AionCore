//! Real-hook session-revocation cleanup e2e.
//!
//! The auth-layer tests exercise the revoke endpoint with an INJECTED hook and
//! only prove "the hook was called". This test boots the real router — whose
//! `session_revoked_hook` is the production closure wired to the real module
//! services — revokes an AionPro session over HTTP, and asserts observable
//! cleanup actually happened:
//!
//!   - channel sessions: the user's `assistant_sessions` rows are deleted by
//!     `ChannelSessionManager::clear_all_sessions` (async part of the hook,
//!     polled with a timeout);
//!   - session invalidation: the revoked cookie token stops working
//!     (synchronous `session_generation` bump).
//!
//! NOT covered here (would need live agent runtimes / websockets / watchers to
//! observe): ws disconnect, team-session stop, channel plugin shutdown,
//! runtime termination, file/office watch stops. Those legs remain covered by
//! the injected-hook unit tests plus their own services' tests.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_db::models::{AssistantSessionRow, AssistantUserRow};
use aionui_db::{IChannelRepository, SqliteChannelRepository};

const BOOTSTRAP: &str = "bootstrap-secret";

fn bootstrap_json(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-aioncore-bootstrap-secret", BOOTSTRAP)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn extract_session_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get(header::SET_COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .split_once('=')
        .map(|(_, v)| v.to_owned())
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn http_revoke_runs_the_real_cleanup_hook_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some(BOOTSTRAP.to_string()),
        data_dir: tmp.path().to_path_buf(),
        work_dir: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    // Full production router: the session_revoked_hook inside is the REAL
    // closure over the real module services, not a test double.
    let app = aionui_app::create_router(&services).await.expect("build router");

    // Provision an AionPro user + session over the internal API.
    let resp = app
        .clone()
        .oneshot(bootstrap_json(
            "PUT",
            "/api/auth/internal/external-users/revoke-e2e",
            r#"{"user_type":"aionpro","username":"Revoke E2E"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "provision failed");

    let resp = app
        .clone()
        .oneshot(bootstrap_json(
            "POST",
            "/api/auth/internal/external-sessions",
            r#"{"user_type":"aionpro","external_user_id":"revoke-e2e"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "session exchange failed");
    let token = extract_session_token(&resp).expect("session cookie");
    let user_id = body_json(resp).await["data"]["user"]["id"]
        .as_str()
        .expect("user id")
        .to_owned();

    // Seed observable channel state owned by that user: one channel user with
    // one active channel session (assistant_sessions row).
    let channel_repo = SqliteChannelRepository::new(services.database.pool().clone());
    let now = aionui_common::now_ms();
    channel_repo
        .create_user(
            &user_id,
            &AssistantUserRow {
                id: "cu-revoke".into(),
                owner_user_id: user_id.clone(),
                platform_user_id: "tg-revoke".into(),
                platform_type: "telegram".into(),
                display_name: Some("TG".into()),
                authorized_at: now,
                last_active: None,
                session_id: None,
            },
        )
        .await
        .unwrap();
    channel_repo
        .get_or_create_session(
            &user_id,
            "cu-revoke",
            "chat-revoke",
            &AssistantSessionRow {
                id: "cs-revoke".into(),
                user_id: "cu-revoke".into(),
                agent_type: "gemini".into(),
                conversation_id: None,
                workspace: None,
                chat_id: Some("chat-revoke".into()),
                created_at: now,
                last_activity: now,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        channel_repo.get_all_sessions(&user_id).await.unwrap().len(),
        1,
        "precondition: one live channel session"
    );

    // The session token works before revocation.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/user")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "token must work pre-revoke");

    // Revoke over HTTP — fires the real hook.
    let resp = app
        .clone()
        .oneshot(bootstrap_json(
            "POST",
            "/api/auth/internal/external-sessions/revoke",
            r#"{"user_type":"aionpro","external_user_id":"revoke-e2e"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "revoke failed");

    // Synchronous effect: the old token is dead.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/user")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "revoked token must stop working"
    );

    // Async effect: the real hook's spawned cleanup clears the user's channel
    // sessions from the database. Poll with a timeout instead of sleeping a
    // fixed amount.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = channel_repo.get_all_sessions(&user_id).await.unwrap();
        if remaining.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real cleanup hook did not clear channel sessions within 5s: {} row(s) left",
            remaining.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    services.database.close().await;
}
