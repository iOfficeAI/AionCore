//! Shared harness for the cross-session delivery integration tests.
//!
//! Real in-memory DB + a real `ConversationService`, because the whole point of
//! this feature is that delivery goes through the human send path. Only the
//! agent process itself is faked — via `AgentInstance::Mock`, the `test-support`
//! trait-object escape hatch — since spawning a real CLI is neither possible
//! nor relevant here.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use aionui_ai_agent::protocol::events::{AgentStreamEvent, FinishEventData};
use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentError, AgentSendError, IWorkerTaskManager, RuntimeTokenService};
use aionui_api_types::WebSocketMessage;
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, TimestampMs};
use aionui_conversation::ConversationService;
use aionui_conversation::runtime_state::TurnClaim;
use aionui_conversation::skill_resolver::{ResolvedAgentSkill, SkillResolver};
use aionui_db::models::ConversationRow;
use aionui_db::{
    IConversationRepository, ISettingsRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    SqliteConversationRepository, SqliteProjectStore, SqliteSettingsRepository, init_database_memory,
};
use aionui_project::ProjectService;
use aionui_realtime::EventBroadcaster;
use aionui_session_message::drainer::Drainer;
use aionui_session_message::queue::{DeliveryQueue, SystemClock, TestClock};
use aionui_session_message::rate_limit::RateLimiter;
use aionui_session_message::service::{SessionMessageDeps, SessionMessageService};
use aionui_session_message::state::SessionMessageRouterState;
use aionui_session_message::targets::MentionableTargets;
use tokio::sync::{Notify, broadcast};

pub const USER: &str = "user_1";
pub const OTHER_USER: &str = "user_2";

// ── Recording broadcaster ───────────────────────────────────────────

#[derive(Default)]
pub struct RecordingBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl RecordingBroadcaster {
    pub fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        self.events.lock().unwrap().clone()
    }

    pub fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        std::mem::take(&mut self.events.lock().unwrap())
    }

    pub fn find(&self, name: &str) -> Option<WebSocketMessage<serde_json::Value>> {
        self.events().into_iter().find(|event| event.name == name)
    }
}

impl EventBroadcaster for RecordingBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

// ── Skill resolver stub ─────────────────────────────────────────────

pub struct NoSkills;

#[async_trait::async_trait]
impl SkillResolver for NoSkills {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &Path,
        _rel_dirs: &[&str],
        _skills: &[ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

// ── Fake agent ──────────────────────────────────────────────────────

/// A fake agent whose mid-turn capability and pending-confirmation state are
/// both settable, because those are exactly the two inputs `send_message`
/// consults when deciding between mid-turn delivery and a 409.
pub struct FakeAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    supports_midturn: bool,
    confirmations: Mutex<Vec<Confirmation>>,
    pub delivered_midturn: Mutex<Vec<SendMessageData>>,
    pub sent: Mutex<Vec<SendMessageData>>,
}

impl FakeAgent {
    pub fn new(conversation_id: &str, supports_midturn: bool) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            supports_midturn,
            confirmations: Mutex::new(Vec::new()),
            delivered_midturn: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
        }
    }

    pub fn with_pending_confirmation(self, confirmation: Confirmation) -> Self {
        self.confirmations.lock().unwrap().push(confirmation);
        self
    }

    /// Answer the card, so the next attempt takes the mid-turn branch.
    pub fn resolve_confirmations(&self) {
        self.confirmations.lock().unwrap().clear();
    }
}

#[async_trait::async_trait]
impl IAgentTask for FakeAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> TimestampMs {
        aionui_common::now_ms()
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    fn supports_midturn_delivery(&self) -> bool {
        self.supports_midturn
    }
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.sent.lock().unwrap().push(data);
        // Finish so the relay of a newly-opened turn completes and the claim is
        // released — otherwise the conversation stays busy forever and every
        // later assertion in the same test would be measuring the wrong thing.
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for FakeAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }
    async fn deliver_midturn(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.delivered_midturn.lock().unwrap().push(data);
        Ok(())
    }
}

// ── Task manager ────────────────────────────────────────────────────

#[derive(Default)]
pub struct StubTaskManager {
    agents: Mutex<HashMap<String, AgentInstance>>,
}

