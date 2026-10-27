use crate::cc_switch;
use crate::manager::acp::mode_normalize::normalize_requested_mode;
use crate::shared_kernel::PersistedSessionState;
use aionui_api_types::{AcpBuildExtra, AgentMetadata};
use aionui_common::CommandSpec;

const CODEX_WINDOWS_UNELEVATED_SANDBOX: &str = "windows.sandbox=\"unelevated\"";

pub(super) struct AcpLaunchPolicyInput<'a> {
    pub metadata: &'a AgentMetadata,
    pub config: &'a AcpBuildExtra,
    pub session_snapshot: Option<&'a PersistedSessionState>,
    pub runtime_env: &'a [(String, String)],
}

pub(super) fn apply_acp_launch_policy(command_spec: &mut CommandSpec, input: AcpLaunchPolicyInput<'_>) {
    apply_codex_runtime_config_args(
        command_spec,
        input.metadata,
        initial_mode_from_build_context(input.metadata, input.config, input.session_snapshot).as_deref(),
    );
    append_runtime_env(command_spec, input.runtime_env);
    append_claude_provider_env(command_spec, input.metadata);
}

fn append_runtime_env(command_spec: &mut CommandSpec, runtime_env: &[(String, String)]) {
    for (name, value) in runtime_env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
}

fn append_claude_provider_env(command_spec: &mut CommandSpec, metadata: &AgentMetadata) {
    if metadata.backend.as_deref() != Some("claude") {
        return;
    }

    let cc_switch_env = cc_switch::read_claude_provider_env();
    if cc_switch_env.is_empty() {
        return;
    }

    let keys: Vec<&str> = cc_switch_env.keys().map(|key| key.as_str()).collect();
    for (name, value) in &cc_switch_env {
        command_spec.env.push(aionui_common::EnvVar {
            name: name.clone(),
            value: value.clone(),
        });
    }
    tracing::info!(?keys, "cc-switch: env vars injected");
}

fn initial_mode_from_build_context(
    metadata: &AgentMetadata,
    config: &AcpBuildExtra,
    session_snapshot: Option<&PersistedSessionState>,
) -> Option<String> {
    session_snapshot
        .and_then(|snapshot| snapshot.current_mode_id.as_ref())
        .map(|mode| normalize_requested_mode(metadata, mode.as_str()))
        .or_else(|| {
            config
                .session_mode
                .as_ref()
                .map(|mode| normalize_requested_mode(metadata, mode))
        })
        .filter(|mode| !mode.is_empty())
}

fn apply_codex_runtime_config_args(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    initial_mode: Option<&str>,
) {
    if metadata.backend.as_deref() != Some("codex") {
        return;
    }

    command_spec
        .args
        .extend(aionui_session::codex_shell_environment_policy_args().map(str::to_owned));

    let sandbox_mode = codex_sandbox_mode_for_requested_mode(initial_mode);
    push_codex_config_arg(command_spec, &format!("sandbox_mode=\"{sandbox_mode}\""));
    if sandbox_mode == "danger-full-access" {
        push_codex_config_arg(command_spec, CODEX_WINDOWS_UNELEVATED_SANDBOX);
    }
}

fn push_codex_config_arg(command_spec: &mut CommandSpec, value: &str) {
    command_spec.args.push("-c".to_owned());
    command_spec.args.push(value.to_owned());
}

fn codex_sandbox_mode_for_requested_mode(mode: Option<&str>) -> &'static str {
    match mode.map(str::trim) {
        Some("agent-full-access" | "full-access" | "yoloNoSandbox") => "danger-full-access",
        _ => "workspace-write",
    }
}

/// True when argv already pins the opencode ACP server port, either as
/// `--port N` or `--port=N` (vendor-configured or user override). We must not
/// append a second `--port`: last-one-wins would silently defeat the
/// operator's explicit choice.
pub(crate) fn command_spec_has_port(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--port" || arg.starts_with("--port="))
}

/// Ask the OS for a currently free TCP port on loopback. The listener is
/// dropped immediately; the window between drop and opencode's own bind is
/// tiny and a collision degrades to the pre-fix behaviour (single conversation
/// on a shared machine), so we accept the race rather than hold the socket
/// across the spawn.
pub(crate) fn reserve_available_tcp_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?;
    Some(listener.local_addr().ok()?.port())
}

