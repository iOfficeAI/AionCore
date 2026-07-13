//! Shared application services for dependency injection.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AppConfig, derive_encryption_key};
use aionui_ai_agent::{
    AcpSessionSyncService, AcpSkillManager, ActiveLeaseRegistry, AgentFactoryDeps, AgentRegistry, IWorkerTaskManager,
    WorkerTaskManagerImpl, build_agent_factory,
};
use aionui_auth::{CookieConfig, JwtService, QrTokenStore, resolve_jwt_secret};
use aionui_common::OnConversationDelete;
use aionui_conversation::{ConversationService, runtime_state::ConversationRuntimeStateService};
use aionui_db::{
    Database, IAcpSessionRepository, IAgentMetadataRepository, IConversationRepository, IMcpServerRepository,
    ISkillRepository, IUserRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository, SqliteAssistantPreferenceRepository,
    SqliteConversationRepository, SqliteMcpServerRepository, SqliteProviderRepository, SqliteSkillRepository,
    SqliteUserRepository,
};
use aionui_realtime::{BroadcastEventBus, WebSocketManager};

pub struct AppServices {
    pub database: Database,
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub ws_manager: Arc<WebSocketManager>,
    pub event_bus: Arc<BroadcastEventBus>,
    pub worker_task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_lease_registry: Arc<ActiveLeaseRegistry>,
    pub conversation_runtime_state: Arc<ConversationRuntimeStateService>,
    pub conversation_service: ConversationService,
    /// Same instance as `worker_task_manager`, exposed through the
    /// `OnConversationDelete` trait so `ConversationService::with_delete_hook`
    /// can wire it up. Optional because tests construct `AppServices` with a
    /// mock `worker_task_manager` that does not implement the trait.
    pub task_manager_delete_hook: Option<Arc<dyn OnConversationDelete>>,
    pub agent_registry: Arc<AgentRegistry>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub acp_session_sync: Arc<AcpSessionSyncService>,
    /// Raw JWT secret string, used to derive encryption keys.
    pub jwt_secret_raw: String,
    pub data_dir: PathBuf,
    pub dump_prompts: bool,
    pub work_dir: PathBuf,
    /// When `true`, skip JWT authentication and use a fixed default user.
    pub local: bool,
    pub app_version: String,
    /// Resolved skill paths. Shared with the `ConversationService` for
    /// snapshot resolution at create time.
    pub skill_paths: Arc<aionui_extension::SkillPaths>,
    /// User skill metadata and import history repository.
    pub skill_repo: Arc<dyn ISkillRepository>,
    runtime_helper_bin: String,
    runtime_base_url: String,
}

impl AppServices {
    pub(crate) fn runtime_helper_bin(&self) -> String {
        self.runtime_helper_bin.clone()
    }

    pub(crate) fn runtime_base_url(&self) -> String {
        self.runtime_base_url.clone()
    }

    /// Replace the worker task manager after construction.
    ///
    /// Primarily used by tests to inject mock implementations.
    pub fn with_worker_task_manager(mut self, wtm: Arc<dyn IWorkerTaskManager>) -> Self {
        self.worker_task_manager = wtm;
        self.conversation_service = build_conversation_service(ConversationServiceDeps {
            database: &self.database,
            work_dir: self.work_dir.clone(),
            event_bus: self.event_bus.clone(),
            skill_paths: self.skill_paths.clone(),
            skill_repo: self.skill_repo.clone(),
            worker_task_manager: self.worker_task_manager.clone(),
            conversation_runtime_state: self.conversation_runtime_state.clone(),
            conversation_repo: self.conversation_repo.clone(),
            task_manager_delete_hook: self.task_manager_delete_hook.clone(),
            runtime_helper_bin: self.runtime_helper_bin.clone(),
            runtime_base_url: self.runtime_base_url.clone(),
        });
        self
    }

