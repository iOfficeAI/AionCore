mod managed;
mod system;
mod types;

use std::sync::OnceLock;
use std::time::Duration;

use tracing::{debug, info, warn};

pub use managed::{
    install_and_validate as install_managed_runtime, managed_node_contract_for_export,
    probe_support as probe_node_runtime_supported,
};
pub use system::{derive_runtime_root, tool_command, validate_same_root};
pub use types::{
    DoctorRow, NodeRuntimeError, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase,
    NodeRuntimeProgressReporter, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe, SharedNodeRuntimeProgressReporter,
};

static MANAGED_RUNTIME_CACHE: OnceLock<tokio::sync::Mutex<Option<ResolvedNodeRuntime>>> = OnceLock::new();
static MANAGED_RUNTIME_INSTALL_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

pub fn probe_runtime_command(command: &str) -> RuntimeCommandProbe {
    let trimmed = command.trim();
    let path = std::path::Path::new(trimmed);

    let probe = if path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        RuntimeCommandProbe::ExplicitPath {
            path: path.to_path_buf(),
        }
    } else {
        match trimmed {
            "node" => RuntimeCommandProbe::NodeTool {
                tool: NodeTool::Node,
                command: trimmed.to_owned(),
            },
            "npm" => RuntimeCommandProbe::NodeTool {
                tool: NodeTool::Npm,
                command: trimmed.to_owned(),
            },
            "npx" => RuntimeCommandProbe::NodeTool {
                tool: NodeTool::Npx,
                command: trimmed.to_owned(),
            },
            _ => RuntimeCommandProbe::PathLookup {
                command: trimmed.to_owned(),
            },
        }
    };

    log_probe_decision(&probe);
    probe
}

pub async fn ensure_node_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    ensure_node_runtime_with_reporter(None).await
}

pub async fn ensure_node_runtime_with_reporter(
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    if let Some(runtime) = cached_managed_runtime_unreported().await {
        log_runtime_selected(&runtime);
        return Ok(runtime);
    }

    let lock = MANAGED_RUNTIME_INSTALL_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;

    if let Some(runtime) = cached_managed_runtime(reporter).await {
        log_runtime_selected(&runtime);
        return Ok(runtime);
    }

    let runtime = install_managed_runtime_with_reporter(reporter).await?;
    *managed_runtime_cache().lock().await = Some(runtime.clone());
    log_runtime_selected(&runtime);
    Ok(runtime)
}

pub async fn ensure_runtime_command(command: &str) -> Result<ResolvedCommand, NodeRuntimeError> {
    ensure_runtime_command_with_reporter(command, None).await
}

pub async fn ensure_runtime_command_with_reporter(
    command: &str,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<ResolvedCommand, NodeRuntimeError> {
    match probe_runtime_command(command) {
        RuntimeCommandProbe::ExplicitPath { path } => {
            if !path.exists() {
                return Err(NodeRuntimeError::system_invalid(format!(
                    "command '{}' not found",
                    path.display()
                )));
            }
            Ok(ResolvedCommand::plain(path))
        }
        RuntimeCommandProbe::PathLookup { command } => crate::resolve_command_path(&command)
            .map(ResolvedCommand::plain)
            .ok_or_else(|| NodeRuntimeError::system_invalid(format!("command '{command}' not found in PATH"))),
        RuntimeCommandProbe::NodeTool { tool, .. } => {
            let runtime = ensure_node_runtime_with_reporter(reporter).await?;
            Ok(tool_command(tool, &runtime))
        }
    }
}

fn runtime_source_label(source: ResolvedNodeSource) -> &'static str {
    match source {
        ResolvedNodeSource::Bundled => "bundled",
        ResolvedNodeSource::Managed => "managed",
    }
}

fn log_probe_decision(probe: &RuntimeCommandProbe) {
    match probe {
        RuntimeCommandProbe::ExplicitPath { path } => {
            debug!(command = %path.display(), probe = "explicit-path", "node runtime probe decided");
        }
        RuntimeCommandProbe::PathLookup { command } => {
            debug!(command, probe = "path-lookup", "node runtime probe decided");
        }
        RuntimeCommandProbe::NodeTool { tool, command } => {
            debug!(command, tool = ?tool, probe = "node-tool", "node runtime probe decided");
        }
    }
}

