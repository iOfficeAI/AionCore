mod bootstrap;
mod cli;
mod commands;
mod process_report;

use std::process::ExitCode;

use clap::Parser;

use aionui_app::AppServices;
use cli::{Cli, Command};

#[derive(Debug)]
enum MainError {
    Bootstrap(bootstrap::BootstrapError),
    Cli(commands::cli_error::CliBoundaryError),
    Other(anyhow::Error),
}

impl MainError {
    fn report(&self) {
        match self {
            Self::Bootstrap(err) => {
                err.log_source();
                eprintln!("{}", err.stderr_line());
            }
            Self::Cli(err) => {
                eprintln!("{}", err.stderr_line());
            }
            Self::Other(err) => {
                eprintln!("CLI_INTERNAL_ERROR: {err}");
            }
        }
    }

    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Bootstrap(err) => err.exit_code(),
            Self::Cli(err) => err.exit_code(),
            Self::Other(_) => ExitCode::from(1),
        }
    }
}

impl From<bootstrap::BootstrapError> for MainError {
    fn from(error: bootstrap::BootstrapError) -> Self {
        Self::Bootstrap(error)
    }
}

impl From<commands::cli_error::CliBoundaryError> for MainError {
    fn from(error: commands::cli_error::CliBoundaryError) -> Self {
        Self::Cli(error)
    }
}

impl From<anyhow::Error> for MainError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

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
    // anchor the bun cache under --data-dir) — these helpers don't host agents.
    //
    // `doctor`, in contrast, is meant to mirror the real server's CLI
    // detection path exactly. It must hit the same `aionui_runtime::init`
    // (so the bundled `bun` resolves through the same cache the server
    // uses) before falling through to PATH probing.
    let needs_runtime = matches!(
        cli.command,
        None | Some(Command::Doctor) | Some(Command::PrepareManagedResources(_))
    );
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
    if command_uses_bootstrap_runtime_boundary(command) {
        return MainError::Bootstrap(
            bootstrap::BootstrapError::new(
                bootstrap::BootstrapErrorCode::RuntimeInitFailed,
                "process.runtime",
                "failed to initialize async runtime",
            )
            .with_source(error),
        );
    }

    MainError::Cli(commands::cli_error::CliBoundaryError::new(
        commands::cli_error::CliBoundaryCode::CliRuntimeInitFailed,
        command_subcommand(command),
        "failed to initialize async runtime",
    ))
}

fn command_uses_bootstrap_runtime_boundary(command: &Option<Command>) -> bool {
    command.is_none()
}

fn command_subcommand(command: &Option<Command>) -> &'static str {
    match command {
        None => "server",
        Some(Command::McpBridge) => "mcp-bridge",
        Some(Command::McpGuideStdio) => "mcp-guide-stdio",
        Some(Command::McpTeamStdio) => "mcp-team-stdio",
        Some(Command::Doctor) => "doctor",
        Some(Command::PrepareManagedResources(_)) => "prepare-managed-resources",
    }
}

async fn async_main(merged_path: String, cli: Cli) -> Result<ExitCode, MainError> {
    // MCP stdio helpers must not touch the database, logging setup, or `AppServices`.
    match cli.command {
        Some(Command::McpBridge) => Ok(commands::run_mcp_bridge().await),
        Some(Command::McpGuideStdio) => Ok(commands::run_team_guide().await),
        Some(Command::McpTeamStdio) => Ok(commands::run_team_stdio().await),
        Some(Command::Doctor) => Ok(commands::run_doctor(&cli, &merged_path).await?),
        Some(Command::PrepareManagedResources(args)) => Ok(commands::run_prepare_managed_resources(args).await?),
        None => {
            let mut env = bootstrap::init_environment(&cli, &merged_path)?;
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
            Ok(commands::run_server(env, services, listener).await?)
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

        assert_eq!(err.code(), commands::cli_error::CliBoundaryCode::CliRuntimeInitFailed);
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

        assert_eq!(err.code(), commands::cli_error::CliBoundaryCode::CliRuntimeInitFailed);
        assert!(
            err.stderr_line()
                .starts_with("CLI_RUNTIME_INIT_FAILED subcommand=doctor")
        );
    }
}
