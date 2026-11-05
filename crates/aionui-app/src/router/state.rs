//! Module-level router states + their builders.
//!
//! `ModuleStates` is the bundle returned by `build_module_states`; each
//! `build_*_state` constructs one `*RouterState` from `AppServices`.

use std::sync::{Arc, Weak};
use std::time::Instant;

use aionui_ai_agent::{
    AgentRouterState, AgentService, CodexAccountService, IWorkerTaskManager, RemoteAgentRouterState, RemoteAgentService,
};
use aionui_assistant::{
    AgentCenterRouterState, AgentCenterService, AgentWorkflowAgentCancellationOutcome,
    AgentWorkflowAgentExecutionCancellationPort, AgentWorkflowToolExecutionPort, AgentWorkflowTurnResult,
    AssistantAgentCatalogPort, AssistantError, AssistantRouterState, AssistantService, BuiltinAssistantRegistry,
    SkillEvolutionRouterState, SkillEvolutionService,
};
use aionui_auth::extract_token_from_ws_headers;
use aionui_channel::ChannelRouterState;
use aionui_common::{
    AgentKillReason, ConversationTurnSettlement, OnConversationTurnSettled, OnConversationTurnStarting,
};
use aionui_conversation::{ConversationRouterState, ConversationService};
use aionui_cron::{CronEventEmitter, CronRouterState, service::CronServiceDeps};
use aionui_db::{
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository, IConversationRepository,
    IMcpServerRepository, IProviderRepository, SqliteAgentMetadataRepository, SqliteAssistantAgentCenterRepository,
    SqliteAssistantDefinitionRepository, SqliteAssistantDefinitionRevisionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteClientPreferenceRepository, SqliteConversationRepository, SqliteExperienceArticleRepository,
    SqliteFeedbackDiagnosticsRepository, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteSkillEvolutionProposalRepository, SqliteSkillEvolutionSettingsRepository,
    SqliteTeamRepository,
};
use aionui_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionStateStore, ExternalPathsManager,
    HubIndexManager, HubInstaller, HubRouterState, SkillHubClient, SkillRegistryService, SkillRouterState,
    resolve_install_target_dir_for_data_dir, resolve_scan_paths_for_data_dir, resolve_state_file_path,
};
use aionui_file::{FileRouterState, FileService, SnapshotService};
use aionui_mcp::{
    AionrsAdapter, AionuiAdapter, ClaudeAdapter, CodeBuddyAdapter, CodexAdapter, GeminiAdapter, McpAgentAdapter,
    McpConfigService, McpConnectionTestService, McpRouterState, McpServer, McpSyncService, OpencodeAdapter,
    QwenAdapter,
};
use aionui_office::{ConversionService, OfficeRouterState, OfficecliWatchManager, ProxyService};
use aionui_project::{ProjectRouterState, ProjectService};
use aionui_realtime::{MessageRouter, TokenUserResolver, WsHandlerState};
use aionui_review::{AllowedRootsResolver, DeliveryRouterState, DeliveryService, ReviewRouterState, ReviewService};
use aionui_session_message::drainer::Drainer;
use aionui_session_message::state::SessionMessageRouterState;
use aionui_session_message::targets::MentionableTargets;
use aionui_shell::ShellRouterState;
use aionui_sidebar::{ArchiveTeardownPorts, SidebarRouterState, SidebarService};
use aionui_skill_runtime::{SkillRuntimeRouterState, SkillRuntimeService};
use aionui_system::{
    ClientPrefService, ConnectionTestRouterState, ConnectionTestService, FeedbackDiagnosticsService, ModelFetchService,
    ProtocolDetectionService, ProviderService, RuntimePrepareService, SettingsService, SystemRouterState,
    VersionCheckService,
};
use aionui_team::{
    AgentTurnCancellationPort, AgentTurnExecutionPort, BoardWorkItemTimelinePort, NativeSlashCommandPort,
    TeamAssistantCatalogEntry, TeamAssistantCatalogPort, TeamConversationProvisioningPort, TeamProjectionMessageStore,
    TeamRouterState, TeamSessionService,
};

use crate::config::{IdentityMode, derive_encryption_key};
use crate::router::team_capability_resolver::TeamCapabilityResolver;
use crate::router::team_conversation_adapters::TeamConversationAdapters;
use crate::services::AppServices;
use crate::skill_evolution_adapters::{
    AgentCenterPinAdapter, ConversationTrajectoryAdapter, ProviderLlmAdapter, SkillHubApplyAdapter,
};

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

