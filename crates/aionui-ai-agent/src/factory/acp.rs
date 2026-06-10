use std::sync::Arc;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use crate::factory::context::FactoryContext;
use crate::manager::acp::{AcpAgentManager, CatalogForwarder};
use crate::session_context::AcpSessionBuildContext;
use agent_client_protocol::schema::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio};
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_common::CommandSpec;
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use aionui_runtime::{
    ManagedAcpToolId, ensure_managed_acp_tool_with_reporter, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, resolve_command_path,
};
use tracing::{debug, info, warn};

use crate::runtime_status::{conversation_acp_tool_runtime_reporter, conversation_runtime_reporter};

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AcpSessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let belongs_to_team = build_context.team.is_some();
    let mut config = build_context.config;

    // Resolve the catalog row — prefer explicit agent_id, fall
    // back to a vendor-label match for legacy payloads.
    let meta = if let Some(ref agent_id) = config.agent_id {
        deps.agent_registry.get(agent_id).await
    } else if let Some(ref vendor) = config.backend {
        deps.agent_registry.find_builtin_by_backend(vendor).await
    } else {
        None
    }
    .ok_or_else(|| AgentError::bad_request("ACP agent requires either agent_id or backend in extra"))?;

    // Trust the catalog row over the client-supplied `backend` when an
    // `agent_id` was provided. The frontend collapses row-scoped rows
    // (custom ACP / remote) to a shared `custom`/`remote` slot string,
    // which downstream consumers (MCP injection, preset-context
    // composition) would mis-interpret. When the caller only supplied a
    // vendor label (builtin path), we preserve it as-is.
    if config.agent_id.is_some() || config.backend.is_none() {
        config.backend.clone_from(&meta.backend);
    }

    // Inject Guide MCP config for solo (non-team) sessions.
    // Team sessions already carry `team_mcp_stdio_config`; the
    // two are mutually exclusive per the build_new_session_request guard.
    if config.team_mcp_stdio_config.is_some() {
        debug!(ctx.conversation_id, "guide_mcp: skipped: has team_mcp");
    } else if belongs_to_team {
        debug!(
            ctx.conversation_id,
            "guide_mcp: skipped: conversation belongs to a team"
        );
    } else if config.guide_mcp_config.is_some() {
        debug!(
            ctx.conversation_id,
            "guide_mcp: skipped: caller already set guide_mcp_config"
        );
    } else if deps.guide_mcp_config.is_none() {
        debug!(ctx.conversation_id, "guide_mcp: skipped: guide server not running");
    } else {
        config.guide_mcp_config.clone_from(&deps.guide_mcp_config);
        info!(
            ctx.conversation_id,
            guide_mcp_port = deps.guide_mcp_config.as_ref().map(|c| c.port),
            "guide_mcp: injected into solo session"
        );
    }

    let mut command_spec =
        resolve_agent_command_spec(&meta, &ctx.workspace, &ctx.conversation_id, deps.broadcaster.clone()).await?;
    if meta.backend.as_deref() == Some("claude") {
        let cc_switch_env = crate::cc_switch::read_claude_provider_env();
        if !cc_switch_env.is_empty() {
            let keys: Vec<&str> = cc_switch_env.keys().map(|k| k.as_str()).collect();
            for (name, value) in &cc_switch_env {
                command_spec.env.push(aionui_common::EnvVar {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            tracing::info!(?keys, "cc-switch: env vars injected");
        }
    }
    let session_snapshot = build_context.session_snapshot;

    // Load user-configured MCP servers from the DB so they reach
    // ACP `session/new` mcpServers payload. Without this the agent
    // starts with zero MCP tools even when the user configured them
    // via Settings → MCP (ELECTRON-1JG).
    let mcp_capabilities = meta
        .handshake
        .agent_capabilities
        .as_ref()
        .map(parse_acp_mcp_capabilities)
        .unwrap_or_default();

    let user_mcp_servers = match deps.mcp_server_repo.as_ref() {
        Some(repo) => {
            load_user_mcp_servers(
                repo.as_ref(),
                config.mcp_server_ids.as_deref(),
                &ctx.conversation_id,
                &mcp_capabilities,
            )
            .await
        }
        None => Vec::new(),
    };
    let mut session_mcp_servers = user_mcp_servers;
    for server in &config.session_mcp_servers {
        if !session_server_supported_by_capabilities(server, &mcp_capabilities) {
            warn!(
                ctx.conversation_id,
                server_id = %server.id,
                server_name = %server.name,
                "session_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match session_server_to_sdk_mcp_server(server).await {
            Ok(server) => session_mcp_servers.push(server),
            Err(err) => {
                warn!(
                    ctx.conversation_id,
                    server_id = %server.id,
                    server_name = %server.name,
                    error = %err,
                    "session_mcp: failed to convert session snapshot; skipping"
                );
            }
        }
    }

    let params = Arc::new(
        assemble_acp_params(
            ctx.conversation_id.clone(),
            WorkspaceInfo {
                path: ctx.workspace,
                is_custom: ctx.is_custom_workspace,
            },
            meta,
            command_spec,
            config,
            session_mcp_servers,
            session_snapshot,
            deps.data_dir.clone(),
        )
        .await,
    );

    let skill_mgr = deps.skill_manager.clone();
    let catalog_tx = deps.agent_registry.catalog_sender();

    let (agent, domain_rx, notification_rx) = AcpAgentManager::build(params, skill_mgr, &catalog_tx).await?;

    let arc = Arc::new(agent);
    arc.start_permission_handler();
    arc.start_session_event_tracker(notification_rx);
    CatalogForwarder::spawn(
        arc.agent_id().to_owned(),
        crate::IAgentTask::subscribe(arc.as_ref()),
        catalog_tx,
    );

    // Desired (mode/model/config) are seeded from `params.session_snapshot`
    // inside `AcpAgentManager::new`. The CLI-assigned session id is still
    // loaded here so the first turn after a task rebuild takes the resume
    // path.
    if let Some(sid) = build_context.session_id {
        arc.set_session_id(sid).await;
    }

    // Open the ACP session eagerly so `POST /warmup` returns only after
    // session/new (or claude-meta-resume / session/load) and the first
    // reconcile pass have completed. Matches aionrs factory behaviour:
    // the caller sees "warmed up" == "ready for PUT /mode | /model".
    arc.warmup_session().await?;

    let instance = AgentInstance::Acp(Arc::clone(&arc));

    // Hand the service the domain event receiver so it can
    // persist user intent changes without reverse-engineering
    // them from CLI observations.
    deps.acp_agent_service.attach(ctx.conversation_id, domain_rx).await;

    Ok(instance)
}

async fn resolve_agent_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
) -> Result<CommandSpec, AgentError> {
    if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && let Some(backend) = meta.backend.as_deref()
        && let Some(tool) = ManagedAcpToolId::from_backend(backend)
    {
        return resolve_builtin_managed_acp_command_spec(meta, workspace, conversation_id, broadcaster, tool).await;
    }

    let command = meta
        .command
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::bad_request(format!("Agent '{}' has no spawn command configured", meta.name)))?;
    let reporter = conversation_runtime_reporter(broadcaster, conversation_id.to_owned());
    let resolved = ensure_runtime_command_with_reporter(command, Some(reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let mut args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.extend(meta.args.iter().cloned());

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

async fn resolve_builtin_managed_acp_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
    tool: ManagedAcpToolId,
) -> Result<CommandSpec, AgentError> {
    if let Some(primary) = meta.agent_source_info.binary_name.as_deref()
        && resolve_command_path(primary).is_none()
    {
        return Err(AgentError::bad_request(format!(
            "Agent '{}' requires `{primary}` to be installed and available on PATH",
            meta.name
        )));
    }

    let node_reporter = conversation_runtime_reporter(broadcaster.clone(), conversation_id.to_owned());
    let node_runtime = ensure_node_runtime_with_reporter(Some(node_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let tool_reporter = conversation_acp_tool_runtime_reporter(broadcaster, conversation_id.to_owned(), tool);
    let managed_tool = ensure_managed_acp_tool_with_reporter(tool, Some(tool_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let resolved = managed_tool.command(&node_runtime);

    let args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

/// Load the operator's enabled MCP servers from the DB, log+skip any rows
/// whose `transport_config` JSON fails to parse (better to start without one
/// MCP tool than fail the whole session), and return them in SDK shape ready
/// for `NewSessionRequest::mcp_servers`.
///
/// When `selected_ids` is present, those rows define the session snapshot and
/// are injected regardless of the current global `enabled` flag. Legacy
/// conversations without a snapshot still fall back to "all enabled rows".
/// Builtins are wired through other paths (e.g. team/guide MCP).
async fn load_user_mcp_servers(
    repo: &dyn IMcpServerRepository,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    capabilities: &AcpMcpCapabilities,
) -> Vec<McpServer> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(ids).await,
        None => repo.list().await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "user_mcp: list() failed; skipping injection"
            );
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        if !selected || row.builtin {
            continue;
        }
        if !row_supported_by_capabilities(&row, capabilities) {
            warn!(
                conversation_id,
                server_id = %row.id,
                server_name = %row.name,
                transport_type = %row.transport_type,
                "user_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match row_to_sdk_mcp_server(&row).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "user_mcp: injected into session/new"
        );
    }
    servers
}

/// Convert an `McpServerRow` into the SDK `McpServer` shape used by
/// `NewSessionRequest::mcp_servers`. Returns an error string when
/// `transport_config` is malformed or required fields are missing.
async fn row_to_sdk_mcp_server(row: &McpServerRow) -> Result<McpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env_entries: Vec<(String, String)> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            env_entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (resolved_command, args, env) = ensure_stdio_launch(command, &args, &env_entries).await?;

            let stdio = McpServerStdio::new(row.name.clone(), resolved_command)
                .args(args)
                .env(env);
            Ok(McpServer::Stdio(stdio))
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Http(
                McpServerHttp::new(row.name.clone(), url).headers(headers),
            ))
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Sse(
                McpServerSse::new(row.name.clone(), url).headers(headers),
            ))
        }
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn parse_headers(value: Option<&serde_json::Value>) -> Vec<HttpHeader> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect()
}

async fn session_server_to_sdk_mcp_server(server: &SessionMcpServer) -> Result<McpServer, String> {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            if command.is_empty() {
                return Err("stdio: missing command".to_owned());
            }
            let mut entries: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (command, args, env) = ensure_stdio_launch(command, args, &entries).await?;
            Ok(McpServer::Stdio(
                McpServerStdio::new(server.name.clone(), command).args(args).env(env),
            ))
        }
        SessionMcpTransport::Http { url, headers } | SessionMcpTransport::StreamableHttp { url, headers } => {
            if url.is_empty() {
                return Err("http: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Http(
                McpServerHttp::new(server.name.clone(), url).headers(headers),
            ))
        }
        SessionMcpTransport::Sse { url, headers } => {
            if url.is_empty() {
                return Err("sse: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Sse(
                McpServerSse::new(server.name.clone(), url).headers(headers),
            ))
        }
    }
}

