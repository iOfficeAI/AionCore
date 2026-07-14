use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::cache;
use crate::http_client;
use crate::managed_resources::{self, ManagedResourceSourceKind};
use crate::managed_resources_contract::{ManagedNodeResourceContract, relative_contract_path};

use super::types::{
    NodeRuntimeError, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressReporter, NodeRuntimeSupport,
    ResolvedNodeRuntime, ResolvedNodeSource,
};

const MANAGED_NODE_VERSION: &str = "24.11.0";
const MANAGED_NODE_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MANAGED_NODE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const MANAGED_NODE_DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MANAGED_NODE_DOWNLOAD_ATTEMPTS: usize = 2;
const MANAGED_NODE_PROGRESS_STEP_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct PlatformSpec {
    folder_suffix: &'static str,
    archive_ext: &'static str,
    runtime_key: &'static str,
    executable: &'static str,
}

impl PlatformSpec {
    fn directory_name(self) -> String {
        format!("node-v{MANAGED_NODE_VERSION}-{}", self.folder_suffix)
    }

    fn official_download_url(self) -> String {
        format!(
            "https://nodejs.org/dist/v{version}/{name}.{ext}",
            version = MANAGED_NODE_VERSION,
            name = self.directory_name(),
            ext = self.archive_ext
        )
    }
}

#[derive(Debug, Clone)]
struct ManagedNodeDownloadSource {
    url: String,
    sha256: Option<String>,
    source: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedNodeArchiveLayout {
    Windows,
    Unix,
}

pub fn probe_support() -> NodeRuntimeSupport {
    match platform_spec() {
        Ok(spec) => NodeRuntimeSupport {
            supported: true,
            detail: format!("managed node runtime supported ({})", spec.folder_suffix),
        },
        Err(error) => NodeRuntimeSupport {
            supported: false,
            detail: error.to_string(),
        },
    }
}

pub(crate) fn probe_preferred_local_runtime() -> Option<ResolvedNodeRuntime> {
    let spec = platform_spec().ok()?;
    let source = managed_resources::node_sources(&spec.directory_name())
        .into_iter()
        .next()?;
    let runtime = probe_runtime_root(&source.root, map_source_kind(source.kind)).ok()?;
    Some(runtime)
}

pub async fn install_and_validate() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    install_and_validate_with_reporter(None).await
}

pub async fn install_and_validate_with_reporter(
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let spec = platform_spec().inspect_err(|error| {
        emit_progress(
            reporter,
            NodeRuntimeProgress::failed(NodeRuntimeFailureKind::UnsupportedPlatform, error.to_string()),
        );
    })?;
    let runtime_root = cache::node_runtime_root()
        .ok_or_else(|| NodeRuntimeError::managed_invalid("managed node runtime root unavailable"))?;
    fs::create_dir_all(&runtime_root).map_err(NodeRuntimeError::io_system)?;
    let _lock =
        InstallLockGuard::acquire(&install_lock_path(&runtime_root), reporter).map_err(NodeRuntimeError::io_system)?;

    let version_dir = runtime_root.join(spec.directory_name());
    match validate_managed_runtime(&version_dir, None).await {
        Ok(runtime) => return Ok(runtime),
        Err(error) => {
            warn!(
                error = %error,
                root = %version_dir.display(),
                "managed node runtime validation failed before install"
            );
        }
    }

    if let Some(runtime) = activate_local_runtime_source(&runtime_root, spec, reporter)
        .await
        .map_err(|error| install_error(error, reporter))?
    {
        emit_progress(
            reporter,
            NodeRuntimeProgress::ready(format!(
                "{} Node runtime {} is ready",
                source_label(runtime.source),
                runtime.version
            )),
        );
        info!(
            version = %runtime.version,
            root = %runtime.root.display(),
            source = source_label(runtime.source),
            "managed node runtime activated from local resources"
        );
        return Ok(runtime);
    }

    info!(
        version = MANAGED_NODE_VERSION,
        root = %runtime_root.display(),
        url = %spec.official_download_url(),
        "managed node runtime install started"
    );
    install_archive_with_retry(&runtime_root, spec, reporter).await?;
    match validate_managed_runtime(&version_dir, reporter).await {
        Ok(runtime) => {
            emit_progress(
                reporter,
                NodeRuntimeProgress::ready(format!("managed Node runtime {} is ready", runtime.version)),
            );
            info!(
                version = %runtime.version,
                root = %runtime.root.display(),
                "managed node runtime install completed"
            );
            Ok(runtime)
        }
        Err(first_error) => {
            warn!(
                error = %first_error,
                root = %version_dir.display(),
                "managed node runtime validation failed after install; retrying"
            );
            let _ = fs::remove_dir_all(&version_dir);
            install_archive_with_retry(&runtime_root, spec, reporter).await?;
            validate_managed_runtime(&version_dir, reporter)
                .await
                .inspect(|runtime| {
                    emit_progress(
                        reporter,
                        NodeRuntimeProgress::ready(format!("managed Node runtime {} is ready", runtime.version)),
                    );
                    info!(
                        version = %runtime.version,
                        root = %runtime.root.display(),
                        "managed node runtime install completed"
                    );
                })
                .map_err(|retry_error| combined_retry_error(first_error, retry_error, reporter))
        }
    }
}

