//! Shared test helpers for aionui-app E2E tests.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::MockServer;

use aionui_ai_agent::{AgentInstance, IAgentTask, IMockAgent, WorkerTaskManagerImpl};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router, create_router_with_states};
use aionui_extension::{ExternalPathsManager, SkillPaths, SkillRouterState};
use aionui_file::FileService;
use aionui_system::VersionCheckService;

pub async fn build_app() -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let router = create_router(&services).await.expect("build router");
    (router, services)
}

/// Build an app whose skill router uses the given temp directories.
///
/// Use for HTTP integration tests that need deterministic on-disk layouts
/// (E1 `/api/skills`, E3/E4 built-in reads, E5 `/api/skills/info`).
/// Returns the router, services, and the `SkillPaths` so the test can seed
/// fixtures at known locations. `/api/skills` reads the database only; tests
/// that seed skill directories should call `sync_skill_catalog_for_test`.
#[allow(dead_code)]
pub async fn build_app_with_skill_paths(root: &std::path::Path) -> (axum::Router, AppServices, SkillPaths) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: root.to_path_buf(),
        work_dir: root.to_path_buf(),
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    sqlx::query("DELETE FROM skill_import_records")
        .execute(services.database.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM skills")
        .execute(services.database.pool())
        .await
        .unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");

    let builtin_dir = root.join("builtin-skills");
    let paths = SkillPaths {
        data_dir: root.to_path_buf(),
        user_skills_dir: root.join("skills"),
        cron_skills_dir: root.join("cron").join("skills"),
        builtin_skills_dir: builtin_dir.clone(),
        builtin_rules_dir: root.join("builtin-rules"),
        assistant_rules_dir: root.join("assistant-rules"),
        assistant_skills_dir: root.join("assistant-skills"),
    };
    for dir in [
        &paths.user_skills_dir,
        &builtin_dir,
        &paths.builtin_rules_dir,
        &paths.assistant_rules_dir,
        &paths.assistant_skills_dir,
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let ext_paths_mgr = std::sync::Arc::new(ExternalPathsManager::with_file(root.join("paths.json")).await);
    states.skill = SkillRouterState {
        skill_paths: paths.clone(),
        skill_repo: std::sync::Arc::new(aionui_db::SqliteSkillRepository::new(services.database.pool().clone())),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: states.skill.assistant_dispatcher.clone(),
    };

    let router = create_router_with_states(&services, states);
    (router, services, paths)
}

pub async fn sync_skill_catalog_for_test(services: &AppServices, paths: &SkillPaths) {
    let repo = aionui_db::SqliteSkillRepository::new(services.database.pool().clone());
    aionui_extension::sync_skill_catalog_into_repo(paths, &repo)
        .await
        .expect("sync skill catalog");
}

pub async fn build_app_with_noop_opener() -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");
    states.shell.shell_service = std::sync::Arc::new(aionui_shell::ShellService::new(std::sync::Arc::new(
        aionui_shell::NoopSystemOpener,
    )));
    let router = create_router_with_states(&services, states);
    (router, services)
}

pub async fn build_app_with_file_roots(allowed_roots: Vec<std::path::PathBuf>) -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");
    states.file.file_service = std::sync::Arc::new(FileService::new(services.event_bus.clone(), allowed_roots));
    let router = create_router_with_states(&services, states);
    (router, services)
}

pub async fn build_app_with_mock_version(
    current_version: &str,
    mock_server: &MockServer,
) -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");
    states.system.version_check_service =
        VersionCheckService::with_api_base(reqwest::Client::new(), current_version.to_owned(), mock_server.uri());
    let router = create_router_with_states(&services, states);
    (router, services)
}

/// Build app with a mock worker task manager that returns noop agents.
///
/// Use for tests that exercise runtime preparation paths (team ensure_session,
/// send_message) where spawning a real CLI process is not feasible.
pub async fn build_app_with_mock_agents() -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let factory: std::sync::Arc<
        dyn Fn(
                aionui_ai_agent::types::BuildTaskOptions,
            )
                -> futures_util::future::BoxFuture<'static, Result<AgentInstance, aionui_ai_agent::AgentError>>
            + Send
            + Sync,
    > = std::sync::Arc::new(|opts| {
        Box::pin(async move {
            Ok(AgentInstance::Mock(std::sync::Arc::new(NoopMockAgent {
                conversation_id: opts.conversation_id().to_owned(),
            })))
        })
    });
    let wtm: std::sync::Arc<dyn aionui_ai_agent::IWorkerTaskManager> =
        std::sync::Arc::new(WorkerTaskManagerImpl::new(factory));
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .unwrap()
        .with_worker_task_manager(wtm);
    let router = create_router(&services).await.expect("build router");
    (router, services)
}

