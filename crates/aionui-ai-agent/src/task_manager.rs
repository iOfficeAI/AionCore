use std::sync::Arc;

use aionui_common::{
    AgentKillReason, AgentType, ConversationStatus, ErrorChain, OnConversationDelete, TimestampMs, now_ms,
};
use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::future::{BoxFuture, join_all};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use crate::active_lease::ActiveLeaseRegistry;
use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::types::{BuildTaskOptions, RuntimeCapabilities};

/// Factory function that creates an [`AgentInstance`] from build options.
///
/// Async so the factory can do real I/O (spawn a CLI process, negotiate the
/// ACP initialize handshake, etc.) without needing to `block_on` inside the
/// `IWorkerTaskManager` call site. Returning `BoxFuture` keeps the trait
/// object-safe for DI.
pub type AgentFactory =
    Arc<dyn Fn(BuildTaskOptions) -> BoxFuture<'static, Result<AgentInstance, AgentError>> + Send + Sync>;

/// Manages the lifecycle of active Agent tasks.
///
/// Each conversation has at most one active task (keyed by conversation ID).
/// The trait is object-safe for dependency injection.
#[async_trait]
pub trait IWorkerTaskManager: Send + Sync {
    /// Get an existing task by conversation ID.
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance>;

    /// Get an existing task or build a new one if none exists.
    ///
    /// Concurrent callers with the same `conversation_id` block on a shared
    /// [`OnceCell`] so the factory runs at most once per conversation —
    /// avoiding the race where two concurrent HTTP requests (e.g.
    /// `/messages` + `/runtime/ensure`) would each spawn their own CLI process and
    /// ACP connection, with one of them leaking.
    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError>;

    /// Kill and remove a task.
    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AgentError>;

    /// Kill a task and return a future that resolves when the process has terminated.
    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

    /// Kill, remove, and wait for all active tasks to stop.
    async fn clear(&self);

    /// Number of active tasks (useful for diagnostics).
    fn active_count(&self) -> usize;

    /// Collect tasks eligible for idle cleanup.
    ///
    /// Returns conversation IDs of tasks that:
    /// - are ACP agents
    /// - have `status == None` or `status == Some(Finished)`
    /// - have been idle longer than `idle_threshold_ms`
    /// - are not protected by an active foreground lease
    fn collect_idle(&self, idle_threshold_ms: TimestampMs) -> Vec<String>;
}

#[derive(Clone)]
struct ManagedAgentTask {
    agent: AgentInstance,
    runtime_capabilities: RuntimeCapabilities,
}

/// Per-conversation slot: an [`OnceCell`] that the first concurrent caller
/// initialises by running the factory, and that every subsequent caller
/// awaits. Failed initialisations leave the cell empty so the next caller
/// may retry; the slot itself is only removed on `kill` / `clear`.
type TaskSlot = Arc<OnceCell<ManagedAgentTask>>;

/// Default implementation of [`IWorkerTaskManager`] using a concurrent hash map.
pub struct WorkerTaskManagerImpl {
    tasks: DashMap<String, TaskSlot>,
    factory: AgentFactory,
    active_leases: Arc<ActiveLeaseRegistry>,
}

impl WorkerTaskManagerImpl {
    pub fn new(factory: AgentFactory) -> Self {
        Self::new_with_active_leases(factory, Arc::new(ActiveLeaseRegistry::new()))
    }

    pub fn new_with_active_leases(factory: AgentFactory, active_leases: Arc<ActiveLeaseRegistry>) -> Self {
        Self {
            tasks: DashMap::new(),
            factory,
            active_leases,
        }
    }

    /// Look up a fully-initialised instance by conversation id.
    fn initialised_instance(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.tasks
            .get(conversation_id)
            .and_then(|slot| slot.get().map(|managed| managed.agent.clone()))
    }

    fn initialised_managed_task(&self, conversation_id: &str) -> Option<ManagedAgentTask> {
        self.tasks.get(conversation_id).and_then(|slot| slot.get().cloned())
    }
}

