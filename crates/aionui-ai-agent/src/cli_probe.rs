use std::time::Duration;

use aionui_api_types::AgentMetadata;
use aionui_runtime::{Builder, resolve_command_path};
#[cfg(test)]
use std::path::PathBuf;

const CLI_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

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

pub(crate) async fn validate(meta: &AgentMetadata) -> Result<(), String> {
    let command = command_name(meta).ok_or_else(|| "agent has no CLI command to probe".to_owned())?;
    let path = resolve_command_path(command).ok_or_else(|| format!("`{command}` not found on PATH"))?;
    if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && meta.agent_source_info.bridge_binary.as_deref() == Some("npx")
        && let Some(backend) = meta.backend.as_deref()
        && aionui_runtime::should_skip_registry_npx_version_probe(backend).map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    validate_version(&path).await
}

#[cfg(test)]
async fn resolve_and_validate_command(command: &str) -> Result<PathBuf, String> {
    let path = resolve_command_path(command).ok_or_else(|| format!("`{command}` not found on PATH"))?;
    validate_version(&path).await?;
    Ok(path)
}

async fn validate_version(path: &std::path::Path) -> Result<(), String> {
    validate_version_with_timeout(path, CLI_VERSION_TIMEOUT).await
}

async fn validate_version_with_timeout(path: &std::path::Path, timeout: Duration) -> Result<(), String> {
    let mut command = Builder::clean_cli(path);
    command.arg("--version");

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| {
            format!(
                "`{} --version` timed out after {}ms",
                path.display(),
                timeout.as_millis()
            )
        })?
        .map_err(|error| format!("failed to run `{} --version`: {error}", path.display()))?;

    if output.status.success() {
        return Ok(());
    }

    let detail = first_nonempty_line(&output.stderr)
        .or_else(|| first_nonempty_line(&output.stdout))
        .unwrap_or_else(|| format!("exited with status {}", output.status));
    Err(format!("`{} --version` failed: {detail}", path.display()))
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
    async fn version_probe_rejects_broken_cli_wrapper() {
        let (_dir, path) = executable_script(
            "agent-cli",
            "#!/bin/sh\nprintf 'native binary missing\\n' >&2\nexit 1\n",
        );
        let error = resolve_and_validate_command(path.to_str().unwrap()).await.unwrap_err();
        assert!(error.contains("native binary missing"), "{error}");
    }

    #[tokio::test]
    async fn version_probe_times_out_hung_cli() {
        let (_dir, path) = executable_script("agent-cli", "#!/bin/sh\nsleep 10\n");
        let error = validate_version_with_timeout(&path, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(error.contains("timed out after 50ms"), "{error}");
    }
}
