//! E2E tests for cron job HTTP endpoints.
//!
//! Covers test-plan items: CJ-1..CJ-12, SK-1..SK-6, SC-3..SC-8, AU-1..AU-2,
//! RN-1..RN-2.
//! Items requiring real AI execution (RN-1, EV-*, SR-*, OC-*, CD-*) are tested
//! at the service integration level in `aionui-cron/tests/service_integration.rs`.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use aionui_db::{
    CreateMcpServerParams, IConversationRepository, ICronRepository, IMcpServerRepository,
    SqliteConversationRepository, SqliteCronRepository, SqliteMcpServerRepository,
};

use common::{
    body_json, build_app, build_app_with_mock_agents, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};

// ── Helpers ──────────────────────────────────────────────────────────

const DEFAULT_CRON_ASSISTANT_ID: &str = "cron-e2e-assistant";

fn default_assistant_agent_config(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "assistant_id": DEFAULT_CRON_ASSISTANT_ID
    })
}

fn create_job_body(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
        "message": "test message",
        "conversation_id": "conv_1",
        "conversation_title": "Test Conv",
        "created_by": "user",
        "agent_config": default_assistant_agent_config(name)
    })
}

fn create_at_job_body(name: &str, at_ms: i64) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "at", "at_ms": at_ms, "description": "once" },
        "message": "at message",
        "conversation_id": "conv_1",
        "created_by": "user",
        "agent_config": default_assistant_agent_config(name)
    })
}

fn create_cron_job_body(name: &str, expr: &str) -> serde_json::Value {
    json!({
        "name": name,
        "schedule": { "kind": "cron", "expr": expr },
        "message": "cron message",
        "conversation_id": "conv_1",
        "created_by": "user",
        "agent_config": default_assistant_agent_config(name)
    })
}

async fn ensure_default_assistant(app: &mut axum::Router, token: &str, csrf: &str) {
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": DEFAULT_CRON_ASSISTANT_ID,
            "name": "Cron E2E Assistant",
            "agent_id": "2d23ff1c"
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::CONFLICT,
        "expected assistant seed to be created or already exist, got {}",
        resp.status()
    );
}

async fn create_job(app: &mut axum::Router, token: &str, csrf: &str, body: serde_json::Value) -> serde_json::Value {
    ensure_default_assistant(app, token, csrf).await;
    let req = json_with_token("POST", "/api/cron/jobs", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    json["data"].clone()
}

// ── AU-1/AU-2: Unauthenticated requests ─────────────────────────────

#[tokio::test]
async fn au1_unauthenticated_list_returns_403() {
    let (app, _services) = build_app().await;
    let req = get_request("/api/cron/jobs");
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn au2_unauthenticated_all_endpoints() {
    let (app, _services) = build_app().await;

    let endpoints = vec![
        ("GET", "/api/cron/jobs"),
        ("GET", "/api/cron/jobs/cron_test"),
        ("GET", "/api/cron/jobs/cron_test/skill"),
        ("DELETE", "/api/cron/jobs/cron_test/skill"),
        ("GET", "/api/internal/conversation-cron/list"),
        ("POST", "/api/internal/conversation-cron/create"),
        ("PUT", "/api/internal/conversation-cron/jobs/cron_test"),
    ];

    for (method, uri) in endpoints {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "{method} {uri} expected 401/403, got {}",
            resp.status()
        );
    }
}

#[test]
fn cron_skill_does_not_instruct_agents_to_write_payload_files() {
    let skill = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject/cron/SKILL.md"),
    )
    .unwrap();

    assert!(!skill.contains("--input"));
    assert!(!skill.contains("cat >"));
    assert!(!skill.contains("/tmp/aionui-cron"));
    assert!(!skill.contains("python3"));
    assert!(!skill.contains("aionui_cron.py"));
    assert!(skill.contains("$AIONUI_HELPER_BIN"));
    assert!(!skill.contains("cron-helper"));
    assert!(skill.contains("config cron current list"));
    assert!(skill.contains("config cron current create"));
    assert!(skill.contains("config cron current update"));
    assert!(skill.contains("\"job_id\""));
    assert!(skill.contains("After a successful create or update"));
    assert!(skill.contains("Do not show internal ids"));
    assert!(skill.contains("cron_..."));
}

