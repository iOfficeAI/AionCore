//! Shard — a sequential worker owning one canonical-hash slice of the runtime.
//!
//! Concurrency model (see `formal/runtime/runtime.md`): WS handlers, watcher
//! callbacks, the grace reaper, and disconnects all touch the tree + registry.
//! Rather than locking, a shard is a **sequential worker** (a task draining an
//! mpsc command queue) that exclusively owns its `Node`s, the `reverse`
//! subscriber index, watch registration, and grace/reaper. Same-canonical
//! mount/apply/unmount are serial for free; fan-out reads local state; cross-
//! shard work runs in parallel. Desktop uses **N = 1** (a single shard, all
//! serial — simplest). `forward` (session → canonicals) is global; this stage-0
//! core models one shard.
//!
//! [`Shard::handle`] is the whole decision core — the spawned worker is just a
//! loop over it — so command sequences unit-test every lifecycle and edge-race
//! guard deterministically, with no real tasks, timers, or channel races.

use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::runtime::subscription::Millis;

use super::error::FsError;
use super::subscription::{SubscribeOutcome, Subscriber, SubscriptionRegistry};
use super::tree_model::{DeltaBatch, Hint, Snapshot, TreeModel};
use super::watcher::RawEvent;

/// A command processed serially by a shard worker. `Mount`/`Unmount` are not
/// external commands — they are internal effects of subscribe/reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Subscribe {
        sub: Subscriber,
        canonical: String,
        now: Millis,
    },
    Unsubscribe {
        sub: Subscriber,
        canonical: String,
        now: Millis,
    },
    Apply {
        canonical: String,
        hint: Hint,
    },
    Overflow {
        canonical: String,
    },
    DropSession {
        session: String,
        now: Millis,
    },
    ReapTick {
        now: Millis,
    },
}

/// A canonical-domain output. Fan-out to the pe-keyed wire is the WS handler's
/// job (stage 1); here each output carries the recipient subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardOutput {
    /// Full listing (subscribe reply, or overflow rescan) to these subscribers.
    Snapshot {
        subscribers: Vec<Subscriber>,
        snapshot: Snapshot,
    },
    /// Incremental changes fanned out to current subscribers.
    Delta {
        subscribers: Vec<Subscriber>,
        delta: DeltaBatch,
    },
}

/// One shard: its tree slice, its subscription state, and its warm-node budget.
pub struct Shard {
    tree: TreeModel,
    subs: SubscriptionRegistry,
    warm_budget: usize,
}

/// Map a raw watcher event to the command the debounce stage would enqueue.
/// `Changed` → re-check the affected child names; `Overflow` → rescan.
pub fn raw_to_command(event: RawEvent) -> Command {
    match event {
        RawEvent::Changed { canonical, paths } => {
            // Non-recursive watch → each path is a direct child; the hint is its
            // file name (apply re-stats those children against the fresh fs).
            let child_names = paths
                .iter()
                .filter_map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .collect();
            Command::Apply {
                canonical,
                hint: Hint::ChildNames(child_names),
            }
        }
        RawEvent::Overflow { canonical } => Command::Overflow { canonical },
    }
}

/// Per-canonical pending state while coalescing raw events within a debounce
/// window. `Overflow` supersedes `Changed` — a rescan already covers any child.
enum Pending {
    Changed(BTreeSet<String>),
    Overflow,
}

