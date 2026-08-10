//! Shared application services for dependency injection.

use std::path::PathBuf;
use std::sync::Arc;
use std::{fs::OpenOptions, io::Write};

use crate::config::{AppConfig, IdentityMode, derive_encryption_key};
use aionui_ai_agent::{
    AcpSessionSyncService, AcpSkillManager, ActiveLeaseRegistry, AgentFactoryDeps, AgentRegistry, IWorkerTaskManager,
    RuntimeTokenService, WorkerTaskManagerImpl, build_agent_factory,
};
use aionui_auth::{
    CookieConfig, JwtService, QrTokenStore, generate_password, hash_password, resolve_jwt_secret, validate_password,
    validate_username, verify_password,
};
use aionui_common::OnConversationDelete;
use aionui_conversation::{ConversationService, runtime_state::ConversationRuntimeStateService};
use aionui_db::{
    Database, IAcpSessionRepository, IAdminUserRepository, IAgentMetadataRepository, IConversationRepository,
    IMcpServerRepository, IProjectStore, IResourceShareRepository, ISkillRepository, IUserRepository,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantOverlayRepository, SqliteAssistantPreferenceRepository, SqliteConversationRepository,
    SqliteMcpServerRepository, SqliteProjectStore, SqliteProviderRepository, SqliteResourceShareRepository,
    SqliteSkillRepository, SqliteUserRepository,
};
use aionui_project::ProjectService;
use aionui_realtime::{BroadcastEventBus, WebSocketManager};

pub struct AppServices {
    pub database: Database,
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub admin_user_repo: Arc<dyn IAdminUserRepository>,
    pub share_repo: Arc<dyn IResourceShareRepository>,
    pub initial_admin_credentials_file: Option<Arc<PathBuf>>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub ws_manager: Arc<WebSocketManager>,
    pub event_bus: Arc<BroadcastEventBus>,
    pub worker_task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_lease_registry: Arc<ActiveLeaseRegistry>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
    pub conversation_runtime_state: Arc<ConversationRuntimeStateService>,
    pub conversation_service: ConversationService,
    /// Project-bind service (project-bind side branch). Shared by conversation
    /// and team wiring to bind/backfill project/folder rows. Cheap to clone.
    pub project_service: ProjectService,
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
    pub identity_mode: IdentityMode,
    pub bootstrap_secret: Option<Arc<str>>,
    pub local_client_secret: Option<Arc<str>>,
    pub allowed_origins: Arc<[String]>,
    pub app_version: String,
    /// Resolved skill paths. Shared with the `ConversationService` for
    /// snapshot resolution at create time.
    pub skill_paths: Arc<aionui_extension::SkillPaths>,
    /// User skill metadata and import history repository.
    pub skill_repo: Arc<dyn ISkillRepository>,
    runtime_helper_bin: String,
    runtime_base_url: String,
    /// Shared with the Antigravity hook endpoint so it can authenticate callbacks.
    pub(crate) antigravity_hook_tokens: Arc<aionui_ai_agent::antigravity_hook::HookTokenRegistry>,
}

impl AppServices {
    pub(crate) fn runtime_helper_bin(&self) -> String {
        self.runtime_helper_bin.clone()
    }

    pub(crate) fn runtime_base_url(&self) -> String {
        self.runtime_base_url.clone()
    }

    pub(crate) fn conversation_workspace_root(&self) -> PathBuf {
        conversation_workspace_root(self.identity_mode, &self.data_dir, &self.work_dir)
    }

    /// Replace the worker task manager after construction.
    ///
    /// Primarily used by tests to inject mock implementations.
    pub fn with_worker_task_manager(mut self, wtm: Arc<dyn IWorkerTaskManager>) -> Self {
        self.worker_task_manager = wtm;
        let workspace_root = self.conversation_workspace_root();
        self.conversation_service = build_conversation_service(ConversationServiceDeps {
            database: &self.database,
            work_dir: workspace_root,
            event_bus: self.event_bus.clone(),
            skill_paths: self.skill_paths.clone(),
            skill_repo: self.skill_repo.clone(),
            worker_task_manager: self.worker_task_manager.clone(),
            conversation_runtime_state: self.conversation_runtime_state.clone(),
            conversation_repo: self.conversation_repo.clone(),
            task_manager_delete_hook: self.task_manager_delete_hook.clone(),
            runtime_helper_bin: self.runtime_helper_bin.clone(),
            runtime_base_url: self.runtime_base_url.clone(),
            runtime_token_service: self.runtime_token_service.clone(),
            project_service: self.project_service.clone(),
        });
        self
    }