// ── CJ-1: Create cron job ───────────────────────────────────────────

#[tokio::test]
async fn cj1_create_cron_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_job(&mut app, &token, &csrf, create_job_body("Daily Report")).await;

    assert!(data["id"].as_str().unwrap().starts_with("cron_"));
    assert_eq!(data["name"], "Daily Report");
    assert_eq!(data["enabled"], true);
    assert!(data["state"]["next_run_at_ms"].as_i64().is_some());
    assert_eq!(data["state"]["run_count"], 0);
    assert_eq!(data["target"]["payload"]["kind"], "message");
    assert_eq!(data["target"]["payload"]["text"], "test message");
    assert_eq!(data["metadata"]["conversation_id"], "conv_1");
    assert_eq!(data["metadata"]["agent_type"], "acp");
    assert_eq!(data["metadata"]["created_by"], "user");
}

#[tokio::test]
async fn cj1b_create_job_allows_missing_task_description() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_job(&mut app, &token, &csrf, create_job_body("No Description")).await;

    assert!(data.get("description").is_none());
    assert_eq!(data["schedule"]["description"], "every minute");
}

// ── CJ-2: Create three schedule types ────────────────────────────────

#[tokio::test]
async fn cj2_create_three_schedule_types() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let now = aionui_common::now_ms();

    let at = create_job(&mut app, &token, &csrf, create_at_job_body("At Job", now + 3_600_000)).await;
    assert_eq!(at["schedule"]["kind"], "at");
    assert!(at["state"]["next_run_at_ms"].as_i64().unwrap() > now);

    let every = create_job(&mut app, &token, &csrf, create_job_body("Every Job")).await;
    assert_eq!(every["schedule"]["kind"], "every");
    let next = every["state"]["next_run_at_ms"].as_i64().unwrap();
    assert!((next - now - 60000).abs() < 3000);

    let cron = create_job(
        &mut app,
        &token,
        &csrf,
        create_cron_job_body("Cron Job", "0 */5 * * * *"),
    )
    .await;
    assert_eq!(cron["schedule"]["kind"], "cron");
    assert!(cron["state"]["next_run_at_ms"].as_i64().unwrap() > now);
}

// ── CJ-3: Create parameter validation ────────────────────────────────

#[tokio::test]
async fn cj3_create_missing_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;

    let invalid_bodies = vec![
        json!({"schedule": {"kind": "every", "every_ms": 60000}, "conversation_id": "c1", "created_by": "user", "agent_config": default_assistant_agent_config("X")}),
        json!({"name": "X", "conversation_id": "c1", "created_by": "user", "agent_config": default_assistant_agent_config("X")}),
        json!({"name": "X", "schedule": {"kind": "every", "every_ms": 60000}, "created_by": "user", "agent_config": default_assistant_agent_config("X")}),
        json!({"name": "X", "schedule": {"kind": "every", "every_ms": 60000}, "conversation_id": "c1", "created_by": "user"}),
    ];

    for body in invalid_bodies {
        let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "missing field should return 400"
        );
    }
}

#[tokio::test]
async fn cj3b_create_accepts_workspace_with_whitespace_segment() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;
    let dir = std::env::temp_dir().join(format!("aionui-cron-test-{}", aionui_common::generate_short_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("Archive ");
    std::fs::create_dir(&workspace).unwrap();

    let body = json!({
        "name": "Whitespace Workspace",
        "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
        "message": "test message",
        "conversation_id": "",
        "created_by": "user",
        "execution_mode": "new_conversation",
        "agent_config": {
            "name": "Cron Agent",
            "assistant_id": DEFAULT_CRON_ASSISTANT_ID,
            "workspace": workspace.to_string_lossy()
        }
    });

    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cj3c_create_rejects_missing_workspace_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Missing Workspace",
        "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
        "message": "test message",
        "conversation_id": "",
        "created_by": "user",
        "execution_mode": "new_conversation",
        "agent_config": {
            "name": "Claude Code",
            "assistant_id": DEFAULT_CRON_ASSISTANT_ID,
            "workspace": "/tmp/cron-job-workspace-missing-path"
        }
    });

    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_UNAVAILABLE");
    assert_eq!(json["details"]["operation"], "create");
    assert_eq!(
        json["details"]["workspace_path"],
        "/tmp/cron-job-workspace-missing-path"
    );

    let list_req = get_with_token("/api/cron/jobs", &token);
    let list_resp = app.oneshot(list_req).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let has_job = list_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|job| job["name"] == "Missing Workspace");
    assert!(!has_job, "invalid cron job should not be persisted");
}

