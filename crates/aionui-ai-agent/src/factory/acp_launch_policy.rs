use crate::cc_switch;
use crate::manager::acp::mode_normalize::normalize_requested_mode;
use crate::shared_kernel::PersistedSessionState;
use aionui_api_types::{AcpBuildExtra, AgentMetadata};
use aionui_common::CommandSpec;

const CODEX_CONFIG_FLAG: &str = "-c";
const CODEX_ENV_POLICY_INHERIT_ALL: &str = "shell_environment_policy.inherit=all";
const CODEX_ENV_POLICY_CLEAR_INCLUDE_ONLY: &str = "shell_environment_policy.include_only=[]";
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

    push_codex_config_arg(command_spec, CODEX_ENV_POLICY_INHERIT_ALL);
    push_codex_config_arg(command_spec, CODEX_ENV_POLICY_CLEAR_INCLUDE_ONLY);

    let sandbox_mode = codex_sandbox_mode_for_requested_mode(initial_mode);
    push_codex_config_arg(command_spec, &format!("sandbox_mode=\"{sandbox_mode}\""));
    if sandbox_mode == "danger-full-access" {
        push_codex_config_arg(command_spec, CODEX_WINDOWS_UNELEVATED_SANDBOX);
    }
}

fn push_codex_config_arg(command_spec: &mut CommandSpec, value: &str) {
    command_spec.args.push(CODEX_CONFIG_FLAG.to_owned());
    command_spec.args.push(value.to_owned());
}

fn codex_sandbox_mode_for_requested_mode(mode: Option<&str>) -> &'static str {
    match mode.map(str::trim) {
        Some("full-access" | "yoloNoSandbox") => "danger-full-access",
        _ => "workspace-write",
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
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: Some("full-access".into()),
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

        assert_eq!(mode.as_deref(), Some("full-access"));
    }
}
