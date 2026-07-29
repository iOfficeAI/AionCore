use std::time::Duration;

use aionui_api_types::AgentMetadata;
use aionui_runtime::{Builder, resolve_command_path};
#[cfg(test)]
use std::path::PathBuf;

/// Inline (startup) `--version` budget. Bounds backend readiness; agents that
/// exceed it are NOT condemned — they go to the background recheck instead.
pub(crate) const CLI_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Background recheck `--version` budget for probes that exceeded the inline
/// budget. Sized to the session-handshake tolerance, not the startup path.
pub(crate) const CLI_VERSION_RECHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Successful probe evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeSuccess {
    /// Wall-clock cost of the `--version` run; 0 when the version step was skipped.
    pub(crate) duration_ms: u64,
    /// Whether `--version` actually ran (false for `skip_version_probe` agents).
    pub(crate) version_checked: bool,
}

/// Classified probe failure. The split matters: a corrupted install fails
/// `--version` fast and deterministically (exit != 0), while a timeout is
/// evidence of a healthy-but-large CLI on slow I/O — the two must not be
/// collapsed into one verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeFailure {
    NoCommand,
    CommandNotFound { command: String },
    SkipLookup { detail: String },
    VersionFailed { detail: String, duration_ms: u64 },
    VersionTimeout { budget_ms: u64 },
}

impl ProbeFailure {
    pub(crate) fn error_code(&self) -> &'static str {
        match self {
            Self::NoCommand | Self::CommandNotFound { .. } => "command_not_found",
            Self::SkipLookup { .. } => "package_lock_invalid",
            Self::VersionFailed { .. } => "version_probe_failed",
            Self::VersionTimeout { .. } => "version_probe_timeout",
        }
    }

    /// Human-readable detail carrying the classification and measured cost,
    /// suitable for `last_check_error_message`.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::NoCommand => "agent has no CLI command to probe".to_owned(),
            Self::CommandNotFound { command } => format!("`{command}` not found on PATH"),
            Self::SkipLookup { detail } => detail.clone(),
            Self::VersionFailed { detail, duration_ms } => {
                format!("version_probe_failed ({duration_ms}ms): {detail}")
            }
            Self::VersionTimeout { budget_ms } => {
                format!(
                    "version_probe_timeout@{budget_ms}ms: `--version` timed out (slow load, not proof of corruption)"
                )
            }
        }
    }
}

pub(crate) fn command_name(meta: &AgentMetadata) -> Option<&str> {
    if meta.has_command_override
        && let Some(command) = meta.command.as_deref().filter(|command| !command.is_empty())
    {
        return Some(command);
    }

    meta.agent_source_info
        .binary_name
        .as_deref()
        .or(meta.command.as_deref())
}

pub(crate) async fn validate_with_budget(meta: &AgentMetadata, budget: Duration) -> Result<ProbeSuccess, ProbeFailure> {
    let command = command_name(meta).ok_or(ProbeFailure::NoCommand)?;
    let path = resolve_command_path(command).ok_or_else(|| ProbeFailure::CommandNotFound {
        command: command.to_owned(),
    })?;
    if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && meta.agent_source_info.bridge_binary.as_deref() == Some("npx")
        && let Some(backend) = meta.backend.as_deref()
        && aionui_runtime::should_skip_registry_npx_version_probe(backend).map_err(|error| {
            ProbeFailure::SkipLookup {
                detail: error.to_string(),
            }
        })?
    {
        return Ok(ProbeSuccess {
            duration_ms: 0,
            version_checked: false,
        });
    }
    validate_version_with_timeout(&path, budget).await
}

#[cfg(test)]
async fn resolve_and_validate_command(command: &str) -> Result<PathBuf, ProbeFailure> {
    let path = resolve_command_path(command).ok_or_else(|| ProbeFailure::CommandNotFound {
        command: command.to_owned(),
    })?;
    validate_version_with_timeout(&path, CLI_VERSION_TIMEOUT).await?;
    Ok(path)
}

async fn validate_version_with_timeout(path: &std::path::Path, budget: Duration) -> Result<ProbeSuccess, ProbeFailure> {
    let mut command = Builder::clean_cli(path);
    command.arg("--version");

    let started = std::time::Instant::now();
    let output = tokio::time::timeout(budget, command.output())
        .await
        .map_err(|_| ProbeFailure::VersionTimeout {
            budget_ms: budget.as_millis() as u64,
        })?
        .map_err(|error| ProbeFailure::VersionFailed {
            detail: format!("failed to run `{} --version`: {error}", path.display()),
            duration_ms: started.elapsed().as_millis() as u64,
        })?;
    let duration_ms = started.elapsed().as_millis() as u64;

    if output.status.success() {
        return Ok(ProbeSuccess {
            duration_ms,
            version_checked: true,
        });
    }

    let detail = first_nonempty_line(&output.stderr)
        .or_else(|| first_nonempty_line(&output.stdout))
        .unwrap_or_else(|| format!("exited with status {}", output.status));
    Err(ProbeFailure::VersionFailed {
        detail: format!("`{} --version` failed: {detail}", path.display()),
        duration_ms,
    })
}

fn first_nonempty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(500).collect())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn executable_script(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn version_probe_accepts_runnable_cli() {
        let (_dir, path) = executable_script("agent-cli", "#!/bin/sh\nprintf 'agent-cli 1.2.3\\n'\n");
        assert_eq!(
            resolve_and_validate_command(path.to_str().unwrap()).await.unwrap(),
            path
        );
    }

    #[tokio::test]
    async fn version_probe_success_reports_duration() {
        let (_dir, path) = executable_script("agent-cli", "#!/bin/sh\nprintf 'agent-cli 1.2.3\\n'\n");
        let ok = validate_version_with_timeout(&path, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(ok.version_checked);
        assert!(
            ok.duration_ms < 5_000,
            "duration should be measured, got {}",
            ok.duration_ms
        );
    }

    #[tokio::test]
    async fn version_probe_classifies_broken_install() {
        let (_dir, path) = executable_script(
            "agent-cli",
            "#!/bin/sh\nprintf 'native binary missing\\n' >&2\nexit 1\n",
        );
        let failure = validate_version_with_timeout(&path, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(failure.error_code(), "version_probe_failed");
        match &failure {
            ProbeFailure::VersionFailed { detail, .. } => {
                assert!(detail.contains("native binary missing"), "{detail}");
            }
            other => panic!("expected VersionFailed, got {other:?}"),
        }
        assert!(
            failure.detail().contains("native binary missing"),
            "{}",
            failure.detail()
        );
    }

    #[tokio::test]
    async fn version_probe_classifies_timeout_without_condemning() {
        let (_dir, path) = executable_script("agent-cli", "#!/bin/sh\nsleep 10\n");
        let failure = validate_version_with_timeout(&path, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(failure, ProbeFailure::VersionTimeout { budget_ms: 50 });
        assert_eq!(failure.error_code(), "version_probe_timeout");
        assert!(failure.detail().contains("timed out"), "{}", failure.detail());
    }
}