// ── CJ-4: Get single job ────────────────────────────────────────────

#[tokio::test]
async fn cj4_get_single_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Get Test")).await;
    let job_id = created["id"].as_str().unwrap();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], job_id);
    assert_eq!(json["data"]["name"], "Get Test");
}

// ── CJ-5: Get nonexistent job ────────────────────────────────────────

#[tokio::test]
async fn cj5_get_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/cron/jobs/cron_nonexistent", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cj5b_run_now_legacy_workspace_with_whitespace_succeeds() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;
    let cron_repo = SqliteCronRepository::new(services.database.pool().clone());
    let now = aionui_common::now_ms();
    let dir = std::env::temp_dir().join(format!("aionui-cron-test-{}", aionui_common::generate_short_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("Archive ");
    std::fs::create_dir(&workspace).unwrap();

    cron_repo
        .insert(&aionui_db::models::CronJobRow {
            id: "cron_whitespace_workspace".into(),
            name: "Legacy Workspace".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("every minute".into()),
            payload_message: "test message".into(),
            execution_mode: "new_conversation".into(),
            agent_config: Some(
                json!({
                    "name": "Cron Agent",
                    "assistant_id": DEFAULT_CRON_ASSISTANT_ID,
                    "workspace": workspace.to_string_lossy()
                })
                .to_string(),
            ),
            conversation_id: String::new(),
            conversation_title: None,
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now,
            updated_at: now,
            next_run_at: Some(now + 60_000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        })
        .await
        .unwrap();

    let req = json_with_token(
        "POST",
        "/api/cron/jobs/cron_whitespace_workspace/run",
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── CJ-6: List all jobs ─────────────────────────────────────────────

#[tokio::test]
async fn cj6_list_all_jobs() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    for i in 0..3 {
        create_job(&mut app, &token, &csrf, create_job_body(&format!("Job {i}"))).await;
    }

    let req = get_with_token("/api/cron/jobs", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert!(items.len() >= 3);
}

// ── CJ-7: List by conversation ID ───────────────────────────────────

#[tokio::test]
async fn cj7_list_by_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let mut body_a = create_job_body("Job A");
    body_a["conversation_id"] = json!("conv_target");
    create_job(&mut app, &token, &csrf, body_a).await;

    let mut body_b = create_job_body("Job B");
    body_b["conversation_id"] = json!("conv_target");
    create_job(&mut app, &token, &csrf, body_b).await;

    let mut body_c = create_job_body("Job C");
    body_c["conversation_id"] = json!("conv_other");
    create_job(&mut app, &token, &csrf, body_c).await;

    let req = get_with_token("/api/cron/jobs?conversation_id=conv_target", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

// ── CJ-8: Update job ────────────────────────────────────────────────

#[tokio::test]
async fn cj8_update_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Original")).await;
    let job_id = created["id"].as_str().unwrap();

    let update_body = json!({"name": "Updated Name", "enabled": false});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Updated Name");
    assert_eq!(json["data"]["enabled"], false);
    assert!(
        json["data"]["metadata"]["updated_at"].as_i64().unwrap() >= created["metadata"]["created_at"].as_i64().unwrap()
    );
}

// ── CJ-9: Update schedule type ──────────────────────────────────────

#[tokio::test]
async fn cj9_update_schedule_type() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Schedule Change")).await;
    let job_id = created["id"].as_str().unwrap();

    let update_body = json!({"schedule": {"kind": "cron", "expr": "0 */5 * * * *"}});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["schedule"]["kind"], "cron");
    assert!(json["data"]["state"]["next_run_at_ms"].as_i64().is_some());
}

#[tokio::test]
async fn cj9b_update_schedule_preserves_existing_timezone_when_omitted() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(
        &mut app,
        &token,
        &csrf,
        json!({
            "name": "Schedule Change With Timezone",
            "schedule": { "kind": "cron", "expr": "0 0 9 * * *", "tz": "Asia/Shanghai" },
            "message": "cron message",
            "conversation_id": "conv_1",
            "created_by": "user",
            "agent_config": default_assistant_agent_config("Schedule Change With Timezone")
        }),
    )
    .await;
    let job_id = created["id"].as_str().unwrap();

    let update_body = json!({"schedule": {"kind": "cron", "expr": "0 30 9 * * *"}});
    let req = json_with_token("PUT", &format!("/api/cron/jobs/{job_id}"), update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["schedule"]["kind"], "cron");
    assert_eq!(json["data"]["schedule"]["expr"], "0 30 9 * * *");
    assert_eq!(json["data"]["schedule"]["tz"], "Asia/Shanghai");
}

// ── CJ-10: Update nonexistent ────────────────────────────────────────

#[tokio::test]
async fn cj10_update_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let update_body = json!({"name": "X"});
    let req = json_with_token("PUT", "/api/cron/jobs/cron_nonexistent", update_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── CJ-11: Delete job ───────────────────────────────────────────────

#[tokio::test]
async fn cj11_delete_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("To Delete")).await;
    let job_id = created["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/cron/jobs/{job_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── CJ-12: Delete nonexistent ────────────────────────────────────────

#[tokio::test]
async fn cj12_delete_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token("/api/cron/jobs/cron_nonexistent", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── RN-2: Run now nonexistent ────────────────────────────────────────

#[tokio::test]
async fn rn1_run_now_returns_conversation_id_for_new_conversation_job() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create_conv_req = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Run Now Source",
            "extra": {}
        }),
        &token,
        &csrf,
    );
    let create_conv_resp = app.clone().oneshot(create_conv_req).await.unwrap();
    assert_eq!(create_conv_resp.status(), StatusCode::CREATED);
    let created_conv = body_json(create_conv_resp).await;
    let conversation_id = created_conv["data"]["id"].as_str().unwrap();

    let mut body = create_job_body("Run Now Job");
    body["conversation_id"] = json!(conversation_id);
    let created = create_job(&mut app, &token, &csrf, body).await;
    let job_id = created["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/run"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["data"]["conversation_id"], json!(conversation_id));
}

