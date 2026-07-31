//! Build-time preparation for the pinned Aion-managed Hermes ACP runtime.
//!
//! The shipped pack contains a portable CPython distribution, the patched
//! first-party Hermes adapter, PortableGit, and ripgrep. End-user startup never
//! downloads or installs any of these components.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::managed_cli::current_runtime_key;
use crate::managed_resources_contract::{
    ManagedCliLaunchContract, ManagedCliLaunchEnvContract, ManagedCliResourceContract, collect_file_hashes,
    relative_contract_path,
};

use super::ManagedCliError;

pub const HERMES_AGENT_VERSION: &str = "0.19.0";
pub const HERMES_MANAGED_VERSION: &str = "0.19.0+aion.1";
pub const HERMES_AGENT_RELEASE_TAG: &str = "v2026.7.20";
pub const HERMES_AGENT_COMMIT: &str = "3ef6bbd201263d354fd83ec55b3c306ded2eb72a";
pub const HERMES_RUNTIME_TARGET: &str = "win32-x64";

#[derive(Debug, Clone)]
pub struct PreparedHermesRuntime {
    pub root: PathBuf,
}

/// Build the pinned Windows x64 Hermes pack under the managed-resources root.
///
/// This is a release/build-machine operation. The PowerShell helper verifies
/// every downloaded archive, applies the pinned first-party adapter patch, and
/// runs the expanded adapter's own `--version` and `--check` entry points.
#[cfg(windows)]
pub async fn prepare_managed_hermes_to_root(out_root: &Path) -> Result<PreparedHermesRuntime, ManagedCliError> {
    if current_runtime_key() != Some(HERMES_RUNTIME_TARGET) {
        return Err(ManagedCliError::new(
            "the bundled Hermes Beta runtime currently supports Windows x64 only",
        ));
    }

    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(ManagedCliError::io)?;
    let script = repository_root.join("scripts/hermes-runtime/prepare.ps1");
    if !script.is_file() {
        return Err(ManagedCliError::new("Hermes runtime preparation script is missing"));
    }

    let runtime_root = out_root
        .join("cli")
        .join("hermes")
        .join(HERMES_MANAGED_VERSION)
        .join(HERMES_RUNTIME_TARGET);
    let mut command = crate::Builder::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    command.arg(&script);
    command.arg("-OutputRoot");
    command.arg(&runtime_root);
    command.arg("-RepositoryRoot");
    command.arg(&repository_root);

    tracing::info!(
        cli = "hermes",
        version = HERMES_MANAGED_VERSION,
        source_commit = HERMES_AGENT_COMMIT,
        target = HERMES_RUNTIME_TARGET,
        "preparing managed Hermes runtime"
    );
    let output = command.output().await.map_err(ManagedCliError::io)?;
    if !output.status.success() {
        return Err(ManagedCliError::new(format!(
            "Hermes runtime preparation failed (code {:?})",
            output.status.code()
        )));
    }

    validate_prepared_layout(&runtime_root)?;
    Ok(PreparedHermesRuntime { root: runtime_root })
}

#[cfg(not(windows))]
pub async fn prepare_managed_hermes_to_root(_out_root: &Path) -> Result<PreparedHermesRuntime, ManagedCliError> {
    Err(ManagedCliError::new(
        "the bundled Hermes Beta runtime can only be prepared on Windows x64",
    ))
}

