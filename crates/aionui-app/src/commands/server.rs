//! `aioncore` (no subcommand): the main HTTP server.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing::{info, warn};

use aionui_api_types::{RuntimeStatusScope, RuntimeStatusScopeKind};
use aionui_app::{AppConfig, AppServices, create_router};
use aionui_system::RuntimePrepareService;

use crate::bootstrap::ServerEnvironment;

const LISTENING_EVENT_PREFIX: &str = "AIONCORE_LISTENING";
const DYNAMIC_BACKEND_BIND_MAX_ATTEMPTS: usize = 50;

pub(crate) struct BoundHttpListener {
    listener: TcpListener,
    addr: SocketAddr,
}

/// Bind the main HTTP listener before constructing services that may start
/// their own local listeners. When `config.port == 0`, the OS-selected port is
/// written back to the config before downstream services are built.
pub(crate) async fn bind_http_listener(config: &mut AppConfig) -> Result<BoundHttpListener> {
    if config.port != 0 && is_fetch_forbidden_backend_port(config.port) {
        anyhow::bail!("backend port {} is blocked by Fetch clients", config.port);
    }

    let dynamic_port = config.port == 0;
    let max_attempts = if dynamic_port {
        DYNAMIC_BACKEND_BIND_MAX_ATTEMPTS
    } else {
        1
    };

    for attempt in 1..=max_attempts {
        let addr = config.socket_addr();
        info!(address = %addr, attempt, "startup: socket bind started");
        let listener = TcpListener::bind(&addr).await?;
        let local_addr = listener.local_addr()?;

        if dynamic_port && is_fetch_forbidden_backend_port(local_addr.port()) {
            warn!(
                port = local_addr.port(),
                attempt, "startup: skipped Fetch-forbidden dynamic backend port"
            );
            continue;
        }

        config.port = local_addr.port();
        info!(address = %local_addr, "startup: socket bind completed");
        emit_listening_event(local_addr);

        return Ok(BoundHttpListener {
            listener,
            addr: local_addr,
        });
    }

    anyhow::bail!("failed to bind a Fetch-compatible dynamic backend port");
}

fn is_fetch_forbidden_backend_port(port: u16) -> bool {
    matches!(
        port,
        1 | 7
            | 9
            | 11
            | 13
            | 15
            | 17
            | 19
            | 20
            | 21
            | 22
            | 23
            | 25
            | 37
            | 42
            | 43
            | 53
            | 69
            | 77
            | 79
            | 87
            | 95
            | 101
            | 102
            | 103
            | 104
            | 109
            | 110
            | 111
            | 113
            | 115
            | 117
            | 119
            | 123
            | 135
            | 137
            | 139
            | 143
            | 161
            | 179
            | 389
            | 427
            | 465
            | 512
            | 513
            | 514
            | 515
            | 526
            | 530
            | 531
            | 532
            | 540
            | 548
            | 554
            | 556
            | 563
            | 587
            | 601
            | 636
            | 989
            | 990
            | 993
            | 995
            | 1719
            | 1720
            | 1723
            | 2049
            | 3659
            | 4045
            | 5060
            | 5061
            | 6000
            | 6566
            | 6665
            | 6666
            | 6667
            | 6668
            | 6669
            | 6697
            | 10080
    )
}

fn format_listening_event(addr: SocketAddr) -> String {
    let payload = serde_json::json!({
        "host": addr.ip().to_string(),
        "port": addr.port(),
    });
    format!("{LISTENING_EVENT_PREFIX} {payload}")
}

fn emit_listening_event(addr: SocketAddr) {
    println!("{}", format_listening_event(addr));
    let _ = io::stdout().flush();
}

/// Start the HTTP server with fully constructed services.
pub(crate) async fn run_server(
    env: ServerEnvironment,
    services: AppServices,
    bound: BoundHttpListener,
) -> Result<ExitCode> {
    let boot = Instant::now();

    let has_users = services.user_repo.has_users().await?;
    if !has_users {
        info!("No configured users detected — initial setup required via /api/auth/status");
    }

    let router = create_router(&services).await;
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: router ready for bound socket"
    );
    let listener = bound.listener;
    let addr = bound.addr;
    info!(elapsed_ms = boot.elapsed().as_millis(), "Server listening on {addr}");

    let runtime_prepare_service = RuntimePrepareService::new(services.event_bus.clone());
    tokio::spawn(async move {
        let scope = RuntimeStatusScope {
            kind: RuntimeStatusScopeKind::CustomAgent,
            id: "startup".into(),
        };
        let prepare_started = Instant::now();
        info!("startup: managed runtime background preparation started");
        let result = async {
            runtime_prepare_service.ensure_node_runtime(scope.clone()).await?;
            runtime_prepare_service
                .ensure_managed_acp_tool(scope.clone(), "codex-acp")
                .await?;
            runtime_prepare_service
                .ensure_managed_acp_tool(scope, "claude-agent-acp")
                .await?;
            Ok::<(), aionui_common::AppError>(())
        }
        .await;

        match result {
            Ok(()) => info!(
                prepare_elapsed_ms = prepare_started.elapsed().as_millis(),
                "startup: managed runtime background preparation completed"
            ),
            Err(error) => warn!(
                prepare_elapsed_ms = prepare_started.elapsed().as_millis(),
                error = %error,
                "startup: managed runtime background preparation failed"
            ),
        }
    });

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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use aionui_app::AppConfig;

    use super::*;

    #[test]
    fn listening_event_line_is_machine_readable() {
        let addr: SocketAddr = "127.0.0.1:49153".parse().unwrap();

        let line = format_listening_event(addr);

        let payload = line
            .strip_prefix("AIONCORE_LISTENING ")
            .expect("line should start with the listening event prefix");
        let parsed: serde_json::Value = serde_json::from_str(payload).expect("payload should be valid JSON");
        assert_eq!(parsed["host"], "127.0.0.1");
        assert_eq!(parsed["port"], 49153);
    }

    #[test]
    fn fetch_forbidden_backend_ports_are_rejected() {
        assert!(is_fetch_forbidden_backend_port(1720));
        assert!(is_fetch_forbidden_backend_port(10080));
        assert!(!is_fetch_forbidden_backend_port(49153));
    }

    #[tokio::test]
    async fn bind_http_listener_updates_dynamic_port_config() {
        let mut config = AppConfig {
            port: 0,
            ..AppConfig::default()
        };

        let bound = bind_http_listener(&mut config).await.expect("bind should succeed");

        assert!(config.port > 0);
        assert_eq!(config.port, bound.addr.port());
    }
}