/// Map an assistant bootstrap failure to a router build error.
///
/// A [`AssistantError::ConcurrentBootstrapContention`] is benign and recoverable
/// (a transient concurrent-startup race), so it gets a distinct boundary stage
/// (`router.assistant.bootstrap.concurrency_contended`) that AionUi maps to a
/// gentle "retry/restart" message instead of the "local data corruption" false
/// alarm. The boundary code stays `BOOTSTRAP_SERVER_FAILED`; only the stage
/// differs (Sentry 135525166). All other errors keep the original stage.
fn assistant_bootstrap_build_error(error: AssistantError) -> RouterBuildError {
    if matches!(error, AssistantError::ConcurrentBootstrapContention(_)) {
        RouterBuildError::new(
            "router.assistant.bootstrap.concurrency_contended",
            "assistant storage bootstrap contended under concurrent startup",
        )
        .with_source(error)
    } else {
        RouterBuildError::new("router.assistant.bootstrap", "failed to bootstrap assistant storage").with_source(error)
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
    pub project: ProjectRouterState,
    pub sidebar: SidebarRouterState,
    pub mcp: McpRouterState,
    pub extension: ExtensionRouterState,
    pub hub: HubRouterState,
    pub skill: SkillRouterState,
    pub channel: ChannelRouterState,
    pub team: TeamRouterState,
    pub squad: aionui_team::squad::SquadRouterState,
    pub session_message: SessionMessageRouterState,
    pub skill_runtime: SkillRuntimeRouterState,
    pub cron: CronRouterState,
    pub office: OfficeRouterState,
    pub shell: ShellRouterState,
    pub assistant: AssistantRouterState,
    pub agent_center: AgentCenterRouterState,
    pub skill_evolution: SkillEvolutionRouterState,
    /// M7 E4 diff annotations (`/api/review-threads*`).
    pub review: ReviewRouterState,
    /// M7 E4-5 PR delivery (`/api/scm-deliveries*`).
    pub scm_delivery: DeliveryRouterState,
    pub worktree: aionui_worktree::WorktreeRouterState,
    /// M7 E3-4 fan-out groups (`/api/fanout-groups*`).
    pub fanout: aionui_worktree::FanoutRouterState,
    /// M8 E8 batch jobs (`/api/batch*`).
    pub batch: aionui_batch::BatchRouterState,
}

fn default_allowed_roots(work_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{label}-temp-{id}/`. On Windows the
    // operator may put `work_dir` on a separate drive (e.g. `X:\AionUi`)
    // that's neither under `temp_dir` nor `home_dir`. Including `work_dir`
    // keeps temp workspaces on the default allowlist without widening it
    // to unrelated paths.
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
    pub owner_user_id: Option<String>,
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
    if services.identity_mode.is_local() {
        assistant
            .service
            .bootstrap_assistant_storage()
            .await
            .map_err(assistant_bootstrap_build_error)?;
    } else {
        assistant
            .service
            .bootstrap_assistant_storage_external()
            .await
            .map_err(assistant_bootstrap_build_error)?;
    }
    let cron = build_cron_state(services);
    // Cron builds its own ConversationService (not a clone of the shared one),
    // so wire the assistant rule dispatcher here — otherwise scheduled runs
    // resolve empty rules. Mirrors the interactive path in build_conversation_state.
    cron.conversation_service
        .with_assistant_dispatcher(assistant.service.clone() as Arc<dyn AssistantRuleDispatcher>);
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

    let generated_assistant_materializer: Arc<
        dyn aionui_channel::channel_settings::ChannelGeneratedAssistantMaterializer,
    > = Arc::new(GeneratedAssistantMaterializerAdapter {
        assistant: assistant.service.clone(),
    });
    let (channel_state, channel_components) = build_channel_state(
        services,
        ext_state.registry.clone(),
        Some(generated_assistant_materializer),
    )
    .await;
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: channel state built");

    // E8-6: Autopilot results go out through the channel sender built above.
    cron.cron_service
        .set_autopilot_notifier(Arc::new(super::autopilot_notifier::ChannelAutopilotNotifier::new(
            Arc::new(aionui_db::SqliteChannelRepository::new(
                services.database.pool().clone(),
            )),
            channel_components.manager.clone(),
            services.conversation_service.clone(),
        )));

    // E9-6: approval / blocked Inbox items go to IM-push devices.
    services
        .conversation_service
        .set_inbox_push_hook(Arc::new(super::device_push::ChannelDevicePush::new(
            services.user_devices.clone(),
            Arc::new(aionui_db::SqliteChannelRepository::new(
                services.database.pool().clone(),
            )),
            channel_components.manager.clone(),
        )));

    let backend_binary_path = services.backend_binary_path();
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: backend binary path resolved"
    );

    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let encryption_key = derive_encryption_key(&services.encryption_secret_raw);
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

    let codex_program = aionui_runtime::resolve_command_path("codex").unwrap_or_else(|| "codex".into());
    let codex_task_manager = services.worker_task_manager.clone();
    let codex_account = CodexAccountService::new(
        services.session_spawner.clone(),
        codex_program,
        services.event_bus.clone(),
        Some(Arc::new(move || {
            let killed = codex_task_manager.kill_backend("codex", Some(AgentKillReason::AccountChanged));
            if killed > 0 {
                tracing::info!(killed, "recycled Codex sessions after account identity change");
            }
        })),
    );
    let startup_account = codex_account.clone();
    tokio::spawn(async move {
        if let Err(error) = startup_account.initialize().await {
            // Runtime installation and authentication are separate axes. A
            // missing/old Codex must not prevent WorkMate from starting.
            tracing::info!(error = %error, "codex account startup read unavailable");
        }
    });

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module states bundle started"
    );
    let system = build_module_state_phase(&boot, "system", || build_system_state(services));
    let agent_center =
        build_module_state_phase(&boot, "agent_center", || build_agent_center_state(services, &assistant));
    match agent_center.service.recover_interrupted_workflow_runs().await {
        Ok(0) => {}
        Ok(recovered) => {
            tracing::warn!(recovered, "agent-workflow: recovered interrupted executions");
        }
        Err(error) => {
            tracing::warn!(error = %error, "agent-workflow: startup recovery failed");
        }
    }
    services
        .conversation_service
        .with_turn_settled_hook(Arc::new(AgentWorkflowTurnSettlementAdapter {
            conversations: services.conversation_repo.clone(),
            agent_center: agent_center.service.clone(),
        }));
    services
        .conversation_service
        .with_turn_starting_hook(Arc::new(AgentWorkflowTurnStartGuard {
            agent_center: Arc::downgrade(&agent_center.service),
        }));
    let worktree = build_module_state_phase(&boot, "worktree", || build_worktree_state(services));
    crate::router::worktree_adapters::wire_worktree_bindings(services, &worktree.service);
    if let Err(error) = worktree.service.recover_on_boot().await {
        tracing::warn!(error = %error, "worktree: startup recovery failed");
    }
    let fanout =
        crate::router::worktree_adapters::build_fanout_state(services, &worktree.service, assistant.service.clone());
    if let Err(error) = fanout.service.recover_on_boot().await {
        tracing::warn!(error = %error, "fanout: startup recovery failed");
    }
    // M8 E8: interrupted batch attempts are marked failed and running jobs
    // paused; nothing restarts on its own (scale / retry resumes).
    let batch =
        crate::router::batch_adapters::build_batch_state(services, &worktree.service, assistant.service.clone());
    if let Err(error) = batch.service.recover_on_boot().await {
        tracing::warn!(error = %error, "batch: startup recovery failed");
    }
    let skill_evolution = build_module_state_phase(&boot, "skill_evolution", || {
        build_skill_evolution_state(services, system.provider_service.clone(), agent_center.service.clone())
    });
    let mut states = ModuleStates {
        system,
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
            codex_account,
        }),
        connection_test: build_module_state_phase(&boot, "connection_test", build_connection_test_state),
        file: build_module_state_phase(&boot, "file", || build_file_state(services))?,
        project: build_module_state_phase(&boot, "project", || build_project_state(services)),
        sidebar: build_module_state_phase(&boot, "sidebar", || build_sidebar_state(services)),
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
                assistant.service.clone(),
            )
        }),
        squad: aionui_team::squad::SquadRouterState {
            repo: Arc::new(aionui_db::SqliteSquadTemplateRepository::new(
                services.database.pool().clone(),
            )),
            catalog: Arc::new(AssistantServiceTeamCatalog {
                assistant_service: assistant.service.clone(),
            }),
            teams: None,
            work_item_worktrees: None,
        },
        session_message: build_module_state_phase(&boot, "session_message", || build_session_message_state(services)),
        skill_runtime: build_module_state_phase(&boot, "skill_runtime", || build_skill_runtime_state(services)),
        cron,
        office: build_module_state_phase(&boot, "office", || build_office_state(services)),
        shell: build_module_state_phase(&boot, "shell", || build_shell_state(services)),
        assistant: assistant.clone(),
        agent_center,
        skill_evolution,
        review: build_module_state_phase(&boot, "review", || build_review_state(services)),
        scm_delivery: build_module_state_phase(&boot, "scm_delivery", || build_delivery_state(services)),
        worktree,
        fanout,
        batch,
    };
    // M8 E7-2: Squad instantiation provisions through the shared Team service.
    states.squad.teams = Some(states.team.service.clone());
    // M8 Q12: Squad instances of one work item share one managed worktree.
    states.squad.work_item_worktrees = Some(Arc::new(
        crate::router::worktree_adapters::SquadWorkItemWorktreeAdapter {
            service: states.worktree.service.clone(),
            task_sessions: Arc::new(aionui_db::SqliteTaskSessionRepository::new(
                services.database.pool().clone(),
            )),
            projects: services.project_service.clone(),
        },
    ));
    // M8 E7-6: board cards show the run state of the linked Squad instance.
    // Weak: the team service already holds the conversation service.
    services
        .conversation_service
        .with_board_squad_run_port(Arc::new(TeamBoardSquadRun {
            teams: Arc::downgrade(&states.team.service),
        }));
    // M8 E6-7: read-only GitHub Issue import through the user's `gh` login.
    services
        .conversation_service
        .with_github_issue_source(Arc::new(crate::github_issue_source::GhCliIssueSource::new()));
    // M8 E7-5: Leader eval comments on the linked work-item timeline.
    states
        .team
        .service
        .with_board_timeline_port(Arc::new(ConversationBoardTimeline {
            conversation: services.conversation_service.clone(),
        }));
    crate::squad_board_relay::spawn(
        &services.event_bus,
        Arc::downgrade(&states.team.service),
        services.conversation_service.clone(),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module state build completed"
    );
    // Late-inject the sidebar's remove-project ports: the team service is built
    // after the sidebar state above, so the wiring cannot happen inside
    // `build_sidebar_state`. `set_remove_project_ports` is set-once.
    states
        .sidebar
        .service
        .set_remove_project_ports(Arc::new(RemoveProjectAdapter {
            conversation: services.conversation_service.clone(),
            team: states.team.service.clone(),
            project: services.project_service.clone(),
        }));
    // Same late-injection as above: the archive paths release the agent runtime
    // via the worker task manager (per-conversation kill) and the team service
    // (team runtime + member kills), neither of which exists when the sidebar
    // state is built. `set_archive_teardown_ports` is set-once.
    states
        .sidebar
        .service
        .set_archive_teardown_ports(Arc::new(ArchiveTeardownAdapter {
            task_manager: services.worker_task_manager.clone(),
            team: states.team.service.clone(),
        }));
    states
        .conversation
        .service
        .recover_stale_runtime_state_on_startup()
        .await;

    Ok((states, channel_components))
}

struct AgentWorkflowTurnSettlementAdapter {
    conversations: Arc<dyn IConversationRepository>,
    agent_center: Arc<AgentCenterService>,
}

struct AgentWorkflowTurnStartGuard {
    agent_center: Weak<AgentCenterService>,
}

#[async_trait::async_trait]
impl OnConversationTurnStarting for AgentWorkflowTurnStartGuard {
    async fn validate_turn_start(
        &self,
        user_id: &str,
        conversation_id: &str,
        conversation_extra: &str,
    ) -> Result<(), String> {
        let Some(agent_center) = self.agent_center.upgrade() else {
            return Err("Agent Workflow service is unavailable".into());
        };
        let Some((run_id, execution_id)) = workflow_execution_context(conversation_extra)? else {
            return Ok(());
        };
        agent_center
            .claim_agent_execution_conversation_for_user(user_id, &run_id, &execution_id, conversation_id)
            .await
            .map_err(|_| "Agent Workflow run is no longer active".to_owned())
    }
}

fn workflow_execution_context(conversation_extra: &str) -> Result<Option<(String, String)>, String> {
    let Ok(extra) = serde_json::from_str::<serde_json::Value>(conversation_extra) else {
        return Ok(None);
    };
    let Some(run_value) = extra.get("agent_workflow_run_id") else {
        return Ok(None);
    };
    let run_id = run_value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Agent Workflow conversation has an invalid run id".to_owned())?;
    let execution_id = extra
        .get("agent_workflow_execution_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Agent Workflow conversation has no execution id".to_owned())?;
    Ok(Some((run_id.to_owned(), execution_id.to_owned())))
}

struct AgentWorkflowExecutionCancellationAdapter {
    conversation_service: ConversationService,
    runtime_state: Arc<aionui_conversation::runtime_state::ConversationRuntimeStateService>,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

#[async_trait::async_trait]
impl AgentWorkflowAgentExecutionCancellationPort for AgentWorkflowExecutionCancellationAdapter {
    async fn cancel_agent_execution(
        &self,
        user_id: &str,
        _run_id: &str,
        _execution_id: &str,
        conversation_id: &str,
    ) -> Result<AgentWorkflowAgentCancellationOutcome, String> {
        let Some(turn_id) = self.runtime_state.active_turn_id_for(conversation_id) else {
            return Ok(AgentWorkflowAgentCancellationOutcome::NotRunning);
        };
        self.conversation_service
            .cancel(user_id, conversation_id, &turn_id, &self.task_manager)
            .await
            .map_err(|error| error.to_string())?;
        Ok(AgentWorkflowAgentCancellationOutcome::Requested)
    }
}

#[async_trait::async_trait]
impl OnConversationTurnSettled for AgentWorkflowTurnSettlementAdapter {
    async fn on_turn_settled(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        settlement: ConversationTurnSettlement,
        error_message: Option<&str>,
        assistant_output: Option<&str>,
    ) {
        let conversation = match self.conversations.get(user_id, conversation_id).await {
            Ok(Some(conversation)) => conversation,
            Ok(None) => return,
            Err(_) => {
                tracing::warn!(
                    user_id,
                    conversation_id,
                    turn_id,
                    "agent-workflow: failed to load settled conversation"
                );
                return;
            }
        };
        let workflow_context = serde_json::from_str::<serde_json::Value>(&conversation.extra)
            .ok()
            .and_then(|extra| {
                let run_id = extra
                    .get("agent_workflow_run_id")
                    .and_then(serde_json::Value::as_str)?
                    .to_owned();
                let execution_id = extra
                    .get("agent_workflow_execution_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                Some((run_id, execution_id))
            });
        let Some((run_id, execution_id)) = workflow_context else {
            return;
        };
        if let Some(execution_id) = execution_id.as_deref()
            && let Err(error) = self
                .agent_center
                .confirm_agent_cancellation_for_user(user_id, &run_id, execution_id, conversation_id)
                .await
        {
            tracing::warn!(
                user_id,
                conversation_id,
                turn_id,
                run_id,
                error = %error,
                "agent-workflow: failed to confirm agent cancellation"
            );
        }
        let assistant_id = match self
            .conversations
            .get_assistant_snapshot(user_id, conversation_id)
            .await
        {
            Ok(Some(snapshot)) => snapshot.assistant_id,
            Ok(None) => return,
            Err(_) => {
                tracing::warn!(
                    user_id,
                    conversation_id,
                    turn_id,
                    run_id,
                    "agent-workflow: failed to load conversation assistant snapshot"
                );
                return;
            }
        };

        let result = self
            .agent_center
            .settle_agent_turn_for_user(
                user_id,
                &run_id,
                AgentWorkflowTurnResult {
                    assistant_id: &assistant_id,
                    execution_id: execution_id.as_deref(),
                    conversation_id,
                    turn_id,
                    success: settlement == ConversationTurnSettlement::Completed,
                    error: error_message.map(str::to_owned),
                    output: assistant_output,
                },
            )
            .await;
        match result {
            Ok(Some(run)) => {
                if let Err(error) = self.agent_center.execute_pending_tools_for_user(user_id, &run.id).await {
                    tracing::warn!(
                        user_id,
                        conversation_id,
                        turn_id,
                        run_id,
                        error = %error,
                        "agent-workflow: failed to execute pending MCP tool"
                    );
                }
            }
            Ok(None) => {}
            Err(_) => {
                tracing::warn!(
                    user_id,
                    conversation_id,
                    turn_id,
                    run_id,
                    "agent-workflow: failed to settle agent turn"
                );
            }
        }
    }
}

struct AgentWorkflowMcpToolAdapter {
    repo: Arc<dyn IMcpServerRepository>,
    client: McpConnectionTestService,
}

#[async_trait::async_trait]
impl AgentWorkflowToolExecutionPort for AgentWorkflowMcpToolAdapter {
    async fn execute(
        &self,
        user_id: &str,
        mcp_server_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let row = self
            .repo
            .find_by_id(user_id, mcp_server_id)
            .await
            .map_err(|_| "failed to load MCP server".to_owned())?
            .ok_or_else(|| "MCP server was not found".to_owned())?;
        if !row.enabled {
            return Err("MCP server is disabled".into());
        }
        let server = McpServer::from_row(row).map_err(|_| "MCP server configuration is invalid".to_owned())?;
        self.client
            .execute_tool(
                &server.transport,
                tool_name,
                arguments,
                Some(user_id),
                Some(mcp_server_id),
            )
            .await
    }
}

/// Cross-session messaging state, plus the process's single drainer.
///
/// The drainer is spawned here rather than in `routes.rs` because this is where
/// this crate already starts background work — see
/// `spawn_assistant_mcp_binding_watcher`, called from `build_assistant_state`.
/// Channel A's state. Read-only, so it takes the conversation repo (for the
/// snapshot allow-list), the skill paths + repo (to resolve and read), and the
/// runtime token service (to authenticate). Deliberately NOT the skill WRITE
/// surface's state: this domain can never import, delete, or enable a skill.
pub fn build_skill_runtime_state(services: &AppServices) -> SkillRuntimeRouterState {
    SkillRuntimeRouterState {
        service: Arc::new(SkillRuntimeService::new(
            services.conversation_repo.clone(),
            services.skill_paths.clone(),
            services.skill_repo.clone(),
        )),
        runtime_token_service: services.runtime_token_service.clone(),
    }
}

pub fn build_session_message_state(services: &AppServices) -> SessionMessageRouterState {
    let state = SessionMessageRouterState {
        service: services.session_message_service.clone(),
        targets: Arc::new(MentionableTargets::new(
            services.conversation_repo.clone(),
            // `ProjectService` is `#[derive(Clone)]` and cheap to clone.
            Arc::new(services.project_service.clone()),
        )),
        runtime_token_service: services.runtime_token_service.clone(),
    };

    // ONE drainer for the whole process — not one per target, not one per
    // message. Both `DeliverySink` and `DrainGate` are implemented by the same
    // service, hence the same Arc twice.
    Arc::new(Drainer::new(
        services.session_message_queue.clone(),
        services.session_message_service.clone(),
        services.session_message_service.clone(),
    ))
    .spawn(services.session_message_notify.clone());

    state
}

