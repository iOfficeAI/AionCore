use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourcesMode {
    Bundled,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourceSourceKind {
    Bundled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedResourceSource {
    pub kind: ManagedResourceSourceKind,
    pub root: PathBuf,
}

const BUNDLED_RESOURCES_ENV: &str = "AIONUI_BUNDLED_MANAGED_RESOURCES";

pub fn set_managed_resources_mode(mode: ManagedResourcesMode) {
    *mode_lock().write().expect("managed resources mode lock poisoned") = mode;
}

pub fn managed_resources_mode() -> ManagedResourcesMode {
    *mode_lock().read().expect("managed resources mode lock poisoned")
}

pub fn bundled_root_path() -> Option<PathBuf> {
    bundled_root().filter(|root| root.is_dir())
}

pub fn bundled_root_candidate() -> Option<PathBuf> {
    bundled_root()
}

pub fn requires_bundled_resources() -> bool {
    matches!(managed_resources_mode(), ManagedResourcesMode::Bundled)
}

pub fn node_sources(directory_name: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("node").join(directory_name),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn acp_tool_sources(tool_slug: &str, version: &str, platform_key: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("acp").join(tool_slug).join(version).join(platform_key),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn export_node_runtime_to_root(root: &Path, source_root: &Path, directory_name: &str) -> std::io::Result<PathBuf> {
    let target = root.join("node").join(directory_name);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn export_acp_tool_to_root(
    root: &Path,
    source_root: &Path,
    tool_slug: &str,
    version: &str,
    platform_key: &str,
) -> std::io::Result<PathBuf> {
    let target = root.join("acp").join(tool_slug).join(version).join(platform_key);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn materialize_directory(source_root: &Path, target_root: &Path) -> std::io::Result<()> {
    if !source_root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("managed resource source missing: {}", source_root.display()),
        ));
    }

    if source_root == target_root {
        return Ok(());
    }
    if let (Ok(source), Ok(target)) = (fs::canonicalize(source_root), fs::canonicalize(target_root))
        && source == target
    {
        return Ok(());
    }

    if target_root.exists() {
        fs::remove_dir_all(target_root)?;
    }
    fs::create_dir_all(target_root)?;

    for entry in WalkDir::new(source_root) {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .expect("walkdir path should stay under source root");

        if relative.as_os_str().is_empty() {
            continue;
        }

        let target_path = target_root.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target_path)?;
            copy_permissions(entry.path(), &target_path)?;
            continue;
        }

        if entry.file_type().is_symlink() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_symlink(entry.path(), &target_path)?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &target_path)?;
        copy_permissions(entry.path(), &target_path)?;
    }

    Ok(())
}

fn resource_roots() -> Vec<ManagedResourceSource> {
    let mut roots = Vec::new();

    match managed_resources_mode() {
        ManagedResourcesMode::Bundled => {
            if let Some(root) = bundled_root()
                && root.is_dir()
            {
                roots.push(ManagedResourceSource {
                    kind: ManagedResourceSourceKind::Bundled,
                    root,
                });
            }
        }
        ManagedResourcesMode::Download => {}
    }

    roots
}

fn mode_lock() -> &'static RwLock<ManagedResourcesMode> {
    static MODE: OnceLock<RwLock<ManagedResourcesMode>> = OnceLock::new();
    MODE.get_or_init(|| RwLock::new(default_managed_resources_mode()))
}

fn default_managed_resources_mode() -> ManagedResourcesMode {
    ManagedResourcesMode::Download
}

fn bundled_root() -> Option<PathBuf> {
    configured_root(BUNDLED_RESOURCES_ENV).or_else(default_bundled_root)
}

fn configured_root(env_key: &str) -> Option<PathBuf> {
    std::env::var_os(env_key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn default_bundled_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = fs::canonicalize(exe).ok()?.parent()?.to_path_buf();
    Some(exe_dir.join("managed-resources"))
}

fn copy_permissions(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(source)?;
    fs::set_permissions(target, metadata.permissions())
}

fn copy_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let link_target = fs::read_link(source)?;
    if target.exists() {
        fs::remove_file(target)?;
    }
    create_symlink(&link_target, target, source)
}

#[cfg(unix)]
fn create_symlink(link_target: &Path, target: &Path, _source: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link_target, target)
}

#[cfg(windows)]
fn create_symlink(link_target: &Path, target: &Path, source: &Path) -> std::io::Result<()> {
    let file_type = fs::metadata(source)?;
    if file_type.is_dir() {
        std::os::windows::fs::symlink_dir(link_target, target)
    } else {
        std::os::windows::fs::symlink_file(link_target, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn default_mode_is_download() {
        let _guard = env_lock().lock().expect("lock env");
        assert_eq!(default_managed_resources_mode(), ManagedResourcesMode::Download);
    }

    #[test]
    fn bundled_mode_uses_configured_bundled_root() {
        let _guard = env_lock().lock().expect("lock env");
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        fs::create_dir_all(root.join("node").join("node-v24.11.0-darwin-arm64")).expect("create node dir");

        unsafe {
            std::env::set_var(BUNDLED_RESOURCES_ENV, &root);
        }
        set_managed_resources_mode(ManagedResourcesMode::Bundled);

        let sources = node_sources("node-v24.11.0-darwin-arm64");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, ManagedResourceSourceKind::Bundled);
        assert_eq!(sources[0].root, root.join("node").join("node-v24.11.0-darwin-arm64"));

        unsafe {
            std::env::remove_var(BUNDLED_RESOURCES_ENV);
        }
        set_managed_resources_mode(ManagedResourcesMode::Download);
    }

    #[test]
    fn download_mode_ignores_configured_bundled_root() {
        let _guard = env_lock().lock().expect("lock env");
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        fs::create_dir_all(root.join("node").join("node-v24.11.0-darwin-arm64")).expect("create node dir");

        unsafe {
            std::env::set_var(BUNDLED_RESOURCES_ENV, &root);
        }
        set_managed_resources_mode(ManagedResourcesMode::Download);

        let sources = node_sources("node-v24.11.0-darwin-arm64");
        assert!(sources.is_empty());
        assert!(!requires_bundled_resources());

        unsafe {
            std::env::remove_var(BUNDLED_RESOURCES_ENV);
        }
        set_managed_resources_mode(ManagedResourcesMode::Download);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_directory_preserves_symlink_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-node");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::create_dir_all(source.join("lib").join("node_modules").join("npm").join("bin")).expect("create npm bin");
        fs::write(
            source
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
            b"#!/usr/bin/env node\n",
        )
        .expect("write npm cli");
        std::os::unix::fs::symlink(
            Path::new("../lib/node_modules/npm/bin/npm-cli.js"),
            source.join("bin").join("npm"),
        )
        .expect("create symlink");

        let target = temp.path().join("target-node");
        materialize_directory(&source, &target).expect("materialize");

        let copied_link = target.join("bin").join("npm");
        let metadata = fs::symlink_metadata(&copied_link).expect("metadata");
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            fs::read_link(&copied_link).expect("read link"),
            PathBuf::from("../lib/node_modules/npm/bin/npm-cli.js")
        );
    }
}
