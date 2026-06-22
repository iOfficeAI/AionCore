use crate::shared_kernel::PersistedSessionState;
use agent_client_protocol::schema::{EnvVariable, McpServer, McpServerStdio, NewSessionRequest};
use aionui_api_types::AgentMetadata;
use aionui_api_types::{AcpBuildExtra, GuideMcpConfig, TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
use aionui_common::CommandSpec;
use aionui_team_prompts::guide as team_guide_prompt;
use std::path::PathBuf;

use aionui_common::constants::TEAM_CAPABLE_BACKENDS;

/// Pre-computed workspace information.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub path: String,
    pub is_custom: bool,
}

/// All pre-computed parameters needed to create and drive an ACP session.
///
/// Assembled once by `assemble_acp_params` in the factory layer; the
/// `AcpAgentManager` reads from this but never mutates it. By front-loading
/// the decision logic (which MCP servers to inject, what preset context to
/// compose) we keep the manager focused on execution + state.
#[derive(Debug, Clone)]
pub struct AcpSessionParams {
    pub conversation_id: String,
    pub workspace: WorkspaceInfo,
    pub metadata: AgentMetadata,
    pub command_spec: CommandSpec,
    pub config: AcpBuildExtra,
    pub mcp_servers: Vec<McpServer>,
    pub preset_context: Option<String>,
    pub session_snapshot: Option<PersistedSessionState>,
    /// Backend data directory (`AppConfig.data_dir`). Passed through to
    /// `CliAgentProcess::spawn_for_sdk` so bun cache / tmp directories
    /// land under the operator-chosen path rather than the OS default.
    pub data_dir: PathBuf,
}

impl AcpSessionParams {
    /// Build a `NewSessionRequest` using the pre-computed MCP servers.
    pub fn new_session_request(&self) -> NewSessionRequest {
        let req = NewSessionRequest::new(&self.workspace.path);
        if self.mcp_servers.is_empty() {
            req
        } else {
            req.mcp_servers(self.mcp_servers.clone())
        }
    }
}

/// Assemble fully-resolved ACP session params from factory inputs.
///
/// This front-loads all decision logic that was previously scattered across
/// `build_new_session_request`, `compose_preset_context_with_team_guide`,
/// and the factory's ACP match arm.
///
/// `user_mcp_servers` are operator-configured MCP servers loaded from the DB
/// by the factory layer; they are appended after the team/guide injection so
/// the agent gets *all* the user's tools on `session/new` (ELECTRON-1JG fix).
#[allow(clippy::too_many_arguments)]
pub async fn assemble_acp_params(
    conversation_id: String,
    workspace: WorkspaceInfo,
    metadata: AgentMetadata,
    command_spec: CommandSpec,
    config: AcpBuildExtra,
    user_mcp_servers: Vec<McpServer>,
    session_snapshot: Option<PersistedSessionState>,
    data_dir: PathBuf,
) -> AcpSessionParams {
    let mcp_servers = resolve_mcp_servers(&config, &conversation_id, user_mcp_servers);
    let preset_context = compose_preset_context(
        config.preset_context.as_deref(),
        config.backend.as_deref(),
        config.team_mcp_stdio_config.is_some(),
    );

    AcpSessionParams {
        conversation_id,
        workspace,
        metadata,
        command_spec,
        config,
        mcp_servers,
        preset_context,
        session_snapshot,
        data_dir,
    }
}

/// Determine which MCP servers to inject into `session/new`.
///
/// Layout: `[team-or-guide?, ...user_mcp_servers]`. The team/guide
/// injection is mutually exclusive (team takes priority); the user's
/// own enabled MCP servers are always appended on top so a team
/// session still gets the operator's tools.
fn resolve_mcp_servers(
    config: &AcpBuildExtra,
    conversation_id: &str,
    user_mcp_servers: Vec<McpServer>,
) -> Vec<McpServer> {
    let mut servers: Vec<McpServer> = Vec::new();
    if let Some(cfg) = config.team_mcp_stdio_config.as_ref() {
        servers.push(team_mcp_server(cfg));
    } else if let Some(guide_cfg) = config.guide_mcp_config.as_ref()
        && config
            .backend
            .as_deref()
            .is_some_and(|b| TEAM_CAPABLE_BACKENDS.contains(&b))
    {
        servers.push(guide_mcp_server(guide_cfg, config, conversation_id));
    }
    servers.extend(user_mcp_servers);
    servers
}

