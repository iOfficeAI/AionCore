//! Build-time preparation of bundled CLIs: install the pinned npm package for
//! the target platform (via the bundled node's npm) and materialize the native
//! binary subtree into `managed-resources/cli/<name>/<version>/<target>/`.
//!
//! This runs on the build machine (the `prepare-managed-resources` subcommand),
//! never in the shipped app. Mirrors the removed ACP-tool prepare, minus the
//! node bridge / local manifest — the CLIs are native binaries.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::managed_cli::{cli_version, current_runtime_key};
use crate::managed_resources;
use crate::managed_resources_contract::ManagedCliResourceContract;
use crate::node_runtime::ensure_node_runtime;
use crate::spawn::Builder;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ManagedCliError(String);

impl ManagedCliError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
    fn io(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

/// The npm top-level package for a CLI name.
fn npm_package(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        _ => None,
    }
}

/// npm `--os` / `--cpu` for the current platform, derived from the runtime key.
fn npm_os_cpu() -> Option<(&'static str, &'static str)> {
    match current_runtime_key()? {
        "darwin-arm64" => Some(("darwin", "arm64")),
        "darwin-x64" => Some(("darwin", "x64")),
        "linux-arm64" => Some(("linux", "arm64")),
        "linux-x64" => Some(("linux", "x64")),
        "win32-x64" => Some(("win32", "x64")),
        "win32-arm64" => Some(("win32", "arm64")),
        _ => None,
    }
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

#[derive(Serialize)]
struct DevPackageJson {
    name: &'static str,
    private: bool,
}

pub struct PreparedCli {
    pub name: String,
    pub version: String,
    pub target: String,
    /// Absolute path to the materialized CLI root (`<out>/cli/<name>/<ver>/<target>`).
    pub root: PathBuf,
    /// Executable path relative to `root`.
    pub executable: String,
    /// Directories (relative to `root`) that must exist — e.g. codex `vendor/<triple>`.
    pub required_directories: Vec<String>,
}

/// Install `name`'s pinned CLI for the current platform and materialize its
/// native binary subtree under `out_root/cli/<name>/<version>/<target>/`.
pub async fn prepare_managed_cli_to_root(name: &str, out_root: &Path) -> Result<PreparedCli, ManagedCliError> {
    let version = cli_version(name).ok_or_else(|| ManagedCliError::new(format!("unknown CLI {name}")))?;
    let target =
        current_runtime_key().ok_or_else(|| ManagedCliError::new("unsupported platform for managed CLI prepare"))?;
    let package = npm_package(name).ok_or_else(|| ManagedCliError::new(format!("unknown CLI {name}")))?;
    let (npm_os, npm_cpu) = npm_os_cpu().ok_or_else(|| ManagedCliError::new("unsupported npm os/cpu"))?;

    let node_runtime = ensure_node_runtime()
        .await
        .map_err(|error| ManagedCliError::new(format!("ensure node runtime: {error}")))?;

    // Deterministic build-time staging dir (this runs on the build machine, one
    // CLI at a time). Cleared first so a re-run starts from a clean install tree.
    let staging = std::env::temp_dir().join(format!("aionui-cli-prepare-{name}-{version}-{target}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(ManagedCliError::io)?;
    }
    let project_dir = staging.join("project");
    let npm_cache_dir = staging.join("npm-cache");
    std::fs::create_dir_all(&project_dir).map_err(ManagedCliError::io)?;
    std::fs::create_dir_all(&npm_cache_dir).map_err(ManagedCliError::io)?;

    let package_json = DevPackageJson {
        name: "aionui-managed-cli-dev",
        private: true,
    };
    std::fs::write(
        project_dir.join("package.json"),
        serde_json::to_vec_pretty(&package_json)
            .map_err(|error| ManagedCliError::new(format!("serialize package.json: {error}")))?,
    )
    .map_err(ManagedCliError::io)?;

    // Install the pinned CLI for the TARGET platform's optional binary package.
    // --include=optional pulls the platform sub-package; --ignore-scripts avoids
    // any postinstall; --os/--cpu cross-select the correct native binary.
    let spec = format!("{package}@{version}");
    let mut builder = Builder::from_resolved(&node_runtime.npm_command());
    builder
        .current_dir(&project_dir)
        .env("npm_config_cache", &npm_cache_dir)
        .args([
            "install",
            "--include=optional",
            "--ignore-scripts",
            "--fund=false",
            "--audit=false",
            "--save-exact",
            "--os",
            npm_os,
            "--cpu",
            npm_cpu,
            &spec,
        ]);
    let output = builder.output().await.map_err(ManagedCliError::io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ManagedCliError::new(format!(
            "npm install {spec} ({npm_os}/{npm_cpu}) failed (code {:?}): {}",
            output.status.code(),
            stderr.trim()
        )));
    }

    let node_modules = project_dir.join("node_modules");
    let cli_root = out_root.join("cli").join(name).join(version).join(target);

    let (executable, required_directories) = match name {
        "claude" => {
            let src = find_platform_package(&node_modules, "@anthropic-ai", "claude-code-")?
                .join(format!("claude{}", exe_suffix()));
            if !src.is_file() {
                return Err(ManagedCliError::new(format!(
                    "claude binary missing at {}",
                    src.display()
                )));
            }
            std::fs::create_dir_all(&cli_root).map_err(ManagedCliError::io)?;
            let dst = cli_root.join(format!("claude{}", exe_suffix()));
            std::fs::copy(&src, &dst).map_err(ManagedCliError::io)?;
            set_executable(&dst)?;
            (format!("claude{}", exe_suffix()), Vec::new())
        }
        "codex" => {
            let pkg = find_platform_package(&node_modules, "@openai", "codex-")?;
            let src_vendor = pkg.join("vendor");
            if !src_vendor.is_dir() {
                return Err(ManagedCliError::new(format!(
                    "codex vendor dir missing at {}",
                    src_vendor.display()
                )));
            }
            managed_resources::materialize_directory(&src_vendor, &cli_root.join("vendor"))
                .map_err(ManagedCliError::io)?;
            // Locate the vendor triple dir (e.g. vendor/aarch64-apple-darwin).
            let triple = first_subdir_name(&cli_root.join("vendor"))?;
            let exe = format!("vendor/{triple}/bin/codex{}", exe_suffix());
            let exe_abs = cli_root.join(&exe);
            if !exe_abs.is_file() {
                return Err(ManagedCliError::new(format!(
                    "codex binary missing at {}",
                    exe_abs.display()
                )));
            }
            set_executable(&exe_abs)?;
            (exe, vec![format!("vendor/{triple}")])
        }
        _ => return Err(ManagedCliError::new(format!("unknown CLI {name}"))),
    };

    Ok(PreparedCli {
        name: name.to_owned(),
        version: version.to_owned(),
        target: target.to_owned(),
        root: cli_root,
        executable,
        required_directories,
    })
}

/// Build the manifest contract entry for a prepared CLI, relative to the bundle root.
pub fn managed_cli_contract_for_export(
    bundle_root: &Path,
    prepared: &PreparedCli,
) -> Result<ManagedCliResourceContract, ManagedCliError> {
    let root = prepared
        .root
        .strip_prefix(bundle_root)
        .map_err(|_| ManagedCliError::new("prepared CLI root escaped bundle root"))?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(ManagedCliResourceContract {
        name: prepared.name.clone(),
        version: prepared.version.clone(),
        root,
        platform_directory: prepared.target.clone(),
        executable: prepared.executable.clone(),
        required_files: Vec::new(),
        required_directories: prepared.required_directories.clone(),
    })
}

/// Find the single platform optional-dependency package directory under
/// `node_modules/<scope>/` whose name starts with `prefix` (e.g. `claude-code-`).
fn find_platform_package(node_modules: &Path, scope: &str, prefix: &str) -> Result<PathBuf, ManagedCliError> {
    let scope_dir = node_modules.join(scope);
    let entries = std::fs::read_dir(&scope_dir)
        .map_err(|error| ManagedCliError::new(format!("read {}: {error}", scope_dir.display())))?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.starts_with(prefix) && entry.path().is_dir() {
            return Ok(entry.path());
        }
    }
    Err(ManagedCliError::new(format!(
        "no platform package {scope}/{prefix}* installed under {}",
        scope_dir.display()
    )))
}