    pub async fn from_config(database: Database, config: &AppConfig) -> anyhow::Result<Self> {
        let data_dir = config.data_dir.clone();
        let work_dir = config.work_dir.clone();
        let identity_mode = config.effective_identity_mode();
        let local = identity_mode.is_local();
        if local {
            let secret = config
                .local_client_secret
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Local identity mode requires AIONCORE_LOCAL_CLIENT_SECRET"))?;
            crate::config::validate_local_client_secret(secret)
                .map_err(|error| anyhow::anyhow!("Invalid AIONCORE_LOCAL_CLIENT_SECRET: {error}"))?;
        }
        let bootstrap_workspace = if local {
            None
        } else {
            config
                .bootstrap_workspace
                .as_deref()
                .map(resolve_bootstrap_workspace)
                .transpose()?
        };
        let conversation_workspace_root = conversation_workspace_root(identity_mode, &data_dir, &work_dir);
        let upload_root = data_dir.join("uploads");
        let (conversation_workspace_root, upload_root) = if local {
            (conversation_workspace_root, upload_root)
        } else {
            (
                prepare_user_session_root(&conversation_workspace_root, bootstrap_workspace.as_deref())?,
                prepare_user_session_root(&upload_root, bootstrap_workspace.as_deref())?,
            )
        };
        let dump_prompts = config.dump_prompts;
        let app_version = config.app_version.clone();
        let sqlite_user_repo = Arc::new(SqliteUserRepository::new(database.pool().clone()));
        let user_repo: Arc<dyn IUserRepository> = sqlite_user_repo.clone();
        let admin_user_repo: Arc<dyn IAdminUserRepository> = sqlite_user_repo;
        let initial_admin_credentials_file = if identity_mode == IdentityMode::WebUi && config.bootstrap_initial_admin {
            bootstrap_initial_webui_admin(user_repo.as_ref(), admin_user_repo.as_ref(), &data_dir)
                .await?
                .map(Arc::new)
        } else {
            None
        };

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

        // Defense-in-depth for the encryption key: generating a NEW secret is
        // only legitimate on a genuinely fresh install. If the read path
        // claimed "no system user" while the row actually exists (as happened
        // when a stale post-migration connection mis-decoded the users table,
        // ELECTRON-3T0), deriving a fresh key would silently break decryption
        // of every stored credential. Verify absence with an independent
        // query and fail startup instead of corrupting.
        if is_new
            && system_user.is_none()
            && user_repo
                .find_by_id("system_default_user")
                .await
                .map_err(|e| anyhow::anyhow!("Failed to verify system user absence: {e}"))?
                .is_some()
        {
            anyhow::bail!(
                "system user row exists but could not be read; refusing to generate a new                  JWT secret (would break decryption of stored credentials)"
            );
        }

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
        // Settle any slow version probes off the readiness path (#675):
        // hydrate never waits beyond the inline budget per agent.
        agent_registry.spawn_slow_probe_recheck();

        let acp_session_repo: Arc<dyn IAcpSessionRepository> =
            Arc::new(SqliteAcpSessionRepository::new(database.pool().clone()));
        let acp_agent_service = AcpSessionSyncService::new(acp_session_repo.clone());

        let conversation_repo: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let skill_repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(database.pool().clone()));
        let share_repo: Arc<dyn IResourceShareRepository> =
            Arc::new(SqliteResourceShareRepository::new(database.pool().clone()));

