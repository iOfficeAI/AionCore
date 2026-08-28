//! The single background drainer. NOT one task per target, and certainly not
//! one per message.
//!
//! Fixed 1s tick, no exponential backoff (spec §6.4). Backoff suits "retrying
//! is expensive" or "the peer may be dead"; neither holds — a retry costs one
//! `try_claim_turn` (an in-memory lock check) and the peer is merely busy. And
//! backoff points the wrong way: the longer the target's turn runs, the longer
//! the wait would grow, so by the time it finally goes idle the drainer would
//! be in its longest sleep. A fixed 1s guarantees "delivered ≤1s after the
//! target frees up".

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::queue::{DeliveryQueue, PendingDelivery};

pub const TICK: Duration = Duration::from_secs(1);

/// Outcome of one delivery attempt, reduced to what the drainer must decide on.
#[derive(Debug)]
pub enum DeliveryOutcome {
    Delivered,
    /// Not deliverable YET. Stays queued, retried next tick. Three sources, all
    /// transient:
    ///
    /// 1. the target is busy and its backend cannot take a mid-turn message;
    /// 2. it can, but that turn is blocked on a confirmation card;
    /// 3. its runtime is mid-restart — a ~1s window after which the
    ///    conversation is idle, which is what the item is waiting for.
    ///
    /// (3) used to be classified as a hard error, which discarded queued work
    /// the cancel hook had deliberately preserved. See
    /// `service::classify_delivery_failure`.
    Busy,
    HardError(String),
}

#[async_trait]
pub trait DeliverySink: Send + Sync {
    async fn deliver(&self, item: &PendingDelivery) -> DeliveryOutcome;
}

/// Per-user feature gate, consulted every tick.
///
/// Per-user rather than process-global because the setting lives on
/// `system_settings`, whose primary key is `user_id`. Reading it once and
/// treating it as global would apply one user's choice to everyone.
#[async_trait]
pub trait DrainGate: Send + Sync {
    async fn is_enabled_for(&self, user_id: &str) -> bool;
}

pub struct Drainer {
    queue: Arc<DeliveryQueue>,
    sink: Arc<dyn DeliverySink>,
    gate: Arc<dyn DrainGate>,
}

impl Drainer {
    pub fn new(queue: Arc<DeliveryQueue>, sink: Arc<dyn DeliverySink>, gate: Arc<dyn DrainGate>) -> Self {
        Self { queue, sink, gate }
    }

    /// One pass. Public so tests advance the drainer by hand instead of
    /// sleeping on a wall clock (spec §11: no `sleep` + wall-clock assertions).
    pub async fn tick_once(&self) {
        let expired = self.queue.drop_expired();
        if expired > 0 {
            // Count only — never the body (spec §10).
            warn!(
                dropped = expired,
                "cross-session deliveries expired before the target freed up"
            );
        }

        // Snapshot under the lock, deliver outside it. `snapshot_heads` returns
        // owned values precisely so no guard can cross an `.await`.
        for item in self.queue.snapshot_heads() {
            if !self.gate.is_enabled_for(&item.user_id).await {
                let cleared = self.queue.clear_for(&item.to);
                info!(
                    to_conversation_id = %item.to,
                    cleared,
                    "cross-session messaging disabled; dropped this target's pending deliveries"
                );
                continue;
            }

            match self.sink.deliver(&item).await {
                DeliveryOutcome::Delivered => {
                    self.queue.pop_head(&item.to);
                    info!(
                        from_conversation_id = %item.from_conversation_id,
                        to_conversation_id = %item.to,
                        outcome = "delivered",
                        "queued cross-session message drained"
                    );
                }
                DeliveryOutcome::Busy => {
                    // Stays at the head; retried on the next tick.
                }
                DeliveryOutcome::HardError(reason) => {
                    self.queue.drop_head(&item.to);
                    warn!(
                        from_conversation_id = %item.from_conversation_id,
                        to_conversation_id = %item.to,
                        reason = %reason,
                        "queued cross-session message dropped after a hard delivery error"
                    );
                }
            }
        }
    }

    /// Run forever. Sleeps on `Notify` while the queue is empty — purely an
    /// idle-cost optimisation, not a correctness requirement (a constant 1s
    /// tick would behave the same).
    pub fn spawn(self: Arc<Self>, notify: Arc<Notify>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if self.queue.is_empty() {
                    notify.notified().await;
                    continue;
                }
                self.tick_once().await;
                tokio::time::sleep(TICK).await;
            }
        })
    }
}

#[cfg(test)]
#[path = "drainer_test.rs"]
mod drainer_test;
