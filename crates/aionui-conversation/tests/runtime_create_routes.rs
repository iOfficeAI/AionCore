//! Route-level contract for `POST /api/runtime/conversations/create`:
//! runtime-token auth, envelope error codes + HTTP statuses, and the happy path
//! through a real in-memory DB and a real `ConversationService`.

use std::sync::{Arc, Mutex};

use aionui_ai_agent::agent_task::AgentInstance;
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_ai_agent::{AgentError, IWorkerTaskManager, RuntimeTokenScope, RuntimeTokenService};
use aionui_api_types::WebSocketMessage;
use aionui_common::{AgentKillReason, TimestampMs};
use aionui_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use aionui_conversation::{ConversationRuntimeRouterState, ConversationService, conversation_runtime_routes};
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantPreferenceRepository, SqliteConversationRepository, init_database_memory,
};
use aionui_realtime::EventBroadcaster;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

const USER: &str = "user_1";
const OTHER_USER: &str = "user_2";
const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";

#[derive(Default)]
struct RecordingBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl RecordingBroadcaster {
    fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        self.events.lock().unwrap().clone()
    }
}

impl EventBroadcaster for RecordingBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoSkills;

#[async_trait::async_trait]
impl SkillResolver for NoSkills {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }
}

/// `create` never builds an agent, so nothing here is reachable; it exists only
/// because `ConversationService::new` takes a task manager.
struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(&self, _: &str, _: BuildTaskOptions) -> Result<AgentInstance, AgentError> {
        Err(AgentError::internal("noop"))
    }

    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        0
    }

    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

struct Ctx {
    router: Router,
    repo: Arc<SqliteConversationRepository>,
    broadcaster: Arc<RecordingBroadcaster>,
    tokens: Arc<RuntimeTokenService>,
    workspace: String,
}

async fn setup() -> Ctx {
    let db = init_database_memory().await.unwrap();
    for user in [USER, OTHER_USER] {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(user)
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let pool = db.pool().clone();
    // Leaked so the shared in-memory pool outlives the test (same as the
    // session-message harness).
    std::mem::forget(db);

    let repo = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let broadcaster = Arc::new(RecordingBroadcaster::default());
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let service = ConversationService::new(
        std::env::temp_dir().join("aionui-runtime-create-routes-root"),
        broadcaster.clone(),
        Arc::new(NoSkills),
        task_manager,
        repo.clone(),
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(pool.clone())),
    );
    // Assistant repos so an unknown `assistant_id` answers with the contract
    // code (404) rather than "repositories not configured".
    service.with_assistant_definition_repo(Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone())));
    service.with_assistant_state_repo(Arc::new(SqliteAssistantOverlayRepository::new(pool.clone())));
    service.with_assistant_preference_repo(Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone())));

    let tokens = Arc::new(RuntimeTokenService::new());
    let router = conversation_runtime_routes(ConversationRuntimeRouterState {
        service,
        runtime_token_service: tokens.clone(),
    });
    let workspace = std::env::temp_dir().join("aionui-runtime-create-routes-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    Ctx {
        router,
        repo,
        broadcaster,
        tokens,
        workspace: workspace.to_string_lossy().into_owned(),
    }
}