pub(crate) async fn validate_managed_runtime(
    root: &Path,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    emit_progress(
        reporter,
        NodeRuntimeProgress::validating(format!("validating managed Node runtime under {}", root.display())),
    );
    let runtime = runtime_from_root(root, ResolvedNodeSource::Managed)?;
    super::validate_runtime(runtime, None)
        .await
        .map_err(|error| validation_error(error, reporter))
}

fn platform_spec() -> Result<PlatformSpec, NodeRuntimeError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "darwin-arm64",
            archive_ext: "tar.gz",
            runtime_key: "darwin-arm64",
            executable: "bin/node",
        }),
        ("macos", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "darwin-x64",
            archive_ext: "tar.gz",
            runtime_key: "darwin-x64",
            executable: "bin/node",
        }),
        ("linux", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "linux-arm64",
            archive_ext: "tar.gz",
            runtime_key: "linux-arm64",
            executable: "bin/node",
        }),
        ("linux", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "linux-x64",
            archive_ext: "tar.gz",
            runtime_key: "linux-x64",
            executable: "bin/node",
        }),
        ("windows", "x86_64") => Ok(PlatformSpec {
            folder_suffix: "win-x64",
            archive_ext: "zip",
            runtime_key: "win32-x64",
            executable: "node.exe",
        }),
        ("windows", "aarch64") => Ok(PlatformSpec {
            folder_suffix: "win-arm64",
            archive_ext: "zip",
            runtime_key: "win32-arm64",
            executable: "node.exe",
        }),
        (os, arch) => Err(NodeRuntimeError::unsupported_platform(format!(
            "managed node runtime unsupported on {os}/{arch}"
        ))),
    }
}

pub fn managed_node_contract_for_export(
    bundle_root: &Path,
    exported_node_root: &Path,
) -> Result<ManagedNodeResourceContract, NodeRuntimeError> {
    managed_node_contract_for_export_with_spec(bundle_root, exported_node_root, platform_spec()?)
}

#[cfg_attr(not(test), allow(dead_code))]
fn managed_node_contract_for_export_with_spec(
    bundle_root: &Path,
    exported_node_root: &Path,
    spec: PlatformSpec,
) -> Result<ManagedNodeResourceContract, NodeRuntimeError> {
    let root = relative_contract_path(bundle_root, exported_node_root)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("managed Node contract path: {error}")))?;
    let expected_suffix = spec.directory_name();
    if !root.ends_with(&expected_suffix) {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "exported managed Node root {} does not match expected {} for {}",
            exported_node_root.display(),
            expected_suffix,
            spec.runtime_key
        )));
    }
    Ok(ManagedNodeResourceContract {
        version: MANAGED_NODE_VERSION.into(),
        root,
        executable: spec.executable.into(),
    })
}

fn runtime_from_root(root: &Path, source: ResolvedNodeSource) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    runtime_from_root_for_layout(root, source, current_managed_node_archive_layout())
}

fn runtime_from_root_for_layout(
    root: &Path,
    source: ResolvedNodeSource,
    layout: ManagedNodeArchiveLayout,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    if !root.is_dir() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node runtime directory missing: {}",
            root.display()
        )));
    }

    prepare_runtime_files(root)?;

    let node_path = managed_node_path_for_layout(root, layout);
    if !node_path.is_file() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node executable missing: {}",
            node_path.display()
        )));
    }

    let (npm_path, npm_args_prefix) = resolve_managed_entrypoint(root, &node_path, layout, "npm")?;
    let (npx_path, npx_args_prefix) = resolve_managed_entrypoint(root, &node_path, layout, "npx")?;

    Ok(ResolvedNodeRuntime {
        source,
        root: root.to_path_buf(),
        version: semver::Version::new(0, 0, 0),
        node_path,
        npm_path,
        npm_args_prefix,
        npx_path,
        npx_args_prefix,
        env: managed_env(root)?,
    })
}