async fn ensure_stdio_launch(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<(std::path::PathBuf, Vec<String>, Vec<EnvVariable>), String> {
    let resolved = ensure_runtime_command(command)
        .await
        .map_err(|error| error.to_string())?;

    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVariable> = env
        .iter()
        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| {
        EnvVariable::new(
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }));

    Ok((resolved.program, final_args, final_env))
}

fn row_supported_by_capabilities(row: &McpServerRow, capabilities: &AcpMcpCapabilities) -> bool {
    match row.transport_type.as_str() {
        "stdio" => capabilities.stdio,
        "http" | "streamable_http" => capabilities.http,
        "sse" => capabilities.sse,
        _ => false,
    }
}

fn session_server_supported_by_capabilities(server: &SessionMcpServer, capabilities: &AcpMcpCapabilities) -> bool {
    match server.transport {
        SessionMcpTransport::Stdio { .. } => capabilities.stdio,
        SessionMcpTransport::Http { .. } | SessionMcpTransport::StreamableHttp { .. } => capabilities.http,
        SessionMcpTransport::Sse { .. } => capabilities.sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use aionui_runtime::init as init_runtime;
    use std::sync::OnceLock;
    use std::{mem, path::PathBuf};

    fn make_row(
        name: &str,
        transport_type: &str,
        transport_config: &str,
        enabled: bool,
        builtin: bool,
    ) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn stdio_config_for_existing_command() -> String {
        let command = std::env::current_exe()
            .expect("current test executable")
            .to_string_lossy()
            .into_owned();
        serde_json::json!({
            "command": command,
            "args": [],
            "env": {},
        })
        .to_string()
    }

    fn path_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    fn test_runtime_data_dir() -> &'static PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().to_path_buf();
            mem::forget(temp);
            init_runtime(&path);
            path
        })
    }

    #[cfg(unix)]
    fn install_fake_bundled_runtime() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime_root = tmp.path().join("node").join("node-v24.11.0-darwin-arm64");
        let bin = runtime_root.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");

        for tool in ["node", "npm", "npx"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\necho v24.11.0\n").expect("write tool");
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }

        tmp
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn row_to_sdk_stdio_flattens_resolved_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", runtime.path()) };

        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );

        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
        match server {
            McpServer::Stdio(s) => {
                let command = s.command.to_string_lossy();
                assert_ne!(command, "npx");
                assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_agent_command_spec_flattens_bare_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", runtime.path()) };

        let meta = aionui_api_types::AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("custom".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Custom,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("npx".into()),
            resolved_command: None,
            args: vec!["-y".into(), "@scope/test-agent".into()],
            env: vec![aionui_api_types::AgentEnvEntry {
                name: "K".into(),
                value: "V".into(),
                description: None,
            }],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            handshake: aionui_api_types::AgentHandshake::default(),
        };

        let spec = resolve_agent_command_spec(
            &meta,
            "/tmp/workspace",
            "conv-acp",
            Arc::new(BroadcastEventBus::new(16)),
        )
        .await
        .expect("resolved command spec");

        unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
        let command = spec.command.to_string_lossy();
        assert_ne!(command, "npx");
        assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
        assert_eq!(spec.args, vec!["-y".to_owned(), "@scope/test-agent".to_owned()]);
        assert!(spec.env.iter().any(|entry| entry.name == "K" && entry.value == "V"));
        assert_eq!(spec.cwd.as_deref(), Some("/tmp/workspace"));
    }

    #[tokio::test]
    async fn row_to_sdk_stdio_roundtrip() {
        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        match server {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "ctx7");
                let command = s.command.to_string_lossy();
                assert!(
                    command == "npx" || command.ends_with("/npx"),
                    "unexpected stdio command path: {command}",
                );
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
                assert!(
                    s.env.iter().any(|entry| entry.name == "K" && entry.value == "V"),
                    "missing user-provided env in stdio launch"
                );
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_http_with_headers() {
        let row = make_row(
            "remote",
            "http",
            r#"{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer tok"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).await.expect("convert");
        match server {
            McpServer::Http(h) => {
                assert_eq!(h.name, "remote");
                assert_eq!(h.url, "https://example.com/mcp");
                assert_eq!(h.headers.len(), 1);
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_unknown_transport_type_errors() {
        let row = make_row("bad", "websocket", "{}", true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    #[tokio::test]
    async fn row_to_sdk_invalid_json_errors() {
        let row = make_row("bad", "stdio", "not-json", true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    #[tokio::test]
    async fn row_to_sdk_stdio_missing_command_errors() {
        let row = make_row("bad", "stdio", r#"{"args":[]}"#, true, false);
        assert!(row_to_sdk_mcp_server(&row).await.is_err());
    }

    // -- load_user_mcp_servers integration -----------------------------------

    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                Err(aionui_db::DbError::Init("simulated".into()))
            } else {
                Ok(self.rows.clone())
            }
        }
        async fn find_by_id(&self, _id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn find_by_name(&self, _name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn list_by_ids_any(&self, ids: &[String]) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("simulated".into()));
            }
            Ok(ids
                .iter()
                .filter_map(|id| self.rows.iter().find(|row| row.id == *id).cloned())
                .collect())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn batch_upsert(
            &self,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_tools(&self, _id: &str, _tools: Option<&str>) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_disabled_and_builtin() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("user-enabled", "stdio", &stdio_config, true, false),
                make_row("user-disabled", "stdio", &stdio_config, false, false),
                make_row(
                    "builtin",
                    "stdio",
                    r#"{"command":"img-gen","args":[],"env":{}}"#,
                    true,
                    true,
                ),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "user-enabled"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_returns_empty_on_repo_failure() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![],
            fail: true,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_malformed_rows_but_keeps_others() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("good", "stdio", &stdio_config, true, false),
                make_row("bad", "stdio", "not-json", true, false),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "good"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_uses_selected_snapshot_over_enabled_state() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("enabled", "stdio", &stdio_config, true, false),
                make_row("disabled-picked", "stdio", &stdio_config, false, false),
            ],
            fail: false,
        });

        let selected = vec!["mcp_disabled-picked".to_owned()];
        let servers = load_user_mcp_servers(repo.as_ref(), Some(&selected), "conv-1", &caps).await;

        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "disabled-picked"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_rows_unsupported_by_capabilities() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: false,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row(
                "stdio-only",
                "stdio",
                r#"{"command":"npx","args":[],"env":{}}"#,
                true,
                false,
            )],
            fail: false,
        });

        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }
}
