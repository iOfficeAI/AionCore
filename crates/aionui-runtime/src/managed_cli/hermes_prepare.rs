//! Build-time preparation for the pinned Aion-managed Hermes ACP runtime.
//!
//! The shipped pack contains a portable CPython distribution, the patched
//! first-party Hermes adapter, PortableGit, and ripgrep. End-user startup never
//! downloads or installs any of these components.

use std::collections::BTreeMap;
use std::fs;
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
        .map(powershell_compatible_path)
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
    if runtime_root.is_dir() {
        match validate_reusable_runtime(&runtime_root, &repository_root) {
            Ok(()) => {
                tracing::info!(
                    cli = "hermes",
                    version = HERMES_MANAGED_VERSION,
                    target = HERMES_RUNTIME_TARGET,
                    "reusing verified managed Hermes runtime"
                );
                return Ok(PreparedHermesRuntime { root: runtime_root });
            }
            Err(error) => {
                tracing::warn!(
                    cli = "hermes",
                    version = HERMES_MANAGED_VERSION,
                    target = HERMES_RUNTIME_TARGET,
                    reason = %error,
                    "existing managed Hermes runtime is not reusable; rebuilding"
                );
            }
        }
    }

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
        return Err(ManagedCliError::new(hermes_prepare_failure_detail(
            output.status.code(),
            &output.stdout,
            &output.stderr,
        )));
    }

    validate_prepared_layout(&runtime_root)?;
    Ok(PreparedHermesRuntime { root: runtime_root })
}

#[cfg(windows)]
fn powershell_compatible_path(path: PathBuf) -> PathBuf {
    let path_text = path.to_string_lossy();
    if let Some(unc_path) = path_text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc_path}"));
    }
    if let Some(drive_path) = path_text.strip_prefix(r"\\?\") {
        return PathBuf::from(drive_path);
    }
    path
}

