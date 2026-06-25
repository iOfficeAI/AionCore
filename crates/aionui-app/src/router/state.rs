//! Module-level router states + their builders.
//!
//! `ModuleStates` is the bundle returned by `build_module_states`; each
//! `build_*_state` constructs one `*RouterState` from `AppServices`.

use std::sync::Arc;
use std::time::Instant;

use aionui_ai_agent::{AgentRouterState, AgentService, RemoteAgentRouterState, RemoteAgentService};
use aionui_assistant::{
    AssistantAgentCatalogPort, AssistantError, AssistantRouterState, AssistantService, BuiltinAssistantRegistry,
};
use aionui_auth::extract_token_from_ws_headers;
use aionui_channel::ChannelRouterState;
use aionui_conversation::{ConversationRouterState, ConversationService};
use aionui_cron::{CronEventEmitter, CronRouterState, service::CronServiceDeps};
use aionui_db::{
    IAcpSessionRepository, IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository, IConversationRepository,
    IProviderRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository, SqliteAssistantOverrideRepository,
    SqliteAssistantPreferenceRepository, SqliteAssistantRepository, SqliteClientPreferenceRepository,
    SqliteConversationRepository, SqliteProviderRepository, SqliteRemoteAgentRepository, SqliteSettingsRepository,
};
use aionui_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionStateStore, ExternalPathsManager,
    HubIndexManager, HubInstaller, HubRouterState, SkillRouterState, resolve_install_target_dir_for_data_dir,
    resolve_scan_paths_for_data_dir, resolve_state_file_path,
};
use aionui_file::{BrowseRoots, FileRouterState, FileService, FileWatchService, SnapshotService};
use aionui_mcp::{
    AionrsAdapter, AionuiAdapter, ClaudeAdapter, CodeBuddyAdapter, CodexAdapter, GeminiAdapter, McpAgentAdapter,
    McpConfigService, McpConnectionTestService, McpRouterState, McpSyncService, OpencodeAdapter, QwenAdapter,
};
use aionui_office::{
    ConversionService, OfficeRouterState, OfficecliWatchManager, ProxyService, SnapshotService as OfficeSnapshotService,
};
use aionui_realtime::{NoopMessageRouter, WsHandlerState};
use aionui_shell::ShellRouterState;
use aionui_system::{
    ClientPrefService, ConnectionTestRouterState, ConnectionTestService, ModelFetchService, ProtocolDetectionService,
    ProviderService, RuntimePrepareService, SettingsService, SystemRouterState, VersionCheckService,
};
use aionui_team::{
    AgentTurnCancellationPort, AgentTurnExecutionPort, TeamConversationLookupPort, TeamConversationProvisioningPort,
    TeamProjectionMessageStore, TeamRouterState, TeamSessionService,
};

use crate::config::derive_encryption_key;
use crate::router::team_conversation_adapters::TeamConversationAdapters;
use crate::services::AppServices;

#[derive(Debug)]
pub struct RouterBuildError {
    stage: &'static str,
    message: &'static str,
    source: Option<anyhow::Error>,
}

