use std::sync::Arc;
use std::time::Duration;

use aionui_common::{AgentKillReason, now_ms};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::task_manager::IWorkerTaskManager;

/// Default idle timeout for solo (single-chat) ACP agents (10 minutes).
const DEFAULT_SOLO_IDLE_TIMEOUT_SECS: i64 = 10 * 60;

/// Default idle timeout for team sessions (30 minutes).
const DEFAULT_TEAM_IDLE_TIMEOUT_SECS: i64 = 30 * 60;

/// Default scan interval for idle agent cleanup (1 minute).
const DEFAULT_SCAN_INTERVAL_SECS: u64 = 60;

/// Environment variable overriding the solo (single-chat) idle timeout, in seconds.
const ENV_SOLO_IDLE_TIMEOUT_SECS: &str = "AIONUI_IDLE_TIMEOUT_SECS";
/// Environment variable overriding the team idle timeout, in seconds.
const ENV_TEAM_IDLE_TIMEOUT_SECS: &str = "AIONUI_TEAM_IDLE_TIMEOUT_SECS";
/// Environment variable overriding the idle scan interval, in seconds.
const ENV_IDLE_SCAN_INTERVAL_SECS: &str = "AIONUI_IDLE_SCAN_INTERVAL_SECS";

#[async_trait]
pub trait IdleCleanupCoordinator: Send + Sync {
    async fn cleanup_idle_conversations(
        &self,
        idle_conversation_ids: Vec<String>,
        idle_threshold_ms: i64,
    ) -> Vec<String>;
}

/// Start the background idle agent scanner.
///
/// Periodically scans active tasks and kills ACP agents that have been
/// idle (finished or warmup-only + no activity) beyond the configured threshold.
///
/// The scanner runs until the provided `shutdown` signal resolves.
pub fn start_idle_scanner(
    worker_task_manager: Arc<dyn IWorkerTaskManager>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    solo_timeout_secs: Option<i64>,
    team_timeout_secs: Option<i64>,
    scan_interval_secs: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    start_idle_scanner_with_coordinator(
        worker_task_manager,
        shutdown,
        solo_timeout_secs,
        team_timeout_secs,
        scan_interval_secs,
        None,
    )
}

pub fn start_idle_scanner_with_coordinator(
    worker_task_manager: Arc<dyn IWorkerTaskManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    solo_timeout_secs: Option<i64>,
    team_timeout_secs: Option<i64>,
    scan_interval_secs: Option<u64>,
    idle_cleanup_coordinator: Option<Arc<dyn IdleCleanupCoordinator>>,
) -> tokio::task::JoinHandle<()> {
    let solo_threshold = solo_timeout_secs.unwrap_or(DEFAULT_SOLO_IDLE_TIMEOUT_SECS);
    let team_threshold = team_timeout_secs.unwrap_or(DEFAULT_TEAM_IDLE_TIMEOUT_SECS);
    let scan_interval = scan_interval_secs.unwrap_or(DEFAULT_SCAN_INTERVAL_SECS);
    info!(
        solo_timeout_secs = solo_threshold,
        team_timeout_secs = team_threshold,
        scan_interval_secs = scan_interval,
        "Starting idle agent scanner"
    );

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(scan_interval));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    scan_and_cleanup(
                        &worker_task_manager,
                        solo_threshold * 1000,
                        team_threshold * 1000,
                        idle_cleanup_coordinator.clone(),
                    ).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Idle scanner received shutdown signal");
                        break;
                    }
                }
            }
        }

        info!("Idle scanner stopped");
    })
}