    pub async fn from_config(database: Database, config: &AppConfig) -> anyhow::Result<Self> {
        let data_dir = config.data_dir.clone();
        let work_dir = config.work_dir.clone();
        let local = config.local;
        let dump_prompts = config.dump_prompts;
        let app_version = config.app_version.clone();
        let user_repo: Arc<dyn IUserRepository> = Arc::new(SqliteUserRepository::new(database.pool().clone()));

        // Resolve JWT secret: env var → system user db field → random generation
        let env_secret = std::env::var("JWT_SECRET").ok();
        let system_user = user_repo
            .get_system_user()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get system user: {e}"))?;

        let db_secret = system_user
            .as_ref()
            .and_then(|u| u.jwt_secret.as_deref())
            .filter(|s| !s.is_empty());

        let (secret, is_new) = resolve_jwt_secret(env_secret.as_deref(), db_secret);

        // Persist newly generated secret to database
        if is_new && let Some(user) = &system_user {
            user_repo
                .update_jwt_secret(&user.id, &secret)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to persist JWT secret: {e}"))?;
            tracing::info!("Generated and persisted new JWT secret");
        }

        let encryption_key = derive_encryption_key(&secret);

        let provider_repo = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        let event_bus = Arc::new(BroadcastEventBus::new(256));
        // User-configured MCP servers — injected into ACP `session/new`
        // so the agent gets the operator's tools (ELECTRON-1JG fix).
        let mcp_server_repo: Arc<dyn IMcpServerRepository> =
            Arc::new(SqliteMcpServerRepository::new(database.pool().clone()));

        let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
            Arc::new(SqliteAgentMetadataRepository::new(database.pool().clone()));
        let agent_registry = AgentRegistry::new(agent_metadata_repo);
        agent_registry
            .hydrate()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to hydrate agent registry: {e}"))?;

        let acp_session_repo: Arc<dyn IAcpSessionRepository> =
            Arc::new(SqliteAcpSessionRepository::new(database.pool().clone()));
        let acp_agent_service = AcpSessionSyncService::new(acp_session_repo.clone());

        let conversation_repo: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let skill_repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(database.pool().clone()));

        // Skill paths need app resource dir (for builtin rules) + data dir
        // (for user skills + materialized views). AcpSkillManager uses these
        // for first-message skill index/body loading.
        let app_resource_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let skill_paths = Arc::new(aionui_extension::resolve_skill_paths(&app_resource_dir, &data_dir));
        aionui_extension::sync_skill_catalog_into_repo(skill_paths.as_ref(), skill_repo.as_ref())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to synchronize skill catalog: {e}"))?;

        // Absolute path to this process's binary. Reused as the `command` for
        // the stdio MCP bridge spawned by ACP CLIs when a team session is
        // attached to a conversation (phase1 mcp.md §4.6 single-binary model).
        let backend_binary_path =
            Arc::new(std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("aioncore")));
        let runtime_helper_bin = backend_binary_path.to_string_lossy().into_owned();
        let runtime_base_url = config.local_base_url();

        let factory = build_agent_factory(AgentFactoryDeps {
            skill_manager: AcpSkillManager::new_with_repo(skill_paths.clone(), skill_repo.clone()),
            provider_repo,
            encryption_key,
            agent_registry: agent_registry.clone(),
            acp_agent_service: acp_agent_service.clone(),
            data_dir: data_dir.clone(),
            dump_prompts,
            broadcaster: event_bus.clone(),
            backend_binary_path: backend_binary_path.clone(),
            mcp_server_repo: Some(mcp_server_repo),
        });

        // Agent factory is now wired. Future extension/custom agents
        // that get written to `agent_metadata` will show up after the
        // relevant service calls `AgentRegistry::hydrate`.
        let active_lease_registry = Arc::new(ActiveLeaseRegistry::new());
        let task_manager_concrete = Arc::new(WorkerTaskManagerImpl::new_with_active_leases(
            factory,
            active_lease_registry.clone(),
        ));
        let worker_task_manager: Arc<dyn IWorkerTaskManager> = task_manager_concrete.clone();
        let task_manager_delete_hook: Arc<dyn OnConversationDelete> = task_manager_concrete;
        let conversation_runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let conversation_service = build_conversation_service(ConversationServiceDeps {
            database: &database,
            work_dir: work_dir.clone(),
            event_bus: event_bus.clone(),
            skill_paths: skill_paths.clone(),
            skill_repo: skill_repo.clone(),
            worker_task_manager: worker_task_manager.clone(),
            conversation_runtime_state: conversation_runtime_state.clone(),
            conversation_repo: conversation_repo.clone(),
            task_manager_delete_hook: Some(task_manager_delete_hook.clone()),
            runtime_helper_bin: runtime_helper_bin.clone(),
            runtime_base_url: runtime_base_url.clone(),
        });

        Ok(Self {
            database,
            jwt_service: Arc::new(JwtService::new(secret.clone())),
            user_repo,
            cookie_config: Arc::new(CookieConfig::from_env()),
            qr_token_store: Arc::new(QrTokenStore::new()),
            ws_manager: Arc::new(WebSocketManager::new()),
            event_bus,
            worker_task_manager,
            active_lease_registry,
            conversation_runtime_state,
            conversation_service,
            task_manager_delete_hook: Some(task_manager_delete_hook),
            agent_registry,
            conversation_repo,
            acp_session_sync: acp_agent_service,
            jwt_secret_raw: secret,
            data_dir,
            dump_prompts,
            work_dir,
            local,
            app_version,
            skill_paths,
            skill_repo,
            runtime_helper_bin,
            runtime_base_url,
        })
    }
}

