mod bootstrap;
mod cli;
mod commands;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use aionui_app::AppServices;
use cli::{Cli, Command};

fn main() -> Result<ExitCode> {
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
    let needs_runtime = matches!(cli.command, None | Some(Command::Doctor));
    if needs_runtime {
        aionui_runtime::init(&cli.data_dir);
    }

    // SAFETY: called before any worker thread exists (including the tokio
    // runtime constructed below). Rust 2024 requires `unsafe` for
    // `std::env::set_var` invoked inside `enhance_process_path`.
    let merged_path = unsafe { aionui_runtime::enhance_process_path() };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async_main(merged_path, cli))
}

async fn async_main(merged_path: String, cli: Cli) -> Result<ExitCode> {
    // MCP stdio helpers must not touch the database, logging setup, or `AppServices`.
    match cli.command {
        Some(Command::McpBridge) => Ok(commands::run_mcp_bridge().await),
        Some(Command::McpGuideStdio) => Ok(commands::run_team_guide().await),
        Some(Command::McpTeamStdio) => Ok(commands::run_team_stdio().await),
        Some(Command::Doctor) => commands::run_doctor(&cli, &merged_path).await,
        None => {
            let mut env = bootstrap::init_environment(&cli, &merged_path)?;
            let listener = commands::bind_http_listener(&mut env.config).await?;
            let database = bootstrap::init_data_layer(&env.config).await?;
            let services = AppServices::from_config(database, &env.config).await?;
            commands::run_server(env, services, listener).await
        }
    }
}
