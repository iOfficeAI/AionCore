use std::sync::Arc;
use std::time::Duration;

use aionui_common::{AgentKillReason, now_ms};
use async_trait::async_trait;
use tracing::{debug, info};

use crate::task_manager::IWorkerTaskManager;

/// Default idle timeout for ACP agents (5 minutes).
const DEFAULT_IDLE_TIMEOUT_SECS: i64 = 5 * 60;

/// Scan interval for idle agent cleanup (1 minute).
const SCAN_INTERVAL_SECS: u64 = 60;

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
    idle_timeout_secs: Option<i64>,
    scan_interval_secs: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    start_idle_scanner_with_coordinator(
        worker_task_manager,
        shutdown,
        idle_timeout_secs,
        scan_interval_secs,
        None,
    )
}

pub fn start_idle_scanner_with_coordinator(
    worker_task_manager: Arc<dyn IWorkerTaskManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    idle_timeout_secs: Option<i64>,
    scan_interval_secs: Option<u64>,
    idle_cleanup_coordinator: Option<Arc<dyn IdleCleanupCoordinator>>,
) -> tokio::task::JoinHandle<()> {
    let threshold = idle_timeout_secs.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    let scan_interval = scan_interval_secs.unwrap_or(SCAN_INTERVAL_SECS);
    info!(
        threshold_secs = threshold,
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
                        threshold*1000,
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
    threshold_ms: i64,
    idle_cleanup_coordinator: Option<Arc<dyn IdleCleanupCoordinator>>,
) {
    let started_at = now_ms();
    let mut idle_ids = manager.collect_idle(threshold_ms);

    if idle_ids.is_empty() {
        debug!(active_count = manager.active_count(), "Idle scan: no idle agents found");
        return;
    }

    if let Some(coordinator) = idle_cleanup_coordinator {
        let before_count = idle_ids.len();
        idle_ids = coordinator.cleanup_idle_conversations(idle_ids, threshold_ms).await;
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
    }

    impl RecordingTaskManager {
        fn new(idle_ids: Vec<&str>) -> Self {
            Self {
                idle_ids: idle_ids.into_iter().map(str::to_owned).collect(),
                killed: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn killed(&self) -> Vec<String> {
            self.killed.lock().unwrap().clone()
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

        fn collect_idle(&self, _idle_threshold_ms: i64) -> Vec<String> {
            self.idle_ids.clone()
        }
    }

    struct RecordingCoordinator {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl IdleCleanupCoordinator for RecordingCoordinator {
        async fn cleanup_idle_conversations(
            &self,
            idle_conversation_ids: Vec<String>,
            _idle_threshold_ms: i64,
        ) -> Vec<String> {
            self.seen.lock().unwrap().extend(idle_conversation_ids);
            vec!["solo".to_owned()]
        }
    }

    #[tokio::test]
    async fn scan_uses_coordinator_and_default_kills_only_unhandled_ids() {
        let manager_impl = Arc::new(RecordingTaskManager::new(vec!["team-lead", "solo"]));
        let manager: Arc<dyn IWorkerTaskManager> = manager_impl.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let coordinator: Arc<dyn IdleCleanupCoordinator> = Arc::new(RecordingCoordinator { seen: seen.clone() });

        scan_and_cleanup(&manager, 300_000, Some(coordinator)).await;

        assert_eq!(seen.lock().unwrap().clone(), vec!["team-lead", "solo"]);
        assert_eq!(manager_impl.killed(), vec!["solo"]);
    }
}