fn hermes_prepare_failure_detail(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    const MAX_DIAGNOSTIC_LINES: usize = 20;

    fn tail(bytes: &[u8], max_lines: usize) -> String {
        let text = String::from_utf8_lossy(bytes);
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        lines[lines.len().saturating_sub(max_lines)..].join(" | ")
    }

    let stderr = tail(stderr, MAX_DIAGNOSTIC_LINES);
    let stdout = tail(stdout, MAX_DIAGNOSTIC_LINES);
    let diagnostic = match (stderr.is_empty(), stdout.is_empty()) {
        (false, false) => format!("stderr: {stderr}; stdout: {stdout}"),
        (false, true) => format!("stderr: {stderr}"),
        (true, false) => format!("stdout: {stdout}"),
        (true, true) => "PowerShell produced no diagnostic output".to_owned(),
    };

    format!("Hermes runtime preparation failed (code {code:?}): {diagnostic}")
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

fn validate_reusable_runtime(root: &Path, repository_root: &Path) -> Result<(), ManagedCliError> {
    validate_prepared_layout(root)?;

    for (runtime_relative, repository_relative) in [
        ("licenses/runtime-lock.json", "vendor/hermes-agent/runtime-lock.json"),
        ("licenses/aion-managed.patch", "vendor/hermes-agent/aion-managed.patch"),
        (
            "licenses/requirements-win32-x64.txt",
            "vendor/hermes-agent/requirements-win32-x64.txt",
        ),
    ] {
        let runtime_contents = fs::read(root.join(runtime_relative)).map_err(ManagedCliError::io)?;
        let repository_contents = fs::read(repository_root.join(repository_relative)).map_err(ManagedCliError::io)?;
        if runtime_contents != repository_contents {
            return Err(ManagedCliError::new(format!(
                "prepared Hermes runtime provenance differs for {runtime_relative}"
            )));
        }
    }

    let checksum_path = root.join("runtime.sha256");
    let checksum_contents = fs::read_to_string(&checksum_path).map_err(ManagedCliError::io)?;
    let mut expected = BTreeMap::new();
    for (index, line) in checksum_contents.lines().enumerate() {
        let (sha256, path) = line.split_once("  ").ok_or_else(|| {
            ManagedCliError::new(format!(
                "prepared Hermes runtime checksum line {} is malformed",
                index + 1
            ))
        })?;
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) || path.is_empty() {
            return Err(ManagedCliError::new(format!(
                "prepared Hermes runtime checksum line {} is malformed",
                index + 1
            )));
        }
        if expected.insert(path.to_owned(), sha256.to_ascii_lowercase()).is_some() {
            return Err(ManagedCliError::new(format!(
                "prepared Hermes runtime checksum contains duplicate path {path}"
            )));
        }
    }

    let actual = collect_file_hashes(root)
        .map_err(|error| ManagedCliError::new(format!("hash existing Hermes runtime: {error}")))?
        .into_iter()
        .filter(|file| file.path != "runtime.sha256")
        .map(|file| (file.path, file.sha256))
        .collect::<BTreeMap<_, _>>();
    if actual != expected {
        return Err(ManagedCliError::new(
            "prepared Hermes runtime files do not match runtime.sha256",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_reusable_runtime_fixture(root: &Path, repository_root: &Path) {
        for (runtime_relative, repository_relative) in [
            ("licenses/runtime-lock.json", "vendor/hermes-agent/runtime-lock.json"),
            ("licenses/aion-managed.patch", "vendor/hermes-agent/aion-managed.patch"),
            (
                "licenses/requirements-win32-x64.txt",
                "vendor/hermes-agent/requirements-win32-x64.txt",
            ),
        ] {
            let repository_path = repository_root.join(repository_relative);
            std::fs::create_dir_all(repository_path.parent().expect("repository parent")).expect("mkdir repository");
            std::fs::write(&repository_path, repository_relative).expect("write repository provenance");

            let runtime_path = root.join(runtime_relative);
            std::fs::create_dir_all(runtime_path.parent().expect("runtime parent")).expect("mkdir runtime");
            std::fs::write(runtime_path, repository_relative).expect("write runtime provenance");
        }
        for relative in [
            "python/python.exe",
            "tools/git/bin/bash.exe",
            "tools/git/cmd/git.exe",
            "tools/rg/rg.exe",
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, relative).expect("write");
        }

        let checksums = collect_file_hashes(root)
            .expect("hash fixture")
            .into_iter()
            .map(|file| format!("{}  {}", file.sha256, file.path))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("runtime.sha256"), format!("{checksums}\n")).expect("write checksums");
    }

    #[cfg(windows)]
    #[test]
    fn powershell_path_removes_windows_verbatim_prefixes() {
        assert_eq!(
            powershell_compatible_path(PathBuf::from(r"\\?\D:\repo")),
            PathBuf::from(r"D:\repo")
        );
        assert_eq!(
            powershell_compatible_path(PathBuf::from(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
        assert_eq!(
            powershell_compatible_path(PathBuf::from(r"D:\repo")),
            PathBuf::from(r"D:\repo")
        );
    }

    #[test]
    fn hermes_prepare_failure_includes_powershell_diagnostic() {
        let detail = hermes_prepare_failure_detail(
            Some(1),
            b"Bootstrapped pinned uv 0.11.33\n",
            b"Invoke-WebRequest: connection failed\n",
        );

        assert!(detail.contains("code Some(1)"));
        assert!(detail.contains("stderr: Invoke-WebRequest: connection failed"));
        assert!(detail.contains("stdout: Bootstrapped pinned uv 0.11.33"));
    }

    #[test]
    fn hermes_prepare_failure_limits_each_stream_to_the_last_twenty_lines() {
        let stderr = (0..25)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let detail = hermes_prepare_failure_detail(Some(1), b"", stderr.as_bytes());

        assert!(!detail.contains("line-4 |"));
        assert!(detail.contains("line-5 |"));
        assert!(detail.contains("line-24"));
    }

    #[test]
    fn reusable_runtime_requires_matching_provenance_and_all_file_hashes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("runtime");
        let repository_root = temp.path().join("repository");
        write_reusable_runtime_fixture(&root, &repository_root);

        validate_reusable_runtime(&root, &repository_root).expect("valid reusable runtime");

        std::fs::write(root.join("python/python.exe"), "tampered").expect("tamper runtime");
        let error = validate_reusable_runtime(&root, &repository_root).expect_err("tampering must fail");
        assert!(error.to_string().contains("do not match runtime.sha256"));
    }

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