impl StubTaskManager {
    pub fn insert(&self, conversation_id: &str, agent: AgentInstance) {
        self.agents.lock().unwrap().insert(conversation_id.to_owned(), agent);
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for StubTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        // An idle target has no agent yet; build one that immediately finishes,
        // which is what an ordinary new-turn send needs.
        let existing = self.get_task(conversation_id);
        if let Some(agent) = existing {
            return Ok(agent);
        }
        let agent = AgentInstance::Mock(Arc::new(FakeAgent::new(conversation_id, false)));
        self.insert(conversation_id, agent.clone());
        Ok(agent)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

// ── Harness ─────────────────────────────────────────────────────────

pub struct Ctx {
    pub service: Arc<SessionMessageService>,
    pub targets: Arc<MentionableTargets>,
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<SqliteConversationRepository>,
    pub settings_repo: Arc<SqliteSettingsRepository>,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub stub_task_manager: Arc<StubTaskManager>,
    pub broadcaster: Arc<RecordingBroadcaster>,
    pub queue: Arc<DeliveryQueue>,
    pub clock: Arc<TestClock>,
    pub notify: Arc<Notify>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
    /// Kept so the whole graph can be rebuilt for a drainer in a test.
    pub rate_limiter: Arc<RateLimiter>,
}

/// `TestClock` for the queue/TTL and the rate window, so no test ever sleeps.
pub async fn setup() -> Ctx {
    setup_with_clock(Arc::new(TestClock::new(1_000))).await
}

pub async fn setup_with_clock(clock: Arc<TestClock>) -> Ctx {
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
    // Leaked so the shared in-memory pool outlives the test, matching the team
    // integration tests.
    std::mem::forget(db);

    let conversation_repo = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let settings_repo = Arc::new(SqliteSettingsRepository::new(pool.clone()));
    let broadcaster = Arc::new(RecordingBroadcaster::default());
    let stub_task_manager = Arc::new(StubTaskManager::default());
    let task_manager: Arc<dyn IWorkerTaskManager> = stub_task_manager.clone();

    let conversation_service = ConversationService::new(
        std::env::temp_dir().join("aionui-session-message-test-workspaces"),
        broadcaster.clone(),
        Arc::new(NoSkills),
        task_manager.clone(),
        conversation_repo.clone(),
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
    );

    let queue = Arc::new(DeliveryQueue::new(clock.clone()));
    let rate_limiter = Arc::new(RateLimiter::new(clock.clone()));
    let notify = Arc::new(Notify::new());
    let service = Arc::new(SessionMessageService::new(SessionMessageDeps {
        conversation_service: conversation_service.clone(),
        conversation_repo: conversation_repo.clone(),
        settings_repo: settings_repo.clone(),
        task_manager: task_manager.clone(),
        broadcaster: broadcaster.clone(),
        queue: queue.clone(),
        rate_limiter: rate_limiter.clone(),
        notify: notify.clone(),
    }));

    let project_service = Arc::new(ProjectService::new(
        Arc::new(SqliteProjectStore::new(pool.clone())),
        std::env::temp_dir().join("aionui-session-message-test-projects"),
    ));

    Ctx {
        service,
        targets: Arc::new(MentionableTargets::new(conversation_repo.clone(), project_service)),
        conversation_service,
        conversation_repo,
        settings_repo,
        task_manager,
        stub_task_manager,
        broadcaster,
        queue,
        clock,
        notify,
        runtime_token_service: Arc::new(RuntimeTokenService::new()),
        rate_limiter,
    }
}

impl Ctx {
    /// A drainer over the same queue and the same service, so a test can advance
    /// delivery by hand.
    pub fn drainer(&self) -> Drainer {
        Drainer::new(self.queue.clone(), self.service.clone(), self.service.clone())
    }

    /// The state the routers are built from, sharing this harness's service.
    pub fn router_state(&self) -> SessionMessageRouterState {
        SessionMessageRouterState {
            service: self.service.clone(),
            targets: self.targets.clone(),
            runtime_token_service: self.runtime_token_service.clone(),
        }
    }

    pub async fn create_conversation(&self, id: &str, name: &str, workspace: &str) -> ConversationRow {
        self.insert_row(USER, id, name, serde_json::json!({ "workspace": workspace }))
            .await
    }

    pub async fn create_conversation_for(
        &self,
        user_id: &str,
        id: &str,
        name: &str,
        workspace: &str,
    ) -> ConversationRow {
        self.insert_row(user_id, id, name, serde_json::json!({ "workspace": workspace }))
            .await
    }

    pub async fn create_team_conversation(&self, id: &str, team_id: &str) -> ConversationRow {
        self.insert_row(USER, id, "team chat", serde_json::json!({ "teamId": team_id }))
            .await
    }

    async fn insert_row(&self, user_id: &str, id: &str, name: &str, extra: serde_json::Value) -> ConversationRow {
        let row = ConversationRow {
            id: id.to_owned(),
            user_id: user_id.to_owned(),
            name: name.to_owned(),
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
        };
        self.conversation_repo.create(&row).await.unwrap();
        row
    }

    pub async fn delete_conversation(&self, id: &str) {
        self.conversation_repo.delete(USER, id).await.unwrap();
    }

    pub async fn mark_conversation_team(&self, id: &str, team_id: &str) {
        self.conversation_repo
            .update(
                USER,
                id,
                &aionui_db::ConversationRowUpdate {
                    extra: Some(serde_json::json!({ "teamId": team_id }).to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    pub async fn disable_feature(&self, user_id: &str) {
        self.settings_repo
            .upsert_settings(user_id, "en-US", true, false, false, false, false)
            .await
            .unwrap();
    }

    /// Make `id` busy with an active turn, backed by a fake agent whose
    /// mid-turn capability is `supports_midturn`. Returns the claim (which must
    /// stay alive to hold the conversation busy) and the agent.
    pub fn make_busy(&self, id: &str, supports_midturn: bool) -> (TurnClaim, Arc<FakeAgent>) {
        let claim = self
            .conversation_service
            .runtime_state()
            .try_claim_turn(id, "turn_active")
            .expect("claim the active turn");
        let agent = Arc::new(FakeAgent::new(id, supports_midturn));
        self.stub_task_manager.insert(id, AgentInstance::Mock(agent.clone()));
        (claim, agent)
    }

    /// Busy AND mid-turn-capable AND blocked on a permission card — the
    /// combination that must still queue (spec §6.4's second Busy source).
    pub fn make_busy_awaiting_confirmation(&self, id: &str) -> (TurnClaim, Arc<FakeAgent>) {
        let claim = self
            .conversation_service
            .runtime_state()
            .try_claim_turn(id, "turn_active")
            .expect("claim the active turn");
        let agent = Arc::new(FakeAgent::new(id, true).with_pending_confirmation(Confirmation {
            id: "confirm_1".to_owned(),
            call_id: "call_1".to_owned(),
            title: Some("Allow shell?".to_owned()),
            action: Some("shell".to_owned()),
            description: "run a command".to_owned(),
            command_type: None,
            options: Vec::new(),
            questions: None,
        }));
        self.stub_task_manager.insert(id, AgentInstance::Mock(agent.clone()));
        (claim, agent)
    }

    pub fn active_turn_id(&self, id: &str) -> Option<String> {
        self.conversation_service.runtime_state().active_turn_id_for(id)
    }

    /// Burn the pair gate so the next `send` to `to` trips it.
    pub fn exhaust_rate_gate(&self, from: &str, to: &str) {
        for _ in 0..aionui_session_message::rate_limit::PAIR_LIMIT {
            let _ = self.rate_limiter.check_and_record(from, to);
        }
    }

    /// The last user message's text for `id`, read back out of the JSON
    /// envelope the message row stores.
    pub async fn last_user_message_content(&self, id: &str) -> String {
        let page = self
            .conversation_repo
            .list_messages_page(
                USER,
                id,
                &aionui_db::MessagePageParams {
                    limit: 50,
                    direction: aionui_db::MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        let row = page
            .items
            .iter()
            .rfind(|m| m.position.as_deref() == Some("right"))
            .expect("a user message was persisted");
        let envelope: serde_json::Value =
            serde_json::from_str(&row.content).expect("message content is a JSON envelope");
        envelope["content"].as_str().unwrap_or_default().to_owned()
    }

    /// Every user message's text. Used instead of "the last one" wherever the
    /// assertion would otherwise depend on how the DB breaks a timestamp tie.
    pub async fn all_user_message_contents(&self, id: &str) -> Vec<String> {
        let page = self
            .conversation_repo
            .list_messages_page(
                USER,
                id,
                &aionui_db::MessagePageParams {
                    limit: 200,
                    direction: aionui_db::MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        page.items
            .iter()
            .filter(|m| m.position.as_deref() == Some("right"))
            .map(|row| {
                let envelope: serde_json::Value =
                    serde_json::from_str(&row.content).expect("message content is a JSON envelope");
                envelope["content"].as_str().unwrap_or_default().to_owned()
            })
            .collect()
    }

    pub async fn user_message_count(&self, id: &str) -> usize {
        self.user_message_count_for(USER, id).await
    }

    /// Same count, for a conversation owned by someone else — the repo scopes by
    /// user, so the caller must name the owner.
    pub async fn user_message_count_for(&self, user_id: &str, id: &str) -> usize {
        let page = self
            .conversation_repo
            .list_messages_page(
                user_id,
                id,
                &aionui_db::MessagePageParams {
                    limit: 200,
                    direction: aionui_db::MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap();
        page.items
            .iter()
            .filter(|m| m.position.as_deref() == Some("right"))
            .count()
    }
}

/// Marker so `SystemClock` stays referenced from the harness's imports without
/// a dead-code warning when a test file does not use it.
pub fn system_clock() -> Arc<SystemClock> {
    Arc::new(SystemClock)
}

/// Kept for tests that need to flip a gate mid-run.
pub struct TogglableGate {
    enabled: AtomicBool,
}

impl TogglableGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
        }
    }

    pub fn set(&self, enabled: bool) {
        self.enabled.store(enabled, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl aionui_session_message::drainer::DrainGate for TogglableGate {
    async fn is_enabled_for(&self, _user_id: &str) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::SeqCst)
    }
}