fn log_runtime_selected(runtime: &ResolvedNodeRuntime) {
    info!(
        source = runtime_source_label(runtime.source),
        version = %runtime.version,
        root = %runtime.root.display(),
        node = %runtime.node_path.display(),
        npm = %runtime.npm_path.display(),
        npx = %runtime.npx_path.display(),
        "node runtime selected"
    );
}

fn managed_runtime_cache() -> &'static tokio::sync::Mutex<Option<ResolvedNodeRuntime>> {
    MANAGED_RUNTIME_CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

async fn cached_managed_runtime_unreported() -> Option<ResolvedNodeRuntime> {
    cached_managed_runtime(None).await
}

async fn cached_managed_runtime(reporter: Option<&dyn NodeRuntimeProgressReporter>) -> Option<ResolvedNodeRuntime> {
    let cached = managed_runtime_cache().lock().await.clone()?;

    match managed::validate_managed_runtime(&cached.root, reporter).await {
        Ok(runtime) => {
            emit_runtime_ready(reporter, &runtime);
            *managed_runtime_cache().lock().await = Some(runtime.clone());
            Some(runtime)
        }
        Err(error) => {
            warn!(
                error = %error,
                root = %cached.root.display(),
                "managed node runtime cache invalidated"
            );
            *managed_runtime_cache().lock().await = None;
            None
        }
    }
}

fn emit_runtime_ready(reporter: Option<&dyn NodeRuntimeProgressReporter>, runtime: &ResolvedNodeRuntime) {
    if let Some(reporter) = reporter {
        reporter.report(NodeRuntimeProgress::ready(format!(
            "{} Node runtime {} is ready",
            runtime_source_label(runtime.source),
            runtime.version
        )));
    }
}

pub fn doctor_snapshot() -> Vec<DoctorRow> {
    if let Some(runtime) = managed::probe_preferred_local_runtime() {
        let source = runtime_source_label(runtime.source);
        return vec![
            DoctorRow {
                tool: "node".into(),
                source: source.into(),
                detail: runtime.node_path.display().to_string(),
            },
            DoctorRow {
                tool: "npm".into(),
                source: source.into(),
                detail: runtime.npm_path.display().to_string(),
            },
            DoctorRow {
                tool: "npx".into(),
                source: source.into(),
                detail: runtime.npx_path.display().to_string(),
            },
        ];
    }

    let support = probe_node_runtime_supported();
    let source = if support.supported { "managed" } else { "unavailable" };
    vec![
        DoctorRow {
            tool: "node".into(),
            source: source.into(),
            detail: support.detail.clone(),
        },
        DoctorRow {
            tool: "npm".into(),
            source: source.into(),
            detail: support.detail.clone(),
        },
        DoctorRow {
            tool: "npx".into(),
            source: source.into(),
            detail: support.detail,
        },
    ]
}

async fn validate_runtime(
    mut runtime: ResolvedNodeRuntime,
    min_node_major: Option<u64>,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    let node_version = command_version(ResolvedCommand::plain(runtime.node_path.clone()), "node").await?;
    if let Some(min_major) = min_node_major
        && node_version.major < min_major
    {
        return Err(NodeRuntimeError::system_invalid(format!(
            "node version {} is below required major {}",
            node_version, min_major
        )));
    }

    let _ = command_version(runtime.npm_command(), "npm").await?;
    let _ = command_version(runtime.npx_command(), "npx").await?;
    runtime.version = node_version;
    Ok(runtime)
}

async fn install_managed_runtime_with_reporter(
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    managed::install_and_validate_with_reporter(reporter).await
}

// Bounded retry budget for the managed Node `--version` validation probe.
// A single transient external-process failure (process fails to start, a non-zero
// exit such as npm exit code 7, or non-semver output) must not be latched into a
// permanent bundled_resource_invalid failure event. Kept as its own constants
// (not reused from the copy-activation budget) so probe and copy tuning stay
// independent; initial values intentionally match MANAGED_NODE_ACTIVATION_COPY_*
// (managed.rs), covering a worst-case ~1.75s interference window per command.
const VERSION_PROBE_ATTEMPTS: usize = 3;
const VERSION_PROBE_BACKOFFS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

