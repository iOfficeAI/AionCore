use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

use tokio::sync::OnceCell;

#[cfg(unix)]
use crate::Builder;

static FULL_SHELL_ENV: OnceCell<Vec<(OsString, OsString)>> = OnceCell::const_new();

/// Build a cleaned environment for long-running agent subprocesses.
///
/// This intentionally does not mutate the backend process environment. Agent
/// spawns should call `env_clear()` and then apply this map, matching the old
/// Electron `spawn(..., { env })` behavior without spreading shell variables to
/// unrelated backend code.
pub async fn agent_process_env() -> Vec<(OsString, OsString)> {
    build_agent_process_env(std::env::vars_os().collect(), full_shell_env().await.to_vec())
}

async fn full_shell_env() -> &'static Vec<(OsString, OsString)> {
    FULL_SHELL_ENV.get_or_init(load_full_shell_env).await
}

fn build_agent_process_env(
    current_env: Vec<(OsString, OsString)>,
    shell_env: Vec<(OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let current_path = get_env_value(&current_env, "PATH").cloned();
    let shell_path = get_env_value(&shell_env, "PATH").cloned();

    let mut merged: BTreeMap<OsString, OsString> = current_env.into_iter().collect();
    merged.extend(shell_env);

    remove_env_key(&mut merged, "PATH");
    if let Some(path) = merge_path_values(current_path.as_deref(), shell_path.as_deref()) {
        merged.insert(OsString::from("PATH"), path);
    }

    clean_agent_env(&mut merged);
    merged.into_iter().collect()
}

fn clean_agent_env(env: &mut BTreeMap<OsString, OsString>) {
    for key in ["NODE_OPTIONS", "NODE_INSPECT", "NODE_DEBUG", "CLAUDECODE"] {
        remove_env_key(env, key);
    }
    env.retain(|key, _| !env_key_starts_with(key, "npm_"));
}

fn get_env_value<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsString> {
    env.iter()
        .find(|(name, _)| env_key_eq(name.as_os_str(), key))
        .map(|(_, value)| value)
}

fn remove_env_key(env: &mut BTreeMap<OsString, OsString>, key: &str) {
    env.retain(|name, _| !env_key_eq(name.as_os_str(), key));
}

#[cfg(windows)]
fn env_key_eq(name: &std::ffi::OsStr, key: &str) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(key)
}

#[cfg(not(windows))]
fn env_key_eq(name: &std::ffi::OsStr, key: &str) -> bool {
    name == std::ffi::OsStr::new(key)
}

#[cfg(windows)]
fn env_key_starts_with(name: &std::ffi::OsStr, prefix: &str) -> bool {
    name.to_string_lossy()
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
}

#[cfg(not(windows))]
fn env_key_starts_with(name: &std::ffi::OsStr, prefix: &str) -> bool {
    name.to_string_lossy().starts_with(prefix)
}

fn merge_path_values(current: Option<&std::ffi::OsStr>, shell: Option<&std::ffi::OsStr>) -> Option<OsString> {
    let mut seen = std::collections::HashSet::<PathBuf>::new();
    let mut parts = Vec::new();
    for value in [current, shell].into_iter().flatten() {
        for path in std::env::split_paths(value) {
            if path.as_os_str().is_empty() {
                continue;
            }
            if seen.insert(path.clone()) {
                parts.push(path);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        std::env::join_paths(parts).ok()
    }
}

#[cfg(unix)]
async fn load_full_shell_env() -> Vec<(OsString, OsString)> {
    let Some(shell) = std::env::var_os("SHELL") else {
        return Vec::new();
    };
    if !Path::new(&shell).is_absolute() {
        tracing::debug!(shell = %shell.to_string_lossy(), "SHELL is not absolute, skipping full shell env probe");
        return Vec::new();
    }

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut builder = Builder::clean_cli(&shell);
        builder.args(["-i", "-l", "-c", "env"]);
        builder.output().await
    })
    .await;

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::debug!(error = %error, "full shell env probe failed to spawn");
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!("full shell env probe timed out after 5s");
            return Vec::new();
        }
    };

    if !output.status.success() {
        tracing::debug!(status = ?output.status, "full shell env probe exited non-zero");
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_env_output(&stdout);
    tracing::info!(var_count = parsed.len(), "full shell env loaded for agent subprocesses");
    parsed
}

#[cfg(not(unix))]
async fn load_full_shell_env() -> Vec<(OsString, OsString)> {
    Vec::new()
}

#[cfg(unix)]
fn parse_env_output(output: &str) -> Vec<(OsString, OsString)> {
    let mut parsed = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    for line in output.split('\n') {
        if let Some((key, value)) = parse_env_start(line) {
            if let Some(key) = current_key.replace(key.to_owned()) {
                parsed.push((OsString::from(key), OsString::from(std::mem::take(&mut current_value))));
            }
            current_value.push_str(value);
        } else if current_key.is_some() {
            current_value.push('\n');
            current_value.push_str(line);
        }
    }

    if let Some(key) = current_key {
        parsed.push((OsString::from(key), OsString::from(current_value)));
    }

    parsed
}

#[cfg(unix)]
fn parse_env_start(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    if is_valid_env_key(key) {
        Some((key, value))
    } else {
        None
    }
}

#[cfg(unix)]
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;

    const CHILD_MARKER: &str = "AIONUI_RUNTIME_AGENT_ENV_TEST_CHILD";

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_process_env_merges_full_shell_env_and_cleans_pollution() {
        if std::env::var_os(CHILD_MARKER).is_none() {
            let temp = tempfile::tempdir().unwrap();
            let shell = temp.path().join("fake-shell");
            write_fake_shell(
                &shell,
                r#"#!/bin/sh
printf '%s\n' \
  'AIONUI_SHELL_ONLY=from-shell' \
  'AIONUI_OVERLAY=from-shell' \
  'PATH=/shell/bin:/current/bin' \
  'NODE_OPTIONS=--inspect' \
  'CLAUDECODE=1' \
  'npm_lifecycle_event=start'
"#,
            );

            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("agent_env::tests::agent_process_env_merges_full_shell_env_and_cleans_pollution")
                .arg("--nocapture")
                .env(CHILD_MARKER, "1")
                .env("SHELL", &shell)
                .env("PATH", "/current/bin")
                .env("AIONUI_CURRENT_ONLY", "from-current")
                .env("AIONUI_OVERLAY", "from-current")
                .env("NODE_OPTIONS", "--require parent")
                .env("CLAUDECODE", "1")
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

        let env = agent_process_env().await;
        let value = |key: &str| {
            env.iter()
                .find(|(name, _)| name == OsStr::new(key))
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };

        assert_eq!(value("AIONUI_CURRENT_ONLY").as_deref(), Some("from-current"));
        assert_eq!(value("AIONUI_SHELL_ONLY").as_deref(), Some("from-shell"));
        assert_eq!(value("AIONUI_OVERLAY").as_deref(), Some("from-shell"));
        assert_eq!(value("NODE_OPTIONS"), None);
        assert_eq!(value("CLAUDECODE"), None);
        assert_eq!(value("npm_config_cache"), None);
        assert_eq!(value("npm_lifecycle_event"), None);

        let path = value("PATH").expect("PATH should be present");
        assert!(
            path.starts_with("/current/bin"),
            "current/enhanced PATH should stay first, got {path}"
        );
        assert!(
            path.contains("/shell/bin"),
            "shell PATH entries should be appended, got {path}"
        );
    }

    #[cfg(unix)]
    fn write_fake_shell(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