#[async_trait]
impl IWorkerTaskManager for WorkerTaskManagerImpl {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.initialised_instance(conversation_id)
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        if let Some(existing) = self.initialised_managed_task(conversation_id)
            && !existing.runtime_capabilities.satisfies(&options.runtime_capabilities)
        {
            info!(
                conversation_id,
                "Rebuilding agent task because runtime capabilities changed"
            );
            self.kill(conversation_id, Some(AgentKillReason::RuntimeCapabilityChanged))?;
        }

        // Atomically obtain the per-conversation slot. `DashMap::entry` is
        // synchronous and side-effect-free — only an empty OnceCell is
        // allocated on the miss path, so concurrent callers for the same id
        // all end up holding the same `Arc<OnceCell>`.
        let slot: TaskSlot = self
            .tasks
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();

        // `OnceCell::get_or_try_init` serialises concurrent initialisers:
        // the first caller to reach it runs the factory, every other caller
        // awaits the same future and ends up with the same instance. On
        // failure the cell stays empty so a later caller can retry.
        let factory = self.factory.clone();
        let runtime_capabilities = options.runtime_capabilities.clone();
        let managed = slot
            .get_or_try_init(|| async move {
                let agent = factory(options).await?;
                Ok::<ManagedAgentTask, AgentError>(ManagedAgentTask {
                    agent,
                    runtime_capabilities,
                })
            })
            .await?;
        Ok(managed.agent.clone())
    }

    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        if let Some((id, slot)) = self.tasks.remove(conversation_id) {
            let agent_type = slot.get().map(|managed| managed.agent.agent_type());
            if matches!(reason, Some(AgentKillReason::IdleTimeout)) {
                info!(
                    conversation_id = %id,
                    ?agent_type,
                    reason = %"IdleTimeout",
                    "Idle kill: task removed from manager"
                );
            } else {
                info!(conversation_id = %id, ?reason, "Killing agent task");
            }
            if let Some(managed) = slot.get() {
                managed.agent.kill(reason)?;
            }
        }
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        if let Some((id, slot)) = self.tasks.remove(conversation_id) {
            let agent_type = slot.get().map(|managed| managed.agent.agent_type());
            if matches!(reason, Some(AgentKillReason::IdleTimeout)) {
                info!(
                    conversation_id = %id,
                    ?agent_type,
                    reason = %"IdleTimeout",
                    "Idle kill: task removed from manager"
                );
            } else {
                info!(conversation_id = %id, ?reason, "Killing agent task (awaitable)");
            }
            if let Some(managed) = slot.get() {
                return managed.agent.kill_and_wait(reason);
            }
            return Box::pin(async move {
                match slot
                    .get_or_try_init(|| async { Err(AgentError::internal("task slot removed before initialization")) })
                    .await
                {
                    Ok(managed) => managed.agent.clone().kill_and_wait(reason).await,
                    Err(error) => {
                        debug!(
                            conversation_id = %id,
                            ?reason,
                            error = %ErrorChain(&error),
                            "Kill requested for task slot that did not finish initialization"
                        );
                    }
                }
            });
        }
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        let keys: Vec<String> = self.tasks.iter().map(|r| r.key().clone()).collect();
        let mut waits = Vec::new();
        for key in keys {
            if let Some((id, slot)) = self.tasks.remove(&key) {
                info!(conversation_id = %id, "Clearing agent task");
                if let Some(managed) = slot.get() {
                    waits.push(managed.agent.kill_and_wait(None));
                }
            }
        }
        join_all(waits).await;
    }

    fn active_count(&self) -> usize {
        self.tasks.iter().filter(|entry| entry.value().get().is_some()).count()
    }

    fn collect_idle(&self, idle_threshold_ms: TimestampMs) -> Vec<String> {
        let now = now_ms();
        self.tasks
            .iter()
            .filter_map(|entry| {
                let agent = &entry.value().get()?.agent;
                let agent_type = agent.agent_type();
                let status = agent.status();
                let last_activity_at = agent.last_activity_at();
                let idle_ms = now.saturating_sub(last_activity_at);

                if agent_type != AgentType::Acp
                    || !matches!(status, None | Some(ConversationStatus::Finished))
                    || idle_ms <= idle_threshold_ms
                {
                    return None;
                }

                if let Some(expires_at) = self.active_leases.active_until(entry.key()) {
                    debug!(
                        conversation_id = %entry.key(),
                        ?status,
                        idle_ms,
                        lease_expires_in_ms = expires_at.saturating_sub(now),
                        reason = %"ActiveLease",
                        "Idle scan: active lease protects idle agent"
                    );
                    return None;
                }

                let idle_class = if status.is_none() { "WarmupOnly" } else { "Finished" };
                info!(
                    conversation_id = %entry.key(),
                    ?agent_type,
                    ?status,
                    idle_ms,
                    threshold_ms = idle_threshold_ms,
                    idle_class = %idle_class,
                    reason = %"IdleTimeout",
                    "Idle scan: selected idle agent"
                );
                Some(entry.key().clone())
            })
            .collect()
    }
}