/// Build the default `AssistantRouterState` from application services.
pub fn build_assistant_state(services: &AppServices) -> AssistantRouterState {
    #[derive(Clone)]
    struct RegistryAssistantAgentCatalog {
        registry: Arc<aionui_ai_agent::AgentRegistry>,
    }

    #[async_trait::async_trait]
    impl AssistantAgentCatalogPort for RegistryAssistantAgentCatalog {
        async fn list_management_agents(
            &self,
            user_id: &str,
        ) -> Result<Vec<aionui_api_types::AgentManagementRow>, AssistantError> {
            self.registry
                .list_management_rows_for_user(user_id)
                .await
                .map_err(|error| AssistantError::Internal(format!("agent catalog unavailable: {error}")))
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

/// Build Agent Center router state on top of the shared AssistantService.
pub fn build_agent_center_state(services: &AppServices, assistant: &AssistantRouterState) -> AgentCenterRouterState {
    let pool = services.database.pool().clone();
    let definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let center_repo = Arc::new(SqliteAssistantAgentCenterRepository::new(pool.clone()));
    let revision_repo = Arc::new(SqliteAssistantDefinitionRevisionRepository::new(pool.clone()));
    let workflow_run_repo = Arc::new(aionui_db::SqliteAgentWorkflowRunRepository::new(pool.clone()));
    let mcp_repo: Arc<dyn IMcpServerRepository> = Arc::new(aionui_db::SqliteMcpServerRepository::new(pool));
    let tool_executor: Arc<dyn AgentWorkflowToolExecutionPort> = Arc::new(AgentWorkflowMcpToolAdapter {
        repo: mcp_repo,
        client: McpConnectionTestService::new(reqwest::Client::new(), services.event_bus.clone()),
    });
    let service = Arc::new(AgentCenterService::new(
        assistant.service.clone(),
        definition_repo,
        center_repo,
        revision_repo,
        workflow_run_repo,
        Some(tool_executor),
    ));
    let agent_execution_canceller = Arc::new(AgentWorkflowExecutionCancellationAdapter {
        conversation_service: services.conversation_service.clone(),
        runtime_state: services.conversation_runtime_state.clone(),
        task_manager: services.worker_task_manager.clone(),
    });
    AgentCenterRouterState {
        service,
        agent_execution_canceller: Some(agent_execution_canceller),
    }
}

/// Build the review-thread (diff annotation) router state. Repositories are
/// read under the same sandbox roots as the file routes.
pub fn build_review_state(services: &AppServices) -> ReviewRouterState {
    let projects = Arc::new(ReviewProjectDirs {
        projects: services.project_service.clone(),
    });
    let resolver = Arc::new(
        AllowedRootsResolver::new(default_allowed_roots(Some(services.work_dir.as_path()))).with_projects(projects),
    );
    let input_sink = Arc::new(ReviewConversationInputs {
        conversations: services.conversation_service.clone(),
    });
    ReviewRouterState {
        service: Arc::new(ReviewService::new(services.database.pool().clone(), resolver).with_input_sink(input_sink)),
    }
}

/// PR delivery through the user's `gh` / `glab`, sandboxed like review scopes.
pub fn build_delivery_state(services: &AppServices) -> DeliveryRouterState {
    let projects = Arc::new(ReviewProjectDirs {
        projects: services.project_service.clone(),
    });
    let resolver = Arc::new(
        AllowedRootsResolver::new(default_allowed_roots(Some(services.work_dir.as_path()))).with_projects(projects),
    );
    DeliveryRouterState {
        service: Arc::new(DeliveryService::new(services.database.pool().clone(), resolver)),
    }
}

/// Build Skill Evolution router state (经验库 / 技能提案 + Phase 2 evolve/apply).
pub fn build_skill_evolution_state(
    services: &AppServices,
    provider_service: aionui_system::ProviderService,
    agent_center: Arc<aionui_assistant::AgentCenterService>,
) -> SkillEvolutionRouterState {
    let pool = services.database.pool().clone();
    let proposals = Arc::new(SqliteSkillEvolutionProposalRepository::new(pool.clone()));
    let experience = Arc::new(SqliteExperienceArticleRepository::new(pool.clone()));
    let settings_repo = Arc::new(SqliteSkillEvolutionSettingsRepository::new(pool.clone()));
    let teams_repo = Arc::new(SqliteTeamRepository::new(pool.clone()));
    let conversations = Arc::new(SqliteConversationRepository::new(pool));
    let trajectory: Arc<dyn aionui_assistant::SkillEvolutionTrajectoryPort> = Arc::new(ConversationTrajectoryAdapter {
        conversations: services.conversation_service.clone(),
    });
    let llm: Arc<dyn aionui_assistant::SkillEvolutionLlmPort> = Arc::new(ProviderLlmAdapter {
        providers: provider_service,
        http: reqwest::Client::new(),
    });
    let apply_port: Arc<dyn aionui_assistant::SkillEvolutionApplyPort> = Arc::new(SkillHubApplyAdapter {
        skill_paths: services.skill_paths.as_ref().clone(),
        skill_repo: services.skill_repo.clone(),
        source_repo: Some(Arc::new(aionui_db::SqliteSkillSourceRepository::new(
            services.database.pool().clone(),
        ))),
    });
    let pin_port: Arc<dyn aionui_assistant::SkillEvolutionPinPort> = Arc::new(AgentCenterPinAdapter { agent_center });
    let service = Arc::new(
        SkillEvolutionService::new(proposals, experience, conversations)
            .with_settings_and_teams(Some(settings_repo), Some(teams_repo))
            .with_ports(Some(trajectory), Some(llm), Some(apply_port), Some(pin_port)),
    );
    SkillEvolutionRouterState { service }
}

/// Build the default `SystemRouterState` from application services.
pub fn build_system_state(services: &AppServices) -> SystemRouterState {
    let encryption_key = derive_encryption_key(&services.encryption_secret_raw);
    let pool = services.database.pool().clone();
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let http_client = reqwest::Client::new();

    let client_pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let keep_awake_controller = Arc::new(aionui_system::SystemKeepAwakeController::new());
    let client_pref_service = if services.identity_mode.is_local() {
        ClientPrefService::with_keep_awake_controller(client_pref_repo, keep_awake_controller, "system_default_user")
    } else {
        ClientPrefService::with_keep_awake_controller_without_restore(client_pref_repo, keep_awake_controller)
    };

    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(pool.clone()))),
        client_pref_service,
        provider_service: ProviderService::new(provider_repo.clone(), encryption_key),
        model_fetch_service: ModelFetchService::new(provider_repo, encryption_key, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, env!("CARGO_PKG_VERSION").to_owned()),
        runtime_prepare_service: RuntimePrepareService::new(services.event_bus.clone()),
        feedback_diagnostics_service: FeedbackDiagnosticsService::new(Arc::new(
            SqliteFeedbackDiagnosticsRepository::new(pool),
        )),
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
    }
    ConversationRouterState {
        service: conversation_service,
        task_manager: services.worker_task_manager.clone(),
        active_leases: services.active_lease_registry.clone(),
    }
}