impl Ctx {
    async fn insert_row(&self, user_id: &str, id: &str, extra: serde_json::Value) {
        self.repo
            .create(&ConversationRow {
                id: id.to_owned(),
                user_id: user_id.to_owned(),
                name: "caller".to_owned(),
                r#type: "acp".to_owned(),
                extra: extra.to_string(),
                model: None,
                status: Some("finished".to_owned()),
                source: Some("aionui".to_owned()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();
    }

    async fn caller(&self, id: &str) {
        self.insert_row(
            USER,
            id,
            serde_json::json!({ "workspace": self.workspace, "backend": "claude" }),
        )
        .await;
    }

    fn mint(&self, user_id: &str, conversation_id: &str) -> String {
        self.tokens
            .issue(
                user_id,
                conversation_id,
                aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
                [RuntimeTokenScope::ConversationHelper],
            )
            .token
    }
}

fn create_request(user_id: &str, conversation_id: &str, token: Option<&str>, body: &str) -> Request<Body> {
    // Deliberately no `x-csrf-token`: the runtime channel is mounted outside
    // the CSRF layer in aionui-app, and this router has none either.
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/runtime/conversations/create")
        .header("content-type", "application/json")
        .header(HEADER_USER_ID, user_id)
        .header(HEADER_CONVERSATION_ID, conversation_id);
    if let Some(token) = token {
        builder = builder.header(HEADER_RUNTIME_TOKEN, token);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn call(ctx: &Ctx, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = ctx.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, json_body(response).await)
}

// ── Security ────────────────────────────────────────────────────────

#[tokio::test]
async fn create_without_a_runtime_token_is_401() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    let (status, envelope) = call(&ctx, create_request(USER, "conv_a", None, r#"{"name":"x"}"#)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(envelope["success"], serde_json::json!(false));
    assert_eq!(envelope["error"]["code"], serde_json::json!("runtime_auth_failed"));
    assert_eq!(envelope["meta"]["command"], serde_json::json!("conversation create"));
}

#[tokio::test]
async fn a_forged_user_header_cannot_create_for_another_user() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    let token = ctx.mint(USER, "conv_a");
    let (status, envelope) = call(
        &ctx,
        create_request(OTHER_USER, "conv_a", Some(&token), r#"{"name":"x"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(envelope["error"]["code"], serde_json::json!("runtime_auth_failed"));
    assert_eq!(
        ctx.repo.list_all_conversation_ids().await.unwrap().len(),
        1,
        "nothing created"
    );
}

// ── Happy path ──────────────────────────────────────────────────────

#[tokio::test]
async fn create_inherits_the_callers_workspace_and_broadcasts_once() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    let token = ctx.mint(USER, "conv_a");

    let (status, envelope) = call(
        &ctx,
        create_request(USER, "conv_a", Some(&token), r#"{"name":"重构鉴权模块"}"#),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{envelope}");
    assert_eq!(envelope["success"], serde_json::json!(true));
    assert_eq!(envelope["meta"]["schema_version"], serde_json::json!(1));
    assert_eq!(envelope["meta"]["command"], serde_json::json!("conversation create"));
    assert_eq!(envelope["data"]["name"], serde_json::json!("重构鉴权模块"));
    assert_eq!(envelope["data"]["workspace"], serde_json::json!(ctx.workspace));
    let new_id = envelope["data"]["id"].as_str().unwrap();
    let row = ctx.repo.get(USER, new_id).await.unwrap().expect("row persisted");
    assert_eq!(row.source.as_deref(), Some("aionui"));
    assert!(row.name_source.is_none());
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert_eq!(extra["backend"], serde_json::json!("claude"));

    let list_changed: Vec<_> = ctx
        .broadcaster
        .events()
        .into_iter()
        .filter(|event| event.name == "conversation.listChanged")
        .collect();
    assert_eq!(list_changed.len(), 1);
    assert_eq!(list_changed[0].data["action"], serde_json::json!("created"));
    assert_eq!(list_changed[0].data["user_id"], serde_json::json!(USER));
}

#[tokio::test]
async fn create_with_an_explicit_workspace_persists_that_path() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    let token = ctx.mint(USER, "conv_a");
    let other = std::env::temp_dir().join("aionui-runtime-create-routes-other");
    std::fs::create_dir_all(&other).unwrap();
    let other = other.to_string_lossy().into_owned();
    let body = serde_json::json!({ "name": "x", "workspace": other }).to_string();

    let (status, envelope) = call(&ctx, create_request(USER, "conv_a", Some(&token), &body)).await;

    assert_eq!(status, StatusCode::OK, "{envelope}");
    assert_eq!(envelope["data"]["workspace"], serde_json::json!(other));
}

// ── Bad paths (code + status) ────────────────────────────────────────

async fn assert_rejected(ctx: &Ctx, conversation_id: &str, body: &str, status: StatusCode, code: &str) {
    let token = ctx.mint(USER, conversation_id);
    let (got_status, envelope) = call(ctx, create_request(USER, conversation_id, Some(&token), body)).await;
    assert_eq!(got_status, status, "{envelope}");
    assert_eq!(envelope["success"], serde_json::json!(false));
    assert_eq!(envelope["error"]["code"], serde_json::json!(code), "{envelope}");
    assert!(envelope.get("data").is_none(), "{envelope}");
}

#[tokio::test]
async fn a_team_caller_is_403_caller_is_team() {
    let ctx = setup().await;
    ctx.insert_row(USER, "conv_team", serde_json::json!({ "teamId": "team-1" }))
        .await;
    assert_rejected(
        &ctx,
        "conv_team",
        r#"{"name":"x"}"#,
        StatusCode::FORBIDDEN,
        "caller_is_team",
    )
    .await;
}

#[tokio::test]
async fn a_blank_name_is_400_schema_validation_failed() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    assert_rejected(
        &ctx,
        "conv_a",
        r#"{"name":"   "}"#,
        StatusCode::BAD_REQUEST,
        "schema_validation_failed",
    )
    .await;
}

#[tokio::test]
async fn an_unknown_body_field_or_malformed_json_is_400_schema_validation_failed() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    assert_rejected(
        &ctx,
        "conv_a",
        r#"{"name":"x","files":[]}"#,
        StatusCode::BAD_REQUEST,
        "schema_validation_failed",
    )
    .await;
    assert_rejected(
        &ctx,
        "conv_a",
        r#"not json"#,
        StatusCode::BAD_REQUEST,
        "schema_validation_failed",
    )
    .await;
}

#[tokio::test]
async fn workspace_errors_are_422_with_distinct_codes() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    assert_rejected(
        &ctx,
        "conv_a",
        r#"{"name":"x","workspace":"src/lib"}"#,
        StatusCode::UNPROCESSABLE_ENTITY,
        "workspace_not_absolute",
    )
    .await;
    let missing = std::env::temp_dir().join("aionui-runtime-create-routes-missing-7d1e");
    let body = serde_json::json!({ "name": "x", "workspace": missing }).to_string();
    assert_rejected(
        &ctx,
        "conv_a",
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "workspace_unavailable",
    )
    .await;
}

#[tokio::test]
async fn an_unknown_assistant_is_404_assistant_not_found() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    assert_rejected(
        &ctx,
        "conv_a",
        r#"{"name":"x","assistant_id":"nope"}"#,
        StatusCode::NOT_FOUND,
        "assistant_not_found",
    )
    .await;
}

#[tokio::test]
async fn a_deleted_caller_is_503_transport_unavailable() {
    let ctx = setup().await;
    ctx.caller("conv_a").await;
    let token = ctx.mint(USER, "conv_a");
    ctx.repo.delete(USER, "conv_a").await.unwrap();
    let (status, envelope) = call(&ctx, create_request(USER, "conv_a", Some(&token), r#"{"name":"x"}"#)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{envelope}");
    assert_eq!(envelope["error"]["code"], serde_json::json!("transport_unavailable"));
}
