use aionui_common::{CommandSpec, ErrorChain};
use aionui_runtime::Builder as CmdBuilder;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
#[cfg(all(test, unix))]
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};

use crate::error::AgentError;

use super::{
    CliAgentProcess, MANAGED_HERMES_ENV_ISOLATION_MARKER, STDERR_BUFFER_MAX, prepare_command_cwd,
    tracked_process_group_id,
};

impl CliAgentProcess {
    /// Spawn a new CLI subprocess in **SDK mode**.
    ///
    /// Unlike [`spawn`](Self::spawn), this does NOT start a stdout reader task.
    /// Instead, the raw stdin/stdout handles are available via [`take_stdio`](Self::take_stdio)
    /// for the ACP SDK transport to own.
    ///
    /// Background tasks are still spawned for:
    /// - stderr buffering
    /// - Process exit monitoring
    pub async fn spawn_for_sdk(mut config: CommandSpec) -> Result<Self, AgentError> {
        let mut cmd = CmdBuilder::new(&config.command);
        let agent_env = prepare_agent_environment(aionui_runtime::agent_process_env().await, &mut config);
        let stderr_redactions = sensitive_env_values(&agent_env, &config);
        cmd.args(&config.args)
            .env_clear()
            .envs(agent_env)
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
                if !line.trim().is_empty() {
                    warn!(pid, stderr_bytes = line.len(), "CLI process emitted stderr");
                }
                let line = redact_sensitive_values(line, &stderr_redactions);
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

fn prepare_agent_environment(
    mut agent_env: Vec<(OsString, OsString)>,
    config: &mut CommandSpec,
) -> Vec<(OsString, OsString)> {
    let isolate_managed_hermes = take_managed_hermes_isolation_marker(config);
    agent_env.retain(|(name, _)| !managed_hermes_isolation_marker_name(name));
    if !isolate_managed_hermes {
        return agent_env;
    }

    let inherited_var_count = agent_env.len();
    agent_env.retain(|(name, _)| managed_hermes_inherited_env_allowed(name));
    info!(
        inherited_var_count,
        retained_var_count = agent_env.len(),
        "Aion-managed Hermes host environment isolated"
    );
    agent_env
}

fn managed_hermes_isolation_marker_name(name: &OsStr) -> bool {
    name.to_string_lossy()
        .eq_ignore_ascii_case(MANAGED_HERMES_ENV_ISOLATION_MARKER)
}

fn take_managed_hermes_isolation_marker(config: &mut CommandSpec) -> bool {
    let mut isolate = false;
    config.env.retain(|entry| {
        if entry.name.eq_ignore_ascii_case(MANAGED_HERMES_ENV_ISOLATION_MARKER) {
            isolate |= entry.value == "1";
            false
        } else {
            true
        }
    });
    isolate
}

fn managed_hermes_inherited_env_allowed(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    if ["_KEY", "_TOKEN"].iter().any(|suffix| upper.ends_with(suffix)) {
        return false;
    }
    upper.starts_with("LC_")
        || MANAGED_HERMES_INHERITED_ENV_KEYS
            .iter()
            .any(|allowed| upper == *allowed)
}

const MANAGED_HERMES_INHERITED_ENV_KEYS: &[&str] = &[
    // Executable discovery and process startup.
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "WINDIR",
    "OS",
    // Writable locations and user profile paths used by Python and Git.
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "USER",
    "LOGNAME",
    "USERNAME",
    "USERDOMAIN",
    "USERDOMAIN_ROAMINGPROFILE",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    // Windows program and architecture metadata.
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    // Locale and time-zone settings.
    "LANG",
    "LANGUAGE",
    "TZ",
    // Network routing and corporate CA configuration.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

fn sensitive_env_values(agent_env: &[(OsString, OsString)], config: &CommandSpec) -> Vec<String> {
    let mut unique = HashSet::new();
    for (name, value) in agent_env {
        if let (Some(name), Some(value)) = (name.to_str(), value.to_str())
            && is_sensitive_env_key(name)
        {
            add_sensitive_value(&mut unique, value);
        }
    }
    for entry in &config.env {
        if is_sensitive_env_key(&entry.name) {
            add_sensitive_value(&mut unique, &entry.value);
        }
    }

    let mut values: Vec<String> = unique.into_iter().collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values
}

fn add_sensitive_value(values: &mut HashSet<String>, value: &str) {
    if value.is_empty() {
        return;
    }
    values.insert(value.to_owned());
    values.extend(value.lines().filter(|line| !line.is_empty()).map(str::to_owned));
}

fn is_sensitive_env_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "BASE_URL", "URL"]
        .iter()
        .any(|marker| upper.contains(marker))
}

fn redact_sensitive_values(mut text: String, sensitive_values: &[String]) -> String {
    for value in sensitive_values {
        text = text.replace(value, "<redacted>");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::super::tests::simple_script_config;
    use super::*;
    use aionui_common::EnvVar;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;

    #[test]
    fn managed_hermes_keeps_required_host_env_and_explicit_overlays_only() {
        let mut config = CommandSpec {
            env: vec![
                EnvVar {
                    name: MANAGED_HERMES_ENV_ISOLATION_MARKER.into(),
                    value: "1".into(),
                },
                EnvVar {
                    name: "HERMES_GIT_BASH_PATH".into(),
                    value: r"C:\Program Files\Git\bin\bash.exe".into(),
                },
                EnvVar {
                    name: "PYTHONHOME".into(),
                    value: r"C:\Aion\Hermes\python".into(),
                },
                EnvVar {
                    name: "OPENAI_API_KEY".into(),
                    value: "selected-provider-key".into(),
                },
            ],
            ..CommandSpec::default()
        };
        let inherited = vec![
            os_env("Path", r"C:\Windows\System32"),
            os_env("SystemRoot", r"C:\Windows"),
            os_env("TEMP", r"C:\Temp"),
            os_env("LANG", "zh_CN.UTF-8"),
            os_env("LC_ALL", "zh_CN.UTF-8"),
            os_env("LC_SERVICE_TOKEN", "host-locale-shaped-secret"),
            os_env("LC_API_KEY", "host-locale-shaped-key"),
            os_env("https_proxy", "http://proxy.internal:8080"),
            os_env("REQUESTS_CA_BUNDLE", r"C:\Corp\ca.pem"),
            os_env("GITHUB_TOKEN", "host-github-secret"),
            os_env("AWS_SECRET_ACCESS_KEY", "host-aws-secret"),
            os_env("UNRELATED_HOST_KEY", "host-unrelated-secret"),
            os_env("ORDINARY_HOST_VAR", "host-ordinary-value"),
        ];

        let prepared = prepare_agent_environment(inherited, &mut config);

        assert_eq!(os_env_value(&prepared, "PATH"), Some(r"C:\Windows\System32"));
        assert_eq!(os_env_value(&prepared, "SYSTEMROOT"), Some(r"C:\Windows"));
        assert_eq!(os_env_value(&prepared, "TEMP"), Some(r"C:\Temp"));
        assert_eq!(os_env_value(&prepared, "LANG"), Some("zh_CN.UTF-8"));
        assert_eq!(os_env_value(&prepared, "LC_ALL"), Some("zh_CN.UTF-8"));
        assert_eq!(
            os_env_value(&prepared, "HTTPS_PROXY"),
            Some("http://proxy.internal:8080")
        );
        assert_eq!(os_env_value(&prepared, "REQUESTS_CA_BUNDLE"), Some(r"C:\Corp\ca.pem"));
        for removed in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "UNRELATED_HOST_KEY",
            "LC_SERVICE_TOKEN",
            "LC_API_KEY",
            "ORDINARY_HOST_VAR",
        ] {
            assert_eq!(os_env_value(&prepared, removed), None, "{removed} leaked");
        }

        assert_eq!(
            explicit_env_value(&config, "HERMES_GIT_BASH_PATH"),
            Some(r"C:\Program Files\Git\bin\bash.exe")
        );
        assert_eq!(
            explicit_env_value(&config, "PYTHONHOME"),
            Some(r"C:\Aion\Hermes\python")
        );
        assert_eq!(
            explicit_env_value(&config, "OPENAI_API_KEY"),
            Some("selected-provider-key")
        );
        assert_eq!(explicit_env_value(&config, MANAGED_HERMES_ENV_ISOLATION_MARKER), None);
    }

    #[test]
    fn other_agents_keep_the_existing_host_environment_behavior() {
        let inherited = vec![
            os_env("PATH", "/usr/bin"),
            os_env("GITHUB_TOKEN", "existing-host-token"),
            os_env("ORDINARY_HOST_VAR", "existing-host-value"),
            os_env(MANAGED_HERMES_ENV_ISOLATION_MARKER, "forged-host-value"),
        ];
        let mut config = CommandSpec {
            env: vec![EnvVar {
                name: "AGENT_SETTING".into(),
                value: "unchanged".into(),
            }],
            ..CommandSpec::default()
        };
        let original_config = config.clone();

        let prepared = prepare_agent_environment(inherited, &mut config);

        assert_eq!(os_env_value(&prepared, "PATH"), Some("/usr/bin"));
        assert_eq!(os_env_value(&prepared, "GITHUB_TOKEN"), Some("existing-host-token"));
        assert_eq!(
            os_env_value(&prepared, "ORDINARY_HOST_VAR"),
            Some("existing-host-value")
        );
        assert_eq!(os_env_value(&prepared, MANAGED_HERMES_ENV_ISOLATION_MARKER), None);
        assert_eq!(config, original_config);
    }

    #[tokio::test]
    async fn managed_hermes_marker_is_consumed_before_child_spawn() {
        let mut config = marker_env_echo_config();
        config.env.push(EnvVar {
            name: MANAGED_HERMES_ENV_ISOLATION_MARKER.into(),
            value: "1".into(),
        });

        let proc = CliAgentProcess::spawn_for_sdk(config).await.unwrap();
        let (_stdin, mut stdout) = proc.take_stdio().await.unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();

        assert_eq!(output.trim(), "unset");
    }

    fn os_env(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    fn os_env_value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(entry_name, _)| entry_name.to_string_lossy().eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.to_str())
    }