#[tokio::test]
async fn rn1b_run_now_returns_active_conversation_when_conversation_is_busy() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create_conv_req = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Busy Run Now Source",
            "extra": {}
        }),
        &token,
        &csrf,
    );
    let create_conv_resp = app.clone().oneshot(create_conv_req).await.unwrap();
    assert_eq!(create_conv_resp.status(), StatusCode::CREATED);
    let created_conv = body_json(create_conv_resp).await;
    let conversation_id = created_conv["data"]["id"].as_str().unwrap().to_owned();

    let mut body = create_job_body("Busy Run Now Job");
    body["conversation_id"] = json!(conversation_id);
    let created = create_job(&mut app, &token, &csrf, body).await;
    let job_id = created["id"].as_str().unwrap();

    let claim = services
        .conversation_runtime_state
        .try_claim_turn(&conversation_id, "turn-busy-e2e")
        .expect("runtime claim should succeed");

    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/run"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["data"]["conversation_id"], json!(conversation_id));

    drop(claim);
}

#[tokio::test]
async fn rn1c_run_now_new_conversation_preset_assistant_uses_fixed_assistant_mcps() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let mcp_repo = SqliteMcpServerRepository::new(services.database.pool().clone());
    let fixed_mcp = mcp_repo
        .create(CreateMcpServerParams {
            name: "fixed-mcp",
            description: None,
            enabled: true,
            transport_type: "http",
            transport_config: r#"{"url":"https://example.invalid/fixed"}"#,
            tools: None,
            original_json: None,
            builtin: false,
        })
        .await
        .expect("create fixed mcp");
    let extra_mcp = mcp_repo
        .create(CreateMcpServerParams {
            name: "extra-mcp",
            description: None,
            enabled: true,
            transport_type: "http",
            transport_config: r#"{"url":"https://example.invalid/extra"}"#,
            tools: None,
            original_json: None,
            builtin: false,
        })
        .await
        .expect("create extra mcp");

    let create_assistant_req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u-fixed-mcp",
            "name": "Cron MCP Assistant",
            "agent_id": "8e1acf31",
            "defaults": {
                "mcps": {
                    "mode": "fixed",
                    "value": [fixed_mcp.id]
                }
            }
        }),
        &token,
        &csrf,
    );
    let create_assistant_resp = app.clone().oneshot(create_assistant_req).await.unwrap();
    assert_eq!(create_assistant_resp.status(), StatusCode::CREATED);

    let create_job_req = json_with_token(
        "POST",
        "/api/cron/jobs",
        json!({
            "name": "Preset Assistant Cron",
            "schedule": { "kind": "every", "every_ms": 60000, "description": "every minute" },
            "message": "cron preset assistant message",
            "conversation_id": "",
            "created_by": "user",
            "execution_mode": "new_conversation",
            "agent_config": {
                "name": "Cron MCP Assistant",
                "assistant_id": "u-fixed-mcp"
            }
        }),
        &token,
        &csrf,
    );
    let create_job_resp = app.clone().oneshot(create_job_req).await.unwrap();
    assert_eq!(create_job_resp.status(), StatusCode::CREATED);
    let create_job_body = body_json(create_job_resp).await;
    let job_id = create_job_body["data"]["id"]
        .as_str()
        .expect("cron job id should be present");
    let saved_skill_name = format!("cron-{job_id}");

    let save_skill_req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        json!({
            "content": "---\nname: saved cron skill\ndescription: saved cron skill\n---\nUse the saved cron skill"
        }),
        &token,
        &csrf,
    );
    let save_skill_resp = app.clone().oneshot(save_skill_req).await.unwrap();
    assert_eq!(save_skill_resp.status(), StatusCode::OK);

    let run_req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/run"),
        json!({}),
        &token,
        &csrf,
    );
    let run_resp = app.clone().oneshot(run_req).await.unwrap();
    assert_eq!(run_resp.status(), StatusCode::OK);
    let run_body = body_json(run_resp).await;
    let conversation_id = run_body["data"]["conversation_id"]
        .as_str()
        .expect("run-now should return created conversation id");

    let conversation_repo = SqliteConversationRepository::new(services.database.pool().clone());
    let conversation = conversation_repo
        .get(conversation_id)
        .await
        .expect("load conversation")
        .expect("conversation should exist");
    let extra: serde_json::Value =
        serde_json::from_str(&conversation.extra).expect("conversation extra should be valid json");
    assert!(extra.get("assistant_id").is_none());
    assert!(extra.get("preset_assistant_id").is_none());
    assert!(extra.get("custom_agent_id").is_none());
    assert_eq!(extra["mcp_server_ids"], json!([fixed_mcp.id]));
    assert_eq!(extra["mcp_servers"], json!(["fixed-mcp"]));
    assert!(
        extra["skills"].as_array().is_some_and(|skills| {
            skills.iter().all(|skill| skill != "cron") && skills.iter().any(|skill| skill == &saved_skill_name)
        }),
        "cron-created conversations must exclude builtin cron but keep the saved job skill"
    );
    assert_ne!(fixed_mcp.id, extra_mcp.id, "fixture should seed two distinct MCP rows");

    let snapshot = conversation_repo
        .get_assistant_snapshot(conversation_id)
        .await
        .expect("load assistant snapshot")
        .expect("preset assistant cron conversation should persist snapshot");
    assert_eq!(snapshot.assistant_id, "u-fixed-mcp");
    assert_eq!(snapshot.resolved_mcp_ids, json!([fixed_mcp.id]).to_string());
}