/// Build the default `RemoteAgentRouterState` from application services.
pub fn build_remote_agent_state(services: &AppServices) -> RemoteAgentRouterState {
    let encryption_key = derive_encryption_key(&services.encryption_secret_raw);
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
    let file_service = Arc::new(FileService::new(broadcaster.clone(), allowed_roots.clone()));
    let snapshot_service = Arc::new(SnapshotService::new());
    // Shell-backed capabilities for `/api/fs/reveal` (open enclosing folder) and
    // `/api/fs/open-system` (open with the default application): adapters over one
    // shared shell service, injected as the file crate's ports so aionui-file stays
    // free of a shell dependency.
    let shell = Arc::new(aionui_shell::ShellService::new(Arc::new(
        aionui_shell::DefaultSystemOpener,
    )));
    let revealer: aionui_file::ItemRevealerRef = Arc::new(super::item_revealer::ShellItemRevealer::new(shell.clone()));
    let system_opener: aionui_file::SystemFileOpenerRef =
        Arc::new(super::system_file_opener::ShellSystemFileOpener::new(shell.clone()));
    // Clipboard capability for `/api/fs/copy-absolute-path`: the backend resolves
    // the path and writes it to the clipboard itself, so the abs never returns.
    let clipboard: aionui_file::ClipboardWriterRef =
        Arc::new(super::clipboard_writer::ShellClipboardWriter::new(shell));
    Ok(FileRouterState {
        file_service,
        snapshot_service,
        project: Arc::new(services.project_service.clone()),
        revealer,
        system_opener,
        clipboard,
        allowed_roots,
    })
}

/// Build the project control-plane router state from application services.
pub fn build_project_state(services: &AppServices) -> ProjectRouterState {
    ProjectRouterState {
        project: Arc::new(services.project_service.clone()),
    }
}

/// Build the sidebar read/ordering router state from application services.
/// The sidebar store and ordering store share the app-wide pool; `work_dir` is
/// the conversation temp-workspace root used for path classification (must match
/// `ProjectService`'s temp root).
pub fn build_sidebar_state(services: &AppServices) -> SidebarRouterState {
    let sidebar_store: Arc<dyn aionui_db::ISidebarStore> =
        Arc::new(aionui_db::SqliteSidebarStore::new(services.database.pool().clone()));
    let service = SidebarService::new(
        sidebar_store,
        services.user_order_store.clone(),
        services.work_dir.join("conversations"),
    );
    SidebarRouterState {
        service: Arc::new(service),
    }
}

/// Adapts the concrete conversation / team / project services to the sidebar's
/// [`aionui_sidebar::RemoveProjectPorts`] trait so `remove_project` (BR-19) can
/// reuse the existing per-unit delete paths (`ConversationService::delete`,
/// `TeamSessionService::remove_team`, `ProjectService::delete_project`) without
/// the sidebar crate depending on any of them.
struct RemoveProjectAdapter {
    conversation: ConversationService,
    team: Arc<TeamSessionService>,
    project: ProjectService,
}

