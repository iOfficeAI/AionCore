//! Tracing subscriber + log file initialization for the binary.
//!
//! Lives in the binary tree (not lib) because it owns process-global
//! subscriber registration that should never be invoked from tests or
//! external consumers of the library.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::Datelike;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use super::{BootstrapError, BootstrapErrorCode};

const NOISE_SUPPRESSIONS: &[&str] = &[
    "sqlx::query=warn",
    "hyper_util=warn",
    "reqwest=warn",
    // The ACP SDK logs raw UntypedMessage values at debug/trace, including
    // session/update chunks with user/agent text. Keep its protocol internals
    // out of default dev logs; aionui_ai_agent::protocol::acp emits sanitized
    // summaries for the ACP flow we need to debug.
    "agent_client_protocol::jsonrpc=info",
    // Aionrs provider/agent debug logs include raw request bodies and SSE
    // chunks. Keep lifecycle info logs, but do not write prompt/output
    // payloads by default.
    "aion_agent=info",
    "aion_providers=info",
];

const AIONRS_TARGETS: &[&str] = &[
    "aion_agent",
    "aion_config",
    "aion_compact",
    "aion_mcp",
    "aion_providers",
    "aion_protocol",
    "aion_tools",
    "aion_skills",
    "aion_memory",
];

const RAW_AIONRS_PAYLOAD_TARGETS: &[&str] = &["aion_agent", "aion_providers"];

fn build_env_filter(log_level: Option<&str>) -> EnvFilter {
    let user_directives = log_level.unwrap_or("info");
    let suppressions = NOISE_SUPPRESSIONS.join(",");
    EnvFilter::new(format!("{suppressions},{user_directives}"))
}

fn build_backend_filter(log_level: Option<&str>) -> EnvFilter {
    let user_directives = log_level.unwrap_or("info");
    let suppressions = NOISE_SUPPRESSIONS.join(",");
    let aionrs_off: String = AIONRS_TARGETS
        .iter()
        .map(|t| format!("{t}=off"))
        .collect::<Vec<_>>()
        .join(",");
    EnvFilter::new(format!("{suppressions},{aionrs_off},{user_directives}"))
}

