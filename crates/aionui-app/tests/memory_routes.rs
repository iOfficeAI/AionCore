//! Memory route composition, security, isolation, and compatibility coverage.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, build_app, build_app_with_mock_agents, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};

const SETTINGS_PATH: &str = "/api/memory/settings";

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": name, "extra": {} }),
            token,
            csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await["data"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn memory_routes_are_registered_and_require_authentication() {
    let (app, _) = build_app().await;

    let response = app.oneshot(get_request(SETTINGS_PATH)).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn memory_mutations_require_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("PUT")
        .uri(SETTINGS_PATH)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(r#"{"enabled":true}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn memory_settings_are_isolated_by_authenticated_user() {
    let (mut app, services) = build_app().await;
    let (admin_token, admin_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;

    let admin_update = json_with_token(
        "PUT",
        SETTINGS_PATH,
        json!({
            "enabled": true,
            "default_capture": true,
            "default_recall": false,
            "consent_version": 1
        }),
        &admin_token,
        &admin_csrf,
    );
    assert_eq!(
        app.clone().oneshot(admin_update).await.unwrap().status(),
        StatusCode::OK
    );

    let other_update = json_with_token(
        "PUT",
        SETTINGS_PATH,
        json!({
            "enabled": false,
            "default_capture": false,
            "default_recall": true
        }),
        &other_token,
        &other_csrf,
    );
    assert_eq!(
        app.clone().oneshot(other_update).await.unwrap().status(),
        StatusCode::OK
    );

    let admin = body_json(
        app.clone()
            .oneshot(get_with_token(SETTINGS_PATH, &admin_token))
            .await
            .unwrap(),
    )
    .await;
    let other = body_json(app.oneshot(get_with_token(SETTINGS_PATH, &other_token)).await.unwrap()).await;

    assert_eq!(admin["data"]["enabled"], true);
    assert_eq!(admin["data"]["default_capture"], true);
    assert_eq!(admin["data"]["default_recall"], false);
    assert_eq!(other["data"]["enabled"], false);
    assert_eq!(other["data"]["default_capture"], false);
    assert_eq!(other["data"]["default_recall"], true);
}

#[tokio::test]
async fn module_state_reuses_the_single_application_memory_service() {
    let database = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(database, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let (states, _) = aionui_app::build_module_states(&services).await.unwrap();

    assert!(std::sync::Arc::ptr_eq(&services.memory_service, &states.memory.service));
}

#[tokio::test]
async fn legacy_send_payload_without_memory_fields_remains_accepted() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let create = json_with_token(
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "name": "Legacy client",
            "extra": {}
        }),
        &token,
        &csrf,
    );
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let conversation_id = body_json(created).await["data"]["id"].as_str().unwrap().to_owned();

    let request = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({ "content": "Hello from an older client" }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn deleting_conversation_removes_exclusive_memory_and_preserves_shared_memory() {
    // The mock-agent builder reconstructs ConversationService through
    // `with_worker_task_manager`, covering lifecycle-hook reinjection.
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let deleted_id = create_conversation(&mut app, &token, &csrf, "Deleted source").await;
    let retained_id = create_conversation(&mut app, &token, &csrf, "Retained source").await;
    let user_id = "system_default_user";

    sqlx::query(
        "INSERT INTO memory_entries
            (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,
             schema_version,created_at,updated_at)
         VALUES
            ('exclusive-entry',?,'decision','exclusive','exclusive-fp','exclusive content','active',0,0,1,1,1),
            ('shared-entry',?,'decision','shared','shared-fp','shared content','active',0,0,1,1,1)",
    )
    .bind(user_id)
    .bind(user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_sources
            (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
         VALUES
            ('exclusive-entry',?,'turn-exclusive','[]',1,1),
            ('shared-entry',?,'turn-deleted','[]',1,1),
            ('shared-entry',?,'turn-retained','[]',1,1)",
    )
    .bind(&deleted_id)
    .bind(&deleted_id)
    .bind(&retained_id)
    .execute(services.database.pool())
    .await
    .unwrap();

    let response = app
        .oneshot(delete_with_token(
            &format!("/api/conversations/{deleted_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let exclusive_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_entries WHERE id = 'exclusive-entry')")
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    let shared_state: String = sqlx::query_scalar("SELECT state FROM memory_entries WHERE id = 'shared-entry'")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let shared_sources: Vec<String> =
        sqlx::query_scalar("SELECT conversation_id FROM memory_sources WHERE memory_entry_id = 'shared-entry'")
            .fetch_all(services.database.pool())
            .await
            .unwrap();

    assert!(!exclusive_exists);
    assert_eq!(shared_state, "active");
    assert_eq!(shared_sources, vec![retained_id]);
}

#[tokio::test]
async fn resetting_conversation_clears_memory_before_evidence_and_fences_stale_workers() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf, "Reset source").await;
    let user_id = "system_default_user";
    let now = aionui_common::now_ms();

    aionui_db::IConversationRepository::insert_message(
        &aionui_db::SqliteConversationRepository::new(services.database.pool().clone()),
        &aionui_db::models::MessageRow {
            id: "reset-memory-message".into(),
            conversation_id: conversation_id.clone(),
            turn_id: Some("reset-memory-turn".into()),
            msg_id: Some("reset-memory-message".into()),
            r#type: "text".into(),
            content: r#"{"content":"canonical evidence"}"#.into(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: now,
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_memories
            (user_id,conversation_id,summary_json,through_turn_id,revision,source,
             schema_version,created_at,updated_at)
         VALUES (?,?,'{\"goal\":\"reset me\"}','reset-memory-turn',0,'memory_update',1,?,?)",
    )
    .bind(user_id)
    .bind(&conversation_id)
    .bind(now)
    .bind(now)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_entries
            (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,
             schema_version,created_at,updated_at)
         VALUES
            ('reset-memory-entry',?,'decision','reset-entry','reset-entry-fp',
             'derived content','active',0,0,1,?,?)",
    )
    .bind(user_id)
    .bind(now)
    .bind(now)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_sources
            (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
         VALUES ('reset-memory-entry',?,'reset-memory-turn','[\"reset-memory-message\"]',?,?)",
    )
    .bind(&conversation_id)
    .bind(now)
    .bind(now)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_jobs
            (id,user_id,conversation_id,through_turn_id,operation_version,global_epoch,
             conversation_epoch,turn_count,queue_digest,input_hash,expected_revision,state,
             attempt_count,lease_owner,lease_token,lease_expires_at,invalid_output_count,
             created_at,updated_at)
         VALUES
            ('reset-memory-job',?,?,'reset-memory-turn','v1',0,0,1,
             '00000000000000000000000000000001','reset-input',0,'running',0,
             'stale-worker','stale-reset-lease',?,0,?,?)",
    )
    .bind(user_id)
    .bind(&conversation_id)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .execute(services.database.pool())
    .await
    .unwrap();

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/conversations/{conversation_id}/reset"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(&conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let summaries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_memories WHERE user_id = ? AND conversation_id = ?")
            .bind(user_id)
            .bind(&conversation_id)
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entries WHERE id = 'reset-memory-entry'")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let sources: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_sources WHERE conversation_id = ?")
        .bind(&conversation_id)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let job: (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT state,lease_owner,lease_token FROM memory_jobs WHERE id = 'reset-memory-job'")
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    let policy: (Option<i64>, i64) = sqlx::query_as(
        "SELECT reset_at,lifecycle_epoch FROM conversation_memory_policies
         WHERE user_id = ? AND conversation_id = ?",
    )
    .bind(user_id)
    .bind(&conversation_id)
    .fetch_one(services.database.pool())
    .await
    .unwrap();

    assert_eq!(messages, 0);
    assert_eq!(summaries, 0);
    assert_eq!(entries, 0);
    assert_eq!(sources, 0);
    assert_eq!(job, ("canceled".into(), None, None));
    assert!(policy.0.is_some());
    assert_eq!(policy.1, 1);
    assert_eq!(
        services
            .memory_service
            .renew_job_lease(user_id, "reset-memory-job", "stale-worker", "stale-reset-lease", 30_000,)
            .await,
        Err(aionui_memory::MemoryError::LeaseLost),
    );
}

#[tokio::test]
async fn app_startup_recovers_expired_running_job_into_its_successor_once() {
    use aionui_db::IConversationRepository;

    let database = aionui_db::init_database_memory().await.unwrap();
    let conversations = aionui_db::SqliteConversationRepository::new(database.pool().clone());
    conversations
        .create(&aionui_db::models::ConversationRow {
            id: "recovery-conversation".into(),
            user_id: "system_default_user".into(),
            name: "Recovery".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory_jobs
            (id,user_id,conversation_id,from_turn_id,through_turn_id,operation_version,
             global_epoch,conversation_epoch,turn_count,queue_digest,input_hash,expected_revision,
             state,attempt_count,lease_owner,lease_token,lease_expires_at,invalid_output_count,
             created_at,updated_at)
         VALUES
            ('expired-running','system_default_user','recovery-conversation',NULL,'turn-1','v1',
             0,0,1,'00000000000000000000000000000001','running-input',0,
             'running',0,'old-worker','old-lease',0,0,1,1),
            ('pending-successor','system_default_user','recovery-conversation','turn-1','turn-3','v1',
             0,0,2,'00000000000000000000000000000002','successor-input',0,
             'pending',0,NULL,NULL,NULL,0,2,2)",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_job_turns
            (job_id,user_id,conversation_id,operation_version,position,turn_id,turn_hash)
         VALUES
            ('expired-running','system_default_user','recovery-conversation','v1',0,'turn-1','hash-1'),
            ('pending-successor','system_default_user','recovery-conversation','v1',0,'turn-2','hash-2'),
            ('pending-successor','system_default_user','recovery-conversation','v1',1,'turn-3','hash-3')",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let services = aionui_app::AppServices::from_config(database, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let old_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_jobs WHERE id = 'expired-running')")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let successor: (String, i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state,turn_count,lease_owner,lease_token FROM memory_jobs WHERE id = 'pending-successor'",
    )
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    let turns: Vec<String> =
        sqlx::query_scalar("SELECT turn_id FROM memory_job_turns WHERE job_id = 'pending-successor' ORDER BY position")
            .fetch_all(services.database.pool())
            .await
            .unwrap();

    assert!(!old_exists);
    assert_eq!(successor, ("pending".into(), 3, None, None));
    assert_eq!(turns, ["turn-1", "turn-2", "turn-3"]);
}