/// Compose first-message preset context, optionally appending the Team Guide
/// prompt for solo sessions on team-capable backends.
fn compose_preset_context(
    base_preset_context: Option<&str>,
    backend: Option<&str>,
    has_team_session: bool,
) -> Option<String> {
    let base = base_preset_context
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if has_team_session {
        return base;
    }
    let backend_key = backend.unwrap_or_default();
    if !team_guide_prompt::is_solo_team_guide_backend(backend_key) {
        return base;
    }

    let guide = team_guide_prompt::build_solo_team_guide_prompt(backend_key);
    match base {
        Some(ctx) => Some(format!("{ctx}\n\n{guide}")),
        None => Some(guide),
    }
}

fn team_mcp_server(cfg: &TeamMcpStdioConfig) -> McpServer {
    let env = vec![
        EnvVariable::new(TeamMcpStdioConfig::ENV_PORT.to_owned(), cfg.port.to_string()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_TOKEN.to_owned(), cfg.token.clone()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_SLOT_ID.to_owned(), cfg.slot_id.clone()),
    ];
    let stdio = McpServerStdio::new(TEAM_MCP_SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-team-stdio".to_owned()])
        .env(env);
    McpServer::Stdio(stdio)
}

fn guide_mcp_server(cfg: &GuideMcpConfig, extra: &AcpBuildExtra, conversation_id: &str) -> McpServer {
    let env = vec![
        EnvVariable::new("AION_MCP_PORT".to_owned(), cfg.port.to_string()),
        EnvVariable::new("AION_MCP_TOKEN".to_owned(), cfg.token.clone()),
        EnvVariable::new("AION_MCP_BACKEND".to_owned(), extra.backend.clone().unwrap_or_default()),
        EnvVariable::new("AION_MCP_CONVERSATION_ID".to_owned(), conversation_id.to_owned()),
        EnvVariable::new("AION_MCP_USER_ID".to_owned(), extra.user_id.clone().unwrap_or_default()),
    ];
    let stdio = McpServerStdio::new("aionui-team-guide", &cfg.binary_path)
        .args(vec!["mcp-guide-stdio".to_owned()])
        .env(env);
    McpServer::Stdio(stdio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_preset_context_no_team_no_backend() {
        let result = compose_preset_context(Some("hello"), None, false);
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_team_session_skips_guide() {
        let result = compose_preset_context(Some("hello"), Some("claude"), true);
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_non_team_capable_backend() {
        let result = compose_preset_context(Some("hello"), Some("unknown"), false);
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_team_capable_backend_appends_guide() {
        let result = compose_preset_context(None, Some("claude"), false);
        assert!(result.is_some());
        let prompt = result.unwrap();
        assert!(prompt.contains("aion_create_team"));
        assert!(prompt.contains("aion_list_models"));
        assert!(prompt.contains("hand off to the created Team conversation"));
        assert!(!prompt.contains("Immediately"));
        assert!(!prompt.contains(
            "use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team"
        ));
    }

    #[test]
    fn compose_preset_context_empty_string_treated_as_none() {
        let result = compose_preset_context(Some("  "), Some("unknown"), false);
        assert_eq!(result, None);
    }

    fn user_stdio(name: &str) -> McpServer {
        McpServer::Stdio(McpServerStdio::new(name, "/bin/sh"))
    }

    fn team_cfg() -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            team_id: "team-1".into(),
            port: 9999,
            token: "tok".into(),
            slot_id: "slot-lead".into(),
            binary_path: "/bin/backend".into(),
        }
    }

    fn test_metadata() -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("claude".into()),
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: true,
            handshake: aionui_api_types::AgentHandshake::default(),
        }
    }

    #[tokio::test]
    async fn assemble_acp_params_uses_frozen_preset_context_and_snapshot_seeds() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            preset_context: Some("frozen rules".into()),
            skills: vec!["pdf".into()],
            mcp_server_ids: Some(vec!["mcp-docs".into()]),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };

        let params = assemble_acp_params(
            "conv-1".into(),
            WorkspaceInfo {
                path: "/tmp/workspace".into(),
                is_custom: false,
            },
            test_metadata(),
            CommandSpec::default(),
            config,
            vec![user_stdio("mcp-docs")],
            None,
            PathBuf::from("/tmp/data"),
        )
        .await;

        assert_eq!(params.preset_context.as_deref(), Some("frozen rules"));
        assert_eq!(params.config.skills, vec!["pdf"]);
        assert_eq!(
            params.config.mcp_server_ids.as_deref(),
            Some(&["mcp-docs".to_owned()][..])
        );
        assert_eq!(params.mcp_servers.len(), 2);
    }

    #[test]
    fn resolve_mcp_servers_prefers_team_over_guide() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: Some(TeamMcpStdioConfig {
                team_id: "team-1".into(),
                port: 9999,
                token: "tok".into(),
                slot_id: "slot-lead".into(),
                binary_path: "/bin/backend".into(),
            }),
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8888,
                token: "guide-tok".into(),
                binary_path: "/bin/backend".into(),
            }),
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let servers = resolve_mcp_servers(&config, "conv-1", Vec::new());
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, TEAM_MCP_SERVER_NAME),
            _ => panic!("expected stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_guide_for_team_capable_solo() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: None,
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8888,
                token: "guide-tok".into(),
                binary_path: "/bin/backend".into(),
            }),
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let servers = resolve_mcp_servers(&config, "conv-1", Vec::new());
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "aionui-team-guide"),
            _ => panic!("expected stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_non_team_capable_backend_gets_none() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("unknown-backend".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: None,
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8888,
                token: "guide-tok".into(),
                binary_path: "/bin/backend".into(),
            }),
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let servers = resolve_mcp_servers(&config, "conv-1", Vec::new());
        assert!(servers.is_empty());
    }

    /// Core ELECTRON-1JG regression contract: when the operator has
    /// configured user MCP servers (e.g. via Settings → MCP), they must
    /// reach the `session/new` payload — even when there's no team or
    /// guide injection. Pre-fix: this returned an empty Vec because the
    /// factory only knew about team_mcp_stdio_config / guide_mcp_config.
    #[test]
    fn resolve_mcp_servers_appends_user_servers_in_solo_session() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("unknown-backend".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: None,
            guide_mcp_config: None,
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let user = vec![user_stdio("ctx7"), user_stdio("playwright")];
        let servers = resolve_mcp_servers(&config, "conv-1", user);
        assert_eq!(servers.len(), 2);
        let names: Vec<_> = servers
            .iter()
            .map(|s| match s {
                McpServer::Stdio(s) => s.name.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(names, vec!["ctx7", "playwright"]);
    }

    /// User-configured MCP servers must coexist with the team injection,
    /// not replace it. Order: team first, then user servers.
    #[test]
    fn resolve_mcp_servers_team_plus_user_servers() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: Some(TeamMcpStdioConfig {
                team_id: "team-1".into(),
                port: 9999,
                token: "tok".into(),
                slot_id: "slot-lead".into(),
                binary_path: "/bin/backend".into(),
            }),
            guide_mcp_config: None,
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let user = vec![user_stdio("ctx7")];
        let servers = resolve_mcp_servers(&config, "conv-1", user);
        assert_eq!(servers.len(), 2);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, TEAM_MCP_SERVER_NAME, "team must come first"),
            _ => panic!("expected stdio"),
        }
        match &servers[1] {
            McpServer::Stdio(s) => assert_eq!(s.name, "ctx7"),
            _ => panic!("expected stdio"),
        }
    }

    /// Guide injection coexists with user MCP servers too.
    #[test]
    fn resolve_mcp_servers_guide_plus_user_servers() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: None,
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8888,
                token: "guide-tok".into(),
                binary_path: "/bin/backend".into(),
            }),
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let user = vec![user_stdio("ctx7")];
        let servers = resolve_mcp_servers(&config, "conv-1", user);
        assert_eq!(servers.len(), 2);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "aionui-team-guide"),
            _ => panic!("expected stdio"),
        }
        match &servers[1] {
            McpServer::Stdio(s) => assert_eq!(s.name, "ctx7"),
            _ => panic!("expected stdio"),
        }
    }

    /// The pre-fix bug: with no team/guide configured and an empty
    /// user-server list, the payload is empty. This is the *no-fix*
    /// scenario and remains valid (no MCP configured anywhere).
    #[test]
    fn resolve_mcp_servers_empty_when_nothing_configured() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_assistant_id: None,
            session_mode: None,
            current_model_id: None,
            thought_level: None,
            cron_job_id: None,
            team_mcp_stdio_config: None,
            guide_mcp_config: None,
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
        };
        let servers = resolve_mcp_servers(&config, "conv-1", Vec::new());
        assert!(servers.is_empty());
    }
}
