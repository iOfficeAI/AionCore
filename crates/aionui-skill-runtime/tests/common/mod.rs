//! Harness for the runtime skills domain.
//!
//! Real in-memory DB, real skill files on disk, real router — because everything
//! this crate does is "read what the snapshot allows", and a mocked repository
//! would let the allow-list pass by construction rather than by behaviour.
//!
//! Conversation rows are inserted directly instead of going through
//! `ConversationService`: this crate never writes them, and the heavier harness
//! would only add ways for a test to pass for the wrong reason.

#![allow(dead_code)]

use std::sync::Arc;

use aionui_ai_agent::runtime_token::{RuntimeTokenScope, RuntimeTokenService};
use aionui_api_types::SKILL_RUNTIME_SCHEMA_VERSION;
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, ISkillRepository, SqliteConversationRepository, SqliteSkillRepository,
    init_database_memory,
};
use aionui_extension::SkillPaths;
use aionui_skill_runtime::{SkillRuntimeRouterState, SkillRuntimeService, skill_runtime_routes};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

pub const SESSION_GENERATION: &str = "default";

pub struct TestHarness {
    _tmp: tempfile::TempDir,
    paths: Arc<SkillPaths>,
    pool: sqlx::SqlitePool,
    conversation_repo: Arc<dyn IConversationRepository>,
    token_service: Arc<RuntimeTokenService>,
    router: Router,
    next_id: std::sync::atomic::AtomicUsize,
}

impl TestHarness {
    pub async fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let paths = Arc::new(SkillPaths {
            data_dir: data_dir.clone(),
            user_skills_dir: data_dir.join("skills"),
            cron_skills_dir: data_dir.join("cron").join("skills"),
            builtin_skills_dir: data_dir.join("builtin-skills"),
            builtin_rules_dir: data_dir.join("builtin-rules"),
            assistant_rules_dir: data_dir.join("assistant-rules"),
            assistant_skills_dir: data_dir.join("assistant-skills"),
        });
        std::fs::create_dir_all(&paths.builtin_skills_dir).unwrap();
        std::fs::create_dir_all(&paths.user_skills_dir).unwrap();

        let db = init_database_memory().await.unwrap();
        let conversation_repo: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let skill_repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        let token_service = Arc::new(RuntimeTokenService::new());

        let service = Arc::new(SkillRuntimeService::new(
            conversation_repo.clone(),
            paths.clone(),
            skill_repo,
        ));
        let router = skill_runtime_routes(SkillRuntimeRouterState {
            service,
            runtime_token_service: token_service.clone(),
        });

        Self {
            _tmp: tmp,
            paths,
            pool: db.pool().clone(),
            conversation_repo,
            token_service,
            router,
            next_id: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// `conversations.user_id` is a foreign key, so a test user must exist before
    /// its conversations do. Idempotent so each test can name users freely.
    async fn ensure_user(&self, user_id: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO users \
             (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.paths.data_dir
    }

    /// Write a builtin skill so the catalog can discover it for any user.
    pub fn seed_skill(&self, name: &str) -> std::path::PathBuf {
        self.seed_skill_with_description(name, &format!("{name} description"))
    }

    pub fn seed_skill_with_description(&self, name: &str, description: &str) -> std::path::PathBuf {
        let dir = self.paths.builtin_skills_dir.join("auto-inject").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{name} body text"),
        )
        .unwrap();
        dir
    }

    pub fn seed_skill_with_reference(&self, name: &str, rel: &str, content: &str) -> std::path::PathBuf {
        let dir = self.seed_skill(name);
        let target = dir.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, content).unwrap();
        dir
    }

    /// Insert a conversation whose `extra.skills` snapshot is `skills`.
    pub async fn create_conversation(&self, user_id: &str, skills: &[&str]) -> String {
        self.ensure_user(user_id).await;
        let n = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("conv_{user_id}_{n}");
        let extra = serde_json::json!({ "skills": skills, "backend": "claude" });
        let row = ConversationRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            name: "test".to_owned(),
            r#type: "acp".to_owned(),
            extra: extra.to_string(),
            model: None,
            // NOT NULL in the schema despite the Option in the row model.
            status: Some("finished".to_owned()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
            name_source: None,
        };
        self.conversation_repo.create(&row).await.unwrap();
        id
    }

    fn token(&self, user_id: &str, conversation_id: &str) -> String {
        self.token_service
            .issue(
                user_id,
                conversation_id,
                SESSION_GENERATION,
                [RuntimeTokenScope::ConversationHelper],
            )
            .token
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    pub async fn get_raw(&self, user_id: &str, conversation_id: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .uri(uri)
            .header("x-aionui-user-id", user_id)
            .header("x-aionui-conversation-id", conversation_id)
            .header("x-aionui-runtime-token", self.token(user_id, conversation_id))
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    /// Same call, but with a token minted for a DIFFERENT conversation — the
    /// shape a compromised or confused agent would produce.
    pub async fn get_with_foreign_token(
        &self,
        user_id: &str,
        conversation_id: &str,
        token_conversation_id: &str,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .uri(uri)
            .header("x-aionui-user-id", user_id)
            .header("x-aionui-conversation-id", conversation_id)
            .header("x-aionui-runtime-token", self.token(user_id, token_conversation_id))
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    pub async fn get_raw_without_token(
        &self,
        user_id: &str,
        conversation_id: &str,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .uri(uri)
            .header("x-aionui-user-id", user_id)
            .header("x-aionui-conversation-id", conversation_id)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    pub async fn get_json(&self, user_id: &str, conversation_id: &str, uri: &str) -> serde_json::Value {
        let (status, body) = self.get_raw(user_id, conversation_id, uri).await;
        assert_eq!(status, StatusCode::OK, "expected 200 for {uri}, got {body}");
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["meta"]["schema_version"], SKILL_RUNTIME_SCHEMA_VERSION);
        body
    }
}