#[async_trait::async_trait]
impl aionui_sidebar::RemoveProjectPorts for RemoveProjectAdapter {
    async fn delete_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), String> {
        self.conversation
            .delete(user_id, conversation_id)
            .await
            .map_err(|err| err.to_string())
    }

    async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), String> {
        self.team
            .remove_team(user_id, team_id)
            .await
            .map_err(|err| err.to_string())
    }

    async fn delete_project_record(&self, user_id: &str, project_id: &str) -> Result<(), String> {
        self.project
            .delete_project(user_id, project_id)
            .await
            .map_err(|err| err.to_string())
    }
}

/// Adapts the process-teardown paths to the sidebar's
/// [`aionui_sidebar::ArchiveTeardownPorts`] trait so the archive flip can
/// release the agent runtime the same way delete does — kill the conversation's
/// agent process, tear down the team runtime — without deleting any data and
/// without the sidebar crate depending on the ai-agent / team crates.
struct ArchiveTeardownAdapter {
    task_manager: Arc<dyn IWorkerTaskManager>,
    team: Arc<TeamSessionService>,
}

#[async_trait::async_trait]
impl ArchiveTeardownPorts for ArchiveTeardownAdapter {
    async fn stop_conversation(&self, _user_id: &str, conversation_id: &str) -> Result<(), String> {
        // Ownership was already verified by the owner-scoped archive flip; this
        // is a pure process kill (no DB write). `kill_and_wait` is a no-op for a
        // conversation with no live runtime, so an already-idle agent is fine.
        self.task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::Archived))
            .await;
        Ok(())
    }

    async fn stop_team(&self, user_id: &str, team_id: &str) -> Result<(), String> {
        self.team
            .stop_team_processes(user_id, team_id)
            .await
            .map_err(|err| err.to_string())
    }
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

/// Adapter exposing the assistant service's lazy generated-assistant
/// materialization to the channel settings service (avoids a channel→assistant
/// crate dependency; the binding happens here in the composition layer).
struct GeneratedAssistantMaterializerAdapter {
    assistant: Arc<aionui_assistant::AssistantService>,
}

#[async_trait::async_trait]
impl aionui_channel::channel_settings::ChannelGeneratedAssistantMaterializer for GeneratedAssistantMaterializerAdapter {
    async fn ensure_generated_assistants(&self, user_id: &str) {
        // list_for_user runs the per-user generated reconcile as a side effect.
        if let Err(error) = self.assistant.list_for_user(user_id).await {
            tracing::warn!(user_id, error = %error, "channel default: generated assistant materialization failed");
        }
    }
}

fn build_channel_settings_service(
    services: &AppServices,
    generated_assistant_materializer: Option<
        Arc<dyn aionui_channel::channel_settings::ChannelGeneratedAssistantMaterializer>,
    >,
) -> Arc<aionui_channel::channel_settings::ChannelSettingsService> {
    let pref_repo: Arc<dyn aionui_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone()));

    let mut service = aionui_channel::channel_settings::ChannelSettingsService::new(pref_repo)
        .with_agent_metadata_repo(Arc::new(SqliteAgentMetadataRepository::new(
            services.database.pool().clone(),
        )))
        .with_assistant_repos(
            Arc::new(SqliteAssistantDefinitionRepository::new(
                services.database.pool().clone(),
            )),
            Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
        );
    if let Some(materializer) = generated_assistant_materializer {
        service = service.with_generated_assistant_materializer(materializer);
    }
    Arc::new(service)
}

async fn build_channel_message_service(
    services: &AppServices,
    channel_settings: Arc<aionui_channel::channel_settings::ChannelSettingsService>,
) -> Arc<aionui_channel::message_service::ChannelMessageService> {
    Arc::new(aionui_channel::message_service::ChannelMessageService::new(
        Arc::new(services.conversation_service.clone()),
        services.worker_task_manager.clone(),
        channel_settings,
    ))
}

async fn startup_channel_owner_user_id(services: &AppServices) -> Option<String> {
    if services.identity_mode == IdentityMode::AionPro {
        return None;
    }

    let owner_user_id = services
        .user_repo
        .get_primary_webui_user()
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
        .unwrap_or_else(|| "system_default_user".to_string());
    Some(owner_user_id)
}

/// Build the default `ChannelRouterState` and orchestrator components.
pub async fn build_channel_state(
    services: &AppServices,
    extension_registry: ExtensionRegistry,
    generated_assistant_materializer: Option<
        Arc<dyn aionui_channel::channel_settings::ChannelGeneratedAssistantMaterializer>,
    >,
) -> (ChannelRouterState, ChannelOrchestratorComponents) {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn aionui_db::IChannelRepository> = Arc::new(aionui_db::SqliteChannelRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.encryption_secret_raw);

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

    // Build channel settings service for per-plugin agent/model configuration.
    let channel_settings = build_channel_settings_service(services, generated_assistant_materializer);
    let startup_owner_user_id = startup_channel_owner_user_id(services).await;

    // Build orchestrator dependencies
    let action_executor = Arc::new(aionui_channel::action::ActionExecutor::new(
        Arc::clone(&pairing_service),
        Arc::clone(&session_manager),
        Arc::clone(&channel_settings),
        startup_owner_user_id.clone(),
    ));

    let message_service = build_channel_message_service(services, Arc::clone(&channel_settings)).await;

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
        owner_user_id: startup_owner_user_id,
    };

    (state, components)
}

/// Team-selectable Assistant catalog shared by Team spawn and Squad templates.
#[derive(Clone)]
pub(crate) struct AssistantServiceTeamCatalog {
    assistant_service: Arc<AssistantService>,
}

/// M8 E7-6: [`aionui_conversation::board::BoardSquadRunPort`] over the team
/// service.
struct TeamBoardSquadRun {
    teams: std::sync::Weak<TeamSessionService>,
}

#[async_trait::async_trait]
impl aionui_conversation::board::BoardSquadRunPort for TeamBoardSquadRun {
    async fn squad_run_for_work_item(
        &self,
        user_id: &str,
        work_item_id: &str,
    ) -> Option<aionui_api_types::SquadRunPayload> {
        let teams = self.teams.upgrade()?;
        match teams.squad_run_for_work_item(user_id, work_item_id).await {
            Ok(run) => run,
            Err(error) => {
                tracing::warn!(work_item_id, %error, "board squad_run lookup failed");
                None
            }
        }
    }

    async fn pause_for_work_item(&self, user_id: &str, work_item_id: &str) {
        let Some(teams) = self.teams.upgrade() else {
            return;
        };
        if let Err(error) = teams.pause_squad_for_work_item(user_id, work_item_id).await {
            tracing::warn!(work_item_id, %error, "board squad pause on backlog failed");
        }
    }
}

/// M8 E7-5: [`BoardWorkItemTimelinePort`] over ConversationService.
struct ConversationBoardTimeline {
    conversation: ConversationService,
}

#[async_trait::async_trait]
impl BoardWorkItemTimelinePort for ConversationBoardTimeline {
    async fn append_leader_eval_comment(
        &self,
        user_id: &str,
        work_item_id: &str,
        author_agent_id: &str,
        body: &str,
    ) -> Result<(), aionui_team::TeamError> {
        self.conversation
            .append_agent_board_comment(user_id, work_item_id, author_agent_id, body)
            .await
            .map(|_| ())
            .map_err(|error| aionui_team::TeamError::InvalidRequest(format!("board timeline write failed: {error}")))
    }
}