pub fn managed_hermes_contract_for_export(
    bundle_root: &Path,
    prepared: &PreparedHermesRuntime,
) -> Result<ManagedCliResourceContract, ManagedCliError> {
    validate_prepared_layout(&prepared.root)?;
    let root = relative_contract_path(bundle_root, &prepared.root)
        .map_err(|error| ManagedCliError::new(format!("Hermes runtime escaped bundle root: {error}")))?;
    let files = collect_file_hashes(&prepared.root)
        .map_err(|error| ManagedCliError::new(format!("hash Hermes runtime files: {error}")))?;

    Ok(ManagedCliResourceContract {
        name: "hermes".into(),
        version: HERMES_MANAGED_VERSION.into(),
        root,
        platform_directory: HERMES_RUNTIME_TARGET.into(),
        executable: "python/python.exe".into(),
        required_files: vec![
            "runtime.sha256".into(),
            "tools/git/bin/bash.exe".into(),
            "tools/git/cmd/git.exe".into(),
            "tools/rg/rg.exe".into(),
        ],
        required_directories: vec![
            "python".into(),
            "tools/git".into(),
            "tools/rg".into(),
            "licenses".into(),
        ],
        launch: Some(ManagedCliLaunchContract {
            program: "python/python.exe".into(),
            args_prefix: vec!["-P".into(), "-m".into(), "acp_adapter".into()],
            env: vec![
                ManagedCliLaunchEnvContract {
                    name: "HERMES_GIT_BASH_PATH".into(),
                    value: None,
                    relative_path: Some("tools/git/bin/bash.exe".into()),
                },
                ManagedCliLaunchEnvContract {
                    name: "HERMES_ACP_SKIP_CONFIGURED_MCP".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "HERMES_ACP_TOOLSET".into(),
                    value: Some("hermes-acp-lite".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "HERMES_DISABLE_LAZY_INSTALLS".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "PYTHONDONTWRITEBYTECODE".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "PYTHONNOUSERSITE".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "PYTHONSAFEPATH".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "PYTHONUTF8".into(),
                    value: Some("1".into()),
                    relative_path: None,
                },
                ManagedCliLaunchEnvContract {
                    name: "PYTHONIOENCODING".into(),
                    value: Some("utf-8".into()),
                    relative_path: None,
                },
            ],
            path_entries: vec!["tools/rg".into(), "tools/git/cmd".into()],
        }),
        files,
        capabilities: BTreeMap::from([
            ("browser".into(), "not-installed".into()),
            ("configuredMcp".into(), "disabled".into()),
            ("lazyInstalls".into(), "disabled".into()),
            ("provider".into(), "openai-compatible".into()),
            ("toolset".into(), "hermes-acp-lite".into()),
            ("web".into(), "disabled".into()),
        ]),
    })
}

fn validate_prepared_layout(root: &Path) -> Result<(), ManagedCliError> {
    for relative in [
        "python/python.exe",
        "runtime.sha256",
        "tools/git/bin/bash.exe",
        "tools/git/cmd/git.exe",
        "tools/rg/rg.exe",
    ] {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(ManagedCliError::new(format!(
                "prepared Hermes runtime is missing {relative}"
            )));
        }
    }
    if !root.join("licenses").is_dir() {
        return Err(ManagedCliError::new("prepared Hermes runtime is missing licenses"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_contract_records_expanded_launch_plan_and_lite_capabilities() {
        let bundle = tempfile::tempdir().expect("bundle");
        let root = bundle
            .path()
            .join("cli/hermes")
            .join(HERMES_MANAGED_VERSION)
            .join(HERMES_RUNTIME_TARGET);
        for relative in [
            "python/python.exe",
            "runtime.sha256",
            "tools/git/bin/bash.exe",
            "tools/git/cmd/git.exe",
            "tools/rg/rg.exe",
            "licenses/hermes-agent-MIT.txt",
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, relative).expect("write");
        }

        let contract =
            managed_hermes_contract_for_export(bundle.path(), &PreparedHermesRuntime { root }).expect("contract");
        let launch = contract.launch.expect("launch");
        assert_eq!(launch.program, "python/python.exe");
        assert_eq!(contract.version, HERMES_MANAGED_VERSION);
        assert_eq!(launch.args_prefix, ["-P", "-m", "acp_adapter"]);
        assert!(
            launch
                .env
                .iter()
                .any(|entry| entry.name == "HERMES_GIT_BASH_PATH" && entry.relative_path.is_some())
        );
        assert!(
            launch
                .env
                .iter()
                .any(|entry| entry.name == "PYTHONDONTWRITEBYTECODE" && entry.value.as_deref() == Some("1"))
        );
        assert_eq!(
            contract.capabilities.get("browser").map(String::as_str),
            Some("not-installed")
        );
        assert_eq!(contract.capabilities.get("web").map(String::as_str), Some("disabled"));
        assert!(contract.files.iter().any(|file| file.path == "python/python.exe"));
    }
}