        // Project-bind temp_root mirrors the conversation service's effective
        // workspace root. Browser sessions keep this under data_dir, never
        // under the optional operator-mounted bootstrap workspace.
        let project_store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(database.pool().clone()));
        let project_service = if identity_mode.is_local() {
            ProjectService::new(project_store, work_dir.join("conversations")).with_share_repo(share_repo.clone())
        } else {
            ProjectService::new_user_session(
                project_store,
                conversation_workspace_root.join("conversations"),
                upload_root,
                bootstrap_workspace,
            )
            .with_share_repo(share_repo.clone())
        };

        // Skill paths need app resource dir (for builtin rules) + data dir
        // (for user skills + materialized views). AcpSkillManager uses these
        // for first-message skill index/body loading.
        let app_resource_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let skill_paths = Arc::new(aionui_extension::resolve_skill_paths(&app_resource_dir, &data_dir));
        if identity_mode.is_local() {
            aionui_extension::sync_skill_catalog_into_repo(skill_paths.as_ref(), skill_repo.as_ref())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to synchronize skill catalog: {e}"))?;
        } else {
            // AionPro: never ingest the legacy shared skill directory — its
            // files carry no account attribution and would only create rows
            // for the never-logged-in local default user.
            aionui_extension::sync_builtin_skill_catalog_into_repo(skill_paths.as_ref(), skill_repo.as_ref())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to synchronize skill catalog: {e}"))?;
        }

        // Absolute path to this process's binary. Reused as the `command` for
        // the stdio MCP bridge spawned by ACP CLIs when a team session is
        // attached to a conversation (phase1 mcp.md §4.6 single-binary model).
        let backend_binary_path =
            Arc::new(std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("aioncore")));
        let runtime_helper_bin = backend_binary_path.to_string_lossy().into_owned();
        let runtime_base_url = config.local_base_url();
        let antigravity_hook_tokens = Arc::new(aionui_ai_agent::antigravity_hook::HookTokenRegistry::new());

        // Session-model port: the subprocess spawner the clean-slate claude/codex
        // SessionBackend uses. Registry-backed (feature 001) so spawned processes are
        // reap-gateable; a fresh per-run epoch (no cross-run reap authority is required
        // for the port's spawn path). claude/codex always run through the direct-CLI
        // SessionAgentTask now — the spawner is unconditionally wired.
        let process_registry = Arc::new(aionui_process::FileRegistryStore::new(&data_dir));
        let machine_id = aionui_process::local_machine_id(&data_dir);
        let session_spawner: Arc<dyn aionui_process::Spawner> = Arc::new(aionui_process::RealSpawner::new(
            process_registry,
            uuid::Uuid::now_v7(),
            machine_id,
        ));

        let factory = build_agent_factory(AgentFactoryDeps {
            skill_manager: AcpSkillManager::new_with_repo(skill_paths.clone(), skill_repo.clone()),
            provider_repo,
            user_repo: user_repo.clone(),
            encryption_key,
            agent_registry: agent_registry.clone(),
            acp_agent_service: acp_agent_service.clone(),
            data_dir: data_dir.clone(),
            dump_prompts,
            broadcaster: event_bus.clone(),
            backend_binary_path: backend_binary_path.clone(),
            mcp_server_repo: Some(mcp_server_repo),
            restrict_member_host_tools: !identity_mode.is_local(),
            session_spawner,
            // agy cannot prompt for tool permission in headless mode, so AionUi
            // registers itself as its PreToolUse hook; the hook process calls
            // back here to raise the user's permission card.
            antigravity_hook_base_url: Some(runtime_base_url.clone()),
            antigravity_hook_tokens: antigravity_hook_tokens.clone(),
        });

        // Agent factory is now wired. Future extension/custom agents
        // that get written to `agent_metadata` will show up after the
        // relevant service calls `AgentRegistry::hydrate`.
        let active_lease_registry = Arc::new(ActiveLeaseRegistry::new());
        let runtime_token_service = Arc::new(RuntimeTokenService::new());
        let task_manager_concrete = Arc::new(
            WorkerTaskManagerImpl::new_with_active_leases(factory, active_lease_registry.clone())
                .with_runtime_token_service(runtime_token_service.clone()),
        );
        let worker_task_manager: Arc<dyn IWorkerTaskManager> = task_manager_concrete.clone();
        let task_manager_delete_hook: Arc<dyn OnConversationDelete> = task_manager_concrete;
        let conversation_runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let conversation_service = build_conversation_service(ConversationServiceDeps {
            database: &database,
            work_dir: conversation_workspace_root,
            event_bus: event_bus.clone(),
            skill_paths: skill_paths.clone(),
            skill_repo: skill_repo.clone(),
            worker_task_manager: worker_task_manager.clone(),
            conversation_runtime_state: conversation_runtime_state.clone(),
            conversation_repo: conversation_repo.clone(),
            task_manager_delete_hook: Some(task_manager_delete_hook.clone()),
            runtime_helper_bin: runtime_helper_bin.clone(),
            runtime_base_url: runtime_base_url.clone(),
            runtime_token_service: runtime_token_service.clone(),
            project_service: project_service.clone(),
        });

        Ok(Self {
            database,
            jwt_service: Arc::new(JwtService::new(secret.clone())),
            antigravity_hook_tokens,
            user_repo,
            admin_user_repo,
            share_repo,
            initial_admin_credentials_file,
            cookie_config: Arc::new(CookieConfig::from_env()),
            qr_token_store: Arc::new(QrTokenStore::new()),
            ws_manager: Arc::new(WebSocketManager::new()),
            event_bus,
            worker_task_manager,
            active_lease_registry,
            runtime_token_service,
            conversation_runtime_state,
            conversation_service,
            project_service,
            task_manager_delete_hook: Some(task_manager_delete_hook),
            agent_registry,
            conversation_repo,
            acp_session_sync: acp_agent_service,
            jwt_secret_raw: secret,
            data_dir,
            dump_prompts,
            work_dir,
            local,
            identity_mode,
            bootstrap_secret: config.bootstrap_secret.clone().map(Arc::<str>::from),
            local_client_secret: config.local_client_secret.clone().map(Arc::<str>::from),
            allowed_origins: Arc::from(config.allowed_origins.clone()),
            app_version,
            skill_paths,
            skill_repo,
            runtime_helper_bin,
            runtime_base_url,
        })
    }
}

