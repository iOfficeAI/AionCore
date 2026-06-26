use std::sync::Arc;
use std::time::Instant;

use crate::registry::AgentRegistry;
use aionui_api_types::AgentMetadata;
use aionui_runtime::resolve_command_path;

pub(crate) struct CliHealthCheckResult {
    pub available: bool,
    pub error: Option<String>,
}

/// Perform a health check for an ACP backend.
///
/// Checks CLI availability and returns an availability/error pair.
pub(crate) async fn health_check(registry: &Arc<AgentRegistry>, backend: &str) -> CliHealthCheckResult {
    let start = Instant::now();

    let Some(meta) = registry.find_builtin_by_backend(backend).await else {
        return CliHealthCheckResult {
            available: false,
            error: Some(format!("No agent_metadata row for backend '{backend}'")),
        };
    };

    let path = probe_command(&meta);
    let _latency_ms = start.elapsed().as_millis() as u64;
    let available = path.is_some();

    CliHealthCheckResult {
        available,
        error: if available {
            None
        } else {
            Some(format!("Spawn command for backend '{backend}' not found in PATH"))
        },
    }
}

fn probe_command(meta: &AgentMetadata) -> Option<String> {
    if let Some(path) = meta.resolved_command.as_ref() {
        return Some(path.to_string_lossy().into_owned());
    }
    let cmd = meta.command.as_deref()?;
    resolve_command_path(cmd).map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{
        AgentHandshake, AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource, AgentSourceInfo, BehaviorPolicy,
    };
    use aionui_common::AgentType;
    use std::path::PathBuf;

    fn metadata_with_resolved_command() -> AgentMetadata {
        AgentMetadata {
            id: "agent-codex".into(),
            icon: None,
            name: "Codex CLI".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("codex".into()),
            agent_type: AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: AgentSourceInfo {
                binary_name: Some("codex".into()),
                ..Default::default()
            },
            enabled: true,
            available: true,
            command: None,
            resolved_command: Some(PathBuf::from("codex-acp")),
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            last_check_status: Some(AgentSnapshotCheckStatus::Online),
            last_check_kind: Some(AgentSnapshotCheckKind::Startup),
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[test]
    fn probe_command_uses_hydrated_resolved_command_when_spawn_command_is_empty() {
        let meta = metadata_with_resolved_command();
        assert_eq!(probe_command(&meta), Some("codex-acp".into()));
    }
}