#[tokio::test]
async fn rn2_run_now_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/cron/jobs/cron_nonexistent/run", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SK-1: Save skill ────────────────────────────────────────────────

#[tokio::test]
async fn sk1_save_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Skill Job")).await;
    let job_id = created["id"].as_str().unwrap();

    let skill_body = json!({"content": "---\nname: test\ndescription: test skill\n---\nDo something"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── SK-2: Has skill (true) ──────────────────────────────────────────

#[tokio::test]
async fn sk2_has_skill_true() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Skill Check")).await;
    let job_id = created["id"].as_str().unwrap();

    let skill_body = json!({"content": "---\nname: x\n---\nContent"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["has_skill"], true);
}

// ── SK-3: Has skill (false) ─────────────────────────────────────────

#[tokio::test]
async fn sk3_has_skill_false() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("No Skill")).await;
    let job_id = created["id"].as_str().unwrap();

    let req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["has_skill"], false);
}

// ── SK-4: Save empty skill ──────────────────────────────────────────

#[tokio::test]
async fn sk4_save_empty_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Empty Skill")).await;
    let job_id = created["id"].as_str().unwrap();

    let skill_body = json!({"content": ""});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SK-5: Save placeholder skill ────────────────────────────────────

#[tokio::test]
async fn sk5_save_placeholder_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Placeholder Skill")).await;
    let job_id = created["id"].as_str().unwrap();

    let skill_body = json!({"content": "TODO: fill in later"});
    let req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SK-6: Save skill for nonexistent job ─────────────────────────────

