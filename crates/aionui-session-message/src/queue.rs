//! In-memory pending-delivery queue.
//!
//! Not persisted, no restart recovery, no waking of sleeping conversations
//! (spec §6.4). "Lost on restart" is consistent with what the product already
//! does: the front-end draft box is `sessionStorage`, and in-flight turns are
//! interrupted by a restart anyway.
//!
//! Lock discipline, enforced by the API shape: every method here is
//! synchronous and returns owned data, so a caller CANNOT hold the mutex
//! across an `.await`. The drainer takes a snapshot, releases, delivers, then
//! comes back to record the result.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::error::SessionMessageError;

/// Per-target cap. Matches the front-end draft box's `MAX_QUEUED_COMMANDS`.
pub const PER_TARGET_LIMIT: usize = 20;
/// Cap across all targets.
pub const GLOBAL_LIMIT: usize = 200;
/// A single message's time to live before it is dropped with a `warn`.
pub const TTL_MS: i64 = 10 * 60 * 1000;

/// Injectable time source. The drainer's tick and the TTL clock must both be
/// injectable so tests can advance time by hand instead of sleeping — a
/// wall-clock `sleep` assertion is flaky by construction (spec §11).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        aionui_common::now_ms()
    }
}

/// Manually advanced clock. Not behind `#[cfg(test)]` because the integration
/// tests under `tests/` need it too, and it is inert in production builds.
pub struct TestClock {
    now_ms: Mutex<i64>,
}

impl TestClock {
    pub fn new(now_ms: i64) -> Self {
        Self {
            now_ms: Mutex::new(now_ms),
        }
    }

    pub fn advance(&self, ms: i64) {
        *self.now_ms.lock().expect("test clock lock") += ms;
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        *self.now_ms.lock().expect("test clock lock")
    }
}

/// One queued delivery.
///
/// `message` is the FINISHED content — the recipient block was composed at
/// enqueue time, when both conversations' rows were already in hand. Draining
/// therefore re-sends these bytes verbatim and never rebuilds anything, which is
/// why the sender's name and workspace are deliberately NOT carried here: a
/// queued copy of them could only go stale, and nothing would read it.
///
/// `from_conversation_id` and `user_id` DO stay, because the drainer needs them
/// for the per-user feature gate and for logging which pair a drop belonged to.
#[derive(Debug, Clone)]
pub struct PendingDelivery {
    pub to: String,
    pub user_id: String,
    pub from_conversation_id: String,
    /// The FULL delivery content, recipient block already prepended. Never
    /// logged (spec §10).
    pub message: String,
    pub expires_at_ms: i64,
}

#[derive(Default)]
struct QueueState {
    by_target: HashMap<String, VecDeque<PendingDelivery>>,
    total: usize,
}

pub struct DeliveryQueue {
    state: Mutex<QueueState>,
    clock: Arc<dyn Clock>,
}

impl DeliveryQueue {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            clock,
        }
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn push(&self, item: PendingDelivery) -> Result<(), SessionMessageError> {
        let mut state = self.lock();
        if state.total >= GLOBAL_LIMIT {
            return Err(SessionMessageError::QueueFull);
        }
        let bucket = state.by_target.entry(item.to.clone()).or_default();
        if bucket.len() >= PER_TARGET_LIMIT {
            return Err(SessionMessageError::QueueFull);
        }
        bucket.push_back(item);
        state.total += 1;
        Ok(())
    }

    /// Drop every item past its deadline. Returns how many were dropped so the
    /// caller can `warn` with a count (never with the body).
    pub fn drop_expired(&self) -> usize {
        let now = self.clock.now_ms();
        let mut state = self.lock();
        let mut dropped = 0;
        for bucket in state.by_target.values_mut() {
            let before = bucket.len();
            bucket.retain(|item| item.expires_at_ms > now);
            dropped += before - bucket.len();
        }
        state.by_target.retain(|_, bucket| !bucket.is_empty());
        state.total -= dropped;
        dropped
    }

    /// The head of each target's queue — one per target, never more. Delivery
    /// is per-message and one turn each; batching is deliberately not done
    /// (spec §6.4).
    pub fn snapshot_heads(&self) -> Vec<PendingDelivery> {
        self.lock()
            .by_target
            .values()
            .filter_map(|bucket| bucket.front().cloned())
            .collect()
    }

    /// Remove the head after a successful delivery.
    pub fn pop_head(&self, to: &str) {
        self.remove_head(to);
    }

    /// Remove the head after a hard (non-busy) failure.
    pub fn drop_head(&self, to: &str) {
        self.remove_head(to);
    }

    fn remove_head(&self, to: &str) {
        let mut state = self.lock();
        let Some(bucket) = state.by_target.get_mut(to) else {
            return;
        };
        if bucket.pop_front().is_some() {
            state.total -= 1;
        }
        if state.by_target.get(to).is_some_and(VecDeque::is_empty) {
            state.by_target.remove(to);
        }
    }

    /// Drop everything queued FOR `to`. Used when that conversation's turn is
    /// cancelled (spec §6.9.1) — without this, "stop" is a lie.
    pub fn clear_for(&self, to: &str) -> usize {
        let mut state = self.lock();
        let removed = state.by_target.remove(to).map_or(0, |bucket| bucket.len());
        state.total -= removed;
        removed
    }

    // No `clear_all`: the "cross-session messaging" toggle lives on
    // `system_settings`, whose primary key is `user_id`, so switching it off is
    // ONE user's decision. A queue-wide wipe would take everyone else's pending
    // deliveries with it. The drainer instead consults the gate per item and
    // calls `clear_for` on that user's target (`drainer.rs`), which reaches the
    // same outcome the spec asks for — re-enabling never flushes stale messages
    // — with the right blast radius.

    pub fn is_empty(&self) -> bool {
        self.lock().total == 0
    }

    pub fn total_len(&self) -> usize {
        self.lock().total
    }

    pub fn len_for(&self, to: &str) -> usize {
        self.lock().by_target.get(to).map_or(0, VecDeque::len)
    }

    /// A poisoned lock here would mean a panic while holding it; the queue is
    /// pure in-memory bookkeeping with no invariant that a panic could leave
    /// half-applied, so recovering the guard is safe and strictly better than
    /// taking the whole drainer down.
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
#[path = "queue_test.rs"]
mod queue_test;