/// Perform one scan: find idle tasks and kill them.
async fn scan_and_cleanup(
    manager: &Arc<dyn IWorkerTaskManager>,
    solo_threshold_ms: i64,
    team_threshold_ms: i64,
    idle_cleanup_coordinator: Option<Arc<dyn IdleCleanupCoordinator>>,
) {
    let started_at = now_ms();
    let mut idle_ids = manager.collect_idle(solo_threshold_ms);

    if idle_ids.is_empty() {
        debug!(active_count = manager.active_count(), "Idle scan: no idle agents found");
        return;
    }

    if let Some(coordinator) = idle_cleanup_coordinator {
        let before_count = idle_ids.len();
        idle_ids = coordinator
            .cleanup_idle_conversations(idle_ids, team_threshold_ms)
            .await;
        let handled_count = before_count.saturating_sub(idle_ids.len());
        if handled_count > 0 {
            info!(
                handled_count,
                remaining_count = idle_ids.len(),
                "Idle scan: coordinator handled idle agents"
            );
        }
    }

    if idle_ids.is_empty() {
        info!(
            elapsed_ms = now_ms().saturating_sub(started_at),
            "Idle scan: cleanup completed by coordinator"
        );
        return;
    }

    let count = idle_ids.len();
    info!(count, "Idle scan: cleaning up idle agents");

    let mut waits = Vec::new();
    for id in idle_ids {
        let manager = Arc::clone(manager);
        waits.push(tokio::spawn(async move {
            info!(conversation_id = %id, "Idle scan: awaiting idle agent shutdown");
            manager.kill_and_wait(&id, Some(AgentKillReason::IdleTimeout)).await;
        }));
    }

    for wait in waits {
        if let Err(error) = wait.await {
            debug!(error = %error, "Idle scan: cleanup task join failed");
        }
    }

    info!(
        count,
        elapsed_ms = now_ms().saturating_sub(started_at),
        "Idle scan: cleanup completed"
    );
}

/// Parse a positive `i64` seconds value, falling back to `default` on
/// missing/invalid input (non-numeric, empty, or `<= 0`).
fn parse_positive_i64(raw: Option<String>, var: &str, default: i64) -> i64 {
    match raw {
        None => default,
        Some(value) => match value.trim().parse::<i64>() {
            Ok(parsed) if parsed > 0 => parsed,
            _ => {
                warn!(env_var = var, value = %value, default, "invalid idle-cleanup env value; using default");
                default
            }
        },
    }
}

/// Parse a positive `u64` seconds value, falling back to `default` on
/// missing/invalid input (non-numeric, empty, or `== 0`).
fn parse_positive_u64(raw: Option<String>, var: &str, default: u64) -> u64 {
    match raw {
        None => default,
        Some(value) => match value.trim().parse::<u64>() {
            Ok(parsed) if parsed > 0 => parsed,
            _ => {
                warn!(env_var = var, value = %value, default, "invalid idle-cleanup env value; using default");
                default
            }
        },
    }
}

/// Resolve idle-cleanup config from raw env values. Pure — takes the raw
/// `Option<String>` values and does not read the environment, so it is unit
/// testable. Returns `(solo_timeout_secs, team_timeout_secs, scan_interval_secs)`.
fn resolve_idle_config(
    solo_raw: Option<String>,
    team_raw: Option<String>,
    scan_raw: Option<String>,
) -> (i64, i64, u64) {
    let solo = parse_positive_i64(solo_raw, ENV_SOLO_IDLE_TIMEOUT_SECS, DEFAULT_SOLO_IDLE_TIMEOUT_SECS);
    let team = parse_positive_i64(team_raw, ENV_TEAM_IDLE_TIMEOUT_SECS, DEFAULT_TEAM_IDLE_TIMEOUT_SECS);
    let scan = parse_positive_u64(scan_raw, ENV_IDLE_SCAN_INTERVAL_SECS, DEFAULT_SCAN_INTERVAL_SECS);
    (solo, team, scan)
}

