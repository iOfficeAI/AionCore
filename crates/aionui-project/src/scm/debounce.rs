//! Debounce for repository dirty signals.
//!
//! A single version-control command produces a burst of metadata events spread
//! over hundreds of milliseconds — a checkout rewrites the index, moves `HEAD`,
//! updates refs, each through lock files. Recomputing on the first event would
//! recompute several times per user action and publish intermediate states, so
//! signals are collected and flushed once the burst has gone quiet.
//!
//! Coalescing is per repository: two repositories going dirty together each get
//! one recompute, and ten signals for one repository still get one.

use std::collections::BTreeSet;
use std::time::Duration;

/// How long a repository must stay quiet before its status is recomputed.
///
/// Chosen from measured burst behaviour: a bulk checkout's metadata events span
/// several hundred milliseconds with gaps approaching that, so a shorter window
/// splits one user action into multiple recomputes. Recomputing is itself
/// milliseconds, so the wait — not the work — is what a user perceives; this is a
/// starting value worth re-measuring per platform rather than a tuned constant.
pub(super) const DEBOUNCE_MS: u64 = 1000;

/// Flush-check cadence. Fine enough that the effective wait stays close to
/// [`DEBOUNCE_MS`], coarse enough not to wake the loop needlessly.
pub(super) const FLUSH_TICK_MS: u64 = 100;

/// Accumulates dirty repositories and releases them once quiet.
#[derive(Default)]
pub(super) struct DirtyDebouncer {
    /// Repositories awaiting recompute, with the tick they last went dirty on.
    pending: Vec<(String, u64)>,
}

impl DirtyDebouncer {
    /// Record that a repository is dirty as of `now_ms`.
    ///
    /// Re-arms the window: a repository that keeps signalling is recomputed after
    /// it settles, not while it is still churning.
    pub(super) fn mark(&mut self, repo_id: String, now_ms: u64) {
        match self.pending.iter_mut().find(|(id, _)| *id == repo_id) {
            Some((_, at)) => *at = now_ms,
            None => self.pending.push((repo_id, now_ms)),
        }
    }

    /// Take the repositories that have been quiet for [`DEBOUNCE_MS`].
    ///
    /// Returned sorted so behaviour is deterministic regardless of signal order.
    pub(super) fn take_ready(&mut self, now_ms: u64) -> Vec<String> {
        let mut ready = BTreeSet::new();
        self.pending.retain(|(repo_id, at)| {
            if now_ms.saturating_sub(*at) >= DEBOUNCE_MS {
                ready.insert(repo_id.clone());
                false
            } else {
                true
            }
        });
        ready.into_iter().collect()
    }

    /// Whether anything is waiting (lets a caller skip work when idle).
    pub(super) fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The flush cadence as a [`Duration`], for building an interval timer.
pub(super) fn flush_interval() -> Duration {
    Duration::from_millis(FLUSH_TICK_MS)
}

#[cfg(test)]
#[path = "debounce_test.rs"]
mod debounce_test;