/// True when this launch targets opencode: either the catalog backend says so
/// (conversation runtime path) or the executable is literally `opencode` /
/// `opencode.exe` (custom-agent probe path, where no backend label exists yet).
/// The name fallback must stay narrow — other ACP CLIs (claude, codex, gemini,
/// …) would die on an unknown `--port` flag.
pub(crate) fn is_opencode_acp_launch(backend: Option<&str>, command: &str) -> bool {
    if backend == Some("opencode") {
        return true;
    }
    // `command` is either a bare program ("opencode"), a program path (which
    // may itself contain spaces on Windows, e.g. "C:\Program Files\…"), or a
    // program with embedded arguments ("npx -y pkg"-style). Test both the
    // whole-string basename (covers paths with spaces) and the first-token
    // basename (covers embedded args).
    let basename = |candidate: &str| {
        std::path::Path::new(candidate)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| candidate.to_owned())
    };
    let whole = basename(command);
    if whole.eq_ignore_ascii_case("opencode") || whole.eq_ignore_ascii_case("opencode.exe") {
        return true;
    }
    match command.split_whitespace().next() {
        Some(first) => {
            let head = basename(first);
            head.eq_ignore_ascii_case("opencode") || head.eq_ignore_ascii_case("opencode.exe")
        }
        None => false,
    }
}