async fn bootstrap_initial_webui_admin(
    user_repo: &dyn IUserRepository,
    admin_repo: &dyn IAdminUserRepository,
    data_dir: &std::path::Path,
) -> anyhow::Result<Option<PathBuf>> {
    let credentials_path = std::env::var_os("AIONUI_INITIAL_ADMIN_CREDENTIALS_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("initial-admin-credentials.json"));
    if user_repo
        .has_usable_admin()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to inspect initial administrator: {error}"))?
    {
        if credentials_path.exists() {
            if initial_admin_credentials_match_pending_user(user_repo, &credentials_path).await? {
                return Ok(Some(credentials_path));
            }
            let _ = std::fs::remove_file(&credentials_path);
        }
        return Ok(None);
    }

    let mut username = std::env::var("AIONUI_INITIAL_ADMIN_USERNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "admin".to_string());
    validate_username(&username).map_err(|error| anyhow::anyhow!("Invalid initial administrator username: {error}"))?;

    let direct_password = std::env::var("AIONUI_INITIAL_ADMIN_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    let password_file = std::env::var_os("AIONUI_INITIAL_ADMIN_PASSWORD_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if direct_password.is_some() && password_file.is_some() {
        anyhow::bail!("AIONUI_INITIAL_ADMIN_PASSWORD and AIONUI_INITIAL_ADMIN_PASSWORD_FILE are mutually exclusive");
    }

    let generated = direct_password.is_none() && password_file.is_none();
    let mut created_credentials_file = false;
    let password = if let Some(password) = direct_password {
        password
    } else if let Some(path) = password_file {
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("Failed to read initial administrator password file: {error}"))?;
        contents
            .lines()
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Initial administrator password file is empty"))?
            .to_owned()
    } else if credentials_path.exists() {
        let resumed = read_initial_admin_credentials(&credentials_path)?;
        username = resumed.0;
        resumed.1
    } else {
        let password = generate_password(20);
        write_initial_admin_credentials(&credentials_path, &username, &password)?;
        created_credentials_file = true;
        password
    };
    validate_password(&password).map_err(|error| anyhow::anyhow!("Invalid initial administrator password: {error}"))?;
    let password_for_hash = password.clone();
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password_for_hash))
        .await
        .map_err(|error| anyhow::anyhow!("Initial administrator hash task failed: {error}"))??;

    let result = admin_repo
        .bootstrap_initial_admin(&username, &password_hash)
        .await
        .map_err(|error| anyhow::anyhow!("Failed to bootstrap initial administrator: {error}"));
    match result {
        Ok(Some(user)) => {
            if generated {
                tracing::warn!(
                    username = %user.username.as_deref().unwrap_or(&username),
                    credentials_file = %credentials_path.display(),
                    "Initial administrator created; retrieve the one-time password from the protected credentials file"
                );
            } else {
                tracing::info!(
                    username = %user.username.as_deref().unwrap_or(&username),
                    "Initial administrator created from operator-supplied credentials"
                );
            }
            Ok(generated.then_some(credentials_path))
        }
        Ok(None) => {
            if generated && credentials_path.exists() {
                if initial_admin_credentials_match_pending_user(user_repo, &credentials_path).await? {
                    return Ok(Some(credentials_path));
                }
                let _ = std::fs::remove_file(&credentials_path);
            }
            if created_credentials_file {
                let _ = std::fs::remove_file(&credentials_path);
            }
            Ok(None)
        }
        // Preserve a newly created write-ahead credential on database errors;
        // the next startup validates and reuses it instead of locking the
        // operator out after a crash/transient failure.
        Err(error) => Err(error),
    }
}

