//! The monitor actor — a single sequential worker owning the runtime [`Shard`].
//!
//! Desktop runs one actor (N = 1): it owns the shard (tree + subscriptions),
//! the debounce buffer, and the `file:` runtime, and drives every state change
//! serially. Its event loop multiplexes four sources — inbound WS frames,
//! watcher raw events, the debounce-flush timer, and the grace-reap timer — so
//! the stage-0 pure cores (`Shard::handle` / `Debouncer` / `raw_to_command`)
//! run against a real clock without any timers living in their unit tests.
//!
//! Request dispatch (initialize / fs commands) lives in [`super::dispatch`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Interval, interval};

use crate::ProjectService;
use crate::runtime::{
    Command, Debouncer, FsError, FsRuntimeRegistry, IFsRuntime, LocalFsRuntime, RawEvent, Shard, ShardOutput, TreeModel,
};

use super::port::{FsInbound, FsWirePush};
use super::wire;

/// Debounce-flush cadence: a burst of watcher events within this window
/// coalesces into one apply per canonical (see `runtime.md` pipeline).
const DEBOUNCE_FLUSH_MS: u64 = 200;

/// Grace-reap cadence: how often expired warm nodes are swept. Coarser than the
/// grace TTL (`GRACE_TTL_MS`, 5 min) — a minute of slack on eviction is fine.
const REAP_INTERVAL_MS: u64 = 60_000;

/// The monitor actor: owns the shard, debounce buffer, `file:` runtime handle
/// (for command IO), the resolve-reference service, and the outbound push port.
pub struct FsMonitorActor {
    shard: Shard,
    debouncer: Debouncer,
    /// Kept alongside the shard's registry-owned copy so commands
    /// (read/write/...) reach the provider without going through the tree.
    runtime: Arc<dyn IFsRuntime>,
    project: Arc<ProjectService>,
    push: Arc<dyn FsWirePush>,
    /// Monotonic origin; `now()` is elapsed millis (immune to wall-clock jumps).
    clock: Instant,
}

impl FsMonitorActor {
    /// Build the actor and the watcher raw-event receiver its loop consumes.
    /// Registers a single `file:` runtime (desktop N = 1).
    pub fn new(
        project: Arc<ProjectService>,
        push: Arc<dyn FsWirePush>,
        warm_budget: usize,
    ) -> Result<(Self, UnboundedReceiver<RawEvent>), FsError> {
        let (runtime, raw_rx) = LocalFsRuntime::new()?;
        let runtime: Arc<dyn IFsRuntime> = Arc::new(runtime);
        let mut registry = FsRuntimeRegistry::new();
        registry.register("file", Arc::clone(&runtime));
        let shard = Shard::new(TreeModel::new(registry), warm_budget);
        let actor = Self {
            shard,
            debouncer: Debouncer::new(),
            runtime,
            project,
            push,
            clock: Instant::now(),
        };
        Ok((actor, raw_rx))
    }

    /// Provider handle for command-path IO (read/write/mkdir/remove/rename).
    pub(super) fn runtime(&self) -> &dyn IFsRuntime {
        self.runtime.as_ref()
    }

    /// Resolve-reference service (identity + lexical containment).
    pub(super) fn project(&self) -> &ProjectService {
        &self.project
    }

    /// Outbound push port.
    pub(super) fn push(&self, session: &str, frame: serde_json::Value) {
        self.push.push(session, frame);
    }