fn first_subdir_name(dir: &Path) -> Result<String, ManagedCliError> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| ManagedCliError::new(format!("read {}: {error}", dir.display())))?;
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            return Ok(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Err(ManagedCliError::new(format!("no subdirectory under {}", dir.display())))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), ManagedCliError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).map_err(ManagedCliError::io)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(ManagedCliError::io)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), ManagedCliError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_package_and_os_cpu_map_known_clis() {
        assert_eq!(npm_package("claude"), Some("@anthropic-ai/claude-code"));
        assert_eq!(npm_package("codex"), Some("@openai/codex"));
        assert_eq!(npm_package("gemini"), None);
        assert!(npm_os_cpu().is_some());
    }

    #[test]
    fn contract_export_makes_paths_relative() {
        let bundle = tempfile::tempdir().unwrap();
        let root = bundle
            .path()
            .join("cli")
            .join("claude")
            .join("2.1.215")
            .join("darwin-arm64");
        std::fs::create_dir_all(&root).unwrap();
        let prepared = PreparedCli {
            name: "claude".into(),
            version: "2.1.215".into(),
            target: "darwin-arm64".into(),
            root,
            executable: "claude".into(),
            required_directories: vec![],
        };
        let contract = managed_cli_contract_for_export(bundle.path(), &prepared).unwrap();
        assert_eq!(contract.root, "cli/claude/2.1.215/darwin-arm64");
        assert_eq!(contract.executable, "claude");
        assert_eq!(contract.platform_directory, "darwin-arm64");
    }
}