struct NoopMockAgent {
    conversation_id: String,
}

#[async_trait::async_trait]
impl IAgentTask for NoopMockAgent {
    fn agent_type(&self) -> aionui_common::AgentType {
        aionui_common::AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<aionui_common::ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> aionui_common::TimestampMs {
        aionui_common::now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<aionui_ai_agent::AgentStreamEvent> {
        let (tx, _) = tokio::sync::broadcast::channel(1);
        tx.subscribe()
    }
    async fn send_message(
        &self,
        _data: aionui_ai_agent::types::SendMessageData,
    ) -> Result<(), aionui_ai_agent::AgentSendError> {
        Ok(())
    }
    async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<aionui_common::AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for NoopMockAgent {
    async fn get_config_options(
        &self,
    ) -> Result<aionui_api_types::GetConfigOptionsResponse, aionui_ai_agent::AgentError> {
        Ok(aionui_api_types::GetConfigOptionsResponse {
            config_options: vec![aionui_api_types::AcpConfigOptionDto {
                id: "model".to_owned(),
                name: Some("Model".to_owned()),
                label: None,
                description: None,
                category: Some("model".to_owned()),
                option_type: "select".to_owned(),
                current_value: Some("mock-model".to_owned()),
                options: vec![aionui_api_types::AcpConfigSelectOptionDto {
                    value: "mock-model".to_owned(),
                    name: Some("Mock Model".to_owned()),
                    label: Some("Mock Model".to_owned()),
                    description: None,
                }],
            }],
        })
    }

    async fn set_config_option(
        &self,
        _option_id: &str,
        _value: &str,
    ) -> Result<aionui_api_types::SetConfigOptionResponse, aionui_ai_agent::AgentError> {
        Ok(aionui_api_types::SetConfigOptionResponse {
            confirmation: aionui_api_types::ConfigOptionConfirmation::Observed,
            config_options: Some(vec![aionui_api_types::AcpConfigOptionDto {
                id: "model".to_owned(),
                name: Some("Model".to_owned()),
                label: None,
                description: None,
                category: Some("model".to_owned()),
                option_type: "select".to_owned(),
                current_value: Some("mock-model-updated".to_owned()),
                options: vec![aionui_api_types::AcpConfigSelectOptionDto {
                    value: "mock-model-updated".to_owned(),
                    name: Some("Mock Model Updated".to_owned()),
                    label: Some("Mock Model Updated".to_owned()),
                    description: None,
                }],
            }]),
        })
    }
}

pub async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn extract_csrf_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("aionui-csrf-token="))
        .map(|s| {
            s.strip_prefix("aionui-csrf-token=")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
}

pub fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

pub fn get_with_token(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

pub fn json_with_token(method_str: &str, uri: &str, body: serde_json::Value, token: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method(method_str)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub fn delete_with_token(uri: &str, token: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(Body::empty())
        .unwrap()
}

/// Set up a user and login, returning (session_token, csrf_token).
///
/// The seeded `system_default_user` row already uses `username = "admin"`; if
/// the test asks for that username, overwrite the seed row's empty credentials
/// in place instead of trying to INSERT a duplicate.
pub async fn setup_and_login(
    app: &mut axum::Router,
    services: &AppServices,
    username: &str,
    password: &str,
) -> (String, String) {
    let hash = aionui_auth::hash_password(password).unwrap();
    if username == "admin" {
        services
            .user_repo
            .set_system_user_credentials(username, &hash)
            .await
            .unwrap();
    } else {
        services.user_repo.create_user(username, &hash).await.unwrap();
    }

    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let csrf = extract_csrf_token(&resp).expect("CSRF cookie should be set");

    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");

    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_owned();

    (token, csrf)
}