#[async_trait::async_trait]
impl TeamAssistantCatalogPort for AssistantServiceTeamCatalog {
    async fn list_team_selectable_assistants(
        &self,
        user_id: &str,
    ) -> Result<Vec<TeamAssistantCatalogEntry>, aionui_team::TeamError> {
        let assistants = self.assistant_service.list_for_user(user_id).await.map_err(|error| {
            aionui_team::TeamError::InvalidRequest(format!("assistant catalog unavailable: {error}"))
        })?;

        Ok(assistants
            .into_iter()
            .filter(|assistant| assistant.team_selectable)
            .filter_map(|assistant| {
                let agent = assistant.agent?;
                let backend = agent
                    .acp_backend
                    .unwrap_or_else(|| agent.r#type.serde_name().to_owned());
                Some(TeamAssistantCatalogEntry {
                    assistant_id: assistant.id,
                    name: assistant.name,
                    backend,
                    description: assistant.description.unwrap_or_default(),
                    skills: assistant
                        .enabled_skills
                        .into_iter()
                        .chain(assistant.custom_skill_names)
                        .collect(),
                })
            })
            .collect())
    }
}

/// Build the default `TeamRouterState` from application services.
///
/// `backend_binary_path` is resolved once while constructing `AppServices` and
/// cloned into each builder that needs it, per
/// `docs/teams/phase1/interface-contracts.md` §10.
pub fn build_team_state(
    services: &AppServices,
    _cron_service: Option<Arc<aionui_cron::service::CronService>>,
    backend_binary_path: Arc<std::path::PathBuf>,
    assistant_service: Arc<AssistantService>,
) -> TeamRouterState {
    let pool = services.database.pool().clone();
    let team_repo: Arc<dyn aionui_db::ITeamRepository> = Arc::new(aionui_db::SqliteTeamRepository::new(pool.clone()));
    let conv_service = services.conversation_service.clone();
    let conv_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool));
    let adapters = Arc::new(TeamConversationAdapters::new(
        conv_service,
        conv_repo,
        Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone())),
        services.worker_task_manager.clone(),
    ));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = adapters.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = adapters.clone();
    let turn_port: Arc<dyn AgentTurnExecutionPort> = adapters.clone();
    let slash_command_port: Arc<dyn NativeSlashCommandPort> = adapters.clone();
    let cancellation_port: Arc<dyn AgentTurnCancellationPort> = adapters;
    let capability_port: Arc<dyn aionui_team::TeamToolCapabilityPort> = Arc::new(TeamCapabilityResolver::new(
        Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone())),
    ));
    let service = TeamSessionService::new_with_prompt_dump(
        team_repo,
        Arc::new(SqliteAgentMetadataRepository::new(services.database.pool().clone())),
        Arc::new(AssistantServiceTeamCatalog { assistant_service }),
        Arc::new(SqliteAssistantDefinitionRepository::new(
            services.database.pool().clone(),
        )),
        Arc::new(SqliteAssistantOverlayRepository::new(services.database.pool().clone())),
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        conversation_port,
        projection_store,
        services.event_bus.clone(),
        services.worker_task_manager.clone(),
        turn_port,
        cancellation_port,
        slash_command_port,
        capability_port,
        backend_binary_path,
        aionui_team::TeamPromptDumpConfig::from_data_dir(&services.data_dir, services.dump_prompts),
    );
    service.with_project_service(Arc::new(services.project_service.clone()));
    // Path-2 cascade: removing a team drops its `user_order` row (sidebar §4.3).
    service.with_user_order_store(services.user_order_store.clone());
    TeamRouterState {
        service,
        active_leases: services.active_lease_registry.clone(),
    }
}

/// Build the M7 E3 worktree state: managed root `{data_dir}/worktrees`,
/// system `git` CLI, events on the shared bus.
pub fn build_worktree_state(services: &AppServices) -> aionui_worktree::WorktreeRouterState {
    let store = aionui_worktree::WorktreeStore::new(services.database.pool().clone());
    let service = aionui_worktree::WorktreeService::new(
        store,
        aionui_worktree::WorktreeServiceConfig {
            root_dir: services.data_dir.join("worktrees"),
            git: aionui_worktree::GitCli::default(),
            events: aionui_worktree::WorktreeEvents::new(services.event_bus.clone()),
        },
    );
    aionui_worktree::WorktreeRouterState { service }
}