fn resolve_managed_entrypoint(
    root: &Path,
    node_path: &Path,
    layout: ManagedNodeArchiveLayout,
    tool: &str,
) -> Result<(PathBuf, Vec<OsString>), NodeRuntimeError> {
    let cli = match tool {
        "npm" => managed_npm_cli_path_for_layout(root, layout),
        "npx" => managed_npx_cli_path_for_layout(root, layout),
        _ => unreachable!("managed Node only resolves npm and npx entrypoints"),
    };
    let wrapper = managed_wrapper_path_for_layout(root, layout, tool);

    match layout {
        ManagedNodeArchiveLayout::Windows => {
            if cli.is_file() {
                return Ok((node_path.to_path_buf(), vec![cli.into_os_string()]));
            }
            if wrapper.is_file() {
                return Ok((wrapper, vec![]));
            }
        }
        ManagedNodeArchiveLayout::Unix => {
            if wrapper.is_file() {
                return Ok((wrapper, vec![]));
            }
            if cli.is_file() {
                return Ok((node_path.to_path_buf(), vec![cli.into_os_string()]));
            }
        }
    }

    Err(NodeRuntimeError::managed_invalid(format!(
        "managed {tool} entrypoint missing under {}",
        root.display()
    )))
}

fn probe_runtime_root(root: &Path, source: ResolvedNodeSource) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    if !root.is_dir() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node runtime directory missing: {}",
            root.display()
        )));
    }

    let layout = current_managed_node_archive_layout();
    let node_path = managed_node_path_for_layout(root, layout);
    if !node_path.is_file() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node runtime is incomplete under {}",
            root.display()
        )));
    }

    let (npm_path, npm_args_prefix) = resolve_managed_entrypoint(root, &node_path, layout, "npm")?;
    let (npx_path, npx_args_prefix) = resolve_managed_entrypoint(root, &node_path, layout, "npx")?;

    Ok(ResolvedNodeRuntime {
        source,
        root: root.to_path_buf(),
        version: semver::Version::new(0, 0, 0),
        node_path,
        npm_path,
        npm_args_prefix,
        npx_path,
        npx_args_prefix,
        env: vec![],
    })
}

async fn activate_local_runtime_source(
    runtime_root: &Path,
    spec: PlatformSpec,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<Option<ResolvedNodeRuntime>, NodeRuntimeError> {
    let version_dir = runtime_root.join(spec.directory_name());
    if managed_resources::requires_bundled_resources() {
        let bundled_root = managed_resources::bundled_root_candidate()
            .ok_or_else(|| NodeRuntimeError::managed_invalid("bundled managed resources root unavailable"))?;
        let bundled_runtime = bundled_root.join("node").join(spec.directory_name());
        if !bundled_runtime.is_dir() {
            return Err(NodeRuntimeError::managed_invalid(format!(
                "bundled Node runtime missing under {}",
                bundled_runtime.display()
            )));
        }
    }

    for source in managed_resources::node_sources(&spec.directory_name()) {
        emit_progress(
            reporter,
            NodeRuntimeProgress::extracting(format!(
                "activating {} Node runtime from {}",
                source_kind_label(source.kind),
                source.root.display()
            )),
        );

        if let Err(error) = managed_resources::materialize_directory(&source.root, &version_dir) {
            warn!(
                source = source_kind_label(source.kind),
                source_root = %source.root.display(),
                target_root = %version_dir.display(),
                error = %error,
                "failed to activate local node runtime source"
            );
            if matches!(source.kind, ManagedResourceSourceKind::Bundled) {
                return Err(NodeRuntimeError::managed_invalid(format!(
                    "bundled Node runtime is invalid under {}: {}",
                    source.root.display(),
                    error
                )));
            }
            continue;
        }

        match validate_managed_runtime(&version_dir, reporter).await {
            Ok(mut runtime) => {
                runtime.source = map_source_kind(source.kind);
                return Ok(Some(runtime));
            }
            Err(error) => {
                warn!(
                    source = source_kind_label(source.kind),
                    source_root = %source.root.display(),
                    target_root = %version_dir.display(),
                    error = %error,
                    "local node runtime source failed validation"
                );
                let _ = fs::remove_dir_all(&version_dir);
                if matches!(source.kind, ManagedResourceSourceKind::Bundled) {
                    return Err(NodeRuntimeError::managed_invalid(format!(
                        "bundled Node runtime failed validation under {}: {}",
                        source.root.display(),
                        error
                    )));
                }
            }
        }
    }

    Ok(None)
}

fn source_label(source: ResolvedNodeSource) -> &'static str {
    match source {
        ResolvedNodeSource::Bundled => "bundled",
        ResolvedNodeSource::Managed => "managed",
    }
}