    fn explicit_env_value<'a>(config: &'a CommandSpec, name: &str) -> Option<&'a str> {
        config
            .env
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.value.as_str())
    }

    #[cfg(not(windows))]
    fn marker_env_echo_config() -> CommandSpec {
        CommandSpec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!("printf '%s\\n' \"${{{MANAGED_HERMES_ENV_ISOLATION_MARKER}:-unset}}\""),
            ],
            env: vec![],
            cwd: None,
        }
    }

    #[cfg(windows)]
    fn marker_env_echo_config() -> CommandSpec {
        CommandSpec {
            command: "powershell.exe".into(),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                format!(
                    "if ($null -eq $env:{MANAGED_HERMES_ENV_ISOLATION_MARKER}) {{ Write-Output 'unset' }} else {{ Write-Output $env:{MANAGED_HERMES_ENV_ISOLATION_MARKER} }}"
                ),
            ],
            env: vec![],
            cwd: None,
        }
    }

    // ── SDK mode tests ───────────────────────────────────────────────

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

        let mut config = simple_script_config(
            "printf 'shell=%s\nconfig=%s\noverlay=%s\nnpm=%s\nnode=%s\n' \
             \"${AIONUI_SHELL_ONLY:-unset}\" \
             \"${AIONUI_CONFIG_ONLY:-unset}\" \
             \"${AIONUI_OVERLAY:-unset}\" \
             \"${npm_lifecycle_event:-unset}\" \
             \"${NODE_OPTIONS:-unset}\"",
        );
        config.env.push(EnvVar {
            name: "AIONUI_CONFIG_ONLY".into(),
            value: "from-config".into(),
        });
        config.env.push(EnvVar {
            name: "AIONUI_OVERLAY".into(),
            value: "from-config".into(),
        });

        let proc = CliAgentProcess::spawn_for_sdk(config).await.unwrap();
        let (_stdin, mut stdout) = proc.take_stdio().await.unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();

        assert!(output.contains("shell=from-shell"), "{output}");
        assert!(output.contains("config=from-config"), "{output}");
        assert!(output.contains("overlay=from-config"), "{output}");
        assert!(output.contains("npm=unset"), "{output}");
        assert!(output.contains("node=unset"), "{output}");
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
    async fn sdk_stderr_redacts_sensitive_env_values_before_buffering() {
        const MARKER: &str = "marker-secret-that-must-not-leak";
        let mut config = stderr_env_echo_config();
        config.env.push(EnvVar {
            name: "OPENAI_API_KEY".into(),
            value: MARKER.into(),
        });

        let preview = CliAgentProcess::sdk_spawn_preview(&config);
        assert!(!preview.contains(MARKER));

        let proc = CliAgentProcess::spawn_for_sdk(config).await.unwrap();
        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        let stderr = proc.take_stderr().await;

        assert!(!stderr.contains(MARKER), "{stderr}");
        assert!(stderr.contains("<redacted>"), "{stderr}");
    }

    #[test]
    fn stderr_redaction_is_limited_to_sensitive_named_env_values() {
        let config = CommandSpec {
            env: vec![
                EnvVar {
                    name: "SERVICE_TOKEN".into(),
                    value: "long-sensitive-value".into(),
                },
                EnvVar {
                    name: "OPENAI_BASE_URL".into(),
                    value: "https://provider.internal/v1".into(),
                },
                EnvVar {
                    name: "ORDINARY_SETTING".into(),
                    value: "visible-value".into(),
                },
                EnvVar {
                    name: "EMPTY_SECRET".into(),
                    value: String::new(),
                },
                EnvVar {
                    name: "MULTILINE_SECRET".into(),
                    value: "first-line-secret\r\nsecond-line-secret".into(),
                },
            ],
            ..CommandSpec::default()
        };
        let inherited = vec![(
            OsString::from("PARENT_PASSWORD"),
            OsString::from("inherited-sensitive-value"),
        )];
        let redactions = sensitive_env_values(&inherited, &config);
        let redacted = redact_sensitive_values(
            "long-sensitive-value https://provider.internal/v1 inherited-sensitive-value visible-value".into(),
            &redactions,
        );

        assert_eq!(redacted, "<redacted> <redacted> <redacted> visible-value");
        assert!(!redactions.iter().any(String::is_empty));
        assert_eq!(
            redact_sensitive_values("first-line-secret".into(), &redactions),
            "<redacted>"
        );
        assert_eq!(
            redact_sensitive_values("second-line-secret".into(), &redactions),
            "<redacted>"
        );
    }

    #[cfg(not(windows))]
    fn stderr_env_echo_config() -> CommandSpec {
        CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "printf '%s\n' \"$OPENAI_API_KEY\" >&2".into()],
            env: vec![],
            cwd: None,
        }
    }

    #[cfg(windows)]
    fn stderr_env_echo_config() -> CommandSpec {
        CommandSpec {
            command: "powershell.exe".into(),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                "[Console]::Error.WriteLine($env:OPENAI_API_KEY)".into(),
            ],
            env: vec![],
            cwd: None,
        }
    }

    #[tokio::test]
    async fn spawn_for_sdk_take_stdio() {
        let config = simple_script_config("read line && echo \"$line\"");
        let proc = CliAgentProcess::spawn_for_sdk(config).await.unwrap();

        let stdio = proc.take_stdio().await;
        assert!(stdio.is_some(), "First take_stdio should succeed");

        let stdio_again = proc.take_stdio().await;
        assert!(stdio_again.is_none(), "Second take_stdio should return None");

        proc.kill(Duration::from_millis(100)).await.unwrap();
    }
}
