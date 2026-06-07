//! Bootstrap layers shared by non-MCP subcommands.

use std::time::Instant;

use tracing::info;

use aionui_app::AppConfig;
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

    let config = AppConfig {
        host: cli.host.clone(),
        port: cli.port,
        data_dir: cli.data_dir.clone(),
        work_dir,
        app_version: cli.app_version.clone(),
        local: cli.local,
    };
    info!(
        "Running in {} mode — authentication is {}",
        if config.local { "local" } else { "remote" },
        if config.local { "disabled" } else { "enabled" }
    );

    Ok(ServerEnvironment {
        _log_guard: log_guard,
        config,
    })
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
    let database = aionui_db::init_database_staged(&db_path).await.map_err(|e| {
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
    #[test]
    fn database_stage_comes_from_db_boundary_error() {
        let err = aionui_db::DatabaseInitError::new(
            "database.migration",
            aionui_db::DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(42)),
        );

        assert_eq!(err.stage(), "database.migration");
    }
}