fn source_kind_label(kind: ManagedResourceSourceKind) -> &'static str {
    match kind {
        ManagedResourceSourceKind::Bundled => "bundled",
    }
}

fn map_source_kind(kind: ManagedResourceSourceKind) -> ResolvedNodeSource {
    match kind {
        ManagedResourceSourceKind::Bundled => ResolvedNodeSource::Bundled,
    }
}

struct InstallLockGuard {
    file: fs::File,
}

impl InstallLockGuard {
    fn acquire(path: &Path, reporter: Option<&dyn NodeRuntimeProgressReporter>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            emit_progress(
                reporter,
                NodeRuntimeProgress::waiting_for_lock("waiting for another process to finish preparing managed Node"),
            );
            FileExt::lock_exclusive(&file)?;
        }
        Ok(Self { file })
    }
}

impl Drop for InstallLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

async fn install_archive(
    runtime_root: &Path,
    spec: PlatformSpec,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<(), NodeRuntimeError> {
    let client = build_http_client()?;
    let download_source = ManagedNodeDownloadSource::official(spec);
    let url = download_source.url.clone();
    let version_dir = runtime_root.join(spec.directory_name());
    let archive_path = archive_download_path(runtime_root, spec);
    if version_dir.exists() {
        let _ = fs::remove_dir_all(&version_dir);
    }
    if archive_path.exists() {
        let _ = fs::remove_file(&archive_path);
    }

    emit_progress(
        reporter,
        NodeRuntimeProgress::downloading(format!("downloading managed Node runtime from {url}")),
    );

    info!(
        version = MANAGED_NODE_VERSION,
        platform = spec.folder_suffix,
        source = download_source.source,
        url = %url,
        "managed node runtime download source selected"
    );

    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| reqwest_error("download archive", &url, &error))?;
    let response = response
        .error_for_status()
        .map_err(|error| reqwest_error("download archive", &url, &error))?;
    stream_archive_to_file(response, &archive_path, &url, reporter).await?;
    if let Some(expected_sha256) = download_source.sha256.as_deref() {
        emit_progress(
            reporter,
            NodeRuntimeProgress::validating("verifying managed Node artifact checksum".to_owned()),
        );
        verify_archive_checksum(&archive_path, expected_sha256)?;
    }

    emit_progress(
        reporter,
        NodeRuntimeProgress::extracting(format!(
            "extracting managed Node runtime into {}",
            runtime_root.display()
        )),
    );
    match spec.archive_ext {
        "tar.gz" => extract_tar_gz(&archive_path, runtime_root)?,
        "zip" => extract_zip(&archive_path, runtime_root)?,
        ext => {
            return Err(NodeRuntimeError::managed_invalid(format!(
                "unsupported archive extension: {ext}"
            )));
        }
    }
    let _ = fs::remove_file(&archive_path);

    Ok(())
}

fn build_http_client() -> Result<reqwest::Client, NodeRuntimeError> {
    http_client::build_http_client(MANAGED_NODE_CONNECT_TIMEOUT, MANAGED_NODE_DOWNLOAD_TIMEOUT)
        .map_err(NodeRuntimeError::managed_invalid)
}

fn verify_archive_checksum(path: &Path, expected_sha256: &str) -> Result<(), NodeRuntimeError> {
    let bytes = fs::read(path).map_err(NodeRuntimeError::io_system)?;
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "managed node archive checksum mismatch for {}: expected {expected_sha256}, got {actual}",
            path.display()
        )));
    }
    Ok(())
}

