use aionui_common::{CommandSpec, ErrorChain};
use aionui_runtime::Builder as CmdBuilder;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, broadcast, watch};
use tracing::{debug, error, info, warn};

use crate::error::AgentError;

use super::{
    CliAgentProcess, EVENT_CHANNEL_CAPACITY, STDERR_BUFFER_MAX, prepare_command_cwd, tracked_process_group_id,
};

impl CliAgentProcess {
    /// Spawn a new CLI subprocess in **SDK mode**.
    ///
    /// Unlike [`spawn`](Self::spawn), this does NOT start a stdout reader task.
    /// Instead, the raw stdin/stdout handles are available via [`take_stdio`](Self::take_stdio)
    /// for the ACP SDK transport to own.
    ///
    /// `data_dir` is the backend's `AppConfig.data_dir` — used as the root
    /// for child-process bun cache / tmp directories so they honour the
    /// operator's `--data-dir` choice instead of falling back to the OS
    /// local data dir.
    ///
    /// Background tasks are still spawned for:
    /// - stderr buffering
    /// - Process exit monitoring
    pub async fn spawn_for_sdk(config: CommandSpec, data_dir: &Path) -> Result<Self, AgentError> {
        let mut cmd = CmdBuilder::new(&config.command);
        cmd.args(&config.args)
            .envs(config.env.iter().map(|e| (&e.name, &e.value)))
            .envs(Self::agent_spawn_env(data_dir))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(prepare_command_cwd(cwd)?);
        }
        let preview = cmd.to_string();
        info!(command = %preview, "Spawning CLI process (SDK mode)");
        let mut child: Child = cmd.spawn().map_err(|e| {
            error!(command = %preview, error = %ErrorChain(&e), "Failed to spawn CLI process");
            AgentError::internal(format!("Failed to spawn CLI process '{preview}': {e}"))
        })?;

        let pid = child.id().ok_or_else(|| {
            error!(command = %preview, "Failed to obtain PID from spawned process");
            AgentError::internal("Failed to obtain PID from spawned process")
        })?;
        info!(pid, command = %preview, "CLI process spawned (SDK mode)");

        let stdout = child.stdout.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdout from child process");
            AgentError::internal("Failed to capture stdout from child process")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            error!(pid, "Failed to capture stderr from child process");
            AgentError::internal("Failed to capture stderr from child process")
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdin for child process");
            AgentError::internal("Failed to capture stdin for child process")
        })?;

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = watch::channel(None);

        // Background task: read stderr → ring buffer + log
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buffer);
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(pid, stderr = trimmed, "CLI process stderr");
                }
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > STDERR_BUFFER_MAX {
                    let cut = buf.len() - STDERR_BUFFER_MAX;
                    buf.drain(..cut);
                }
            }

            debug!(pid, "Stderr reader finished");
        });

        // Background task: monitor process exit
        let exit_handle = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    info!(pid, ?status, "CLI process exited");
                    let _ = exit_tx.send(Some(status));
                }
                Err(e) => {
                    error!(pid, error = %ErrorChain(&e), "Failed to wait on CLI process");
                    let _ = exit_tx.send(None);
                }
            }
        });

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(stdout)),
            pid,
            process_group_id: tracked_process_group_id(pid),
            event_tx,
            exit_rx,
            initial_rx: std::sync::Mutex::new(None),
            stderr_buffer,
            _stdout_handle: None,
            _stderr_handle: Arc::new(stderr_handle),
            _exit_handle: Arc::new(exit_handle),
        })
    }

    /// Build environment variables for agent subprocess spawn.
    /// Mirrors the frontend `acpConnectors.ts::getCleanAgentEnv` logic:
    /// - Set BUN_INSTALL_CACHE_DIR / BUN_TMPDIR to stable paths under
    ///   the backend's `AppConfig.data_dir`
    ///
    /// Claude SDK resolves its packaged native binary by default. Callers may
    /// still provide `CLAUDE_CODE_EXECUTABLE` explicitly via `CommandSpec.env`.
    fn agent_spawn_env(data_dir: &Path) -> Vec<(String, String)> {
        let bun_cache = data_dir.join("bun-cache");
        let bun_tmp = data_dir.join("bun-tmp");

        vec![
            ("BUN_INSTALL_CACHE_DIR".into(), bun_cache.to_string_lossy().into_owned()),
            ("BUN_TMPDIR".into(), bun_tmp.to_string_lossy().into_owned()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::simple_script_config;
    use super::*;
    use std::time::Duration;

    // ── SDK mode tests ───────────────────────────────────────────────

    #[test]
    fn agent_spawn_env_does_not_override_claude_code_executable_from_path() {
        const CHILD_ENV: &str = "AIONUI_TEST_AGENT_SPAWN_ENV_CHILD";

        if let Some(data_dir) = std::env::var_os(CHILD_ENV) {
            let env = CliAgentProcess::agent_spawn_env(Path::new(&data_dir));

            assert!(
                !env.iter().any(|(name, _)| name == "CLAUDE_CODE_EXECUTABLE"),
                "managed claude-agent-acp should use its packaged Claude SDK binary by default"
            );
            assert!(
                env.iter()
                    .any(|(name, value)| name == "BUN_INSTALL_CACHE_DIR" && value.contains("bun-cache")),
                "non-Claude SDK spawn env entries should still be present"
            );
            assert!(
                env.iter()
                    .any(|(name, value)| name == "BUN_TMPDIR" && value.contains("bun-tmp")),
                "non-Claude SDK spawn env entries should still be present"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let fake_claude = temp.path().join(if cfg!(windows) { "claude.cmd" } else { "claude" });
        std::fs::write(
            &fake_claude,
            if cfg!(windows) { "@echo off\r\n" } else { "#!/bin/sh\n" },
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("capability::cli_process::spawn_sdk::tests::agent_spawn_env_does_not_override_claude_code_executable_from_path")
            .arg("--nocapture")
            .env(CHILD_ENV, temp.path())
            .env("PATH", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn spawn_for_sdk_take_stdio() {
        let config = simple_script_config("read line && echo \"$line\"");
        let tmp = std::env::temp_dir();
        let proc = CliAgentProcess::spawn_for_sdk(config, &tmp).await.unwrap();

        let stdio = proc.take_stdio().await;
        assert!(stdio.is_some(), "First take_stdio should succeed");

        let stdio_again = proc.take_stdio().await;
        assert!(stdio_again.is_none(), "Second take_stdio should return None");

        proc.kill(Duration::from_millis(100)).await.unwrap();
    }
}