#[tokio::test]
async fn sk6_save_skill_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let skill_body = json!({"content": "---\nname: x\n---\nOk"});
    let req = json_with_token(
        "POST",
        "/api/cron/jobs/cron_nonexistent/skill",
        skill_body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SK-7: Delete existing skill ──────────────────────────────────────

#[tokio::test]
async fn sk7_delete_skill() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_job(&mut app, &token, &csrf, create_job_body("Delete Skill Job")).await;
    let job_id = created["id"].as_str().unwrap();

    let save_req = json_with_token(
        "POST",
        &format!("/api/cron/jobs/{job_id}/skill"),
        json!({"content": "---\nname: delete-me\n---\nContent"}),
        &token,
        &csrf,
    );
    let save_resp = app.clone().oneshot(save_req).await.unwrap();
    assert_eq!(save_resp.status(), StatusCode::OK);

    let delete_req = delete_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token, &csrf);
    let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
    assert_eq!(delete_resp.status(), StatusCode::OK);

    let has_req = get_with_token(&format!("/api/cron/jobs/{job_id}/skill"), &token);
    let has_resp = app.oneshot(has_req).await.unwrap();
    assert_eq!(has_resp.status(), StatusCode::OK);

    let json = body_json(has_resp).await;
    assert_eq!(json["data"]["has_skill"], false);
}

// ── SK-8: Delete skill for nonexistent job ───────────────────────────

#[tokio::test]
async fn sk8_delete_skill_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token("/api/cron/jobs/cron_nonexistent/skill", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── SC-5: Invalid cron expression ────────────────────────────────────

#[tokio::test]
async fn sc5_invalid_cron_expression() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = create_cron_job_body("Invalid Cron", "invalid cron");
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SC-6: Cron with timezone ─────────────────────────────────────────

#[tokio::test]
async fn sc6_cron_with_timezone() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Shanghai Job",
        "schedule": { "kind": "cron", "expr": "0 0 9 * * *", "tz": "Asia/Shanghai" },
        "message": "hello",
        "conversation_id": "conv_1",
        "created_by": "user",
        "agent_config": default_assistant_agent_config("Shanghai Job")
    });

    let data = create_job(&mut app, &token, &csrf, body).await;
    let now = aionui_common::now_ms();
    assert!(data["state"]["next_run_at_ms"].as_i64().unwrap() > now);
}

// ── SC-7: Every zero interval ────────────────────────────────────────

#[tokio::test]
async fn sc7_every_zero_interval() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Zero Interval",
        "schedule": { "kind": "every", "every_ms": 0 },
        "message": "x",
        "conversation_id": "conv_1",
        "created_by": "user",
        "agent_config": default_assistant_agent_config("Zero Interval")
    });
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── SC-8: Every negative interval ────────────────────────────────────

#[tokio::test]
async fn sc8_every_negative_interval() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_assistant(&mut app, &token, &csrf).await;

    let body = json!({
        "name": "Negative Interval",
        "schedule": { "kind": "every", "every_ms": -1000 },
        "message": "x",
        "conversation_id": "conv_1",
        "created_by": "user",
        "agent_config": default_assistant_agent_config("Negative Interval")
    });
    let req = json_with_token("POST", "/api/cron/jobs", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