impl ManagedNodeDownloadSource {
    fn official(spec: PlatformSpec) -> Self {
        Self {
            url: spec.official_download_url(),
            sha256: None,
            source: "nodejs.org",
        }
    }
}

async fn install_archive_with_retry(
    runtime_root: &Path,
    spec: PlatformSpec,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<(), NodeRuntimeError> {
    let mut last_error = None;
    for attempt in 1..=MANAGED_NODE_DOWNLOAD_ATTEMPTS {
        match install_archive(runtime_root, spec, reporter).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt < MANAGED_NODE_DOWNLOAD_ATTEMPTS => {
                warn!(
                    attempt,
                    max_attempts = MANAGED_NODE_DOWNLOAD_ATTEMPTS,
                    error = %error,
                    root = %runtime_root.display(),
                    "managed node runtime install attempt failed; retrying"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(install_error(error, reporter)),
        }
    }

    Err(last_error
        .map(|error| install_error(error, reporter))
        .unwrap_or_else(|| NodeRuntimeError::managed_invalid("managed node runtime install failed")))
}

fn archive_download_path(runtime_root: &Path, spec: PlatformSpec) -> PathBuf {
    runtime_root.join(format!("{}.download", spec.directory_name()))
}

async fn stream_archive_to_file(
    mut response: reqwest::Response,
    archive_path: &Path,
    url: &str,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<(), NodeRuntimeError> {
    let mut writer = fs::File::create(archive_path).map_err(NodeRuntimeError::io_system)?;
    let total_bytes = response.content_length();
    let mut downloaded_bytes = 0_u64;
    let mut next_report_threshold = MANAGED_NODE_PROGRESS_STEP_BYTES;

    loop {
        let chunk = tokio::time::timeout(MANAGED_NODE_DOWNLOAD_IDLE_TIMEOUT, response.chunk())
            .await
            .map_err(|_| timeout_error("read archive body", url, MANAGED_NODE_DOWNLOAD_IDLE_TIMEOUT))?
            .map_err(|error| reqwest_error("read archive body", url, &error))?;
        let Some(chunk) = chunk else {
            break;
        };

        writer.write_all(&chunk).map_err(NodeRuntimeError::io_system)?;
        downloaded_bytes += chunk.len() as u64;

        if downloaded_bytes == chunk.len() as u64 || downloaded_bytes >= next_report_threshold {
            emit_progress(
                reporter,
                NodeRuntimeProgress::downloading(download_progress_message(url, downloaded_bytes, total_bytes)),
            );
            while downloaded_bytes >= next_report_threshold {
                next_report_threshold += MANAGED_NODE_PROGRESS_STEP_BYTES;
            }
        }
    }

    writer.flush().map_err(NodeRuntimeError::io_system)?;
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, runtime_root: &Path) -> Result<(), NodeRuntimeError> {
    let archive_file = fs::File::open(archive_path).map_err(NodeRuntimeError::io_system)?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(runtime_root)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("extract tar.gz failed: {error}")))
}

fn extract_zip(archive_path: &Path, runtime_root: &Path) -> Result<(), NodeRuntimeError> {
    let archive_file = fs::File::open(archive_path).map_err(NodeRuntimeError::io_system)?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("open zip failed: {error}")))?;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| NodeRuntimeError::managed_invalid(format!("read zip entry failed: {error}")))?;
        let Some(relative_path) = file.enclosed_name().map(|path| path.to_path_buf()) else {
            continue;
        };
        let output_path = runtime_root.join(relative_path);
        if file.is_dir() {
            fs::create_dir_all(&output_path).map_err(NodeRuntimeError::io_system)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(NodeRuntimeError::io_system)?;
        }

        let mut writer = fs::File::create(&output_path).map_err(NodeRuntimeError::io_system)?;
        std::io::copy(&mut file, &mut writer).map_err(NodeRuntimeError::io_system)?;
        writer.flush().map_err(NodeRuntimeError::io_system)?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = writer.metadata().map_err(NodeRuntimeError::io_system)?.permissions();
            perms.set_mode(mode);
            fs::set_permissions(&output_path, perms).map_err(NodeRuntimeError::io_system)?;
        }
    }

    Ok(())
}

