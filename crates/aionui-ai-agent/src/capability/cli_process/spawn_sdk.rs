use aionui_common::{CommandSpec, ErrorChain};
use aionui_runtime::Builder as CmdBuilder;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};

use crate::error::AgentError;

use super::{CliAgentProcess, STDERR_BUFFER_MAX, prepare_command_cwd, tracked_process_group_id};

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
        let agent_env = aionui_runtime::agent_process_env().await;
        cmd.args(&config.args)
            .env_clear()
            .envs(agent_env)
            .envs(Self::agent_spawn_env(data_dir))
            .envs(config.env.iter().map(|e| (&e.name, &e.value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(prepare_command_cwd(cwd)?);
        }
        let preview = Self::sdk_spawn_preview(&config);
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
                super::trim_to_tail(&mut buf, STDERR_BUFFER_MAX);
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
            exit_rx,
            stderr_buffer,
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

    fn sdk_spawn_preview(config: &CommandSpec) -> String {
        let explicit_env_key_names: Vec<&str> = config.env.iter().map(|entry| entry.name.as_str()).collect();
        format!(
            "program={} args={} explicit_env_keys={} explicit_env_key_names={:?} cwd={}",
            config.command.display(),
            config.args.len(),
            config.env.len(),
            explicit_env_key_names,
            config.cwd.as_deref().unwrap_or("<inherit>")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::simple_script_config;
    use super::*;
    use aionui_common::EnvVar;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_for_sdk_uses_clean_agent_env_and_explicit_overrides() {
        const CHILD_ENV: &str = "AIONUI_TEST_SDK_AGENT_ENV_CHILD";

        if std::env::var_os(CHILD_ENV).is_none() {
            let temp = tempfile::tempdir().unwrap();
            let shell = temp.path().join("fake-shell");
            write_fake_shell(
                &shell,
                r#"#!/bin/sh
printf '%s\n' \
  'AIONUI_SHELL_ONLY=from-shell' \
  'AIONUI_OVERLAY=from-shell' \
  'PATH=/shell/bin:/bin:/usr/bin' \
  'NODE_OPTIONS=--inspect' \
  'npm_lifecycle_event=start'
"#,
            );

            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "capability::cli_process::spawn_sdk::tests::spawn_for_sdk_uses_clean_agent_env_and_explicit_overrides",
                )
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .env("SHELL", &shell)
                .env("PATH", "/bin:/usr/bin")
                .env("NODE_OPTIONS", "--require parent")
                .env("npm_config_cache", "/tmp/parent-cache")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let data_dir = tempfile::tempdir().unwrap();
        let mut config = simple_script_config(
            "printf 'shell=%s\nconfig=%s\noverlay=%s\nnpm=%s\nnode=%s\nbun=%s\n' \
             \"${AIONUI_SHELL_ONLY:-unset}\" \
             \"${AIONUI_CONFIG_ONLY:-unset}\" \
             \"${AIONUI_OVERLAY:-unset}\" \
             \"${npm_lifecycle_event:-unset}\" \
             \"${NODE_OPTIONS:-unset}\" \
             \"${BUN_INSTALL_CACHE_DIR:-unset}\"",
        );
        config.env.push(EnvVar {
            name: "AIONUI_CONFIG_ONLY".into(),
            value: "from-config".into(),
        });
        config.env.push(EnvVar {
            name: "AIONUI_OVERLAY".into(),
            value: "from-config".into(),
        });

        let proc = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await.unwrap();
        let (_stdin, mut stdout) = proc.take_stdio().await.unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();

        assert!(output.contains("shell=from-shell"), "{output}");
        assert!(output.contains("config=from-config"), "{output}");
        assert!(output.contains("overlay=from-config"), "{output}");
        assert!(output.contains("npm=unset"), "{output}");
        assert!(output.contains("node=unset"), "{output}");
        assert!(output.contains("bun="), "{output}");
        assert!(output.contains("bun-cache"), "{output}");
    }

    #[cfg(unix)]
    fn write_fake_shell(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn sdk_spawn_preview_omits_env_values_and_arg_bodies() {
        let config = CommandSpec {
            command: "node".into(),
            args: vec!["--api-key=secret-arg-value".into()],
            env: vec![
                EnvVar {
                    name: "SECRET_TOKEN".into(),
                    value: "secret-env-value".into(),
                },
                EnvVar {
                    name: "PATH".into(),
                    value: "/secret/path".into(),
                },
            ],
            cwd: Some("/workspace".into()),
        };

        let preview = CliAgentProcess::sdk_spawn_preview(&config);
        assert!(preview.contains("program=node"));
        assert!(preview.contains("args=1"));
        assert!(preview.contains("explicit_env_keys=2"));
        assert!(preview.contains("explicit_env_key_names=[\"SECRET_TOKEN\", \"PATH\"]"));
        assert!(preview.contains("cwd=/workspace"));
        assert!(!preview.contains("secret-arg-value"));
        assert!(!preview.contains("secret-env-value"));
        assert!(!preview.contains("/secret/path"));
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