/// Append a dedicated `--port <free>` to an opencode ACP launch argv unless
/// one is already pinned.
///
/// Why: opencode's ACP adapter boots an internal HTTP server per process. Its
/// `--port` CLI *default* (0 = random) loses to a user-pinned `server.port` in
/// the global opencode config (`~/.config/opencode/opencode.jsonc`): yargs
/// defaults do not override config values. A pinned port serialises spawns —
/// the second concurrent conversation fails to bind, dies with `ServeError`
/// before the ACP `initialize` handshake completes, and the UI surfaces
/// `USER_AGENT_STARTUP_FAILED`. Passing an *explicit* `--port` does win over
/// the config, so every spawn gets its own free port. Covers BOTH spawn paths:
/// the conversation factory and the custom-agent validation probe.
///
/// Returns the assigned port when one was appended.
pub(crate) fn pin_opencode_port(args: &mut Vec<String>, backend: Option<&str>, command: &str) -> Option<u16> {
    if !is_opencode_acp_launch(backend, command) || command_spec_has_port(args) {
        return None;
    }
    match reserve_available_tcp_port() {
        Some(port) => {
            args.extend(["--port".to_string(), port.to_string()]);
            tracing::info!(
                port,
                "opencode: assigned dedicated ACP server port to avoid config-pinned collisions"
            );
            Some(port)
        }
        None => {
            tracing::warn!(
                "opencode: could not reserve a free port; falling back to opencode's default port selection"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_spec_has_port_detects_both_spellings() {
        assert!(command_spec_has_port(&["acp".into(), "--port".into(), "4711".into()]));
        assert!(command_spec_has_port(&["acp".into(), "--port=4711".into()]));
        assert!(!command_spec_has_port(&["acp".into()]));
        // `--portmap` style lookalikes must not count as a pinned port.
        assert!(!command_spec_has_port(&["acp".into(), "--portmap".into()]));
    }

    #[test]
    fn reserve_available_tcp_port_returns_a_bindable_port() {
        let port = reserve_available_tcp_port().expect("loopback ephemeral port must be available");
        assert_ne!(port, 0);
        // Port must still be bindable right after reservation (nothing else
        // grabbed it), i.e. opencode's later bind would succeed.
        let probe = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(probe.is_ok(), "reserved port {port} was taken immediately");
    }

    #[test]
    fn opencode_detection_accepts_backend_and_executable_name() {
        assert!(is_opencode_acp_launch(Some("opencode"), "whatever"));
        assert!(is_opencode_acp_launch(None, "opencode"));
        assert!(is_opencode_acp_launch(None, "opencode.exe"));
        assert!(is_opencode_acp_launch(
            None,
            r"C:\Program Files\x\resources\opencode-cli\opencode.exe"
        ));
        // Whole string is the path (may contain spaces) AND program-with-args form.
        assert!(is_opencode_acp_launch(None, r"C:\tools\opencode.exe"));
        assert!(is_opencode_acp_launch(None, "opencode acp"));
        // Other ACP vendors must never receive an injected --port.
        assert!(!is_opencode_acp_launch(Some("claude"), "claude"));
        assert!(!is_opencode_acp_launch(None, "codex"));
        assert!(!is_opencode_acp_launch(None, "gemini"));
    }

    #[test]
    fn pin_opencode_port_appends_once_and_respects_explicit_choice() {
        let mut args = vec!["acp".to_string()];
        let port = pin_opencode_port(&mut args, Some("opencode"), "opencode").expect("port assigned");
        assert_eq!(args, vec!["acp".to_string(), "--port".to_string(), port.to_string()]);
        // Second call must be a no-op (already pinned now).
        assert!(pin_opencode_port(&mut args, Some("opencode"), "opencode").is_none());
        assert_eq!(args.len(), 3);
        // User-pinned port is never overridden, and non-opencode is untouched.
        let mut pinned = vec!["acp".to_string(), "--port".to_string(), "9999".to_string()];
        assert!(pin_opencode_port(&mut pinned, Some("opencode"), "opencode").is_none());
        assert_eq!(pinned.len(), 3);
        let mut claude = vec!["acp".to_string()];
        assert!(pin_opencode_port(&mut claude, Some("claude"), "claude").is_none());
        assert_eq!(claude, vec!["acp".to_string()]);
    }

    fn agent_metadata_with_backend(backend: Option<&str>) -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(str::to_owned),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            skill_delivery: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: Some("agent-full-access".into()),
            sort_order: 0,
            team_capable: false,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: aionui_api_types::AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[test]
    fn apply_acp_launch_policy_adds_runtime_env_and_codex_full_access_config() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = AcpBuildExtra {
            session_mode: Some("full-access".into()),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[("AIONUI_CONVERSATION_ID".into(), "conv-1".into())],
            },
        );

        assert_eq!(
            command_spec.args,
            vec![
                "codex-acp.js",
                "-c",
                "shell_environment_policy.inherit=all",
                "-c",
                "shell_environment_policy.include_only=[]",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "-c",
                "windows.sandbox=\"unelevated\"",
            ]
        );
        assert!(
            command_spec
                .env
                .iter()
                .any(|entry| entry.name == "AIONUI_CONVERSATION_ID" && entry.value == "conv-1")
        );
    }

    #[test]
    fn apply_acp_launch_policy_adds_codex_full_access_config_for_agent_full_access() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let config = AcpBuildExtra {
            session_mode: Some("agent-full-access".into()),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[],
            },
        );

        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "sandbox_mode=\"danger-full-access\"")
        );
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == CODEX_WINDOWS_UNELEVATED_SANDBOX)
        );
    }

    #[test]
    fn apply_acp_launch_policy_keeps_legacy_full_access_dangerous_for_persisted_snapshots() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["codex-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("codex"));
        let snapshot = PersistedSessionState {
            current_mode_id: Some(crate::shared_kernel::ModeId::new("full-access")),
            ..Default::default()
        };

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &AcpBuildExtra::default(),
                session_snapshot: Some(&snapshot),
                runtime_env: &[],
            },
        );

        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == "sandbox_mode=\"danger-full-access\"")
        );
        assert!(
            command_spec
                .args
                .iter()
                .any(|arg| arg == CODEX_WINDOWS_UNELEVATED_SANDBOX)
        );
    }

    #[test]
    fn apply_acp_launch_policy_skips_codex_config_for_non_codex_agents() {
        let mut command_spec = CommandSpec {
            command: "node".into(),
            args: vec!["claude-agent-acp.js".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("claude"));
        let config = AcpBuildExtra::default();

        apply_acp_launch_policy(
            &mut command_spec,
            AcpLaunchPolicyInput {
                metadata: &metadata,
                config: &config,
                session_snapshot: None,
                runtime_env: &[],
            },
        );

        assert_eq!(command_spec.args, vec!["claude-agent-acp.js"]);
    }

    #[test]
    fn initial_mode_from_build_context_prefers_persisted_snapshot() {
        let snapshot = PersistedSessionState {
            current_mode_id: Some(crate::shared_kernel::ModeId::new("full-access")),
            ..Default::default()
        };
        let config = AcpBuildExtra {
            session_mode: Some("auto".into()),
            ..Default::default()
        };

        let mode =
            initial_mode_from_build_context(&agent_metadata_with_backend(Some("codex")), &config, Some(&snapshot));

        assert_eq!(mode.as_deref(), Some("agent-full-access"));
    }
}
