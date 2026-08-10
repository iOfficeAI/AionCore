//! Bootstrap layers shared by non-MCP subcommands.

use std::path::PathBuf;
use std::time::Instant;

use tracing::info;

use aionui_app::{AppConfig, IdentityMode, parse_allowed_origins, validate_local_client_secret};
use aionui_db::Database;

use crate::cli::Cli;

use super::builtin_skills::materialize_builtin_skills;
use super::tracing_init::{LogGuards, init_tracing};
use super::work_dir::resolve_work_dir;
use super::{BootstrapError, BootstrapErrorCode};

/// Resolved environment needed by all non-MCP subcommands.
pub struct ServerEnvironment {
    /// Must be held alive for the process lifetime to flush log buffers.
    pub _log_guard: LogGuards,
    pub config: AppConfig,
}

/// Layer 1: Logging + config resolution.
///
/// Cheap, synchronous, no IO beyond creating the log directory.
/// All subcommands that need logging and config should call this first.
pub fn init_environment(cli: &Cli, merged_path: &str) -> Result<ServerEnvironment, BootstrapError> {
    let log_dir = cli.log_dir.clone().unwrap_or_else(|| cli.data_dir.join("logs"));
    let log_guard = init_tracing(&log_dir, cli.log_level.as_deref())?;

    info!(
        path_segments = merged_path.split(if cfg!(windows) { ';' } else { ':' }).count(),
        path_len = merged_path.len(),
        "startup: PATH ready"
    );

    let work_dir = resolve_work_dir(cli.work_dir.clone(), &cli.data_dir);

    // SAFETY: called before any service initialization; no concurrent reads.
    unsafe {
        std::env::set_var("AIONUI_WORK_DIR", &work_dir);
    }

    let mut identity_mode: IdentityMode = cli.identity_mode.into();
    if cli.local {
        identity_mode = IdentityMode::Local;
    }
    let bootstrap_secret = std::env::var("AIONCORE_BOOTSTRAP_SECRET")
        .ok()
        .filter(|secret| !secret.is_empty());
    let bootstrap_workspace = std::env::var_os("AIONUI_BOOTSTRAP_WORKSPACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let allowed_origins_raw = std::env::var("AIONCORE_ALLOWED_ORIGINS").ok();
    let allowed_origins = parse_allowed_origins(allowed_origins_raw.as_deref()).map_err(|error| {
        BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "config.allowed_origins",
            "AIONCORE_ALLOWED_ORIGINS contains an invalid origin",
        )
        .with_field("reason", error)
    })?;
    let local_client_secret =
        resolve_local_client_secret(identity_mode, std::env::var("AIONCORE_LOCAL_CLIENT_SECRET").ok())?;
    validate_identity_environment(
        identity_mode,
        bootstrap_secret.as_deref(),
        bootstrap_workspace.as_deref(),
    )?;

    let config = AppConfig {
        host: cli.host.clone(),
        port: cli.port,
        data_dir: cli.data_dir.clone(),
        work_dir,
        app_version: cli.app_version.clone(),
        local: cli.local || identity_mode.is_local(),
        identity_mode,
        bootstrap_secret,
        bootstrap_workspace,
        bootstrap_initial_admin: identity_mode == IdentityMode::WebUi,
        local_client_secret,
        allowed_origins,
        dump_prompts: cli.dump_prompts,
        recover_corrupted_database: cli.recover_corrupted_database,
    };
    info!(
        identity_mode = config.identity_mode.auth_label(),
        auth = if config.identity_mode.is_local() {
            "disabled"
        } else {
            "enabled"
        },
        bootstrap_secret_configured = config.bootstrap_secret.is_some(),
        bootstrap_workspace_configured = config.bootstrap_workspace.is_some(),
        allowed_origin_count = config.allowed_origins.len(),
        local_client_secret_configured = config.local_client_secret.is_some(),
        "startup: identity mode resolved"
    );

    Ok(ServerEnvironment {
        _log_guard: log_guard,
        config,
    })
}

fn resolve_local_client_secret(
    identity_mode: IdentityMode,
    secret: Option<String>,
) -> Result<Option<String>, BootstrapError> {
    if identity_mode == IdentityMode::Local {
        let secret = secret.ok_or_else(|| {
            BootstrapError::new(
                BootstrapErrorCode::ConfigInvalid,
                "config.local_client_secret",
                "Local identity mode requires AIONCORE_LOCAL_CLIENT_SECRET",
            )
        })?;
        validate_local_client_secret(&secret).map_err(|error| {
            BootstrapError::new(
                BootstrapErrorCode::ConfigInvalid,
                "config.local_client_secret",
                "AIONCORE_LOCAL_CLIENT_SECRET is invalid",
            )
            .with_field("reason", error)
        })?;
        Ok(Some(secret))
    } else {
        Ok(None)
    }
}

fn validate_identity_environment(
    identity_mode: IdentityMode,
    bootstrap_secret: Option<&str>,
    bootstrap_workspace: Option<&std::path::Path>,
) -> Result<(), BootstrapError> {
    if identity_mode == IdentityMode::AionPro && bootstrap_secret.is_none() {
        return Err(BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "config.identity_mode",
            "AionPro identity mode requires AIONCORE_BOOTSTRAP_SECRET",
        ));
    }

    if bootstrap_workspace.is_some_and(|path| !path.is_absolute()) {
        return Err(BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "config.bootstrap_workspace",
            "AIONUI_BOOTSTRAP_WORKSPACE must be an absolute path",
        ));
    }

    Ok(())
}

