use std::sync::Arc;

use aionui_auth::CurrentUser;
use aionui_realtime::BroadcastEventBus;
use aionui_system::{
    ClientPrefService, FeedbackDiagnosticsService, ModelFetchService, ProtocolDetectionService, ProviderService,
    RuntimePrepareService, SettingsService, SystemRouterState, VersionCheckService, system_routes,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use aionui_db::{
    SqliteClientPreferenceRepository, SqliteFeedbackDiagnosticsRepository, SqliteProviderRepository,
    SqliteSettingsRepository, init_database_memory,
};

const TEST_ENCRYPTION_KEY: [u8; 32] = [0x42; 32];

fn build_state(db: &aionui_db::Database) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_ENCRYPTION_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_ENCRYPTION_KEY, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(Arc::new(BroadcastEventBus::new(16))),
        feedback_diagnostics_service: FeedbackDiagnosticsService::new(Arc::new(
            SqliteFeedbackDiagnosticsRepository::new(db.pool().clone()),
        )),
    }
}

async fn setup() -> (axum::Router, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    insert_fixture(&db).await;
    (system_routes(build_state(&db)), db)
}

async fn insert_fixture(db: &aionui_db::Database) {
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO providers \
            (id, platform, name, base_url, api_key_encrypted, models, enabled, \
             capabilities, context_limit, model_protocols, model_enabled, model_health, \
             bedrock_config, is_full_url, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("prov-route")
    .bind("openrouter")
    .bind("OpenRouter")
    .bind("https://sk-route-secret@example.invalid/v1")
    .bind("encrypted-sk-route-secret")
    .bind(r#"["sakana/fugu-ultra"]"#)
    .bind(true)
    .bind(r#"[{"type":"text"}]"#)
    .bind(128000_i64)
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(false)
    .bind(1000_i64)
    .bind(2000_i64)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, model, status, source, channel_chat_id, \
             pinned, pinned_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("conv-route")
    .bind("system_default_user")
    .bind("Route-selected conversation")
    .bind("aionrs")
    .bind(json!({"agentId":"opencode"}).to_string())
    .bind(json!({"providerId":"prov-route","modelId":"sakana/fugu-ultra"}).to_string())
    .bind("running")
    .bind("aionui")
    .bind(None::<String>)
    .bind(false)
    .bind(None::<i64>)
    .bind(3000_i64)
    .bind(4000_i64)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages \
            (id, conversation_id, msg_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-route-error")
    .bind("conv-route")
    .bind("turn-route")
    .bind("tips")
    .bind(
        json!({
            "error": {
                "code": "RouteProviderAuthFailed",
                "message": "Route raw error should not leak sk-route-message-secret"
            },
            "feedbackRecommended": true
        })
        .to_string(),
    )
    .bind("center")
    .bind("error")
    .bind(false)
    .bind(5000_i64)
    .execute(pool)
    .await
    .unwrap();
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn diagnostics_request(uri: &str) -> Request<Body> {
    let mut req = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
    req.extensions_mut().insert(CurrentUser {
        id: "system_default_user".to_owned(),
        username: "system_default_user".to_owned(),
    });
    req
}

#[tokio::test]
async fn diagnostics_uses_route_context_and_unions_selected_module_profiles() {
    let (app, _db) = setup().await;
    let resp = app
        .oneshot(diagnostics_request(
            "/api/system/diagnostics/feedback-report?route_at_submit=%23%2Fconversations%2Fconv-route&selected_module=system-settings",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["context"]["conversation_id"], "conv-route");
    assert_eq!(json["data"]["profiles"][0]["name"], "conversation-session");
    let profile_names = json["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|profile| profile["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(profile_names.contains(&"conversation-session"));
    assert!(profile_names.contains(&"model-auth"));
    assert!(profile_names.contains(&"mcp-tools"));
    assert!(profile_names.contains(&"global-summary"));

    let serialized = serde_json::to_string(&json).unwrap();
    assert!(!serialized.contains("sk-route-secret"));
    assert!(!serialized.contains("encrypted-sk-route-secret"));
}

#[tokio::test]
async fn diagnostics_privacy_statement_matches_included_raw_diagnostics() {
    let (app, _db) = setup().await;
    let resp = app
        .oneshot(diagnostics_request(
            "/api/system/diagnostics/feedback-report?route_at_submit=%23%2Fguid&selected_module=conversation-session",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["privacy"]["raw_content_included"], true);
    assert_eq!(json["data"]["privacy"]["api_keys_included"], false);

    let redaction = json["data"]["privacy"]["redaction"]
        .as_str()
        .expect("privacy redaction should be text");
    assert!(redaction.contains("raw error and tool-call diagnostic content may be included"));
    assert!(redaction.contains("MCP original_json keeps connection structure"));
    assert!(redaction.contains("credential values are redacted"));
    assert!(redaction.contains("non-error message content and prompts are summarized or redacted"));
}

#[tokio::test]
async fn diagnostics_home_route_uses_module_and_global_summary_context() {
    let (app, _db) = setup().await;
    let resp = app
        .oneshot(diagnostics_request(
            "/api/system/diagnostics/feedback-report?route_at_submit=%23%2Fguid&selected_module=agent-detection",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"]["context"]["conversation_id"].is_null());

    let profile_names = json["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|profile| profile["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(profile_names.contains(&"conversation-session"));
    assert!(profile_names.contains(&"model-auth"));
    assert!(profile_names.contains(&"mcp-tools"));
    assert!(profile_names.contains(&"global-summary"));

    let global = json["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["name"] == "global-summary")
        .expect("global summary should be included");
    assert_eq!(
        global["data"]["recent_conversations"]["direct"]["items"][0]["title"],
        "Route-selected conversation"
    );
    assert_eq!(
        global["data"]["recent_conversations"]["direct"]["items"][0]["recent_errors"][0]["content"]["error"]["message"],
        "Route raw error should not leak sk-route-message-secret"
    );
    assert_eq!(
        global["data"]["recent_errors"]["items"][0]["code"],
        "RouteProviderAuthFailed"
    );
    assert_eq!(
        global["data"]["recent_errors"]["items"][0]["content"]["error"]["message"],
        "Route raw error should not leak sk-route-message-secret"
    );

    let serialized = serde_json::to_string(&json).unwrap();
    assert!(!serialized.contains("sk-route-secret"));
    assert!(!serialized.contains("encrypted-sk-route-secret"));
}