async fn command_version(command: ResolvedCommand, label: &str) -> Result<semver::Version, NodeRuntimeError> {
    probe_command_version_with_retry(label, VERSION_PROBE_ATTEMPTS, &VERSION_PROBE_BACKOFFS, || {
        command_version_once(command.clone(), label)
    })
    .await
}

/// Run the `--version` probe up to `attempts` times, sleeping the matching backoff
/// after every failed attempt (including the last, so a persistent failure spends
/// the full ~1.75s window before the verdict — matching `activate_copy_with_retry`
/// in managed.rs). Every error `command_version_once` can produce is a transient
/// external-process failure, so all are retried. A `warn` is logged only when a
/// retry will follow; the final, budget-exhausted failure is intentionally NOT
/// logged here — its caller (`activate_local_runtime_source`, managed.rs) already
/// warns on the validation failure, so re-logging would duplicate it.
async fn probe_command_version_with_retry<F, Fut>(
    label: &str,
    attempts: usize,
    backoffs: &[Duration],
    mut probe_once: F,
) -> Result<semver::Version, NodeRuntimeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<semver::Version, NodeRuntimeError>>,
{
    for attempt in 1..=attempts {
        match probe_once().await {
            Ok(version) => return Ok(version),
            Err(error) => {
                let backoff = backoffs.get(attempt - 1).copied().unwrap_or_default();
                if attempt == attempts {
                    tokio::time::sleep(backoff).await;
                    return Err(error);
                }
                warn!(
                    label,
                    attempt,
                    max_attempts = attempts,
                    error = %error,
                    "transient node --version probe failure; will retry after backoff"
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }
    unreachable!("version probe retry loop always returns on the final attempt")
}

async fn command_version_once(command: ResolvedCommand, label: &str) -> Result<semver::Version, NodeRuntimeError> {
    let mut builder = crate::Builder::from_resolved(&command);
    builder.arg("--version");
    let output = builder
        .output()
        .await
        .map_err(|error| NodeRuntimeError::managed_invalid(format!("{label} failed to start: {error}")))?;

    if !output.status.success() {
        return Err(NodeRuntimeError::managed_invalid(format!(
            "{label} exited with {}",
            output.status
        )));
    }

    parse_version_output(std::str::from_utf8(&output.stdout).unwrap_or_default(), label)
}

fn parse_version_output(output: &str, label: &str) -> Result<semver::Version, NodeRuntimeError> {
    let mut versions = output
        .lines()
        .filter_map(parse_independent_semver_line)
        .collect::<Vec<_>>();

    match versions.len() {
        1 => Ok(versions.remove(0)),
        0 => {
            let normalized = output.trim();
            Err(NodeRuntimeError::managed_invalid(format!(
                "{label} returned non-semver version output '{normalized}'"
            )))
        }
        _ => {
            let normalized = output.trim();
            Err(NodeRuntimeError::managed_invalid(format!(
                "{label} returned ambiguous semver version output '{normalized}'"
            )))
        }
    }
}

fn parse_independent_semver_line(line: &str) -> Option<semver::Version> {
    let trimmed = line.trim();
    let version_text = trimmed.strip_prefix('v').unwrap_or(trimmed);
    semver::Version::parse(version_text).ok()
}

pub fn doctor_snapshot_for_test(rows: Vec<(&str, &str, &str)>) -> Vec<DoctorRow> {
    rows.into_iter()
        .map(|(tool, source, detail)| DoctorRow {
            tool: tool.into(),
            source: source.into(),
            detail: detail.into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};

    use std::io::Write;
    use tracing::Level;
    use tracing_subscriber::fmt;

    static TEST_MANAGED_RUNTIME_CACHE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(level: Level, f: impl FnOnce()) -> String {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };

        let subscriber = fmt::Subscriber::builder()
            .with_max_level(level)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
    }

    fn write_executable(path: &std::path::Path, body: &str) {
        fs::write(path, body).expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("set permissions");
        }
    }

    fn fake_managed_runtime(root: &std::path::Path) -> ResolvedNodeRuntime {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("create runtime bin");
        write_executable(&bin.join("node"), "#!/bin/sh\necho v24.11.0\n");
        write_executable(&bin.join("npm"), "#!/bin/sh\necho 24.11.0\n");
        write_executable(&bin.join("npx"), "#!/bin/sh\necho 24.11.0\n");

        ResolvedNodeRuntime {
            source: ResolvedNodeSource::Managed,
            root: root.to_path_buf(),
            version: semver::Version::new(0, 0, 0),
            node_path: bin.join("node"),
            npm_path: bin.join("npm"),
            npm_args_prefix: vec![],
            npx_path: bin.join("npx"),
            npx_args_prefix: vec![],
            env: vec![],
        }
    }

    fn test_managed_runtime_cache_lock() -> &'static tokio::sync::Mutex<()> {
        TEST_MANAGED_RUNTIME_CACHE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[test]
    fn probe_non_node_command_is_path_only() {
        let probe = probe_runtime_command("sh");
        assert!(matches!(probe, RuntimeCommandProbe::PathLookup { .. }));
    }

    #[test]
    fn probe_bare_node_uses_runtime_probe() {
        let probe = probe_runtime_command("node");
        assert!(matches!(
            probe,
            RuntimeCommandProbe::NodeTool {
                tool: NodeTool::Node,
                ..
            }
        ));
    }

    #[test]
    fn probe_explicit_path_is_passthrough() {
        let probe = probe_runtime_command("/tmp/custom-node");
        assert!(matches!(probe, RuntimeCommandProbe::ExplicitPath { .. }));
    }

    #[test]
    fn parse_version_output_accepts_banner_crlf_single_semver_line() {
        let version = parse_version_output(
            "Bienvenido a FenixFat\r\nFenixFat 2026-07-02 10:00:00\r\n11.6.1\r\n",
            "npm",
        )
        .expect("single independent semver line should parse");

        assert_eq!(version, semver::Version::new(11, 6, 1));
    }

    #[test]
    fn parse_version_output_accepts_v_prefixed_single_semver_line() {
        let version = parse_version_output("v24.11.0\r\n", "node").expect("v prefix should parse");

        assert_eq!(version, semver::Version::new(24, 11, 0));
    }

    #[test]
    fn parse_version_output_rejects_output_without_independent_semver_line() {
        let error = parse_version_output("Bienvenido\r\nFenixFat\r\n", "npm").expect_err("missing semver should fail");

        assert!(
            error.to_string().contains("npm returned non-semver version output"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_version_output_rejects_embedded_semver_text() {
        let error = parse_version_output("npm version 11.6.1\r\n", "npm").expect_err("embedded semver should fail");

        assert!(
            error.to_string().contains("npm returned non-semver version output"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_version_output_rejects_multiple_independent_semver_lines() {
        let error = parse_version_output("11.6.1\r\n10.9.0\r\n", "npm").expect_err("ambiguous versions should fail");

        assert!(
            error
                .to_string()
                .contains("npm returned ambiguous semver version output"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn version_probe_retries_until_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let result = probe_command_version_with_retry(
            "npm",
            VERSION_PROBE_ATTEMPTS,
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            move || {
                let probe_calls = probe_calls.clone();
                async move {
                    let n = probe_calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        // The exact transient failure this issue latched: npm exit 7.
                        Err(NodeRuntimeError::managed_invalid("npm exited with exit code: 7"))
                    } else {
                        Ok(semver::Version::new(24, 11, 0))
                    }
                }
            },
        )
        .await;
        assert_eq!(
            result.expect("probe should succeed after transient failures"),
            semver::Version::new(24, 11, 0)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 2 transient failures then 1 success"
        );
    }

    #[tokio::test]
    async fn version_probe_succeeds_without_retry_on_first_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let result =
            probe_command_version_with_retry("node", VERSION_PROBE_ATTEMPTS, &VERSION_PROBE_BACKOFFS, move || {
                let probe_calls = probe_calls.clone();
                async move {
                    probe_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(semver::Version::new(24, 11, 0))
                }
            })
            .await;
        assert!(result.is_ok(), "first-attempt success must return Ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "success on first attempt must not retry"
        );
    }

    #[tokio::test]
    async fn version_probe_returns_last_error_after_budget_exhausted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let result = probe_command_version_with_retry(
            "npm",
            VERSION_PROBE_ATTEMPTS,
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            move || {
                let probe_calls = probe_calls.clone();
                async move {
                    probe_calls.fetch_add(1, Ordering::SeqCst);
                    Err::<semver::Version, _>(NodeRuntimeError::managed_invalid("npm exited with exit code: 7"))
                }
            },
        )
        .await;
        let error = result.expect_err("exhausted budget must return the final Err");
        assert!(
            error.to_string().contains("npm exited with exit code: 7"),
            "final error must be the last probe error, got: {error}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            VERSION_PROBE_ATTEMPTS,
            "must attempt exactly the full budget"
        );
    }

    #[test]
    fn version_probe_budget_matches_spec() {
        assert_eq!(VERSION_PROBE_ATTEMPTS, 3);
        assert_eq!(
            VERSION_PROBE_BACKOFFS,
            [
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1000),
            ]
        );
    }

    #[test]
    fn doctor_snapshot_for_test_includes_source_and_detail() {
        let rows = doctor_snapshot_for_test(vec![("node", "managed", "/tmp/node")]);
        assert_eq!(rows[0].tool, "node");
        assert_eq!(rows[0].source, "managed");
        assert!(rows[0].detail.contains("/tmp/node"));
    }

    #[test]
    fn log_runtime_selected_emits_source_and_version() {
        let runtime = ResolvedNodeRuntime {
            source: ResolvedNodeSource::Managed,
            root: PathBuf::from("/opt/node-v24"),
            version: semver::Version::new(24, 11, 0),
            node_path: PathBuf::from("/opt/node-v24/bin/node"),
            npm_path: PathBuf::from("/opt/node-v24/bin/npm"),
            npm_args_prefix: vec![],
            npx_path: PathBuf::from("/opt/node-v24/bin/npx"),
            npx_args_prefix: vec![],
            env: vec![],
        };

        let captured = capture_logs(Level::INFO, || log_runtime_selected(&runtime));
        assert!(
            captured.contains("node runtime selected"),
            "missing selection log: {captured}"
        );
        assert!(
            captured.contains("source=managed") || captured.contains("source=\"managed\""),
            "missing source field: {captured}"
        );
        assert!(
            captured.contains("version=24.11.0"),
            "missing version field: {captured}"
        );
    }

    #[tokio::test]
    async fn ensure_explicit_path_requires_existing_file() {
        let missing = PathBuf::from("/tmp/aionui-missing-node-runtime-command");
        let error = ensure_runtime_command(missing.to_string_lossy().as_ref())
            .await
            .expect_err("missing explicit path should fail");
        assert!(
            error.to_string().contains("not found"),
            "expected not-found error, got: {error}"
        );
    }

    #[tokio::test]
    async fn stale_managed_runtime_cache_is_evicted_when_root_is_deleted() {
        let _guard = test_managed_runtime_cache_lock().lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("node-v24.11.0-test");
        let runtime = fake_managed_runtime(&root);
        *managed_runtime_cache().lock().await = Some(runtime.clone());

        let cached = cached_managed_runtime_unreported()
            .await
            .expect("cache should validate");
        assert_eq!(cached.root, runtime.root);

        fs::remove_dir_all(&root).expect("remove runtime root");

        assert!(
            cached_managed_runtime_unreported().await.is_none(),
            "deleted managed runtime should invalidate cache"
        );
        assert!(
            managed_runtime_cache().lock().await.is_none(),
            "stale managed runtime cache should be cleared"
        );
    }

    #[tokio::test]
    async fn cached_managed_runtime_emits_ready_after_validation() {
        let _guard = test_managed_runtime_cache_lock().lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("node-v24.11.0-test");
        let runtime = fake_managed_runtime(&root);
        *managed_runtime_cache().lock().await = Some(runtime.clone());

        let phases = Arc::new(Mutex::new(Vec::<NodeRuntimeProgressPhase>::new()));
        let reporter = {
            let phases = Arc::clone(&phases);
            move |update: NodeRuntimeProgress| {
                phases.lock().expect("lock").push(update.phase);
            }
        };

        let cached = cached_managed_runtime(Some(&reporter))
            .await
            .expect("cache should validate");

        assert_eq!(cached.root, runtime.root);
        assert_eq!(
            *phases.lock().expect("lock"),
            vec![NodeRuntimeProgressPhase::Validating, NodeRuntimeProgressPhase::Ready]
        );

        *managed_runtime_cache().lock().await = None;
    }
}