fn prepare_runtime_files(root: &Path) -> Result<(), NodeRuntimeError> {
    fs::create_dir_all(root.join("cache")).map_err(NodeRuntimeError::io_system)?;
    fs::create_dir_all(default_npm_prefix(root)).map_err(NodeRuntimeError::io_system)?;
    if !cfg!(windows) {
        fs::create_dir_all(default_npm_prefix(root).join("bin")).map_err(NodeRuntimeError::io_system)?;
    }
    fs::write(root.join("blank_user_npmrc"), []).map_err(NodeRuntimeError::io_system)?;
    fs::write(root.join("blank_global_npmrc"), []).map_err(NodeRuntimeError::io_system)?;
    Ok(())
}

fn managed_env(root: &Path) -> Result<Vec<(OsString, OsString)>, NodeRuntimeError> {
    let node_bin = managed_bin_dir(root);
    let global_bin = managed_prefix_bin_dir(root);
    let mut paths = vec![node_bin, global_bin];
    if let Some(current_path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current_path));
    }
    let path = std::env::join_paths(paths)
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("failed to build PATH: {error}")))?;

    Ok(vec![
        ("PATH".into(), path),
        ("npm_config_cache".into(), root.join("cache").into_os_string()),
        (
            "npm_config_userconfig".into(),
            root.join("blank_user_npmrc").into_os_string(),
        ),
        (
            "npm_config_globalconfig".into(),
            root.join("blank_global_npmrc").into_os_string(),
        ),
        ("npm_config_prefix".into(), default_npm_prefix(root).into_os_string()),
    ])
}

fn managed_bin_dir(root: &Path) -> PathBuf {
    managed_bin_dir_for_layout(root, current_managed_node_archive_layout())
}

fn current_managed_node_archive_layout() -> ManagedNodeArchiveLayout {
    if cfg!(windows) {
        ManagedNodeArchiveLayout::Windows
    } else {
        ManagedNodeArchiveLayout::Unix
    }
}

fn managed_bin_dir_for_layout(root: &Path, layout: ManagedNodeArchiveLayout) -> PathBuf {
    match layout {
        ManagedNodeArchiveLayout::Windows => root.to_path_buf(),
        ManagedNodeArchiveLayout::Unix => root.join("bin"),
    }
}

fn managed_node_path_for_layout(root: &Path, layout: ManagedNodeArchiveLayout) -> PathBuf {
    match layout {
        ManagedNodeArchiveLayout::Windows => root.join("node.exe"),
        ManagedNodeArchiveLayout::Unix => root.join("bin").join("node"),
    }
}

fn managed_wrapper_path_for_layout(root: &Path, layout: ManagedNodeArchiveLayout, tool: &str) -> PathBuf {
    match layout {
        ManagedNodeArchiveLayout::Windows => root.join(format!("{tool}.cmd")),
        ManagedNodeArchiveLayout::Unix => root.join("bin").join(tool),
    }
}

fn managed_npm_package_bin_dir_for_layout(root: &Path, layout: ManagedNodeArchiveLayout) -> PathBuf {
    match layout {
        ManagedNodeArchiveLayout::Windows => root.join("node_modules").join("npm").join("bin"),
        ManagedNodeArchiveLayout::Unix => root.join("lib").join("node_modules").join("npm").join("bin"),
    }
}

fn managed_npm_cli_path_for_layout(root: &Path, layout: ManagedNodeArchiveLayout) -> PathBuf {
    managed_npm_package_bin_dir_for_layout(root, layout).join("npm-cli.js")
}

fn managed_npx_cli_path_for_layout(root: &Path, layout: ManagedNodeArchiveLayout) -> PathBuf {
    managed_npm_package_bin_dir_for_layout(root, layout).join("npx-cli.js")
}

fn default_npm_prefix(root: &Path) -> PathBuf {
    root.join("tools").join("global")
}

fn managed_prefix_bin_dir(root: &Path) -> PathBuf {
    if cfg!(windows) {
        default_npm_prefix(root)
    } else {
        default_npm_prefix(root).join("bin")
    }
}

fn install_lock_path(runtime_root: &Path) -> PathBuf {
    runtime_root.join("node-runtime-install.lock")
}

fn emit_progress(reporter: Option<&dyn NodeRuntimeProgressReporter>, update: NodeRuntimeProgress) {
    if let Some(reporter) = reporter {
        reporter.report(update);
    }
}