fn build_aionrs_level(log_level: Option<&str>) -> String {
    let level = log_level.unwrap_or("info");
    AIONRS_TARGETS
        .iter()
        .map(|target| {
            let target_level = if RAW_AIONRS_PAYLOAD_TARGETS.contains(target) {
                "info"
            } else {
                level
            };
            format!("{target}={target_level}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// RAII guards that flush log buffers on drop. Hold for the process lifetime.
pub struct LogGuards {
    _backend: tracing_appender::non_blocking::WorkerGuard,
    _aionrs: tracing_appender::non_blocking::WorkerGuard,
}

const LOGGING_INIT_MESSAGE: &str = "failed to initialize logging";

/// Outcome of picking the log root: which root is active plus, when the
/// requested custom directory was unusable, the details needed to report the
/// degradation once the tracing subscriber is ready.
#[derive(Debug)]
struct LogDirSelection {
    root: PathBuf,
    active_dir: PathBuf,
    fallback: Option<LogDirFallback>,
}

/// Custom log-dir failure that triggered the default-dir fallback. Held until
/// the subscriber is initialized, then emitted as a structured warn.
#[derive(Debug)]
struct LogDirFallback {
    requested_dir: PathBuf,
    error_kind: io::ErrorKind,
}

fn logging_dir_error(active_log_dir: &Path, error: io::Error) -> BootstrapError {
    // ErrorKind is the enum name only (no path, no OS message), so it can ride
    // the stable stderr boundary line while the raw source stays private.
    let error_kind = format!("{:?}", error.kind());
    BootstrapError::new(
        BootstrapErrorCode::LoggingInitFailed,
        "logging.dir",
        LOGGING_INIT_MESSAGE,
    )
    .with_source(error)
    .with_field("logDir", active_log_dir.display().to_string())
    .with_field("errorKind", error_kind)
}

/// Pick the log root directory, creating today's dated partition.
///
/// A custom directory that cannot be created (permissions, AV interference,
/// path occupied by a file — Jira AIONUI-231) must not permanently brick
/// bootstrap: fall back to the default directory instead. Failure to create
/// the default directory itself remains fatal.
fn select_log_root(custom_log_dir: Option<&Path>, default_log_dir: &Path) -> Result<LogDirSelection, BootstrapError> {
    if let Some(custom) = custom_log_dir
        && custom != default_log_dir
    {
        let custom_active = dated_log_dir(custom);
        match std::fs::create_dir_all(&custom_active) {
            Ok(()) => {
                return Ok(LogDirSelection {
                    root: custom.to_path_buf(),
                    active_dir: custom_active,
                    fallback: None,
                });
            }
            Err(custom_error) => {
                let custom_kind = custom_error.kind();
                let default_active = dated_log_dir(default_log_dir);
                std::fs::create_dir_all(&default_active).map_err(|default_error| {
                    logging_dir_error(&default_active, default_error)
                        .with_field("requestedLogDir", custom.display().to_string())
                        .with_field("requestedErrorKind", format!("{custom_kind:?}"))
                })?;
                return Ok(LogDirSelection {
                    root: default_log_dir.to_path_buf(),
                    active_dir: default_active,
                    fallback: Some(LogDirFallback {
                        requested_dir: custom.to_path_buf(),
                        error_kind: custom_kind,
                    }),
                });
            }
        }
    }

    let root = custom_log_dir.unwrap_or(default_log_dir);
    let active_dir = dated_log_dir(root);
    std::fs::create_dir_all(&active_dir).map_err(|e| logging_dir_error(&active_dir, e))?;
    Ok(LogDirSelection {
        root: root.to_path_buf(),
        active_dir,
        fallback: None,
    })
}

pub fn init_tracing(
    custom_log_dir: Option<&Path>,
    default_log_dir: &Path,
    log_level: Option<&str>,
) -> Result<LogGuards, BootstrapError> {
    let selection = select_log_root(custom_log_dir, default_log_dir)?;
    let log_dir = selection.root.as_path();
    let active_log_dir = selection.active_dir.as_path();

    let console_layer = fmt::layer().with_target(true).with_filter(build_env_filter(log_level));

    // Backend file layer — excludes aion_* targets
    let file_appender = DailyDatedLogWriter::new(log_dir.to_path_buf(), "aioncore.log");
    let (non_blocking, backend_guard) = tracing_appender::non_blocking(file_appender);

    let backend_file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_filter(build_backend_filter(log_level));

    // Aionrs file layer — only aion_* targets
    let aionrs_level = build_aionrs_level(log_level);
    let aionrs_filter = EnvFilter::try_new(&aionrs_level).map_err(|e| {
        BootstrapError::new(
            BootstrapErrorCode::LoggingInitFailed,
            "logging.filter",
            LOGGING_INIT_MESSAGE,
        )
        .with_source(e)
        .with_field("filter", aionrs_level.clone())
        .with_field("logDir", active_log_dir.display().to_string())
    })?;
    let aionrs_appender = DailyDatedLogWriter::new(log_dir.to_path_buf(), "aionrs.log");
    let (aionrs_non_blocking, aionrs_guard) = tracing_appender::non_blocking(aionrs_appender);
    let aionrs_layer = fmt::layer()
        .json()
        .with_writer(aionrs_non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_filter(aionrs_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(backend_file_layer)
        .with(aionrs_layer)
        .try_init()
        .map_err(|e| {
            BootstrapError::new(
                BootstrapErrorCode::LoggingInitFailed,
                "logging.subscriber",
                LOGGING_INIT_MESSAGE,
            )
            .with_source(e)
            .with_field("logDir", active_log_dir.display().to_string())
        })?;

    if let Some(fallback) = &selection.fallback {
        // Production-visible degradation marker (AIONUI-231): the requested
        // custom log dir was unusable and logging continues in the default dir.
        tracing::warn!(
            code = "BOOTSTRAP_DEGRADED_LOG_DIR",
            stage = "logging.dir.fallback",
            requested_log_dir = %fallback.requested_dir.display(),
            active_log_dir = %active_log_dir.display(),
            error_kind = ?fallback.error_kind,
            "custom log directory is unusable; falling back to default log directory"
        );
    }

    Ok(LogGuards {
        _backend: backend_guard,
        _aionrs: aionrs_guard,
    })
}

fn dated_log_dir(log_root: &Path) -> PathBuf {
    dated_log_dir_for(log_root, LogDate::today())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogDate {
    year: i32,
    month: u32,
    day: u32,
}

impl LogDate {
    fn today() -> Self {
        let now = chrono::Local::now();
        Self {
            year: now.year(),
            month: now.month(),
            day: now.day(),
        }
    }

    fn file_name(self, suffix: &str) -> String {
        format!("{:04}-{:02}-{:02}.{}", self.year, self.month, self.day, suffix)
    }
}

fn dated_log_dir_for(log_root: &Path, date: LogDate) -> PathBuf {
    log_root
        .join(format!("{:04}", date.year))
        .join(format!("{:02}", date.month))
        .join(format!("{:02}", date.day))
}

fn dated_log_file_path(log_root: &Path, date: LogDate, suffix: &str) -> PathBuf {
    dated_log_dir_for(log_root, date).join(date.file_name(suffix))
}

struct DailyDatedLogWriter {
    log_root: PathBuf,
    filename_suffix: &'static str,
    date_provider: Box<dyn Fn() -> LogDate + Send + Sync>,
    active_date: Option<LogDate>,
    active_file: Option<File>,
}

impl DailyDatedLogWriter {
    fn new(log_root: PathBuf, filename_suffix: &'static str) -> Self {
        Self::new_with_date_provider(log_root, filename_suffix, Box::new(LogDate::today))
    }

    fn new_with_date_provider(
        log_root: PathBuf,
        filename_suffix: &'static str,
        date_provider: Box<dyn Fn() -> LogDate + Send + Sync>,
    ) -> Self {
        Self {
            log_root,
            filename_suffix,
            date_provider,
            active_date: None,
            active_file: None,
        }
    }

    fn active_file(&mut self) -> io::Result<&mut File> {
        let date = (self.date_provider)();
        if self.active_date != Some(date) {
            let file_path = dated_log_file_path(&self.log_root, date, self.filename_suffix);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.active_file = Some(OpenOptions::new().create(true).append(true).open(file_path)?);
            self.active_date = Some(date);
        }

        self.active_file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file was not opened"))
    }
}

impl Write for DailyDatedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.active_file()?.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.active_file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn env_filter_suppresses_raw_acp_sdk_jsonrpc_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_env_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "agent_client_protocol::jsonrpc::handlers", Level::DEBUG),
                "ACP SDK JSON-RPC debug logs include raw UntypedMessage payloads"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::protocol::acp", Level::DEBUG),
                "AionUi ACP sanitized debug summaries should still be available"
            );
        });
    }

    #[test]
    fn backend_filter_suppresses_raw_acp_sdk_jsonrpc_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_backend_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "agent_client_protocol::jsonrpc::handlers", Level::DEBUG),
                "ACP SDK JSON-RPC debug logs include raw UntypedMessage payloads"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::protocol::acp", Level::DEBUG),
                "AionUi ACP sanitized debug summaries should still be available"
            );
        });
    }

    #[test]
    fn env_filter_suppresses_raw_aionrs_provider_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_env_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "aion_agent", Level::DEBUG),
                "aion_agent debug logs include raw request bodies"
            );
            assert!(
                !tracing::enabled!(target: "aion_providers", Level::DEBUG),
                "aion_providers debug logs include raw SSE chunks"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::manager::aionrs::agent", Level::DEBUG),
                "AionUi aionrs lifecycle debug logs should still be available"
            );
        });
    }

    #[test]
    fn aionrs_file_level_suppresses_raw_provider_targets_even_when_debug_enabled() {
        let level = build_aionrs_level(Some("debug"));
        assert!(level.contains("aion_agent=info"), "{level}");
        assert!(level.contains("aion_providers=info"), "{level}");
        assert!(level.contains("aion_tools=debug"), "{level}");
    }

    #[test]
    fn dated_log_dir_appends_date_partition() {
        let root = Path::new("/tmp/aionui-logs");
        let dated = dated_log_dir(root);
        let relative = dated.strip_prefix(root).expect("dated log dir should stay under root");
        let parts = relative
            .iter()
            .map(|part| part.to_str().expect("log dir should be utf-8"))
            .collect::<Vec<_>>();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        assert!(parts[0].chars().all(|ch| ch.is_ascii_digit()));
        assert!(parts[1].chars().all(|ch| ch.is_ascii_digit()));
        assert!(parts[2].chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn dated_file_writer_moves_new_day_files_into_matching_day_directory() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let first_day = LogDate {
            year: 2026,
            month: 7,
            day: 2,
        };
        let second_day = LogDate {
            year: 2026,
            month: 7,
            day: 3,
        };
        let days = std::sync::Arc::new(std::sync::Mutex::new(vec![second_day, first_day]));
        let mut writer = DailyDatedLogWriter::new_with_date_provider(
            tmp.path().to_path_buf(),
            "aioncore.log",
            Box::new({
                let days = std::sync::Arc::clone(&days);
                move || days.lock().expect("date queue").pop().expect("date")
            }),
        );

        std::io::Write::write_all(&mut writer, b"july 2\n").expect("write first day");
        std::io::Write::write_all(&mut writer, b"july 3\n").expect("write second day");
        std::io::Write::flush(&mut writer).expect("flush");

        let first_path = tmp.path().join("2026/07/02/2026-07-02.aioncore.log");
        let second_path = tmp.path().join("2026/07/03/2026-07-03.aioncore.log");
        assert_eq!(std::fs::read_to_string(first_path).expect("first day log"), "july 2\n");
        assert_eq!(
            std::fs::read_to_string(second_path).expect("second day log"),
            "july 3\n"
        );
        assert!(!tmp.path().join("2026/07/02/2026-07-03.aioncore.log").exists());
    }

    #[test]
    fn select_log_root_uses_creatable_custom_dir_without_fallback() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("custom-logs");
        let default = tmp.path().join("default-logs");

        let selection = select_log_root(Some(&custom), &default).expect("creatable custom dir should be selected");

        assert_eq!(selection.root, custom);
        assert!(selection.fallback.is_none());
        assert!(selection.active_dir.starts_with(&custom));
        assert!(selection.active_dir.is_dir());
    }

    #[test]
    fn select_log_root_falls_back_to_default_when_custom_dir_is_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        // A file occupying the custom path makes create_dir_all fail the same
        // way an unwritable path does (AIONUI-231 repro without root).
        let custom = tmp.path().join("occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        let default = tmp.path().join("default-logs");

        let selection = select_log_root(Some(&custom), &default).expect("unusable custom dir must degrade, not fail");

        assert_eq!(selection.root, default);
        assert!(selection.active_dir.starts_with(&default));
        assert!(selection.active_dir.is_dir());
        let fallback = selection
            .fallback
            .expect("fallback details must be recorded for the warn log");
        assert_eq!(fallback.requested_dir, custom);
    }

    #[test]
    fn select_log_root_stays_fatal_with_error_kind_when_default_dir_is_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let default = tmp.path().join("occupied");
        std::fs::write(&default, b"not a directory").expect("occupy default path");

        let err = select_log_root(None, &default).expect_err("default dir failure must stay fatal");

        assert_eq!(err.code(), BootstrapErrorCode::LoggingInitFailed);
        assert_eq!(err.stage(), "logging.dir");
        let stderr = err.stderr_line();
        assert!(stderr.contains("BOOTSTRAP_LOGGING_INIT_FAILED"), "{stderr}");
        assert!(stderr.contains("errorKind="), "{stderr}");
    }

    #[test]
    fn select_log_root_stays_fatal_when_both_custom_and_default_dirs_are_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("custom-occupied");
        let default = tmp.path().join("default-occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        std::fs::write(&default, b"not a directory").expect("occupy default path");

        let err = select_log_root(Some(&custom), &default).expect_err("both dirs failing must stay fatal");

        assert_eq!(err.code(), BootstrapErrorCode::LoggingInitFailed);
        assert_eq!(err.stage(), "logging.dir");
        let stderr = err.stderr_line();
        assert!(stderr.contains("errorKind="), "{stderr}");
        assert!(stderr.contains("requestedErrorKind="), "{stderr}");
    }

    /// The only test allowed to call `init_tracing`: it registers the
    /// process-global subscriber, and a second registration would fail.
    #[test]
    fn init_tracing_survives_unusable_custom_dir_by_falling_back_to_default() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        let default = tmp.path().join("default-logs");

        let _guards = init_tracing(Some(&custom), &default, Some("info"))
            .expect("bootstrap must survive an unusable custom log dir");

        assert!(dated_log_dir(&default).is_dir());
    }
}