    /// Current logical time: monotonic millis since actor start.
    pub(super) fn now(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    /// Feed a command through the shard and return its raw outputs (no fan-out).
    /// Used by request dispatch that must place snapshots in a reply rather than
    /// push them as notifications (e.g. `fs/subscribe`).
    pub(super) async fn shard_handle(&mut self, command: Command) -> Result<Vec<ShardOutput>, FsError> {
        self.shard.handle(command).await
    }

    /// Feed a command through the shard and fan any outputs out to the wire.
    /// Errors are logged, never fatal to the loop.
    pub(super) async fn drive(&mut self, command: Command) {
        // Lifecycle/flow trace. Overflow (kernel dropped events → rescan) is a
        // low-volume, production-diagnostic boundary → info; the affected watched
        // dir (an absolute uri) stays at debug. High-frequency apply → debug.
        match &command {
            Command::Overflow { canonical } => {
                tracing::info!("fs overflow: watcher dropped events, rescanning watched dir");
                tracing::debug!(canonical = %canonical, "fs overflow rescan target");
            }
            Command::Apply { canonical, .. } => tracing::debug!(canonical = %canonical, "fs apply"),
            _ => {}
        }
        match self.shard.handle(command).await {
            Ok(outputs) => self.fan_out(outputs),
            Err(err) => tracing::warn!(error = %err, "fs monitor: shard command failed"),
        }
    }

    /// Run the event loop until the inbound channel closes. Consumes `self`.
    pub async fn run(mut self, mut inbound: UnboundedReceiver<FsInbound>, mut raw_rx: UnboundedReceiver<RawEvent>) {
        let mut flush: Interval = interval(Duration::from_millis(DEBOUNCE_FLUSH_MS));
        let mut reap: Interval = interval(Duration::from_millis(REAP_INTERVAL_MS));
        loop {
            tokio::select! {
                inbound_event = inbound.recv() => match inbound_event {
                    Some(event) => self.on_inbound(event).await,
                    // All senders dropped (router + app shutdown) → stop.
                    None => break,
                },
                // The watcher's sender lives inside `self.runtime`, so this
                // receiver stays open for the actor's whole life.
                raw = raw_rx.recv() => if let Some(raw) = raw {
                    self.debouncer.push(raw);
                },
                _ = flush.tick() => self.flush_debounced().await,
                _ = reap.tick() => self.drive(Command::ReapTick { now: self.now() }).await,
            }
        }
    }

    /// Handle one inbound transport event.
    async fn on_inbound(&mut self, event: FsInbound) {
        match event {
            FsInbound::Frame { session, frame } => self.dispatch_frame(&session, frame).await,
            FsInbound::Disconnect { session } => {
                // Connection teardown — lifecycle boundary; releases the session's
                // subscriptions (nodes go warm, reaper unmounts later).
                tracing::info!(session = %session, "fs session disconnect");
                let now = self.now();
                self.drive(Command::DropSession { session, now }).await;
            }
        }
    }

    /// Drain the debounce buffer into coalesced apply/overflow commands.
    async fn flush_debounced(&mut self) {
        if self.debouncer.is_empty() {
            return;
        }
        for command in self.debouncer.drain() {
            self.drive(command).await;
        }
    }

    /// Translate canonical-domain shard outputs into pe-keyed notifications and
    /// unicast each to its subscriber (scoped push — never a broadcast).
    fn fan_out(&self, outputs: Vec<ShardOutput>) {
        for output in outputs {
            match output {
                ShardOutput::Snapshot { subscribers, snapshot } => {
                    // High-frequency fan-out → debug; counts + subscriber count only.
                    tracing::debug!(
                        subscribers = subscribers.len(),
                        entries = snapshot.entries.len(),
                        "fs snapshot fan-out"
                    );
                    for sub in &subscribers {
                        let target = wire::ResourceRef {
                            pe_id: sub.pe_id.clone(),
                            relative_path: sub.rel.clone(),
                        };
                        let params = wire::snapshot_params(&snapshot, &target);
                        self.push.push(&sub.session, wire::notification("fs/snapshot", params));
                    }
                }
                ShardOutput::Delta { subscribers, delta } => {
                    tracing::debug!(
                        subscribers = subscribers.len(),
                        changes = delta.changes.len(),
                        "fs delta fan-out"
                    );
                    for sub in &subscribers {
                        let target = wire::ResourceRef {
                            pe_id: sub.pe_id.clone(),
                            relative_path: sub.rel.clone(),
                        };
                        let params = wire::delta_params(&delta, &target);
                        self.push.push(&sub.session, wire::notification("fs/delta", params));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "actor_test.rs"]
mod actor_test;