/// Build the default `CronRouterState` from application services.
pub fn build_cron_state(services: &AppServices) -> CronRouterState {
    let pool = services.database.pool().clone();
    let cron_repo: Arc<dyn aionui_db::ICronRepository> = Arc::new(aionui_db::SqliteCronRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(pool));

    // Reuse the app-level ConversationService (AppServices is the sole service
    // construction center). A separate instance carries its own
    // background-watcher registry — `ensure_background_watcher` is idempotent
    // only within one instance — so a cron-triggered conversation would get a
    // second BackgroundStreamWatcher on the same agent broadcast channel and
    // every CLI-initiated (orphan) turn would be persisted twice.
    let conv_service = services.conversation_service.clone();

    let executor = Arc::new(aionui_cron::executor::JobExecutor::new(
        services.worker_task_manager.clone(),
        conv_service.conversation_repo().clone(),
        Arc::new(conv_service.clone()),
        services.work_dir.clone(),
        services.data_dir.clone(),
        services.event_bus.clone(),
        services.agent_registry.clone(),
    ));

    let tick_service_ref: Arc<CronServiceTickRef> = Arc::new(CronServiceTickRef::default());
    let tick_ref = tick_service_ref.clone();
    let scheduler = Arc::new(aionui_cron::scheduler::CronScheduler::new(Arc::new(
        move |tick: aionui_cron::scheduler::ScheduledTick| {
            let svc = tick_ref.0.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Some(svc) = svc {
                    svc.tick(&tick.job_id, tick.scheduled_at).await;
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
        skill_repo: services.skill_repo.clone(),
        scheduler,
        executor,
        emitter,
        data_dir: services.data_dir.clone(),
    }));
    cron_service.set_governance(services.governance.clone());

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

    let conversion_service = Arc::new(ConversionService::with_data_dir(None, data_dir.to_path_buf()));
    let proxy_service = Arc::new(ProxyService::new(watch_manager.clone()));
    let presentation_service = Arc::new(aionui_office::PresentationService::new(None));

    OfficeRouterState {
        watch_manager,
        conversion_service,
        proxy_service,
        presentation_service,
        allowed_roots,
        project: Arc::new(services.project_service.clone()),
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
    let skill_registry_service = SkillRegistryService::new(
        SkillHubClient::production().expect("fixed CSBU SkillHub client must build"),
        services.skill_paths.as_ref().clone(),
        services.skill_repo.clone(),
    );

    let ext_state = ExtensionRouterState::new(registry.clone());

    let hub_state = HubRouterState {
        index_manager,
        installer,
    };

    let skill_state = SkillRouterState {
        skill_paths: services.skill_paths.as_ref().clone(),
        skill_repo: services.skill_repo.clone(),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None,
        registry_service: Some(skill_registry_service),
        governance: Some(aionui_extension::skill_governance::SkillGovernanceState {
            skill_paths: services.skill_paths.as_ref().clone(),
            skill_repo: services.skill_repo.clone(),
            source_repo: Arc::new(aionui_db::SqliteSkillSourceRepository::new(
                services.database.pool().clone(),
            )),
            // E10-5: the installation is one implicit team; owner/admin roles
            // (E12 governance) manage it, and only when multi-user roles are active.
            team_roles: Arc::new(aionui_extension::skill_governance::InstallationTeamRoles::new(
                services.governance.clone(),
            )),
        }),
    };

    (ext_state, hub_state, skill_state)
}

/// Build the `WsHandlerState` from application services with an explicit inbound
/// `router`. Callers supply the filesystem-monitor router in production and a
/// no-op router for router-only/test assembly.
pub fn build_ws_state(services: &AppServices, router: Arc<dyn MessageRouter>) -> WsHandlerState {
    if services.local && services.local_api_secret.is_none() {
        // Legacy launcher without a per-launch secret: every local socket is
        // the desktop user.
        return WsHandlerState {
            manager: services.ws_manager.clone(),
            router,
            token_validator: Arc::new(|_| true),
            token_user_resolver: Arc::new(|_| Box::pin(async { Some("system_default_user".to_owned()) })),
            token_extractor: Arc::new(|_| Some("local".into())),
        };
    }

    // With a local secret the desktop socket presents it as a header; any
    // other socket (WebUI browser through the desktop proxy) needs a session
    // JWT like non-local mode. The extractor maps a matching header to an
    // unguessable per-process marker so a forged cookie can never equal it.
    let local_marker: Option<Arc<str>> = services
        .local_api_secret
        .as_ref()
        .map(|_| Arc::from(format!("local-desktop:{}", aionui_auth::generate_random_secret_string()).as_str()));

    let jwt_service = services.jwt_service.clone();
    let validator_marker = local_marker.clone();
    let token_validator = Arc::new(move |token: &str| {
        if validator_marker.as_deref() == Some(token) {
            return true;
        }
        // Device tokens (E9-2) are checked against the store at connect
        // time; a revocation closes their sockets directly.
        if aionui_auth::device::token::is_device_token(token) {
            return true;
        }
        jwt_service.verify(token).is_ok()
    });
    let jwt_service = services.jwt_service.clone();
    let user_repo = services.user_repo.clone();
    let identity_mode = services.identity_mode;
    let resolver_marker = local_marker.clone();
    let user_devices = services.user_devices.clone();
    let token_user_resolver: TokenUserResolver = Arc::new(move |token: String| {
        let jwt_service = jwt_service.clone();
        let user_repo = user_repo.clone();
        let user_devices = user_devices.clone();
        let is_local_desktop = resolver_marker.as_deref() == Some(token.as_str());
        Box::pin(async move {
            if is_local_desktop {
                return Some("system_default_user".to_owned());
            }
            if aionui_auth::device::token::is_device_token(&token) {
                return crate::router::device_ws::resolve_device_ws_user(
                    user_devices.as_ref(),
                    user_repo.as_ref(),
                    identity_mode,
                    &token,
                )
                .await;
            }
            let payload = jwt_service.verify(&token).ok()?;
            let user = user_repo.find_active_by_id(&payload.user_id).await.ok()??;
            if identity_mode == IdentityMode::AionPro && user.user_type != aionui_db::UserType::Aionpro {
                return None;
            }
            (payload.session_generation == user.session_generation).then_some(user.id)
        })
    });

    let local_secret = services.local_api_secret.clone();
    let token_extractor = Arc::new(move |headers: &axum::http::HeaderMap| {
        if let (Some(secret), Some(marker)) = (local_secret.as_ref(), local_marker.as_ref())
            && secret.matches_headers(headers)
        {
            return Some(marker.to_string());
        }
        extract_token_from_ws_headers(headers)
    });

    WsHandlerState {
        manager: services.ws_manager.clone(),
        router: Arc::new(crate::router::device_ws::DeviceInboundGuard {
            inner: router,
            manager: services.ws_manager.clone(),
        }),
        token_validator,
        token_user_resolver,
        token_extractor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // T-B4 — concurrent-startup contention gets a distinct boundary stage so it
    // is not misreported as local data corruption (Sentry 135525166).
    #[test]
    fn concurrent_contention_maps_to_distinct_bootstrap_stage() {
        let contended =
            assistant_bootstrap_build_error(AssistantError::ConcurrentBootstrapContention("contended".into()));
        assert_eq!(contended.stage(), "router.assistant.bootstrap.concurrency_contended");

        let other = assistant_bootstrap_build_error(AssistantError::Internal("boom".into()));
        assert_eq!(other.stage(), "router.assistant.bootstrap");
    }

    use crate::AppConfig;
    use aionui_ai_agent::types::{AIONUI_BASE_URL_ENV, AIONUI_HELPER_BIN_ENV, BuildTaskOptions, SendMessageData};
    use aionui_ai_agent::{
        AgentError, AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent, IWorkerTaskManager,
        WorkerTaskManagerImpl,
    };
    use aionui_api_types::{CreateConversationRequest, SendMessageRequest};
    use aionui_channel::types::PluginType;
    use aionui_common::{AgentKillReason, AgentType, ConversationStatus, TimestampMs};
    use aionui_db::models::{AssistantSessionRow, UpsertAssistantDefinitionParams};
    use aionui_db::{
        IAssistantDefinitionRepository, IClientPreferenceRepository, IConversationRepository,
        SqliteAssistantDefinitionRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    };
    use aionui_extension::{ExtensionSource, ScanPath};

    struct ChannelStateNoopAgent {
        conversation_id: String,
        workspace: String,
    }

    #[async_trait::async_trait]
    impl IAgentTask for ChannelStateNoopAgent {
        fn agent_type(&self) -> AgentType {
            AgentType::Aionrs
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            &self.workspace
        }

        fn status(&self) -> Option<ConversationStatus> {
            Some(ConversationStatus::Finished)
        }

        fn last_activity_at(&self) -> TimestampMs {
            0
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentStreamEvent> {
            let (tx, _) = tokio::sync::broadcast::channel(1);
            tx.subscribe()
        }

        async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
            Ok(())
        }

        async fn cancel(&self) -> Result<(), AgentError> {
            Ok(())
        }

        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            Ok(())
        }
    }

    impl IMockAgent for ChannelStateNoopAgent {}

    type CapturedRuntimeEnv = Arc<Mutex<Vec<Vec<(String, String)>>>>;

    fn mock_worker_task_manager() -> Arc<dyn IWorkerTaskManager> {
        let factory = Arc::new(|opts: BuildTaskOptions| {
            Box::pin(async move {
                Ok(AgentInstance::Mock(Arc::new(ChannelStateNoopAgent {
                    conversation_id: opts.conversation_id().to_owned(),
                    workspace: opts.context.workspace.path,
                })))
            }) as futures_util::future::BoxFuture<'static, Result<AgentInstance, AgentError>>
        });

        Arc::new(WorkerTaskManagerImpl::new(factory))
    }

    fn capturing_worker_task_manager(captured_env: CapturedRuntimeEnv) -> Arc<dyn IWorkerTaskManager> {
        let factory = Arc::new(move |opts: BuildTaskOptions| {
            let captured_env = captured_env.clone();
            Box::pin(async move {
                let conversation_id = opts.conversation_id().to_owned();
                let workspace = opts.context.workspace.path.clone();
                captured_env.lock().unwrap().push(opts.context.runtime_env.clone());
                Ok(AgentInstance::Mock(Arc::new(ChannelStateNoopAgent {
                    conversation_id,
                    workspace,
                })))
            }) as futures_util::future::BoxFuture<'static, Result<AgentInstance, AgentError>>
        });

        Arc::new(WorkerTaskManagerImpl::new(factory))
    }

    async fn wait_for_captured_env(captured_env: &CapturedRuntimeEnv) -> Vec<(String, String)> {
        for _ in 0..50 {
            if let Some(env) = captured_env.lock().unwrap().first().cloned() {
                return env;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected task options to be captured");
    }

    fn make_send_message_request() -> SendMessageRequest {
        serde_json::from_value(serde_json::json!({
            "content": "Check runtime env"
        }))
        .unwrap()
    }

    fn channel_state_assistant_definition() -> UpsertAssistantDefinitionParams<'static> {
        UpsertAssistantDefinitionParams {
            id: "asstdef-channel-state-aionrs",
            assistant_id: "bare-channel-aionrs",
            source: "generated",
            owner_type: "system",
            source_ref: Some("bare-channel-aionrs"),
            name: "Bare Channel Aionrs",
            name_i18n: "{}",
            description: Some("Channel state regression assistant"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("A"),
            agent_id: "632f31d2",
            rule_resource_type: "user_file",
            rule_resource_ref: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_thought_level_mode: "auto",
            default_thought_level_value: None,
            default_skills_mode: "auto",
            default_skill_ids: "[]",
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        }
    }

    #[tokio::test]
    async fn build_ws_state_rejects_stale_session_generation() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
        let ws_state = build_ws_state(&services, std::sync::Arc::new(aionui_realtime::NoopMessageRouter));
        let token = services
            .jwt_service
            .sign_with_session_generation("system_default_user", "admin", 0)
            .unwrap();

        let resolved = (ws_state.token_user_resolver)(token.clone()).await;
        assert_eq!(resolved.as_deref(), Some("system_default_user"));

        services
            .user_repo
            .increment_session_generation("system_default_user")
            .await
            .unwrap();

        let resolved = (ws_state.token_user_resolver)(token).await;
        assert_eq!(resolved, None);

        services.database.close().await;
    }

    #[tokio::test]
    async fn build_ws_state_local_secret_requires_header_or_session() {
        const SECRET: &str = "ws-local-secret-0123456789-abcdefghijklmnop";
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            local: true,
            identity_mode: IdentityMode::Local,
            local_api_secret: Some(aionui_auth::LocalApiSecret::parse(SECRET).unwrap()),
            ..AppConfig::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();
        let ws_state = build_ws_state(&services, std::sync::Arc::new(aionui_realtime::NoopMessageRouter));

        // No credentials: nothing to authenticate with.
        let empty = axum::http::HeaderMap::new();
        assert_eq!((ws_state.token_extractor)(&empty), None);

        // Desktop header resolves to the default user.
        let mut desktop = axum::http::HeaderMap::new();
        desktop.insert(aionui_auth::LOCAL_API_SECRET_HEADER, SECRET.parse().unwrap());
        let token = (ws_state.token_extractor)(&desktop).expect("desktop marker");
        assert!(!token.contains(SECRET), "marker must not echo the secret");
        assert!((ws_state.token_validator)(&token));
        assert_eq!(
            (ws_state.token_user_resolver)(token).await.as_deref(),
            Some("system_default_user")
        );

        // A forged cookie that looks like a marker is just an invalid JWT.
        let mut forged = axum::http::HeaderMap::new();
        forged.insert(axum::http::header::COOKIE, "aionui-session=local".parse().unwrap());
        let forged_token = (ws_state.token_extractor)(&forged).unwrap();
        assert!(!(ws_state.token_validator)(&forged_token));

        // WebUI session JWT still works.
        let jwt = services
            .jwt_service
            .sign_with_session_generation("system_default_user", "admin", 0)
            .unwrap();
        assert!((ws_state.token_validator)(&jwt));
        assert_eq!(
            (ws_state.token_user_resolver)(jwt).await.as_deref(),
            Some("system_default_user")
        );

        services.database.close().await;
    }

    #[tokio::test]
    async fn build_ws_state_aionpro_rejects_local_user_token() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            identity_mode: IdentityMode::AionPro,
            bootstrap_secret: Some("bootstrap-secret".to_owned()),
            ..AppConfig::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();
        let ws_state = build_ws_state(&services, std::sync::Arc::new(aionui_realtime::NoopMessageRouter));
        let token = services
            .jwt_service
            .sign_with_session_generation("system_default_user", "admin", 0)
            .unwrap();

        let resolved = (ws_state.token_user_resolver)(token).await;
        assert_eq!(resolved, None);

        services.database.close().await;
    }

    #[tokio::test]
    async fn build_channel_message_service_uses_app_conversation_service_for_assistant_bindings() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default())
            .await
            .unwrap()
            .with_worker_task_manager(mock_worker_task_manager());

        let pool = services.database.pool().clone();
        let definition_repo = SqliteAssistantDefinitionRepository::new(pool.clone());
        definition_repo
            .upsert(&channel_state_assistant_definition())
            .await
            .unwrap();

        let pref_repo = SqliteClientPreferenceRepository::new(pool.clone());
        pref_repo
            .upsert_batch(
                "system_default_user",
                &[(
                    "assistant.weixin.agent",
                    r#"{"assistant_id":"bare-channel-aionrs","name":"Weixin Aionrs"}"#,
                )],
            )
            .await
            .unwrap();

        let settings = build_channel_settings_service(&services, None);
        let message_service = build_channel_message_service(&services, settings).await;
        let session = AssistantSessionRow {
            id: "session-channel-state".to_owned(),
            user_id: "channel-user-state".to_owned(),
            agent_type: "aionrs".to_owned(),
            conversation_id: None,
            workspace: None,
            chat_id: Some("wx-chat-state".to_owned()),
            created_at: 1,
            last_activity: 1,
        };

        let first = message_service
            .send_to_agent("system_default_user", &session, "hello", PluginType::Weixin)
            .await
            .unwrap();

        let conversation_repo = SqliteConversationRepository::new(pool);
        let user_id = conversation_repo
            .owner_user_id(&first.conversation_id)
            .await
            .unwrap()
            .expect("channel-created conversation should have an owner");
        let snapshot = conversation_repo
            .get_assistant_snapshot(&user_id, &first.conversation_id)
            .await
            .unwrap()
            .expect("channel-created conversation should persist assistant snapshot");
        let conversation = conversation_repo
            .get(&user_id, &first.conversation_id)
            .await
            .unwrap()
            .expect("channel-created conversation should be persisted");

        assert_eq!(snapshot.assistant_id, "bare-channel-aionrs");
        assert_eq!(snapshot.agent_id, "632f31d2");
        assert_eq!(conversation.r#type, AgentType::Aionrs.serde_name());
        assert_eq!(conversation.name, "Weixin Aionrs");

        let second_session = AssistantSessionRow {
            conversation_id: Some(first.conversation_id.clone()),
            ..session
        };
        let second = message_service
            .send_to_agent("system_default_user", &second_session, "again", PluginType::Weixin)
            .await
            .unwrap();
        assert_eq!(second.conversation_id, first.conversation_id);

        services.database.close().await;
    }

    #[tokio::test]
    async fn build_cron_state_conversation_service_injects_runtime_helper_context() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = AppConfig {
            data_dir: tmp.path().join("data"),
            work_dir: tmp.path().join("work"),
            ..Default::default()
        };
        let db = aionui_db::init_database_memory().await.unwrap();
        let captured_env = Arc::new(Mutex::new(Vec::new()));
        let task_manager = capturing_worker_task_manager(captured_env.clone());
        let services = AppServices::from_config(db, &config)
            .await
            .unwrap()
            .with_worker_task_manager(task_manager.clone());
        let cron = build_cron_state(&services);
        let conversation = cron
            .conversation_service
            .create(
                "system_default_user",
                serde_json::from_value::<CreateConversationRequest>(serde_json::json!({
                    "type": "acp",
                    "extra": {
                        "workspace": workspace,
                        "custom_workspace": true
                    }
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        cron.conversation_service
            .send_message(
                "system_default_user",
                &conversation.id,
                make_send_message_request(),
                &task_manager,
            )
            .await
            .unwrap();

        let env = wait_for_captured_env(&captured_env).await;
        assert!(
            env.iter()
                .any(|(key, value)| key == AIONUI_HELPER_BIN_ENV && !value.is_empty()),
            "cron conversation runtime env should include AIONUI_HELPER_BIN"
        );
        assert!(
            env.contains(&(AIONUI_BASE_URL_ENV.to_owned(), config.local_base_url())),
            "cron conversation runtime env should include AIONUI_BASE_URL"
        );

        services.database.close().await;
    }

    /// The cron path must share the app-level `ConversationService` (a clone,
    /// so all `Arc` internals — including the background-watcher registry —
    /// are shared) rather than construct its own instance. A second instance
    /// carries its own watcher registry: `ensure_background_watcher` is
    /// idempotent only within one instance, so a cron-triggered conversation
    /// got a SECOND `BackgroundStreamWatcher` subscribed to the same agent
    /// broadcast channel and every CLI-initiated (orphan) turn's text and
    /// thinking rows were persisted twice.
    #[tokio::test]
    async fn build_cron_state_reuses_app_conversation_service() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = AppConfig {
            data_dir: tmp.path().join("data"),
            work_dir: tmp.path().join("work"),
            ..Default::default()
        };
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &config).await.unwrap();

        let cron = build_cron_state(&services);

        assert!(
            Arc::ptr_eq(
                cron.conversation_service.conversation_repo(),
                services.conversation_service.conversation_repo(),
            ),
            "cron's ConversationService must be a clone of AppServices' instance, \
             not an independently constructed one"
        );

        services.database.close().await;
    }

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
}

/// Delivers review batches as ordinary user input through the conversation
/// input queue (same path as the send box's followup / mid-turn steer).
struct ReviewConversationInputs {
    conversations: ConversationService,
}

fn review_input_error(error: aionui_conversation::ConversationError) -> aionui_review::ReviewInputError {
    use aionui_conversation::ConversationError as E;
    match error {
        E::NotFound { .. } | E::NotFoundReason { .. } => aionui_review::ReviewInputError::NotFound,
        E::CapabilityUnsupported { .. } => aionui_review::ReviewInputError::Unsupported,
        other => aionui_review::ReviewInputError::Rejected(other.to_string()),
    }
}

#[async_trait::async_trait]
impl aionui_review::ReviewInputSink for ReviewConversationInputs {
    async fn steer_available(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<bool, aionui_review::ReviewInputError> {
        let capabilities = self
            .conversations
            .conversation_capabilities(user_id, conversation_id)
            .await
            .map_err(review_input_error)?;
        if !capabilities.steer {
            return Ok(false);
        }
        Ok(self
            .conversations
            .runtime_summary_for(conversation_id)
            .await
            .is_processing)
    }

    async fn submit(
        &self,
        user_id: &str,
        conversation_id: &str,
        mode: aionui_review::ReviewDeliveryMode,
        content: &str,
        client_key: &str,
    ) -> Result<aionui_review::ReviewInputAccepted, aionui_review::ReviewInputError> {
        let mode = match mode {
            aionui_review::ReviewDeliveryMode::Steer => aionui_api_types::ConversationInputMode::Steer,
            aionui_review::ReviewDeliveryMode::Followup => aionui_api_types::ConversationInputMode::Followup,
        };
        let receipt = self
            .conversations
            .submit_input(
                user_id,
                conversation_id,
                aionui_api_types::SubmitConversationInputRequest {
                    mode,
                    content: content.to_owned(),
                    files: Vec::new(),
                    inject_skills: Vec::new(),
                    hidden: false,
                    client_key: client_key.to_owned(),
                },
            )
            .await
            .map_err(review_input_error)?;
        Ok(aionui_review::ReviewInputAccepted {
            input_id: receipt.input.input_id.clone(),
            receipt: serde_json::to_value(&receipt)
                .map_err(|error| aionui_review::ReviewInputError::Rejected(error.to_string()))?,
        })
    }
}

/// Resolves review `pe:` scopes through the caller's own project entries.
struct ReviewProjectDirs {
    projects: aionui_project::ProjectService,
}

#[async_trait::async_trait]
impl aionui_review::ProjectDirResolver for ReviewProjectDirs {
    async fn resolve_dir(&self, user_id: &str, pe_id: &str, relative_path: &str) -> Option<std::path::PathBuf> {
        let input = aionui_project::types::ReferenceInput {
            pe_id: pe_id.to_owned(),
            relative_path: relative_path.to_owned(),
            op: aionui_project::types::FileOp::Browse,
        };
        let resolved = self.projects.resolve_reference(user_id, input).await.ok()?;
        resolved.absolute_path.map(std::path::PathBuf::from)
    }
}
