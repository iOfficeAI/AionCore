use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Builder;

use super::{NodeRuntimeProgressReporter, ensure_node_runtime_with_reporter};

pub const DEEPSEEK_HARNESS_RUNTIME_ID: &str = "deepseek-harness";

const MANIFEST_JSON: &str = include_str!("../../resources/deepseek-harness/runtime-manifest.json");
const PACKAGE_JSON: &str = include_str!("../../resources/deepseek-harness/package.json");
const PACKAGE_LOCK_JSON: &str = include_str!("../../resources/deepseek-harness/package-lock.json");
const CORDIS_CONFIG: &str = include_str!("../../resources/deepseek-harness/cordis.yml");
const ACP_HANDSHAKE_FIXTURE: &str = include_str!("../../resources/deepseek-harness/acp-handshake.fixture.jsonl");
const THIRD_PARTY_LICENSES: &str = include_str!("../../resources/deepseek-harness/THIRD_PARTY_LICENSES.md");
const READY_MARKER: &str = ".aionui-runtime-ready.json";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedNpmAppManifest {
    pub schema_version: u32,
    pub runtime_id: String,
    pub release: String,
    pub upstream_commit: String,
    pub entry_package: String,
    pub entry_version: String,
    pub entry_path: PathBuf,
    pub config_path: PathBuf,
    pub fixture_path: PathBuf,
    pub license_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepseekHarnessRuntime {
    pub runtime_id: String,
    pub release: String,
    pub root: PathBuf,
    pub node_path: PathBuf,
    pub entry_path: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedNpmAppProgressPhase {
    WaitingForLock,
    Installing,
    Validating,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedNpmAppProgress {
    pub phase: ManagedNpmAppProgressPhase,
    pub message: Option<String>,
}

pub trait ManagedNpmAppProgressReporter: Send + Sync {
    fn report(&self, update: ManagedNpmAppProgress);
}

impl<F> ManagedNpmAppProgressReporter for F
where
    F: Fn(ManagedNpmAppProgress) + Send + Sync,
{
    fn report(&self, update: ManagedNpmAppProgress) {
        self(update);
    }
}

pub type SharedManagedNpmAppProgressReporter = Arc<dyn ManagedNpmAppProgressReporter>;

#[derive(Debug, thiserror::Error)]
pub enum ManagedNpmAppError {
    #[error("managed npm app manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("managed runtime root is unavailable")]
    RuntimeRootUnavailable,
    #[error("managed Node runtime is unavailable: {0}")]
    NodeRuntime(String),
    #[error("failed to prepare managed npm app files: {0}")]
    Io(#[from] std::io::Error),
    #[error("managed npm app install timed out")]
    Timeout,
    #[error("managed npm app install failed: {0}")]
    InstallFailed(String),
    #[error("managed npm app validation failed: {0}")]
    ValidationFailed(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadyMarker {
    runtime_id: String,
    release: String,
    package_lock_sha256: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddedPackageJson {
    dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct EmbeddedPackageLock {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    packages: BTreeMap<String, LockedPackage>,
}

#[derive(Debug, Deserialize)]
struct LockedPackage {
    version: Option<String>,
    resolved: Option<String>,
    integrity: Option<String>,
    dependencies: Option<BTreeMap<String, String>>,
}

static INSTALL_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

pub fn deepseek_harness_manifest() -> Result<ManagedNpmAppManifest, ManagedNpmAppError> {
    let manifest: ManagedNpmAppManifest =
        serde_json::from_str(MANIFEST_JSON).map_err(|error| ManagedNpmAppError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest)?;
    validate_embedded_package_lock(&manifest)?;
    Ok(manifest)
}

pub fn probe_deepseek_harness_runtime() -> Option<DeepseekHarnessRuntime> {
    let manifest = deepseek_harness_manifest().ok()?;
    probe_current_runtime(&manifest).or_else(|| probe_previous_runtime(&manifest))
}

pub fn probe_deepseek_harness_current_runtime() -> Option<DeepseekHarnessRuntime> {
    let manifest = deepseek_harness_manifest().ok()?;
    probe_current_runtime(&manifest)
}

fn probe_current_runtime(manifest: &ManagedNpmAppManifest) -> Option<DeepseekHarnessRuntime> {
    let root = release_root(manifest)?;
    validate_release(&root, manifest, None).ok()
}

pub async fn ensure_deepseek_harness_runtime(
    reporter: Option<&dyn ManagedNpmAppProgressReporter>,
    node_reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<DeepseekHarnessRuntime, ManagedNpmAppError> {
    let manifest = deepseek_harness_manifest()?;
    if let Some(runtime) = probe_current_runtime(&manifest) {
        emit(
            reporter,
            ManagedNpmAppProgressPhase::Ready,
            "DeepSeek Harness runtime is ready",
        );
        return Ok(runtime);
    }

    let in_process_lock = INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _in_process_guard = in_process_lock.lock().await;
    if let Some(runtime) = probe_current_runtime(&manifest) {
        emit(
            reporter,
            ManagedNpmAppProgressPhase::Ready,
            "DeepSeek Harness runtime is ready",
        );
        return Ok(runtime);
    }

    let app_root = app_root(&manifest).ok_or(ManagedNpmAppError::RuntimeRootUnavailable)?;
    let lock_path = app_root.join("install.lock");
    let pending_lock = InstallLockGuard::try_acquire(&lock_path)?;
    let cross_process_guard = match pending_lock {
        LockAttempt::Acquired(guard) => guard,
        LockAttempt::Contended(file) => {
            emit(
                reporter,
                ManagedNpmAppProgressPhase::WaitingForLock,
                "Waiting for another DeepSeek Harness installation",
            );
            tokio::task::spawn_blocking(move || InstallLockGuard::acquire_file(file))
                .await
                .map_err(|error| ManagedNpmAppError::InstallFailed(error.to_string()))??
        }
    };

    if let Some(runtime) = probe_current_runtime(&manifest) {
        drop(cross_process_guard);
        emit(
            reporter,
            ManagedNpmAppProgressPhase::Ready,
            "DeepSeek Harness runtime is ready",
        );
        return Ok(runtime);
    }

    let result = install_release(&manifest, &app_root, reporter, node_reporter).await;
    drop(cross_process_guard);
    match result {
        Ok(runtime) => {
            emit(
                reporter,
                ManagedNpmAppProgressPhase::Ready,
                "DeepSeek Harness runtime is ready",
            );
            Ok(runtime)
        }
        Err(error) => {
            emit(reporter, ManagedNpmAppProgressPhase::Failed, error.to_string());
            if let Some(previous) = probe_previous_runtime(&manifest) {
                tracing::warn!(
                    runtime_id = manifest.runtime_id,
                    failed_release = manifest.release,
                    rollback_release = previous.release,
                    "managed npm app activation failed; continuing with previous verified release"
                );
                Ok(previous)
            } else {
                Err(error)
            }
        }
    }
}

fn probe_previous_runtime(manifest: &ManagedNpmAppManifest) -> Option<DeepseekHarnessRuntime> {
    let root = app_root(manifest)?;
    let mut candidates = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().ok().is_some_and(|kind| kind.is_dir()))
        .filter(|entry| entry.file_name().to_string_lossy() != manifest.release)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    candidates
        .into_iter()
        .find_map(|entry| validate_previous_release(&entry.path(), manifest).ok())
}

fn validate_previous_release(
    root: &Path,
    manifest: &ManagedNpmAppManifest,
) -> Result<DeepseekHarnessRuntime, ManagedNpmAppError> {
    let previous_manifest: ManagedNpmAppManifest =
        serde_json::from_slice(&fs::read(root.join("runtime-manifest.json"))?)
            .map_err(|error| ManagedNpmAppError::ValidationFailed(error.to_string()))?;
    validate_manifest(&previous_manifest)?;
    let marker: ReadyMarker = serde_json::from_slice(&fs::read(root.join(READY_MARKER))?)
        .map_err(|error| ManagedNpmAppError::ValidationFailed(error.to_string()))?;
    let release_name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ManagedNpmAppError::ValidationFailed("previous release directory is invalid".into()))?;
    if marker.runtime_id != manifest.runtime_id
        || marker.runtime_id != previous_manifest.runtime_id
        || marker.release != release_name
        || marker.release != previous_manifest.release
    {
        return Err(ManagedNpmAppError::ValidationFailed(
            "previous release marker does not match directory".into(),
        ));
    }
    let lock = fs::read(root.join("package-lock.json"))?;
    if marker.package_lock_sha256 != hex::encode(Sha256::digest(lock)) {
        return Err(ManagedNpmAppError::ValidationFailed(
            "previous release lock checksum is invalid".into(),
        ));
    }
    let entry_path = root.join(&previous_manifest.entry_path);
    let config_path = root.join(&previous_manifest.config_path);
    if !entry_path.is_file()
        || !config_path.is_file()
        || !root.join(&previous_manifest.fixture_path).is_file()
        || !root.join(&previous_manifest.license_path).is_file()
    {
        return Err(ManagedNpmAppError::ValidationFailed(
            "previous release files are incomplete".into(),
        ));
    }
    let node_path = super::managed::probe_preferred_local_runtime()
        .map(|runtime| runtime.node_path)
        .ok_or_else(|| ManagedNpmAppError::ValidationFailed("managed Node runtime is unavailable".into()))?;
    Ok(DeepseekHarnessRuntime {
        runtime_id: marker.runtime_id,
        release: marker.release,
        root: root.to_path_buf(),
        node_path,
        entry_path,
        config_path,
    })
}

async fn install_release(
    manifest: &ManagedNpmAppManifest,
    app_root: &Path,
    reporter: Option<&dyn ManagedNpmAppProgressReporter>,
    node_reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<DeepseekHarnessRuntime, ManagedNpmAppError> {
    let node = ensure_node_runtime_with_reporter(node_reporter)
        .await
        .map_err(|error| ManagedNpmAppError::NodeRuntime(error.to_string()))?;
    fs::create_dir_all(app_root)?;
    let staging = app_root.join(format!(".staging-{}", std::process::id()));
    remove_contained_dir_if_exists(app_root, &staging)?;
    fs::create_dir_all(&staging)?;
    write_embedded_files(&staging)?;

    emit(
        reporter,
        ManagedNpmAppProgressPhase::Installing,
        format!("Installing DeepSeek Harness runtime {}", manifest.release),
    );
    tracing::info!(
        runtime_id = manifest.runtime_id,
        release = manifest.release,
        upstream_commit = manifest.upstream_commit,
        "managed npm app installation started"
    );

    let npm = node.npm_command();
    let mut command = Builder::clean_cli(&npm.program);
    command
        .args(&npm.args_prefix)
        .args(["ci", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"])
        .envs(npm.env)
        .current_dir(&staging);
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| ManagedNpmAppError::Timeout)??;
    if !output.status.success() {
        let stderr = bounded_text(&output.stderr, 4096);
        return Err(ManagedNpmAppError::InstallFailed(format!(
            "npm ci exited with status {:?}: {stderr}",
            output.status.code()
        )));
    }

    emit(
        reporter,
        ManagedNpmAppProgressPhase::Validating,
        "Validating DeepSeek Harness runtime",
    );
    let marker = ReadyMarker {
        runtime_id: manifest.runtime_id.clone(),
        release: manifest.release.clone(),
        package_lock_sha256: package_lock_sha256(),
    };
    fs::write(
        staging.join(READY_MARKER),
        serde_json::to_vec_pretty(&marker).map_err(|error| ManagedNpmAppError::InvalidManifest(error.to_string()))?,
    )?;
    validate_release(&staging, manifest, Some(&node.node_path))?;

    let release_root = app_root.join(&manifest.release);
    if release_root.exists() {
        let corrupt = app_root.join(format!(".corrupt-{}-{}", manifest.release, std::process::id()));
        remove_contained_dir_if_exists(app_root, &corrupt)?;
        fs::rename(&release_root, corrupt)?;
    }
    fs::rename(&staging, &release_root)?;
    let runtime = validate_release(&release_root, manifest, Some(&node.node_path))?;
    tracing::info!(
        runtime_id = manifest.runtime_id,
        release = manifest.release,
        root = %release_root.display(),
        "managed npm app installation activated"
    );
    Ok(runtime)
}

fn validate_manifest(manifest: &ManagedNpmAppManifest) -> Result<(), ManagedNpmAppError> {
    if manifest.schema_version != 1 {
        return Err(ManagedNpmAppError::InvalidManifest(format!(
            "unsupported schema_version {}",
            manifest.schema_version
        )));
    }
    if manifest.runtime_id != DEEPSEEK_HARNESS_RUNTIME_ID {
        return Err(ManagedNpmAppError::InvalidManifest("unexpected runtime_id".into()));
    }
    if manifest.release.is_empty()
        || !manifest
            .release
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(ManagedNpmAppError::InvalidManifest(
            "release contains unsafe characters".into(),
        ));
    }
    for path in [
        &manifest.entry_path,
        &manifest.config_path,
        &manifest.fixture_path,
        &manifest.license_path,
    ] {
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ManagedNpmAppError::InvalidManifest(format!(
                "path '{}' must stay relative",
                path.display()
            )));
        }
    }
    semver::Version::parse(&manifest.entry_version)
        .map_err(|error| ManagedNpmAppError::InvalidManifest(format!("invalid entry version: {error}")))?;
    Ok(())
}

fn validate_embedded_package_lock(manifest: &ManagedNpmAppManifest) -> Result<(), ManagedNpmAppError> {
    let package: EmbeddedPackageJson =
        serde_json::from_str(PACKAGE_JSON).map_err(|error| ManagedNpmAppError::InvalidManifest(error.to_string()))?;
    for (name, version) in &package.dependencies {
        semver::Version::parse(version).map_err(|error| {
            ManagedNpmAppError::InvalidManifest(format!("dependency '{name}' is not exact: {error}"))
        })?;
    }
    for line in CORDIS_CONFIG.lines().map(str::trim) {
        let Some(name) = line.strip_prefix("name: '").and_then(|value| value.strip_suffix('\'')) else {
            continue;
        };
        if !package.dependencies.contains_key(name) {
            return Err(ManagedNpmAppError::InvalidManifest(format!(
                "Cordis config references unlocked package '{name}'"
            )));
        }
    }
    if package.dependencies.get(&manifest.entry_package) != Some(&manifest.entry_version) {
        return Err(ManagedNpmAppError::InvalidManifest(
            "entry package version does not match package.json".into(),
        ));
    }

    let lock: EmbeddedPackageLock = serde_json::from_str(PACKAGE_LOCK_JSON)
        .map_err(|error| ManagedNpmAppError::InvalidManifest(error.to_string()))?;
    if lock.lockfile_version != 3 {
        return Err(ManagedNpmAppError::InvalidManifest(format!(
            "unsupported package lock version {}",
            lock.lockfile_version
        )));
    }
    let root = lock
        .packages
        .get("")
        .and_then(|entry| entry.dependencies.as_ref())
        .ok_or_else(|| ManagedNpmAppError::InvalidManifest("package lock has no root dependencies".into()))?;
    if root != &package.dependencies {
        return Err(ManagedNpmAppError::InvalidManifest(
            "package.json dependencies differ from package lock".into(),
        ));
    }
    for (path, entry) in lock.packages.iter().filter(|(path, _)| !path.is_empty()) {
        if entry.resolved.is_some() && entry.integrity.as_deref().unwrap_or_default().is_empty() {
            return Err(ManagedNpmAppError::InvalidManifest(format!(
                "locked package '{path}' has no integrity"
            )));
        }
        if let Some(version) = &entry.version {
            semver::Version::parse(version).map_err(|error| {
                ManagedNpmAppError::InvalidManifest(format!("locked package '{path}' has invalid version: {error}"))
            })?;
        }
    }
    Ok(())
}

fn validate_release(
    root: &Path,
    manifest: &ManagedNpmAppManifest,
    node_path: Option<&Path>,
) -> Result<DeepseekHarnessRuntime, ManagedNpmAppError> {
    let marker: ReadyMarker = serde_json::from_slice(
        &fs::read(root.join(READY_MARKER))
            .map_err(|error| ManagedNpmAppError::ValidationFailed(format!("ready marker is unavailable: {error}")))?,
    )
    .map_err(|error| ManagedNpmAppError::ValidationFailed(format!("ready marker is invalid: {error}")))?;
    if marker.runtime_id != manifest.runtime_id
        || marker.release != manifest.release
        || marker.package_lock_sha256 != package_lock_sha256()
    {
        return Err(ManagedNpmAppError::ValidationFailed(
            "ready marker does not match release lock".into(),
        ));
    }
    let installed_lock = fs::read(root.join("package-lock.json"))
        .map_err(|error| ManagedNpmAppError::ValidationFailed(format!("installed lock is unavailable: {error}")))?;
    if hex::encode(Sha256::digest(installed_lock)) != marker.package_lock_sha256 {
        return Err(ManagedNpmAppError::ValidationFailed(
            "installed package lock checksum is invalid".into(),
        ));
    }
    let entry_path = root.join(&manifest.entry_path);
    let config_path = root.join(&manifest.config_path);
    let fixture_path = root.join(&manifest.fixture_path);
    let license_path = root.join(&manifest.license_path);
    for required in [
        &entry_path,
        &config_path,
        &fixture_path,
        &license_path,
        &root.join("package-lock.json"),
        &root.join("runtime-manifest.json"),
    ] {
        if !required.is_file() {
            return Err(ManagedNpmAppError::ValidationFailed(format!(
                "required runtime file '{}' is missing",
                required.display()
            )));
        }
    }
    let installed_manifest: ManagedNpmAppManifest =
        serde_json::from_slice(&fs::read(root.join("runtime-manifest.json"))?).map_err(|error| {
            ManagedNpmAppError::ValidationFailed(format!("installed runtime manifest is invalid: {error}"))
        })?;
    if &installed_manifest != manifest {
        return Err(ManagedNpmAppError::ValidationFailed(
            "installed runtime manifest does not match the embedded release".into(),
        ));
    }
    let package: EmbeddedPackageJson = serde_json::from_str(PACKAGE_JSON).map_err(|error| {
        ManagedNpmAppError::ValidationFailed(format!("embedded package manifest is invalid: {error}"))
    })?;
    for (name, expected_version) in package.dependencies {
        let installed_manifest = root.join("node_modules").join(&name).join("package.json");
        let installed: serde_json::Value = serde_json::from_slice(&fs::read(&installed_manifest).map_err(|error| {
            ManagedNpmAppError::ValidationFailed(format!("installed package '{name}' is unavailable: {error}"))
        })?)
        .map_err(|error| {
            ManagedNpmAppError::ValidationFailed(format!("installed package '{name}' manifest is invalid: {error}"))
        })?;
        if installed.get("version").and_then(serde_json::Value::as_str) != Some(expected_version.as_str()) {
            return Err(ManagedNpmAppError::ValidationFailed(format!(
                "installed package '{name}' version does not match the release lock"
            )));
        }
    }
    let node_path = match node_path {
        Some(path) => path.to_path_buf(),
        None => super::managed::probe_preferred_local_runtime()
            .map(|runtime| runtime.node_path)
            .ok_or_else(|| ManagedNpmAppError::ValidationFailed("managed Node runtime is unavailable".into()))?,
    };
    Ok(DeepseekHarnessRuntime {
        runtime_id: manifest.runtime_id.clone(),
        release: manifest.release.clone(),
        root: root.to_path_buf(),
        node_path,
        entry_path,
        config_path,
    })
}

fn write_embedded_files(root: &Path) -> Result<(), std::io::Error> {
    fs::write(root.join("runtime-manifest.json"), MANIFEST_JSON)?;
    fs::write(root.join("package.json"), PACKAGE_JSON)?;
    fs::write(root.join("package-lock.json"), PACKAGE_LOCK_JSON)?;
    fs::write(root.join("cordis.yml"), CORDIS_CONFIG)?;
    fs::write(root.join("acp-handshake.fixture.jsonl"), ACP_HANDSHAKE_FIXTURE)?;
    fs::write(root.join("THIRD_PARTY_LICENSES.md"), THIRD_PARTY_LICENSES)?;
    Ok(())
}

fn package_lock_sha256() -> String {
    hex::encode(Sha256::digest(PACKAGE_LOCK_JSON.as_bytes()))
}

fn app_root(manifest: &ManagedNpmAppManifest) -> Option<PathBuf> {
    crate::cache::runtime_root().map(|root| root.join(&manifest.runtime_id))
}

fn release_root(manifest: &ManagedNpmAppManifest) -> Option<PathBuf> {
    app_root(manifest).map(|root| root.join(&manifest.release))
}

fn remove_contained_dir_if_exists(parent: &Path, target: &Path) -> Result<(), std::io::Error> {
    if !target.starts_with(parent) || target == parent {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed npm app cleanup target escapes its root",
        ));
    }
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    Ok(())
}

fn bounded_text(bytes: &[u8], max_bytes: usize) -> String {
    let start = bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

fn emit(
    reporter: Option<&dyn ManagedNpmAppProgressReporter>,
    phase: ManagedNpmAppProgressPhase,
    message: impl Into<String>,
) {
    if let Some(reporter) = reporter {
        reporter.report(ManagedNpmAppProgress {
            phase,
            message: Some(message.into()),
        });
    }
}

struct InstallLockGuard {
    file: File,
}

enum LockAttempt {
    Acquired(InstallLockGuard),
    Contended(File),
}

impl InstallLockGuard {
    fn try_acquire(path: &Path) -> std::io::Result<LockAttempt> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(LockAttempt::Acquired(Self { file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(LockAttempt::Contended(file)),
            Err(error) => Err(error),
        }
    }

    fn acquire_file(file: File) -> std::io::Result<Self> {
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for InstallLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_release_contract_is_exact_and_integrity_locked() {
        let manifest = deepseek_harness_manifest().unwrap();
        assert_eq!(manifest.runtime_id, DEEPSEEK_HARNESS_RUNTIME_ID);
        assert_eq!(manifest.entry_version, "0.0.1-rc.5");
    }

    #[test]
    fn handshake_fixture_keeps_preview_capabilities_conservative() {
        let frames = ACP_HANDSHAKE_FIXTURE
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["result"]["protocolVersion"], 1);
        assert_eq!(
            frames[0]["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            false
        );
        assert!(frames[0]["result"].get("mcpCapabilities").is_none());
        assert_eq!(frames[1]["result"]["sessionId"], "<normalized-session-id>");
    }

    #[test]
    fn containment_guard_rejects_runtime_root() {
        let root = tempfile::tempdir().unwrap();
        let error = remove_contained_dir_if_exists(root.path(), root.path()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn containment_guard_removes_only_a_child() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        fs::create_dir_all(&child).unwrap();
        remove_contained_dir_if_exists(root.path(), &child).unwrap();
        assert!(!child.exists());
    }

    #[test]
    fn bounded_install_error_keeps_only_the_tail() {
        assert_eq!(bounded_text(b"0123456789", 4), "6789");
    }
}