/// Debounce/coalesce stage of the apply pipeline: collapses a burst of raw
/// events (editor saves, bulk writes) per canonical into one command, so the
/// tree applies once and one delta carries many changes. Pure and clock-free —
/// the caller flushes on its own timer (the worker's ~200ms window); this keeps
/// coalescing deterministically testable. See stage-0 impl notes for why the
/// window lives here (pipeline stage) rather than inside the watcher.
#[derive(Default)]
pub struct Debouncer {
    pending: HashMap<String, Pending>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accumulate one raw event into the pending set.
    pub fn push(&mut self, event: RawEvent) {
        match event {
            RawEvent::Overflow { canonical } => {
                self.pending.insert(canonical, Pending::Overflow);
            }
            RawEvent::Changed { canonical, paths } => {
                let names = paths.iter().filter_map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                });
                match self
                    .pending
                    .entry(canonical)
                    .or_insert_with(|| Pending::Changed(BTreeSet::new()))
                {
                    // Overflow already pending → rescan subsumes these children.
                    Pending::Overflow => {}
                    Pending::Changed(set) => set.extend(names),
                }
            }
        }
    }

    /// Drain all pending state into coalesced commands (order-stable by
    /// canonical), clearing the buffer.
    pub fn drain(&mut self) -> Vec<Command> {
        let mut entries: Vec<(String, Pending)> = self.pending.drain().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
            .into_iter()
            .map(|(canonical, pending)| match pending {
                Pending::Overflow => Command::Overflow { canonical },
                Pending::Changed(names) => Command::Apply {
                    canonical,
                    hint: Hint::ChildNames(names.into_iter().collect()),
                },
            })
            .collect()
    }

    /// Whether any events are buffered.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl Shard {
    pub fn new(tree: TreeModel, warm_budget: usize) -> Self {
        Self {
            tree,
            subs: SubscriptionRegistry::new(),
            warm_budget,
        }
    }

    /// Process one command, mutating owned state and returning any outputs.
    pub async fn handle(&mut self, command: Command) -> Result<Vec<ShardOutput>, FsError> {
        match command {
            Command::Subscribe { sub, canonical, now } => {
                let outcome = self.subs.subscribe(sub.clone(), &canonical, now);
                // Deliver the current listing to the subscribing session. On a
                // cold canonical this mounts (arm watch → baseline); a warm node
                // rescued from grace, or an already-live node, serves its node.
                let snapshot = match outcome {
                    SubscribeOutcome::Mount => self.tree.mount(&canonical).await?,
                    SubscribeOutcome::RescuedFromGrace | SubscribeOutcome::AlreadyLive => {
                        match self.tree.snapshot(&canonical) {
                            Some(snap) => snap,
                            // Warm in subs but node gone (shouldn't happen while
                            // watch is held) → re-mount defensively.
                            None => self.tree.mount(&canonical).await?,
                        }
                    }
                };
                Ok(vec![ShardOutput::Snapshot {
                    subscribers: vec![sub],
                    snapshot,
                }])
            }

            Command::Unsubscribe { sub, canonical, now } => {
                // Emptying enters grace: the node stays mounted + watched for the
                // TTL; the reaper unmounts later. No immediate output.
                let _ = self.subs.unsubscribe(&sub, &canonical, now);
                Ok(Vec::new())
            }

            Command::Apply { canonical, hint } => {
                // Reconcile; unmounted node (in-flight event after unmount) →
                // None → drop silently.
                match self.tree.apply(&canonical, hint).await? {
                    Some(delta) => {
                        let subscribers = self.recipients(&canonical);
                        if subscribers.is_empty() {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![ShardOutput::Delta { subscribers, delta }])
                        }
                    }
                    None => Ok(Vec::new()),
                }
            }

            Command::Overflow { canonical } => {
                // Kernel dropped events → node facts are stale. Rescan (apply All
                // to refresh) and push the full snapshot; subscribers replace the
                // whole level. Unmounted → drop.
                if self.tree.snapshot(&canonical).is_none() {
                    return Ok(Vec::new());
                }
                self.tree.apply(&canonical, Hint::All).await?;
                let subscribers = self.recipients(&canonical);
                match (subscribers.is_empty(), self.tree.snapshot(&canonical)) {
                    (false, Some(snapshot)) => Ok(vec![ShardOutput::Snapshot { subscribers, snapshot }]),
                    _ => Ok(Vec::new()),
                }
            }

            Command::DropSession { session, now } => {
                // Disconnect: remove all of this session's subscriptions. Emptied
                // nodes go warm (grace); reaper unmounts later. No output.
                let _ = self.subs.drop_session(&session, now);
                Ok(Vec::new())
            }

            Command::ReapTick { now } => {
                // TTL-expired warm nodes, then over-budget warm nodes (warm-LRU),
                // are unmounted + unwatched.
                let mut grace_expired = 0usize;
                for canonical in self.subs.reap(now) {
                    self.tree.unmount(&canonical);
                    grace_expired += 1;
                }
                let mut budget_evicted = 0usize;
                for canonical in self.subs.enforce_watch_budget(self.warm_budget) {
                    self.tree.unmount(&canonical);
                    budget_evicted += 1;
                }
                // Low-volume lifecycle boundary (only when something was evicted);
                // counts only, no paths.
                if grace_expired + budget_evicted > 0 {
                    tracing::info!(grace_expired, budget_evicted, "fs grace evict: unmounted warm nodes");
                }
                Ok(Vec::new())
            }
        }
    }

    /// Whether `canonical` is currently mounted+watched (live or warm-in-grace).
    pub fn is_watched(&self, canonical: &str) -> bool {
        self.subs.is_watched(canonical)
    }

    /// Current subscribers of a canonical as an owned vec (fan-out recipients).
    fn recipients(&self, canonical: &str) -> Vec<Subscriber> {
        self.subs
            .subscribers_of(canonical)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "actor_test.rs"]
mod actor_test;