/// Wired up by `aionui-app` so deleting a conversation tears down its
/// agent process. Without this hook, ACP/aionrs subprocesses keep
/// streaming events for a `conversation_id` whose DB row is already gone
/// (Sentry ELECTRON-1BD).
#[async_trait]
impl OnConversationDelete for WorkerTaskManagerImpl {
    async fn on_conversation_deleted(&self, conversation_id: &str) {
        if let Err(e) = self.kill(conversation_id, Some(AgentKillReason::ConversationDeleted)) {
            warn!(
                conversation_id,
                error = %ErrorChain(&e),
                "Failed to kill agent task on conversation delete (non-fatal)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{IAgentTask, IMockAgent};
    use crate::protocol::events::AgentStreamEvent;
    use crate::session_context::{
        AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, ConversationContext, WorkspaceContext,
    };
    use crate::types::{CONVERSATION_RUNTIME_CONTEXT_VERSION, SendMessageData};
    use aionui_common::{AgentKillReason, AgentType, ConversationStatus, ProviderWithModel};
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicI64, Ordering};
    use tokio::sync::broadcast;

    /// A minimal mock agent for testing task manager logic. Lives behind
    /// the `AgentInstance::Mock` trait-object variant so we don't have to
    /// stand up a real `AcpAgentManager` just to exercise lifecycle
    /// dispatch.
    struct MockAgent {
        agent_type: AgentType,
        conversation_id: String,
        workspace: String,
        status: Option<ConversationStatus>,
        last_activity: AtomicI64,
        killed: Arc<std::sync::atomic::AtomicUsize>,
        event_tx: broadcast::Sender<AgentStreamEvent>,
    }

    impl MockAgent {
        fn new(conversation_id: &str, status: Option<ConversationStatus>) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                agent_type: AgentType::Acp,
                conversation_id: conversation_id.to_owned(),
                workspace: "/tmp/test".to_owned(),
                status,
                last_activity: AtomicI64::new(now_ms()),
                killed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                event_tx,
            }
        }

        fn with_kill_counter(mut self, killed: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            self.killed = killed;
            self
        }

        fn with_agent_type(mut self, t: AgentType) -> Self {
            self.agent_type = t;
            self
        }

        fn with_last_activity(mut self, ts: TimestampMs) -> Self {
            self.last_activity = AtomicI64::new(ts);
            self
        }
    }

