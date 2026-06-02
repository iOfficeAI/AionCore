//! `aioncore` (no subcommand): the main HTTP server.

use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::{info, warn};

use aionui_app::{AppServices, create_router};

use crate::bootstrap::ServerEnvironment;

/// Start the HTTP server with fully constructed services.
pub async fn run_server(env: ServerEnvironment, services: AppServices) -> Result<ExitCode> {
    let boot = Instant::now();

    let has_users = services.user_repo.has_users().await?;
    if !has_users {
        info!("No configured users detected — initial setup required via /api/auth/status");
    }

    let router = create_router(&services).await;
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: router ready for socket bind"
    );
    let addr = env.config.socket_addr();
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        address = %addr,
        "startup: socket bind started"
    );
    let listener = TcpListener::bind(&addr).await?;
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        address = %addr,
        "startup: socket bind completed"
    );
    info!(elapsed_ms = boot.elapsed().as_millis(), "Server listening on {addr}");

    // Kick off the idle-ACP-agent reaper. `start_idle_scanner` returns
    // immediately with a `JoinHandle`; the scanner task polls every 60 s
    // and kills ACP agents whose `status == Finished` + last_activity
    // exceeds the default 5-minute idle threshold. The watch channel
    // propagates graceful-shutdown so the scanner exits on SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let idle_scanner_handle =
        aionui_ai_agent::start_idle_scanner(services.worker_task_manager.clone(), shutdown_rx, None, None);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        })
        .await?;

    // Wait for the scanner to observe the shutdown watch value and
    // return; at worst this blocks for the current 60 s tick.
    if let Err(e) = idle_scanner_handle.await {
        warn!(error = %e, "idle scanner join failed");
    }

    services.database.close().await;
    info!("Server shut down gracefully");

    // Prevent the log guard from being dropped before final log flush.
    drop(env);

    Ok(ExitCode::SUCCESS)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            info!("Received SIGINT, shutting down...");
        }
        () = terminate => {
            info!("Received SIGTERM, shutting down...");
        }
    }
}