/// Read the idle-cleanup env vars and resolve the effective config.
/// Returns `(solo_timeout_secs, team_timeout_secs, scan_interval_secs)`.
pub fn resolve_idle_config_from_env() -> (i64, i64, u64) {
    resolve_idle_config(
        std::env::var(ENV_SOLO_IDLE_TIMEOUT_SECS).ok(),
        std::env::var(ENV_TEAM_IDLE_TIMEOUT_SECS).ok(),
        std::env::var(ENV_IDLE_SCAN_INTERVAL_SECS).ok(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use aionui_common::AgentKillReason;
    use async_trait::async_trait;

    use super::*;
    use crate::agent_task::AgentInstance;
    use crate::error::AgentError;
    use crate::types::BuildTaskOptions;

    struct RecordingTaskManager {
        idle_ids: Vec<String>,
        killed: Arc<Mutex<Vec<String>>>,
        collect_threshold: Arc<Mutex<Option<i64>>>,
    }

    impl RecordingTaskManager {
        fn new(idle_ids: Vec<&str>) -> Self {
            Self {
                idle_ids: idle_ids.into_iter().map(str::to_owned).collect(),
                killed: Arc::new(Mutex::new(Vec::new())),
                collect_threshold: Arc::new(Mutex::new(None)),
            }
        }

        fn killed(&self) -> Vec<String> {
            self.killed.lock().unwrap().clone()
        }

        fn collect_threshold(&self) -> Option<i64> {
            *self.collect_threshold.lock().unwrap()
        }
    }

    #[async_trait]
    impl IWorkerTaskManager for RecordingTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
            None
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, AgentError> {
            Err(AgentError::internal("not used"))
        }

        fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            self.killed.lock().unwrap().push(conversation_id.to_owned());
            Ok(())
        }

        fn kill_and_wait(
            &self,
            conversation_id: &str,
            reason: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            let _ = self.kill(conversation_id, reason);
            Box::pin(std::future::ready(()))
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            self.idle_ids.len()
        }

        fn collect_idle(&self, idle_threshold_ms: i64) -> Vec<String> {
            *self.collect_threshold.lock().unwrap() = Some(idle_threshold_ms);
            self.idle_ids.clone()
        }
    }

    struct RecordingCoordinator {
        seen: Arc<Mutex<Vec<String>>>,
        threshold: Arc<Mutex<Option<i64>>>,
    }

    #[async_trait]
    impl IdleCleanupCoordinator for RecordingCoordinator {
        async fn cleanup_idle_conversations(
            &self,
            idle_conversation_ids: Vec<String>,
            idle_threshold_ms: i64,
        ) -> Vec<String> {
            *self.threshold.lock().unwrap() = Some(idle_threshold_ms);
            self.seen.lock().unwrap().extend(idle_conversation_ids);
            vec!["solo".to_owned()]
        }
    }

    #[tokio::test]
    async fn scan_uses_coordinator_and_default_kills_only_unhandled_ids() {
        let manager_impl = Arc::new(RecordingTaskManager::new(vec!["team-lead", "solo"]));
        let manager: Arc<dyn IWorkerTaskManager> = manager_impl.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let coordinator: Arc<dyn IdleCleanupCoordinator> = Arc::new(RecordingCoordinator {
            seen: seen.clone(),
            threshold: Arc::new(Mutex::new(None)),
        });

        scan_and_cleanup(&manager, 600_000, 1_800_000, Some(coordinator)).await;

        assert_eq!(seen.lock().unwrap().clone(), vec!["team-lead", "solo"]);
        assert_eq!(manager_impl.killed(), vec!["solo"]);
    }

    #[tokio::test]
    async fn scan_passes_solo_to_collect_and_team_to_coordinator() {
        let manager_impl = Arc::new(RecordingTaskManager::new(vec!["team-lead", "solo"]));
        let manager: Arc<dyn IWorkerTaskManager> = manager_impl.clone();
        let coordinator_impl = Arc::new(RecordingCoordinator {
            seen: Arc::new(Mutex::new(Vec::new())),
            threshold: Arc::new(Mutex::new(None)),
        });
        let coordinator: Arc<dyn IdleCleanupCoordinator> = coordinator_impl.clone();

        scan_and_cleanup(&manager, 600_000, 1_800_000, Some(coordinator)).await;

        assert_eq!(manager_impl.collect_threshold(), Some(600_000));
        assert_eq!(*coordinator_impl.threshold.lock().unwrap(), Some(1_800_000));
    }

    #[test]
    fn resolve_idle_config_defaults_when_absent() {
        let (solo, team, scan) = resolve_idle_config(None, None, None);
        assert_eq!(solo, 600);
        assert_eq!(team, 1800);
        assert_eq!(scan, 60);
    }

    #[test]
    fn resolve_idle_config_parses_valid_values() {
        let (solo, team, scan) =
            resolve_idle_config(Some("300".to_string()), Some("300".to_string()), Some("10".to_string()));
        assert_eq!(solo, 300);
        assert_eq!(team, 300);
        assert_eq!(scan, 10);
    }

    #[test]
    fn resolve_idle_config_falls_back_on_invalid_values() {
        // negative, zero, non-numeric, empty -> defaults
        let (solo, team, scan) =
            resolve_idle_config(Some("-5".to_string()), Some("0".to_string()), Some("abc".to_string()));
        assert_eq!(solo, 600);
        assert_eq!(team, 1800);
        assert_eq!(scan, 60);

        let (solo2, _, _) = resolve_idle_config(Some("".to_string()), None, None);
        assert_eq!(solo2, 600);
    }
}