    #[async_trait::async_trait]
    impl IAgentTask for MockAgent {
        fn agent_type(&self) -> AgentType {
            self.agent_type
        }
        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }
        fn workspace(&self) -> &str {
            &self.workspace
        }
        fn status(&self) -> Option<ConversationStatus> {
            self.status
        }
        fn last_activity_at(&self) -> TimestampMs {
            self.last_activity.load(Ordering::Relaxed)
        }
        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }
        async fn send_message(
            &self,
            _data: SendMessageData,
        ) -> Result<(), crate::protocol::send_error::AgentSendError> {
            Ok(())
        }
        async fn cancel(&self) -> Result<(), AgentError> {
            Ok(())
        }
        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl IMockAgent for MockAgent {}

    fn make_options(conversation_id: &str) -> BuildTaskOptions {
        BuildTaskOptions::new(AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: conversation_id.into(),
                user_id: "user-1".into(),
                agent_type: AgentType::Acp,
                source: None,
            },
            workspace: WorkspaceContext {
                path: "/tmp/test".into(),
                stored_path: "/tmp/test".into(),
                is_custom: true,
            },
            model: ProviderWithModel {
                provider_id: "p1".into(),
                model: "test".into(),
                use_model: None,
            },
            skills: vec![],
            runtime_env: vec![],
            team: None,
            kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
                config: Default::default(),
                team: None,
                belongs_to_team: false,
                session_id: None,
                session_snapshot: None,
            })),
        })
    }

    fn mock_instance(agent: MockAgent) -> AgentInstance {
        AgentInstance::Mock(Arc::new(agent))
    }

    fn managed_instance(agent: AgentInstance) -> ManagedAgentTask {
        ManagedAgentTask {
            agent,
            runtime_capabilities: RuntimeCapabilities::default(),
        }
    }

    fn make_manager() -> WorkerTaskManagerImpl {
        let factory: AgentFactory = Arc::new(|opts: BuildTaskOptions| {
            async move { Ok(mock_instance(MockAgent::new(opts.conversation_id(), None))) }.boxed()
        });
        WorkerTaskManagerImpl::new(factory)
    }

    fn capture_logs(max_level: tracing::Level, f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };

        let subscriber = fmt::Subscriber::builder()
            .with_max_level(max_level)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    /// Two [`AgentInstance`]s point to the same underlying agent iff they
    /// share an `Arc` — check by pointer identity on the inner trait object.
    fn same_mock(a: &AgentInstance, b: &AgentInstance) -> bool {
        match (a, b) {
            (AgentInstance::Mock(x), AgentInstance::Mock(y)) => Arc::ptr_eq(x, y),
            _ => false,
        }
    }

    #[test]
    fn get_task_returns_none_when_empty() {
        let mgr = make_manager();
        assert!(mgr.get_task("nonexistent").is_none());
    }

    #[tokio::test]
    async fn get_or_build_creates_task() {
        let mgr = make_manager();
        let instance = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(instance.conversation_id(), "conv-1");
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_or_build_returns_existing() {
        let mgr = make_manager();
        let h1 = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        let h2 = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert!(same_mock(&h1, &h2));
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_or_build_rebuilds_when_existing_task_lacks_requested_runtime_context_capability() {
        let mgr = make_manager();
        let h1 = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        let mut options = make_options("conv-1");
        options.runtime_capabilities.conversation_runtime_context_version = Some(CONVERSATION_RUNTIME_CONTEXT_VERSION);

        let h2 = mgr.get_or_build_task("conv-1", options).await.unwrap();

        assert!(!same_mock(&h1, &h2));
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_or_build_is_single_flight_under_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = Arc::clone(&calls);
        let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
            let calls = Arc::clone(&calls_for_factory);
            async move {
                // Simulate a slow build (CLI spawn + initialize handshake).
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(mock_instance(MockAgent::new(opts.conversation_id(), None)))
            }
            .boxed()
        });
        let mgr = Arc::new(WorkerTaskManagerImpl::new(factory));

        // Ten concurrent callers all racing on the same conversation id.
        let mut joins = Vec::new();
        for _ in 0..10 {
            let mgr = Arc::clone(&mgr);
            joins.push(tokio::spawn(async move {
                mgr.get_or_build_task("conv-race", make_options("conv-race")).await
            }));
        }
        let handles: Vec<_> = futures_util::future::join_all(joins)
            .await
            .into_iter()
            .map(|r| r.unwrap().unwrap())
            .collect();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "factory must run only once");
        assert_eq!(mgr.active_count(), 1);
        for h in handles.iter().skip(1) {
            assert!(same_mock(&handles[0], h), "all callers see the same handle");
        }
    }

    #[tokio::test]
    async fn get_or_build_retries_after_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fail_next = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&fail_next);
        let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
            let flag = Arc::clone(&flag);
            async move {
                if flag.swap(false, Ordering::SeqCst) {
                    Err(AgentError::internal("first call fails"))
                } else {
                    Ok(mock_instance(MockAgent::new(opts.conversation_id(), None)))
                }
            }
            .boxed()
        });
        let mgr = WorkerTaskManagerImpl::new(factory);

        // First call fails, slot stays empty.
        assert!(mgr.get_or_build_task("conv-1", make_options("conv-1")).await.is_err());
        // Second call retries and succeeds.
        let h = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(h.conversation_id(), "conv-1");
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn kill_and_wait_waits_for_in_flight_build_and_kills_result() {
        let build_started = Arc::new(tokio::sync::Notify::new());
        let release_build = Arc::new(tokio::sync::Notify::new());
        let killed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let factory: AgentFactory = Arc::new({
            let build_started = Arc::clone(&build_started);
            let release_build = Arc::clone(&release_build);
            let killed = Arc::clone(&killed);
            move |opts: BuildTaskOptions| {
                let build_started = Arc::clone(&build_started);
                let release_build = Arc::clone(&release_build);
                let killed = Arc::clone(&killed);
                async move {
                    build_started.notify_one();
                    release_build.notified().await;
                    Ok(mock_instance(
                        MockAgent::new(opts.conversation_id(), None).with_kill_counter(killed),
                    ))
                }
                .boxed()
            }
        });
        let mgr = Arc::new(WorkerTaskManagerImpl::new(factory));

        let build = {
            let mgr = Arc::clone(&mgr);
            tokio::spawn(async move { mgr.get_or_build_task("conv-1", make_options("conv-1")).await })
        };
        build_started.notified().await;

        let wait = mgr.kill_and_wait("conv-1", Some(AgentKillReason::TeamMcpRebuild));
        let wait_task = tokio::spawn(async move {
            wait.await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !wait_task.is_finished(),
            "kill_and_wait must wait for the in-flight build before returning"
        );

        release_build.notify_one();
        build.await.unwrap().unwrap();
        wait_task.await.unwrap();

        assert_eq!(killed.load(Ordering::SeqCst), 1);
        assert_eq!(mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn get_task_finds_existing() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        let handle = mgr.get_task("conv-1");
        assert!(handle.is_some());
        assert_eq!(handle.unwrap().conversation_id(), "conv-1");
    }

    #[tokio::test]
    async fn kill_removes_task() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(mgr.active_count(), 1);

        mgr.kill("conv-1", Some(AgentKillReason::IdleTimeout)).unwrap();
        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.get_task("conv-1").is_none());
    }

    #[test]
    fn kill_nonexistent_is_ok() {
        let factory: AgentFactory = Arc::new(|_| async { unreachable!() }.boxed());
        let mgr = WorkerTaskManagerImpl::new(factory);
        assert!(mgr.kill("nothing", None).is_ok());
    }

    #[tokio::test]
    async fn clear_removes_all() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        mgr.get_or_build_task("conv-2", make_options("conv-2")).await.unwrap();
        assert_eq!(mgr.active_count(), 2);

        mgr.clear().await;
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn collect_idle_finds_finished_and_warmup_only_stale_acp_tasks() {
        let factory: AgentFactory = Arc::new(|_| async { unreachable!() }.boxed());
        let mgr = WorkerTaskManagerImpl::new(factory);

        // Helper: insert a pre-initialised slot bypassing the async factory path.
        let insert = |id: &str, instance: AgentInstance| {
            let cell: OnceCell<ManagedAgentTask> = OnceCell::new();
            cell.set(managed_instance(instance)).ok();
            mgr.tasks.insert(id.into(), Arc::new(cell));
        };

        // ACP + Finished + old activity → should be collected
        insert(
            "conv-stale",
            mock_instance(
                MockAgent::new("conv-stale", Some(ConversationStatus::Finished)).with_last_activity(now_ms() - 600_000),
            ),
        );

        // ACP + warmup-only + old activity → should be collected
        insert(
            "conv-warmup-only",
            mock_instance(MockAgent::new("conv-warmup-only", None).with_last_activity(now_ms() - 600_000)),
        );

        // ACP + Finished + recent activity → should NOT be collected
        insert(
            "conv-recent",
            mock_instance(
                MockAgent::new("conv-recent", Some(ConversationStatus::Finished)).with_last_activity(now_ms()),
            ),
        );

        // ACP + Running + old activity → should NOT be collected
        insert(
            "conv-running",
            mock_instance(
                MockAgent::new("conv-running", Some(ConversationStatus::Running))
                    .with_last_activity(now_ms() - 600_000),
            ),
        );

        // Non-ACP (Aionrs) + Finished + old activity → should NOT be collected
        insert(
            "conv-aionrs",
            mock_instance(
                MockAgent::new("conv-aionrs", Some(ConversationStatus::Finished))
                    .with_agent_type(AgentType::Aionrs)
                    .with_last_activity(now_ms() - 600_000),
            ),
        );

        let idle = mgr.collect_idle(300_000); // 5-min threshold
        assert_eq!(idle.len(), 2);
        assert!(idle.contains(&"conv-stale".to_owned()));
        assert!(idle.contains(&"conv-warmup-only".to_owned()));
    }

    #[test]
    fn collect_idle_skips_tasks_with_active_lease() {
        let active_leases = Arc::new(crate::ActiveLeaseRegistry::new());
        active_leases.renew("conv-active");
        let factory: AgentFactory = Arc::new(|_| async { unreachable!() }.boxed());
        let mgr = WorkerTaskManagerImpl::new_with_active_leases(factory, active_leases);

        let cell: OnceCell<ManagedAgentTask> = OnceCell::new();
        cell.set(managed_instance(mock_instance(
            MockAgent::new("conv-active", Some(ConversationStatus::Finished)).with_last_activity(now_ms() - 600_000),
        )))
        .ok();
        mgr.tasks.insert("conv-active".into(), Arc::new(cell));

        let captured = capture_logs(tracing::Level::DEBUG, || {
            let idle = mgr.collect_idle(300_000);
            assert!(idle.is_empty());
        });

        assert!(captured.contains("reason=ActiveLease"));
        assert!(captured.contains("lease_expires_in_ms="));
    }

    #[test]
    fn collect_idle_logs_selected_agent_with_idle_fields() {
        let manager = WorkerTaskManagerImpl::new(Arc::new(|_options| {
            async { Err(AgentError::bad_gateway("not used")) }.boxed()
        }));
        let now = now_ms();
        let agent =
            Arc::new(MockAgent::new("conv_idle", Some(ConversationStatus::Finished)).with_last_activity(now - 10_000));
        let slot = Arc::new(OnceCell::new());
        assert!(slot.set(managed_instance(AgentInstance::Mock(agent))).is_ok());
        manager.tasks.insert("conv_idle".to_owned(), slot);

        let captured = capture_logs(tracing::Level::INFO, || {
            let ids = manager.collect_idle(5_000);
            assert_eq!(ids, vec!["conv_idle".to_owned()]);
        });

        assert!(captured.contains("Idle scan: selected idle agent"));
        assert!(captured.contains("conversation_id=conv_idle"));
        assert!(captured.contains("agent_type=Acp"));
        assert!(captured.contains("status=Some(Finished)"));
        assert!(captured.contains("idle_ms="));
        assert!(captured.contains("threshold_ms=5000"));
        assert!(captured.contains("idle_class=Finished"));
        assert!(captured.contains("reason=IdleTimeout"));
    }

    #[test]
    fn kill_and_wait_logs_idle_task_removed_with_agent_type() {
        let manager = WorkerTaskManagerImpl::new(Arc::new(|_options| {
            async { Err(AgentError::bad_gateway("not used")) }.boxed()
        }));
        let agent = Arc::new(MockAgent::new("conv_idle", Some(ConversationStatus::Finished)));
        let slot = Arc::new(OnceCell::new());
        assert!(slot.set(managed_instance(AgentInstance::Mock(agent))).is_ok());
        manager.tasks.insert("conv_idle".to_owned(), slot);

        let captured = capture_logs(tracing::Level::INFO, || {
            let wait = manager.kill_and_wait("conv_idle", Some(AgentKillReason::IdleTimeout));
            drop(wait);
        });

        assert!(captured.contains("Idle kill: task removed from manager"));
        assert!(captured.contains("conversation_id=conv_idle"));
        assert!(captured.contains("agent_type=Some(Acp)"));
        assert!(captured.contains("reason=IdleTimeout"));
    }

    #[test]
    fn collect_idle_empty_when_no_tasks() {
        let mgr = make_manager();
        let idle = mgr.collect_idle(300_000);
        assert!(idle.is_empty());
    }
}