async fn initial_admin_credentials_match_pending_user(
    user_repo: &dyn IUserRepository,
    path: &std::path::Path,
) -> anyhow::Result<bool> {
    let (credential_username, credential_password) = read_initial_admin_credentials(path)?;
    let Some(user) = user_repo.find_by_username(&credential_username).await? else {
        return Ok(false);
    };
    if user.site_role != aionui_db::SiteRole::Admin || !user.must_change_password {
        return Ok(false);
    }
    let Some(password_hash) = user.password_hash else {
        return Ok(false);
    };
    tokio::task::spawn_blocking(move || verify_password(&credential_password, &password_hash))
        .await
        .map_err(|error| anyhow::anyhow!("Initial administrator verification task failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("Initial administrator credential verification failed: {error}"))
}

fn read_initial_admin_credentials(path: &std::path::Path) -> anyhow::Result<(String, String)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("Initial administrator credentials path must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("Initial administrator credentials file must not be accessible by group or others");
        }
    }
    let value: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let username = value
        .get("username")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Initial administrator credentials file has no username"))?
        .to_owned();
    let password = value
        .get("temporary_password")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Initial administrator credentials file has no temporary password"))?
        .to_owned();
    if value.get("must_change_password").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!("Initial administrator credentials file is not a temporary credential");
    }
    validate_username(&username)?;
    validate_password(&password)?;
    Ok((username, password))
}

fn write_initial_admin_credentials(path: &std::path::Path, username: &str, password: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        anyhow::anyhow!(
            "Refusing to overwrite initial administrator credentials file '{}': {error}",
            path.display()
        )
    })?;
    let document = serde_json::json!({
        "username": username,
        "temporary_password": password,
        "created_at": aionui_common::now_ms(),
        "must_change_password": true,
    });
    serde_json::to_writer(&mut file, &document)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn conversation_workspace_root(
    identity_mode: IdentityMode,
    data_dir: &std::path::Path,
    work_dir: &std::path::Path,
) -> PathBuf {
    if identity_mode.is_local() {
        work_dir.to_path_buf()
    } else {
        data_dir.join("user-workspaces")
    }
}

fn prepare_user_session_root(
    root: &std::path::Path,
    bootstrap_workspace: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(root)
        .map_err(|error| anyhow::anyhow!("failed to initialize managed user filesystem root: {error}"))?;
    let canonical = std::fs::canonicalize(root)
        .map_err(|error| anyhow::anyhow!("managed user filesystem root is not accessible: {error}"))?;
    if let Some(bootstrap) = bootstrap_workspace {
        let bootstrap = std::fs::canonicalize(bootstrap)
            .map_err(|error| anyhow::anyhow!("AIONUI_BOOTSTRAP_WORKSPACE is not accessible: {error}"))?;
        if canonical.starts_with(&bootstrap) || bootstrap.starts_with(&canonical) {
            anyhow::bail!("managed user filesystem roots and AIONUI_BOOTSTRAP_WORKSPACE must be disjoint");
        }
    }
    Ok(canonical)
}