impl RouterBuildError {
    pub fn new(stage: &'static str, message: &'static str) -> Self {
        Self {
            stage,
            message,
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<anyhow::Error>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn stage(&self) -> &'static str {
        self.stage
    }

    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl std::fmt::Display for RouterBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for RouterBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// All module-level router states bundled into a single struct.
///
/// Reduces parameter bloat on router constructors and makes it easy for
/// tests to override individual modules.
pub struct ModuleStates {
    pub system: SystemRouterState,
    pub conversation: ConversationRouterState,
    pub remote_agent: RemoteAgentRouterState,
    pub agent: AgentRouterState,

    pub connection_test: ConnectionTestRouterState,
    pub file: FileRouterState,
    pub mcp: McpRouterState,
    pub extension: ExtensionRouterState,
    pub hub: HubRouterState,
    pub skill: SkillRouterState,
    pub channel: ChannelRouterState,
    pub team: TeamRouterState,
    pub cron: CronRouterState,
    pub office: OfficeRouterState,
    pub shell: ShellRouterState,
    pub assistant: AssistantRouterState,
}

fn default_allowed_roots(work_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{label}-temp-{id}/`. On Windows the
    // operator may put `work_dir` on a separate drive (e.g. `X:\AionUi`)
    // that's neither under `temp_dir` nor `home_dir`, which previously
    // caused `/api/fs/list` to 403 every Hermes-mode session
    // (ELECTRON-1BT). Including `work_dir` keeps temp + custom-on-drive
    // workspaces on the allowlist without widening the sandbox to
    // unrelated paths.
    if let Some(wd) = work_dir
        && !wd.as_os_str().is_empty()
        && !roots.iter().any(|r| r == wd)
    {
        roots.push(wd.to_path_buf());
    }
    roots
}

fn build_module_state_phase<T>(boot: &Instant, phase: &'static str, build: impl FnOnce() -> T) -> T {
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        phase,
        "startup: module state phase started"
    );
    let value = build();
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        phase,
        "startup: module state phase completed"
    );
    value
}

/// Components needed to start the channel orchestrator.
///
/// Returned alongside `ChannelRouterState` by `build_channel_state`.
/// The caller must spawn the orchestrator as a background task.
pub struct ChannelOrchestratorComponents {
    pub orchestrator: aionui_channel::orchestrator::ChannelOrchestrator,
    pub message_rx: tokio::sync::mpsc::Receiver<aionui_channel::types::UnifiedIncomingMessage>,
    pub confirm_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    pub manager: Arc<aionui_channel::manager::ChannelManager>,
    pub plugin_factory: Arc<aionui_channel::manager::PluginFactory>,
}

/// Build all default `ModuleStates` from application services.
pub async fn build_module_states(
    services: &AppServices,
) -> Result<(ModuleStates, ChannelOrchestratorComponents), RouterBuildError> {
    let boot = Instant::now();
    tracing::info!("startup: module state build started");

    let (ext_state, hub_state, mut skill_state) = build_extension_states(services).await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension states built"
    );

    let scan_paths = resolve_scan_paths_for_data_dir(&services.data_dir);
    if let Err(error) = ext_state.registry.initialize_with_scan_paths(scan_paths).await {
        tracing::warn!(
            code = "BOOTSTRAP_DEGRADED_EXTENSION_REGISTRY",
            stage = "extension.registry.initialize",
            error = %error,
            "extension registry initialize failed"
        );
    }
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension registry initialized"
    );

    let assistant = build_assistant_state(services);
    assistant.service.bootstrap_assistant_storage().await.map_err(|error| {
        RouterBuildError::new("router.assistant.bootstrap", "failed to bootstrap assistant storage").with_source(error)
    })?;
    let cron = build_cron_state(services);
    cron.cron_service.init().await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: cron state initialized"
    );

    // The agent catalog already hydrated at startup (see `lib.rs`).
    // Extension-contributed rows will land in `agent_metadata` in a
    // later step; for now we rely on the builtin + internal seed rows.

    let dispatcher: Arc<dyn AssistantRuleDispatcher> = assistant.service.clone();
    skill_state.assistant_dispatcher = Some(dispatcher);

    let (channel_state, channel_components) = build_channel_state(services, ext_state.registry.clone()).await;
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: channel state built");

    let backend_binary_path = Arc::new(
        std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("aioncore")),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: backend binary path resolved"
    );

    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let agent_service = AgentService::new(
        services.agent_registry.clone(),
        services.event_bus.clone(),
        provider_repo,
        encryption_key,
        services.data_dir.clone(),
    );
    services
        .conversation_service
        .with_agent_availability_feedback(agent_service.availability_feedback_port());
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: agent service built");

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module states bundle started"
    );
    let states = ModuleStates {
        system: build_module_state_phase(&boot, "system", || build_system_state(services)),
        conversation: build_module_state_phase(&boot, "conversation", || {
            build_conversation_state(
                services,
                Some(cron.cron_service.clone()),
                Some(assistant.service.clone() as Arc<dyn AssistantRuleDispatcher>),
            )
        }),
        remote_agent: build_module_state_phase(&boot, "remote_agent", || build_remote_agent_state(services)),
        agent: build_module_state_phase(&boot, "agent", || AgentRouterState {
            agent_registry: services.agent_registry.clone(),
            service: agent_service,
        }),
        connection_test: build_module_state_phase(&boot, "connection_test", build_connection_test_state),
        file: build_module_state_phase(&boot, "file", || build_file_state(services))?,
        mcp: build_module_state_phase(&boot, "mcp", || build_mcp_state(services)),
        extension: ext_state,
        hub: hub_state,
        skill: skill_state,
        channel: channel_state,
        team: build_module_state_phase(&boot, "team", || {
            build_team_state(
                services,
                Some(cron.cron_service.clone()),
                backend_binary_path.clone(),
                services.guide_mcp_config.clone(),
            )
        }),
        cron,
        office: build_module_state_phase(&boot, "office", || build_office_state(services)),
        shell: build_module_state_phase(&boot, "shell", || build_shell_state(services)),
        assistant,
    };
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module state build completed"
    );
    states
        .conversation
        .service
        .recover_stale_runtime_state_on_startup()
        .await;

    Ok((states, channel_components))
}

/// Build the default `AssistantRouterState` from application services.
pub fn build_assistant_state(services: &AppServices) -> AssistantRouterState {
    #[derive(Clone)]
    struct RegistryAssistantAgentCatalog {
        registry: Arc<aionui_ai_agent::AgentRegistry>,
    }

    #[async_trait::async_trait]
    impl AssistantAgentCatalogPort for RegistryAssistantAgentCatalog {
        async fn list_management_agents(&self) -> Result<Vec<aionui_api_types::AgentManagementRow>, AssistantError> {
            self.registry.refresh_availability().await;
            Ok(self.registry.list_management_rows().await)
        }
    }

    let pool = services.database.pool().clone();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
        Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let state_repo: Arc<dyn IAssistantOverlayRepository> =
        Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()));
    let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(pool.clone()));
    let override_repo: Arc<dyn IAssistantOverrideRepository> =
        Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()));
    // Used by `AssistantService::resolve_default_agent_type` to infer a
    // working `agent_id` from the configured provider list when
    // the caller does not supply one (ELECTRON-1J1 / 1KV).
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let builtin = Arc::new(BuiltinAssistantRegistry::load());
    // Pin user_data_dir to the runtime-resolved data directory so dev /
    // packaged / multi-instance launches all keep their assistant rule files
    // alongside the matching SQLite database (avoiding the historical bug
    // where dev wrote rules to the release `~/.aionui/` while the db lived
    // under `~/.aionui-dev/`).
    let service = Arc::new(AssistantService::new(
        pool,
        aionui_assistant::service::AssistantServiceDeps {
            definition_repo,
            state_repo,
            preference_repo,
            repo,
            override_repo,
            provider_repo,
            builtin,
            agent_catalog: Some(Arc::new(RegistryAssistantAgentCatalog {
                registry: services.agent_registry.clone(),
            })),
        },
        services.data_dir.clone(),
    ));
    AssistantRouterState { service }
}

/// Build the default `SystemRouterState` from application services.
pub fn build_system_state(services: &AppServices) -> SystemRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let http_client = reqwest::Client::new();

    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(pool.clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(pool))),
        provider_service: ProviderService::new(provider_repo.clone(), encryption_key),
        model_fetch_service: ModelFetchService::new(provider_repo, encryption_key, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, env!("CARGO_PKG_VERSION").to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(services.event_bus.clone()),
    }
}

/// Build the default `ConversationRouterState` from application services.
pub fn build_conversation_state(
    services: &AppServices,
    cron_service: Option<Arc<aionui_cron::service::CronService>>,
    assistant_dispatcher: Option<Arc<dyn AssistantRuleDispatcher>>,
) -> ConversationRouterState {
    let conversation_service = services.conversation_service.clone();
    if let Some(dispatcher) = assistant_dispatcher {
        conversation_service.with_assistant_dispatcher(dispatcher);
    }
    if let Some(cron_service) = cron_service {
        conversation_service.with_delete_hook(cron_service.clone());
        conversation_service.with_cron_service(Some(cron_service));
    }
    ConversationRouterState {
        service: conversation_service,
        task_manager: services.worker_task_manager.clone(),
    }
}

/// Build the default `RemoteAgentRouterState` from application services.
pub fn build_remote_agent_state(services: &AppServices) -> RemoteAgentRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let repo = Arc::new(SqliteRemoteAgentRepository::new(pool));
    RemoteAgentRouterState {
        service: Arc::new(RemoteAgentService::new(repo, encryption_key)),
    }
}

/// Build the default `ConnectionTestRouterState`.
pub fn build_connection_test_state() -> ConnectionTestRouterState {
    ConnectionTestRouterState {
        service: ConnectionTestService::new(reqwest::Client::new()),
    }
}

/// Build the default `FileRouterState` from application services.
pub fn build_file_state(services: &AppServices) -> Result<FileRouterState, RouterBuildError> {
    let broadcaster = services.event_bus.clone();
    let allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));
    let browse_roots = BrowseRoots::new();
    let file_service = Arc::new(FileService::new(broadcaster.clone(), allowed_roots.clone()));
    let watch_service = Arc::new(FileWatchService::new(broadcaster).map_err(file_watch_init_error)?);
    let snapshot_service = Arc::new(SnapshotService::new());
    Ok(FileRouterState {
        file_service,
        watch_service,
        snapshot_service,
        allowed_roots,
        browse_roots,
    })
}

fn file_watch_init_error(error: aionui_file::FileError) -> RouterBuildError {
    RouterBuildError::new("router.file_watch", "failed to initialize file watch service").with_source(error)
}

/// Build the default `McpRouterState` from application services.
pub fn build_mcp_state(services: &AppServices) -> McpRouterState {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn aionui_db::IMcpServerRepository> = Arc::new(aionui_db::SqliteMcpServerRepository::new(pool));

    let adapters: Vec<Arc<dyn McpAgentAdapter>> = vec![
        Arc::new(ClaudeAdapter),
        Arc::new(GeminiAdapter),
        Arc::new(QwenAdapter),
        Arc::new(CodexAdapter),
        Arc::new(CodeBuddyAdapter),
        Arc::new(OpencodeAdapter),
        Arc::new(AionrsAdapter),
        Arc::new(AionuiAdapter::new(repo.clone())),
    ];

    let oauth_token_repo: Arc<dyn aionui_db::IOAuthTokenRepository> = Arc::new(
        aionui_db::SqliteOAuthTokenRepository::new(services.database.pool().clone()),
    );
    let http_client = reqwest::Client::new();

    McpRouterState {
        config_service: McpConfigService::new(repo.clone()),
        sync_service: McpSyncService::new(repo, adapters),
        connection_test_service: McpConnectionTestService::new(http_client.clone(), services.event_bus.clone()),
        oauth_service: aionui_mcp::McpOAuthService::new(oauth_token_repo, http_client),
    }
}

/// Build the default `ChannelRouterState` and orchestrator components.
pub async fn build_channel_state(
    services: &AppServices,
    extension_registry: ExtensionRegistry,
) -> (ChannelRouterState, ChannelOrchestratorComponents) {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn aionui_db::IChannelRepository> = Arc::new(aionui_db::SqliteChannelRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);

    let (message_tx, message_rx) = tokio::sync::mpsc::channel(256);
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::channel(256);

    let manager = Arc::new(aionui_channel::manager::ChannelManager::new(
        repo.clone(),
        services.event_bus.clone(),
        encryption_key,
        message_tx,
        confirm_tx,
    ));

    let pairing_service = Arc::new(aionui_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
    ));

    let session_manager = Arc::new(aionui_channel::session::SessionManager::new(repo.clone()));

    let plugin_factory: Arc<aionui_channel::manager::PluginFactory> =
        Arc::new(Box::new(aionui_channel::plugins::create_plugin));

    // Build channel settings service for per-plugin agent/model configuration
    let pref_pool = services.database.pool().clone();
    let pref_repo: Arc<dyn aionui_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pref_pool));
    let channel_settings = Arc::new(
        aionui_channel::channel_settings::ChannelSettingsService::new(pref_repo)
            .with_agent_metadata_repo(Arc::new(SqliteAgentMetadataRepository::new(
                services.database.pool().clone(),
            )))
            .with_assistant_repos(
                Arc::new(SqliteAssistantDefinitionRepository::new(
                    services.database.pool().clone(),
                )),
                Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
            ),
    );

    // Build orchestrator dependencies
    let action_executor = Arc::new(aionui_channel::action::ActionExecutor::new(
        Arc::clone(&pairing_service),
        Arc::clone(&session_manager),
        Arc::clone(&channel_settings),
    ));

    let conv_repo: Arc<dyn aionui_db::IConversationRepository> = Arc::new(
        aionui_db::SqliteConversationRepository::new(services.database.pool().clone()),
    );
    let skill_resolver = Arc::new(aionui_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
        services.skill_repo.clone(),
    ));
    let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> = Arc::new(
        aionui_db::SqliteAgentMetadataRepository::new(services.database.pool().clone()),
    );
    let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> = Arc::new(
        aionui_db::SqliteAcpSessionRepository::new(services.database.pool().clone()),
    );
    let conversation_svc = Arc::new(
        ConversationService::new(
            services.work_dir.clone(),
            services.event_bus.clone(),
            skill_resolver,
            services.worker_task_manager.clone(),
            conv_repo,
            agent_metadata_repo,
            acp_session_repo,
        )
        .with_runtime_state(services.conversation_runtime_state.clone()),
    );
    conversation_svc.with_mcp_server_repo(Arc::new(aionui_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    if let Some(hook) = services.task_manager_delete_hook.clone() {
        conversation_svc.with_delete_hook(hook);
    }

    let owner_user_id = services
        .user_repo
        .get_primary_webui_user()
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
        .unwrap_or_else(|| "system_default_user".to_string());

    let message_service = Arc::new(aionui_channel::message_service::ChannelMessageService::new(
        conversation_svc,
        services.worker_task_manager.clone(),
        Arc::clone(&channel_settings),
        owner_user_id,
    ));

    let orchestrator = aionui_channel::orchestrator::ChannelOrchestrator::new(
        action_executor,
        message_service,
        Arc::clone(&session_manager),
        manager.clone() as Arc<dyn aionui_channel::stream_relay::ChannelSender>,
    );

    let state = ChannelRouterState {
        manager: Arc::clone(&manager),
        pairing_service,
        session_manager,
        repo,
        plugin_factory: Arc::clone(&plugin_factory),
        settings_service: channel_settings,
        extension_registry,
    };

    let components = ChannelOrchestratorComponents {
        orchestrator,
        message_rx,
        confirm_rx,
        manager,
        plugin_factory,
    };

    (state, components)
}

/// Build the default `TeamRouterState` from application services.
///
/// `backend_binary_path` is resolved once in `build_module_states` via
/// `std::env::current_exe()` and cloned into each builder that needs it,
/// per `docs/teams/phase1/interface-contracts.md` §10.
pub fn build_team_state(
    services: &AppServices,
    _cron_service: Option<Arc<aionui_cron::service::CronService>>,
    backend_binary_path: Arc<std::path::PathBuf>,
    guide_mcp_config: Option<aionui_api_types::GuideMcpConfig>,
) -> TeamRouterState {
    let pool = services.database.pool().clone();
    let team_repo: Arc<dyn aionui_db::ITeamRepository> = Arc::new(aionui_db::SqliteTeamRepository::new(pool.clone()));
    let conv_service = services.conversation_service.clone();
    let conv_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool));
    let adapters = Arc::new(TeamConversationAdapters::new(
        conv_service,
        conv_repo,
        services.event_bus.clone(),
        services.worker_task_manager.clone(),
    ));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = adapters.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = adapters.clone();
    let lookup_port: Arc<dyn TeamConversationLookupPort> = adapters.clone();
    let turn_port: Arc<dyn AgentTurnExecutionPort> = adapters.clone();
    let cancellation_port: Arc<dyn AgentTurnCancellationPort> = adapters;
    let service = TeamSessionService::new(
        team_repo,
        Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone())),
        Arc::new(SqliteAssistantDefinitionRepository::new(
            services.database.pool().clone(),
        )),
        Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        conversation_port,
        projection_store,
        lookup_port,
        services.event_bus.clone(),
        services.worker_task_manager.clone(),
        turn_port,
        cancellation_port,
        backend_binary_path,
        guide_mcp_config,
    );
    TeamRouterState { service }
}

/// Build the default `CronRouterState` from application services.
pub fn build_cron_state(services: &AppServices) -> CronRouterState {
    let pool = services.database.pool().clone();
    let cron_repo: Arc<dyn aionui_db::ICronRepository> = Arc::new(aionui_db::SqliteCronRepository::new(pool.clone()));

    let conv_repo: Arc<dyn aionui_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let skill_resolver = Arc::new(aionui_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
        services.skill_repo.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo.clone(),
        agent_metadata_repo.clone(),
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(aionui_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_definition_repo(Arc::new(SqliteAssistantDefinitionRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_state_repo(Arc::new(SqliteAssistantOverlayRepository::new(
        services.database.pool().clone(),
    )));
    conv_service.with_assistant_preference_repo(Arc::new(SqliteAssistantPreferenceRepository::new(
        services.database.pool().clone(),
    )));

    let executor = Arc::new(aionui_cron::executor::JobExecutor::new(
        services.worker_task_manager.clone(),
        conv_repo,
        Arc::new(conv_service.clone()),
        services.work_dir.clone(),
        services.data_dir.clone(),
        services.event_bus.clone(),
        services.agent_registry.clone(),
    ));

    let tick_service_ref: Arc<CronServiceTickRef> = Arc::new(CronServiceTickRef::default());
    let tick_ref = tick_service_ref.clone();
    let scheduler = Arc::new(aionui_cron::scheduler::CronScheduler::new(Arc::new(
        move |job_id: String| {
            let svc = tick_ref.0.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Some(svc) = svc {
                    svc.tick(&job_id).await;
                }
            });
        },
    )));

    let emitter = CronEventEmitter::new(services.event_bus.clone());
    let assistant_definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(
        services.database.pool().clone(),
    ));
    let assistant_overlay_repo = Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone()));
    let cron_service = Arc::new(aionui_cron::service::CronService::new(CronServiceDeps {
        repo: cron_repo,
        agent_metadata_repo,
        assistant_definition_repo,
        assistant_overlay_repo,
        scheduler,
        executor,
        emitter,
        data_dir: services.data_dir.clone(),
    }));

    tick_service_ref.0.lock().unwrap().replace(cron_service.clone());

    CronRouterState {
        cron_service,
        conversation_service: conv_service,
    }
}

/// Build the default `OfficeRouterState` from application services.
pub fn build_office_state(services: &AppServices) -> OfficeRouterState {
    let data_dir = services.data_dir.as_path();
    let allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));

    let spawner: Arc<dyn aionui_office::ProcessSpawner> =
        Arc::new(aionui_office::DefaultProcessSpawner::new(data_dir.to_path_buf()));
    let watch_manager = Arc::new(OfficecliWatchManager::new(spawner, services.event_bus.clone()));

    let snapshot_service = Arc::new(OfficeSnapshotService::new(data_dir));
    let conversion_service = Arc::new(ConversionService::with_data_dir(None, data_dir.to_path_buf()));
    let proxy_service = Arc::new(ProxyService::new(watch_manager.clone()));

    OfficeRouterState {
        watch_manager,
        snapshot_service,
        conversion_service,
        proxy_service,
        allowed_roots,
    }
}

/// Build the default `ShellRouterState` from application services.
pub fn build_shell_state(services: &AppServices) -> ShellRouterState {
    let pool = services.database.pool().clone();
    let client_pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool));
    let client_pref_service = ClientPrefService::new(client_pref_repo);

    ShellRouterState {
        shell_service: Arc::new(aionui_shell::ShellService::new(Arc::new(
            aionui_shell::DefaultSystemOpener,
        ))),
        stt_service: Arc::new(aionui_shell::SttService::new(reqwest::Client::new())),
        client_pref_service,
    }
}

/// Helper to break the circular reference between CronScheduler and CronService.
#[derive(Default)]
struct CronServiceTickRef(std::sync::Mutex<Option<Arc<aionui_cron::service::CronService>>>);

/// Build the default extension-related router states.
///
/// Returns `(ExtensionRouterState, HubRouterState, SkillRouterState)`.
pub async fn build_extension_states(
    services: &AppServices,
) -> (ExtensionRouterState, HubRouterState, SkillRouterState) {
    let skill_data_dir = services.data_dir.clone();

    let state_store = ExtensionStateStore::new(resolve_state_file_path(&skill_data_dir));
    let registry = ExtensionRegistry::new(state_store, services.event_bus.clone(), services.app_version.clone());

    let hub_dir = resolve_install_target_dir_for_data_dir(&skill_data_dir);
    let index_manager = HubIndexManager::new(hub_dir, registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());

    let ext_paths_mgr = Arc::new(ExternalPathsManager::new(&skill_data_dir).await);

    let ext_state = ExtensionRouterState {
        registry: registry.clone(),
    };

    let hub_state = HubRouterState {
        index_manager,
        installer,
    };

    let skill_state = SkillRouterState {
        skill_paths: services.skill_paths.as_ref().clone(),
        skill_repo: services.skill_repo.clone(),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None,
    };

    (ext_state, hub_state, skill_state)
}

/// Build the default `WsHandlerState` from application services.
pub fn build_ws_state(services: &AppServices) -> WsHandlerState {
    if services.local {
        return WsHandlerState {
            manager: services.ws_manager.clone(),
            router: Arc::new(NoopMessageRouter),
            token_validator: Arc::new(|_| true),
            token_extractor: Arc::new(|_| Some("local".into())),
        };
    }

    let jwt_service = services.jwt_service.clone();
    let token_validator = Arc::new(move |token: &str| jwt_service.verify(token).is_ok());

    let token_extractor = Arc::new(|headers: &axum::http::HeaderMap| extract_token_from_ws_headers(headers));

    WsHandlerState {
        manager: services.ws_manager.clone(),
        router: Arc::new(NoopMessageRouter),
        token_validator,
        token_extractor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::AppConfig;
    use aionui_extension::{ExtensionSource, ScanPath};

    #[tokio::test]
    async fn build_extension_states_uses_host_app_version_for_engine_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("demo-ext");

        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "demo-ext",
                "version": "1.0.0",
                "engine": {
                    "aionui": "^2.0.0"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let db = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            data_dir: data_dir.clone(),
            work_dir: data_dir,
            app_version: "2.1.0".to_string(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        let (ext_state, _hub_state, _skill_state) = build_extension_states(&services).await;
        ext_state
            .registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();

        let loaded = ext_state.registry.get_loaded_extensions().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo-ext");

        services.database.close().await;
    }

    #[test]
    fn file_watch_init_error_maps_to_bootstrap_server_failed() {
        let err = file_watch_init_error(aionui_file::FileError::Internal("watch backend unavailable".into()));

        assert_eq!(err.stage(), "router.file_watch");
        assert_eq!(err.message(), "failed to initialize file watch service");
        assert!(!err.to_string().contains("watch backend unavailable"));
    }
}
