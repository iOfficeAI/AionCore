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
    apply_dsh_launch_policy(command_spec, input.metadata);
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

const DSH_DEFAULT_PATCH_CONTENT: &str = r#"# DeepSeek Harness (DSH) OpenAI-compatible Protocol Patch
- id: llm-deepseek
  config:
    protocol: chat-completions
"#;

fn resolve_dsh_patch_path() -> std::path::PathBuf {
    if let Ok(env_path) = std::env::var("AIONUI_DSH_PATCH") {
        let p = std::path::PathBuf::from(env_path.trim());
        if p.exists() {
            return p;
        }
    }

    let candidate_rel_paths = [
        "resources/dsh/dsh.patch.yml",
        "../resources/dsh/dsh.patch.yml",
        "../../resources/dsh/dsh.patch.yml",
    ];
    for rel in candidate_rel_paths {
        let candidate = std::path::PathBuf::from(rel);
        if candidate.exists() {
            if let Ok(abs) = candidate.canonicalize() {
                return abs;
            }
            return candidate;
        }
    }

    let fallback_dir = std::env::temp_dir().join("aionui").join("dsh");
    let _ = std::fs::create_dir_all(&fallback_dir);
    let fallback_file = fallback_dir.join("dsh.patch.yml");
    let _ = std::fs::write(&fallback_file, DSH_DEFAULT_PATCH_CONTENT);
    fallback_file
}

fn apply_dsh_launch_policy(command_spec: &mut CommandSpec, metadata: &AgentMetadata) {
    if metadata.backend.as_deref() != Some("dsh") {
        return;
    }

    if !command_spec.args.iter().any(|arg| arg == "--patch") {
        let patch_file = resolve_dsh_patch_path();
        command_spec.args.push("--patch".to_string());
        command_spec.args.push(patch_file.to_string_lossy().to_string());
        tracing::info!(patch = %patch_file.display(), "DSH: chat-completions patch injected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn apply_acp_launch_policy_injects_dsh_patch() {
        let mut command_spec = CommandSpec {
            command: "dsh".into(),
            args: vec!["--profile".into(), "acp".into()],
            env: vec![],
            cwd: None,
        };
        let metadata = agent_metadata_with_backend(Some("dsh"));
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

        assert!(command_spec.args.contains(&"--patch".to_string()));
    }
}