struct ConversationServiceDeps<'a> {
    database: &'a Database,
    work_dir: PathBuf,
    event_bus: Arc<BroadcastEventBus>,
    skill_paths: Arc<aionui_extension::SkillPaths>,
    skill_repo: Arc<dyn ISkillRepository>,
    worker_task_manager: Arc<dyn IWorkerTaskManager>,
    conversation_runtime_state: Arc<ConversationRuntimeStateService>,
    conversation_repo: Arc<dyn IConversationRepository>,
    task_manager_delete_hook: Option<Arc<dyn OnConversationDelete>>,
    runtime_helper_bin: String,
    runtime_base_url: String,
}

fn build_conversation_service(deps: ConversationServiceDeps<'_>) -> ConversationService {
    let skill_resolver = Arc::new(aionui_conversation::skill_resolver::ExtensionSkillResolver::new(
        deps.skill_paths,
        deps.skill_repo,
    ));
    let service = ConversationService::new(
        deps.work_dir,
        deps.event_bus,
        skill_resolver,
        deps.worker_task_manager,
        deps.conversation_repo,
        Arc::new(SqliteAgentMetadataRepository::new(deps.database.pool().clone())),
        Arc::new(SqliteAcpSessionRepository::new(deps.database.pool().clone())),
    )
    .with_runtime_state(deps.conversation_runtime_state)
    .with_runtime_helper_context(deps.runtime_helper_bin, deps.runtime_base_url);
    service.with_mcp_server_repo(Arc::new(SqliteMcpServerRepository::new(deps.database.pool().clone())));
    service.with_assistant_definition_repo(Arc::new(SqliteAssistantDefinitionRepository::new(
        deps.database.pool().clone(),
    )));
    service.with_assistant_state_repo(Arc::new(SqliteAssistantOverlayRepository::new(
        deps.database.pool().clone(),
    )));
    service.with_assistant_preference_repo(Arc::new(SqliteAssistantPreferenceRepository::new(
        deps.database.pool().clone(),
    )));
    if let Some(hook) = deps.task_manager_delete_hook {
        service.with_delete_hook(hook);
    }
    service
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_app_services_from_memory_db() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();

        // JWT service should be functional
        let token = services.jwt_service.sign("test_user", "testuser").unwrap();
        let payload = services.jwt_service.verify(&token).unwrap();
        assert_eq!(payload.user_id, "test_user");

        // User repo should have system user
        let has_users = services.user_repo.has_users().await.unwrap();
        assert!(!has_users); // system user has empty password → not counted

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_jwt_secret_persisted_to_db() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();

        // System user should now have a jwt_secret persisted
        let system_user = services.user_repo.get_system_user().await.unwrap();
        let jwt_secret = system_user.unwrap().jwt_secret;
        assert!(jwt_secret.is_some());
        assert!(!jwt_secret.unwrap().is_empty());

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_app_services_uses_supplied_app_version() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            app_version: "9.9.9".to_string(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        assert_eq!(services.app_version, "9.9.9");

        services.database.close().await;
    }
}
