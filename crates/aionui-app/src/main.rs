mod bootstrap;
mod cli;
mod commands;
mod error;
mod process_report;

use std::process::ExitCode;

use clap::Parser;

use aionui_app::AppServices;
use cli::{Cli, Command};

use crate::bootstrap::parent_exit_signal;
use crate::error::MainError;

// MainError has been moved to src/error.rs

fn main() -> ExitCode {
    match run_main() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            error.report();
            error.exit_code()
        }
    }
}

fn run_main() -> Result<ExitCode, MainError> {
    let cli = Cli::parse();

    // mcp-* subcommands route into short-lived stdio helpers that live entirely
    // outside the main HTTP server. They share the global flags so clap can
    // parse a uniform CLI, but bypass `aionui_runtime::init` (which would
    // anchor managed runtime state under --data-dir) — these helpers don't
    // host agents.
    //
    // `doctor`, in contrast, is meant to mirror the real server's CLI
    // detection path exactly. It must hit the same `aionui_runtime::init`
    // before performing managed-runtime and PATH probing.
    let needs_runtime = cli.command.as_ref().is_none_or(Command::need_runtime);
    if needs_runtime {
        aionui_runtime::set_managed_resources_mode(cli.managed_resources_mode.into());
        aionui_runtime::init(&cli.data_dir);
    }

    // SAFETY: called before any worker thread exists (including the tokio
    // runtime constructed below). Rust 2024 requires `unsafe` for
    // `std::env::set_var` invoked inside `enhance_process_path`.
    let merged_path = unsafe { aionui_runtime::enhance_process_path() };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| runtime_init_error_for_command(&cli.command, error))?;
    runtime.block_on(async_main(merged_path, cli))
}

fn runtime_init_error_for_command(command: &Option<Command>, error: std::io::Error) -> MainError {
    if command.is_none() {
        return MainError::Bootstrap(
            bootstrap::BootstrapError::new(
                bootstrap::BootstrapErrorCode::RuntimeInitFailed,
                "process.runtime",
                "failed to initialize async runtime",
            )
            .with_source(error),
        );
    }

    MainError::Cli(commands::CliBoundaryError::new(
        commands::CliBoundaryCode::CliRuntimeInitFailed,
        command.as_ref().map_or("server", Command::as_str),
        "failed to initialize async runtime",
    ))
}

async fn async_main(merged_path: String, cli: Cli) -> Result<ExitCode, MainError> {
    // MCP stdio helpers must not touch the database, logging setup, or `AppServices`.
    match cli.command {
        Some(Command::Capabilities) => Ok(commands::run_capabilities()),
        Some(Command::Config(args)) => Ok(commands::run_config(args).await),
        Some(Command::Diagnose(args)) => Ok(commands::run_diagnose(args).await),
        Some(Command::Team(args)) => Ok(commands::run_team(args).await),
        Some(Command::McpBridge) => Ok(commands::run_mcp_bridge().await),
        Some(Command::McpTeamStdio) => Ok(commands::run_team_stdio().await),
        Some(Command::Doctor) => Ok(commands::run_doctor(&cli, &merged_path).await?),
        Some(Command::PrepareManagedResources(args)) => Ok(commands::run_prepare_managed_resources(args).await?),
        None => {
            let mut env = bootstrap::init_environment(&cli, &merged_path)?;

            // Acquire the data-dir process-level guard before binding a port or
            // touching the DB, so a second aioncore yields structurally rather
            // than racing the assistant bootstrap over the same data directory
            // (Sentry 135525166). Held (via `_instance_guard`, not a bare `_`,
            // which would drop and release it immediately) for the whole server
            // lifetime; the kernel releases the flock on process exit.
            let db_path = env.config.database_path();
            let _instance_guard = match aionui_db::DataDirInstanceGuard::try_acquire(&db_path) {
                Ok(Some(guard)) => {
                    tracing::info!(stage = "instance_guard.acquire", "acquired data-dir instance guard");
                    Some(guard)
                }
                Ok(None) => {
                    // A peer already owns the data dir. Wait a bounded window in
                    // case it is a crash-orphan about to self-exit, then yield.
                    match bootstrap::wait_for_instance_guard(&db_path) {
                        Ok(Some(guard)) => Some(guard),
                        Ok(None) => {
                            tracing::info!(
                                stage = "instance_guard.acquire",
                                "another aioncore owns the data directory; yielding"
                            );
                            return Err(MainError::Bootstrap(bootstrap::BootstrapError::new(
                                bootstrap::BootstrapErrorCode::PeerAlreadyRunning,
                                "instance_guard.acquire",
                                "another aioncore already owns this data directory",
                            )));
                        }
                        Err(error) => {
                            tracing::warn!(
                                code = "BOOTSTRAP_DEGRADED_INSTANCE_GUARD",
                                stage = "instance_guard.acquire",
                                error = %error,
                                "data-dir instance guard unavailable; proceeding without structural guard"
                            );
                            None
                        }
                    }
                }
                Err(error) => {
                    // flock unavailable (e.g. some network filesystems): proceed
                    // and rely on Option B bootstrap concurrency safety as the
                    // last line of defence.
                    tracing::warn!(
                        code = "BOOTSTRAP_DEGRADED_INSTANCE_GUARD",
                        stage = "instance_guard.acquire",
                        error = %error,
                        "data-dir instance guard unavailable; proceeding without structural guard"
                    );
                    None
                }
            };

            let listener = commands::bind_http_listener(&mut env.config).await?;
            let database = bootstrap::init_data_layer(&env.config).await?;
            let services = AppServices::from_config(database, &env.config).await.map_err(|error| {
                bootstrap::BootstrapError::new(
                    bootstrap::BootstrapErrorCode::ServiceInitFailed,
                    "services.init",
                    "failed to initialize application services",
                )
                .with_source(error)
            })?;
            let parent_exit = parent_exit_signal(cli.parent_pid);
            Ok(commands::run_server(env, services, listener, parent_exit).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_error(command: Option<Command>) -> MainError {
        runtime_init_error_for_command(&command, std::io::Error::other("raw runtime source"))
    }

    #[test]
    fn runtime_init_failure_for_server_uses_bootstrap_boundary() {
        let MainError::Bootstrap(err) = runtime_error(None) else {
            panic!("expected bootstrap error");
        };

        assert_eq!(err.code(), bootstrap::BootstrapErrorCode::RuntimeInitFailed);
        assert_eq!(err.stage(), "process.runtime");
        assert!(err.stderr_line().starts_with("BOOTSTRAP_RUNTIME_INIT_FAILED"));
        assert!(!err.stderr_line().contains("raw runtime source"));
    }

    #[test]
    fn runtime_init_failure_for_helper_uses_cli_boundary() {
        let MainError::Cli(err) = runtime_error(Some(Command::McpTeamStdio)) else {
            panic!("expected CLI error");
        };

        assert_eq!(err.code(), commands::CliBoundaryCode::CliRuntimeInitFailed);
        assert!(
            err.stderr_line()
                .starts_with("CLI_RUNTIME_INIT_FAILED subcommand=mcp-team-stdio")
        );
        assert!(!err.stderr_line().contains("raw runtime source"));
    }

    #[test]
    fn runtime_init_failure_for_doctor_uses_cli_boundary() {
        let MainError::Cli(err) = runtime_error(Some(Command::Doctor)) else {
            panic!("expected CLI error");
        };

        assert_eq!(err.code(), commands::CliBoundaryCode::CliRuntimeInitFailed);
        assert!(
            err.stderr_line()
                .starts_with("CLI_RUNTIME_INIT_FAILED subcommand=doctor")
        );
    }
}
