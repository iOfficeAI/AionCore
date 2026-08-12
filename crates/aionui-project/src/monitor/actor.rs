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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Interval, interval};

use crate::ProjectService;
use crate::runtime::{
    Command, Debouncer, FsError, FsRuntimeRegistry, IFsRuntime, LocalFsRuntime, RawEvent, Shard, ShardOutput, TreeModel,
};

use super::port::{FsInbound, FsWirePush, SessionId};
use super::search::{ActiveSearch, SearchDone};
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
    /// In-flight filename search per connection (at most one; a new search on the
    /// same session supersedes the previous). Keyed by session so cancel /
    /// supersede / disconnect can reach the running search's cancel token.
    active_searches: HashMap<SessionId, ActiveSearch>,
    /// Spawned search coordinators signal natural completion here so the loop can
    /// clear the finished `active_searches` entry. Sender is cloned into each
    /// coordinator; receiver is taken by `run`.
    search_done_tx: UnboundedSender<SearchDone>,
    search_done_rx: Option<UnboundedReceiver<SearchDone>>,
    /// Test-only override for the filename-search provider, letting tests inject a
    /// barrier/scripted provider (the real runtime is built internally and cannot
    /// otherwise be swapped). Never set outside tests.
    #[cfg(test)]
    search_provider_override: Option<Arc<dyn crate::runtime::IFsSearchProvider>>,
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
        let (search_done_tx, search_done_rx) = unbounded_channel();
        let actor = Self {
            shard,
            debouncer: Debouncer::new(),
            runtime,
            project,
            push,
            clock: Instant::now(),
            active_searches: HashMap::new(),
            search_done_tx,
            search_done_rx: Some(search_done_rx),
            #[cfg(test)]
            search_provider_override: None,
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

    /// Clonable outbound push handle (moved into the spawned search coordinator).
    pub(super) fn push_handle(&self) -> Arc<dyn FsWirePush> {
        Arc::clone(&self.push)
    }

    /// The `file:` runtime's filename-search provider, if the scheme supports it.
    pub(super) fn search_provider(&self) -> Option<Arc<dyn crate::runtime::IFsSearchProvider>> {
        #[cfg(test)]
        if let Some(provider) = &self.search_provider_override {
            return Some(Arc::clone(provider));
        }
        self.runtime.search_provider()
    }

    /// Test-only: swap the filename-search provider for a barrier/scripted double.
    #[cfg(test)]
    pub(super) fn set_search_provider_override(&mut self, provider: Arc<dyn crate::runtime::IFsSearchProvider>) {
        self.search_provider_override = Some(provider);
    }

    /// Register a newly-started search for `session`, superseding (cancelling)
    /// any previous in-flight search on the same connection.
    pub(super) fn register_search(&mut self, session: &str, active: ActiveSearch) {
        if let Some(prev) = self.active_searches.insert(session.to_owned(), active) {
            prev.cancel.cancel();
        }
    }

    /// Cancel the in-flight search for `session` iff it matches `search_id`
    /// (explicit `fs/searchCancel`). Returns whether a search was cancelled.
    pub(super) fn cancel_search(&mut self, session: &str, search_id: &serde_json::Value) -> bool {
        if self
            .active_searches
            .get(session)
            .is_some_and(|a| &a.search_id == search_id)
        {
            let active = self.active_searches.remove(session).expect("just checked present");
            active.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Drop any in-flight search for a disconnected session, cascading cancel.
    pub(super) fn drop_session_search(&mut self, session: &str) {
        if let Some(active) = self.active_searches.remove(session) {
            active.cancel.cancel();
        }
    }

    /// Clonable completion-signal sender, moved into a spawned search coordinator.
    pub(super) fn search_done_handle(&self) -> UnboundedSender<SearchDone> {
        self.search_done_tx.clone()
    }

    /// A coordinator finished naturally: clear its `active_searches` entry, but
    /// only if it is still the current search for that session — a superseding
    /// search may have replaced it (different id) before this signal arrived.
    fn on_search_done(&mut self, done: SearchDone) {
        if self
            .active_searches
            .get(&done.session)
            .is_some_and(|a| a.search_id == done.search_id)
        {
            self.active_searches.remove(&done.session);
        }
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
        // Search coordinators (spawned tasks) report natural completion here; the
        // sender clone lives on in `self.search_done_tx`, so the channel never
        // closes for the actor's life.
        let mut search_done = self.search_done_rx.take().expect("search_done_rx taken once in run");
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
                done = search_done.recv() => if let Some(done) = done {
                    self.on_search_done(done);
                },
                _ = flush.tick() => self.flush_debounced().await,
                _ = reap.tick() => self.drive(Command::ReapTick { now: self.now() }).await,
            }
        }
    }

    /// Handle one inbound transport event.
    async fn on_inbound(&mut self, event: FsInbound) {
        match event {
            FsInbound::Frame {
                session,
                user_id,
                frame,
            } => self.dispatch_frame(&session, &user_id, frame).await,
            FsInbound::Disconnect { session } => {
                // Connection teardown — lifecycle boundary; releases the session's
                // subscriptions (nodes go warm, reaper unmounts later) and cancels
                // any in-flight search on the gone connection.
                tracing::info!(session = %session, "fs session disconnect");
                self.drop_session_search(&session);
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
                        // Only overflow reaches this fan-out: the subscribe reply is
                        // returned to its caller by `shard_handle`, and no other
                        // command yields a snapshot. So this push is always a rescan
                        // and is tagged as one.
                        let params = wire::overflow_snapshot_params(&snapshot, &target);
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