fn reqwest_error(stage: &str, url: &str, error: &reqwest::Error) -> NodeRuntimeError {
    if error.is_timeout() {
        return timeout_error(stage, url, MANAGED_NODE_DOWNLOAD_TIMEOUT);
    }
    if let Some(status) = error.status() {
        return http_status_error(stage, url, status);
    }
    if error.is_connect() {
        return NodeRuntimeError::managed_invalid(format!("{stage} connect failed for {url}: {error}"));
    }
    NodeRuntimeError::managed_invalid(format!("{stage} failed for {url}: {error}"))
}

fn timeout_error(stage: &str, url: &str, timeout: Duration) -> NodeRuntimeError {
    NodeRuntimeError::managed_invalid(format!("{stage} timed out after {}s for {url}", timeout.as_secs()))
}

fn download_progress_message(url: &str, downloaded_bytes: u64, total_bytes: Option<u64>) -> String {
    let downloaded_mb = downloaded_bytes / (1024 * 1024);
    match total_bytes {
        Some(total) if total > 0 => {
            let total_mb = total / (1024 * 1024);
            format!("downloading managed Node runtime from {url} ({downloaded_mb}MB / {total_mb}MB)")
        }
        _ => format!("downloading managed Node runtime from {url} ({downloaded_mb}MB)"),
    }
}

fn http_status_error(stage: &str, url: &str, status: reqwest::StatusCode) -> NodeRuntimeError {
    NodeRuntimeError::managed_invalid(format!("{stage} returned HTTP {} for {url}", status.as_u16()))
}

fn classify_error(error: &NodeRuntimeError) -> (NodeRuntimeFailureKind, Option<u16>) {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("timed out") {
        return (NodeRuntimeFailureKind::Timeout, None);
    }
    if let Some(status) = parse_http_status(&message) {
        return (NodeRuntimeFailureKind::HttpStatus, Some(status));
    }
    if message.contains("unsupported") {
        return (NodeRuntimeFailureKind::UnsupportedPlatform, None);
    }
    if message.contains("bundled node runtime missing")
        || message.contains("bundled managed resources root unavailable")
    {
        return (NodeRuntimeFailureKind::BundledResourceMissing, None);
    }
    if message.contains("bundled node runtime is invalid") || message.contains("bundled node runtime failed validation")
    {
        return (NodeRuntimeFailureKind::BundledResourceInvalid, None);
    }
    if message.contains("checksum mismatch") {
        return (NodeRuntimeFailureKind::ChecksumMismatch, None);
    }
    if message.contains("validate") || message.contains("executable missing") || message.contains("entrypoint missing")
    {
        return (NodeRuntimeFailureKind::ValidationFailed, None);
    }
    if message.contains("download") || message.contains("extract") || message.contains("connect failed") {
        return (NodeRuntimeFailureKind::DownloadFailed, None);
    }
    (NodeRuntimeFailureKind::Unknown, None)
}

fn parse_http_status(message: &str) -> Option<u16> {
    let marker = "http ";
    let start = message.find(marker)? + marker.len();
    let digits: String = message[start..].chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<u16>().ok()
}

fn install_error(error: NodeRuntimeError, reporter: Option<&dyn NodeRuntimeProgressReporter>) -> NodeRuntimeError {
    let (kind, status_code) = classify_error(&error);
    emit_progress(
        reporter,
        match status_code {
            Some(status) => NodeRuntimeProgress::failed_with_status(kind, status, error.to_string()),
            None => NodeRuntimeProgress::failed(kind, error.to_string()),
        },
    );
    error
}

fn validation_error(error: NodeRuntimeError, reporter: Option<&dyn NodeRuntimeProgressReporter>) -> NodeRuntimeError {
    emit_progress(
        reporter,
        NodeRuntimeProgress::failed(NodeRuntimeFailureKind::ValidationFailed, error.to_string()),
    );
    error
}

fn combined_retry_error(
    first_error: NodeRuntimeError,
    retry_error: NodeRuntimeError,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> NodeRuntimeError {
    let combined = NodeRuntimeError::managed_invalid(format!("{first_error}; retry failed: {retry_error}"));
    let (kind, status_code) = classify_error(&retry_error);
    emit_progress(
        reporter,
        match status_code {
            Some(status) => NodeRuntimeProgress::failed_with_status(kind, status, combined.to_string()),
            None => NodeRuntimeProgress::failed(kind, combined.to_string()),
        },
    );
    combined
}

#[cfg(test)]
mod tests;