/// Layer 2: Materialize builtin skills + initialize the database.
///
/// Requires only `data_dir`. Subcommands that need persistent state
/// (database, skill files) should call this after `init_environment`.
pub async fn init_data_layer(config: &AppConfig) -> Result<Database, BootstrapError> {
    let boot = Instant::now();

    materialize_builtin_skills(&config.data_dir).await.map_err(|e| {
        BootstrapError::new(
            BootstrapErrorCode::DataInitFailed,
            "data.builtin_skills",
            "failed to initialize application data",
        )
        .with_source(e)
        .with_field("dataDir", config.data_dir.display().to_string())
    })?;
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: builtin skills materialized"
    );

    let db_path = config.database_path();
    aionui_db::maybe_copy_legacy_database(&db_path).map_err(|e| {
        BootstrapError::new(
            BootstrapErrorCode::DataInitFailed,
            "data.legacy_db",
            "failed to initialize application data",
        )
        .with_source(e)
        .with_field("databasePath", db_path.display().to_string())
    })?;
    info!("Initializing database at {}", db_path.display());
    let database = aionui_db::init_database_staged_with_options(
        &db_path,
        aionui_db::DatabaseInitOptions {
            recover_corrupted_database: config.recover_corrupted_database,
        },
    )
    .await
    .map_err(|e| {
        let stage = e.stage();
        BootstrapError::new(
            BootstrapErrorCode::DataInitFailed,
            stage,
            "failed to initialize application data",
        )
        .with_source(e.into_source())
        .with_field("databasePath", db_path.display().to_string())
    })?;
    info!(elapsed_ms = boot.elapsed().as_millis(), "startup: database initialized");

    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_stage_comes_from_db_boundary_error() {
        let err = aionui_db::DatabaseInitError::new(
            "database.migration",
            aionui_db::DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(42)),
        );

        assert_eq!(err.stage(), "database.migration");
    }

    #[test]
    fn database_schema_repair_stage_comes_from_db_boundary_error() {
        let err = aionui_db::DatabaseInitError::new(
            "database.schema_repair",
            aionui_db::DbError::Init("repair failed".into()),
        );

        assert_eq!(err.stage(), "database.schema_repair");
    }

    #[test]
    fn database_recoverable_corruption_stage_comes_from_db_boundary_error() {
        let err = aionui_db::DatabaseInitError::new(
            "database.recoverable_corruption",
            aionui_db::DbError::Migration(sqlx::migrate::MigrateError::ExecuteMigration(
                sqlx::Error::Protocol("database disk image is malformed".into()),
                13,
            )),
        );

        assert_eq!(err.stage(), "database.recoverable_corruption");
    }

    #[test]
    fn aionpro_identity_requires_bootstrap_secret() {
        let err = validate_identity_environment(IdentityMode::AionPro, None, None)
            .expect_err("AionPro startup must require bootstrap secret");

        assert_eq!(err.code(), BootstrapErrorCode::ConfigInvalid);
        assert_eq!(err.stage(), "config.identity_mode");
    }

    #[test]
    fn aionpro_identity_accepts_bootstrap_secret() {
        validate_identity_environment(IdentityMode::AionPro, Some("secret"), None)
            .expect("AionPro startup should accept configured bootstrap secret");
    }

    #[test]
    fn non_aionpro_identity_does_not_require_bootstrap_secret() {
        validate_identity_environment(IdentityMode::WebUi, None, None)
            .expect("WebUI startup should not require bootstrap secret");
        validate_identity_environment(IdentityMode::Local, None, None)
            .expect("local startup should not require bootstrap secret");
    }

    #[test]
    fn bootstrap_workspace_must_be_absolute() {
        let err = validate_identity_environment(
            IdentityMode::WebUi,
            None,
            Some(std::path::Path::new("relative/workspace")),
        )
        .expect_err("relative bootstrap workspace must fail closed");

        assert_eq!(err.code(), BootstrapErrorCode::ConfigInvalid);
        assert_eq!(err.stage(), "config.bootstrap_workspace");
    }

    #[test]
    fn local_identity_requires_a_valid_per_launch_client_secret() {
        let missing = resolve_local_client_secret(IdentityMode::Local, None).unwrap_err();
        assert_eq!(missing.code(), BootstrapErrorCode::ConfigInvalid);
        assert_eq!(missing.stage(), "config.local_client_secret");

        let invalid = resolve_local_client_secret(IdentityMode::Local, Some("too-short".to_string())).unwrap_err();
        assert_eq!(invalid.code(), BootstrapErrorCode::ConfigInvalid);
        assert_eq!(invalid.stage(), "config.local_client_secret");

        assert_eq!(
            resolve_local_client_secret(
                IdentityMode::Local,
                Some("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678".to_string()),
            )
            .unwrap()
            .as_deref(),
            Some("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678")
        );
    }

    #[test]
    fn non_local_identity_does_not_retain_the_local_client_secret() {
        assert!(
            resolve_local_client_secret(
                IdentityMode::WebUi,
                Some("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678".to_string()),
            )
            .unwrap()
            .is_none()
        );
    }
}