fn resolve_bootstrap_workspace(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("AIONUI_BOOTSTRAP_WORKSPACE must be an absolute path");
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("AIONUI_BOOTSTRAP_WORKSPACE is not accessible: {error}"))?;
    if !canonical.is_dir() {
        anyhow::bail!("AIONUI_BOOTSTRAP_WORKSPACE must reference a directory");
    }
    Ok(canonical)
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
    runtime_token_service: Arc<RuntimeTokenService>,
    project_service: ProjectService,
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
    .with_runtime_helper_context(deps.runtime_helper_bin, deps.runtime_base_url)
    .with_runtime_token_service(deps.runtime_token_service);
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
    service.with_project_service(Arc::new(deps.project_service));
    service
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_config() -> AppConfig {
        AppConfig {
            local: true,
            identity_mode: IdentityMode::Local,
            local_client_secret: Some("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_app_services_from_memory_db() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &local_config()).await.unwrap();

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
        let services = AppServices::from_config(db, &local_config()).await.unwrap();

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
            ..local_config()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        assert_eq!(services.app_version, "9.9.9");

        services.database.close().await;
    }

    #[test]
    fn initial_admin_credentials_are_private_resumable_and_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("initial-admin.json");
        write_initial_admin_credentials(&path, "admin", "StrongP@ssword1").unwrap();

        let resumed = read_initial_admin_credentials(&path).unwrap();
        assert_eq!(resumed.0, "admin");
        assert_eq!(resumed.1, "StrongP@ssword1");
        assert!(write_initial_admin_credentials(&path, "other", "OtherP@ssword2").is_err());
        assert_eq!(read_initial_admin_credentials(&path).unwrap(), resumed);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn initial_admin_credentials_reject_loose_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("initial-admin.json");
        write_initial_admin_credentials(&path, "admin", "StrongP@ssword1").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_initial_admin_credentials(&path).is_err());

        let link = directory.path().join("credentials-link.json");
        symlink(&path, &link).unwrap();
        assert!(read_initial_admin_credentials(&link).is_err());
    }

    #[tokio::test]
    async fn webui_bootstrap_resumes_write_ahead_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let credentials = directory.path().join("initial-admin-credentials.json");
        write_initial_admin_credentials(&credentials, "admin", "CrashSafeP@ss1").unwrap();
        let database = aionui_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            data_dir: directory.path().to_path_buf(),
            work_dir: directory.path().to_path_buf(),
            bootstrap_initial_admin: true,
            ..Default::default()
        };

        let services = AppServices::from_config(database, &config).await.unwrap();
        let admin = services.user_repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(admin.site_role, aionui_db::SiteRole::Admin);
        assert!(admin.must_change_password);
        assert!(aionui_auth::verify_password("CrashSafeP@ss1", admin.password_hash.as_deref().unwrap()).unwrap());
        assert_eq!(
            services.initial_admin_credentials_file.as_deref().map(AsRef::as_ref),
            Some(credentials.as_path())
        );

        // A restart before the initial password change must retain the path
        // so the successful self-change can consume the credential file.
        let resumed_path = bootstrap_initial_webui_admin(
            services.user_repo.as_ref(),
            services.admin_user_repo.as_ref(),
            directory.path(),
        )
        .await
        .unwrap();
        assert_eq!(resumed_path.as_deref(), Some(credentials.as_path()));
        services.database.close().await;
    }

    #[tokio::test]
    async fn webui_bootstrap_discards_stale_credentials_after_password_reset() {
        let directory = tempfile::tempdir().unwrap();
        let credentials = directory.path().join("initial-admin-credentials.json");
        write_initial_admin_credentials(&credentials, "admin", "StaleP@ssword1").unwrap();
        let database = aionui_db::init_database_memory().await.unwrap();
        let repository = SqliteUserRepository::new(database.pool().clone());
        let current_hash = hash_password("CurrentP@ssword2").unwrap();
        repository
            .bootstrap_initial_admin("admin", &current_hash)
            .await
            .unwrap();

        let result = bootstrap_initial_webui_admin(&repository, &repository, directory.path())
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(!credentials.exists());
        database.close().await;
    }

    #[test]
    fn managed_user_roots_and_bootstrap_workspace_must_be_disjoint() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("managed");
        let bootstrap_inside_managed = managed.join("bootstrap");
        std::fs::create_dir_all(&bootstrap_inside_managed).unwrap();
        assert!(prepare_user_session_root(&managed, Some(&bootstrap_inside_managed)).is_err());

        let bootstrap = directory.path().join("operator-bootstrap");
        let managed_inside_bootstrap = bootstrap.join("managed");
        std::fs::create_dir_all(&managed_inside_bootstrap).unwrap();
        assert!(prepare_user_session_root(&managed_inside_bootstrap, Some(&bootstrap)).is_err());
    }
}
