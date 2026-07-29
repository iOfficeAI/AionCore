//! `SubscriptionRegistry` — per-connection subscription state driving what the
//! tree model mounts/watches, via **set reconciliation** (not an integer
//! refcount).
//!
//! Two indexes: `forward` (session → canonicals, for disconnect cleanup) and
//! `reverse` (canonical → subscribers, for fan-out with pe context). A canonical
//! is watched ⟺ it has subscribers OR is being kept warm in `grace`. When the
//! last subscriber leaves, the canonical enters `grace` (watch stays live for a
//! TTL) so debounced expand/collapse, overlap switches, and reconnect windows
//! reuse it; a `warm-LRU` budget caps total warm nodes.
//!
//! Time is a logical millisecond clock injected by the caller (the actor stamps
//! `now`), keeping grace fully deterministic and testable without a real clock.

use std::collections::HashMap;
use std::collections::HashSet;

/// Logical millisecond timestamp (injected; not wall-clock here).
pub type Millis = u64;

/// Grace keep-warm TTL. A placeholder knob (design: 5 min); centralized so it
/// is easy to retune. See the stage-0 impl notes — value is deferred.
pub const GRACE_TTL_MS: Millis = 5 * 60 * 1000;

/// One subscriber: a connection's interest in a canonical, tagged with the pe
/// context needed to translate canonical-domain changes back to the pe-keyed
/// wire on fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subscriber {
    pub session: String,
    pub pe_id: String,
    pub rel: String,
}

/// Result of a subscribe: does the caller need to mount, was a warm node
/// rescued from grace, or was it already live?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeOutcome {
    /// First subscriber for a cold canonical → caller must mount + watch.
    Mount,
    /// Node was warm in grace → grace cancelled, already mounted.
    RescuedFromGrace,
    /// Already had subscribers → nothing to do.
    AlreadyLive,
}

/// Result of an unsubscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnsubscribeOutcome {
    /// Last subscriber left → node entered grace (still watched for TTL).
    EnteredGrace,
    /// Other subscribers remain → node stays live.
    StillLive,
    /// The canonical had no subscribers to remove.
    NotSubscribed,
}

/// Subscription state. Empty `reverse` entries are never kept — an emptied
/// canonical is moved to `grace`, so `reverse` and `grace` key sets are disjoint.
#[derive(Default)]
pub(crate) struct SubscriptionRegistry {
    forward: HashMap<String, HashSet<String>>,
    reverse: HashMap<String, HashSet<Subscriber>>,
    grace: HashMap<String, Millis>,
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `sub`'s interest in `canonical`, updating both indexes.
    pub fn subscribe(&mut self, sub: Subscriber, canonical: &str, _now: Millis) -> SubscribeOutcome {
        self.forward
            .entry(sub.session.clone())
            .or_default()
            .insert(canonical.to_owned());

        let rescued = self.grace.remove(canonical).is_some();
        let set = self.reverse.entry(canonical.to_owned()).or_default();
        let was_empty = set.is_empty();
        set.insert(sub);

        if rescued {
            SubscribeOutcome::RescuedFromGrace
        } else if was_empty {
            SubscribeOutcome::Mount
        } else {
            SubscribeOutcome::AlreadyLive
        }
    }

    /// Drop `sub`'s interest in `canonical`. Empties → grace at `now + TTL`.
    pub fn unsubscribe(&mut self, sub: &Subscriber, canonical: &str, now: Millis) -> UnsubscribeOutcome {
        if let Some(cs) = self.forward.get_mut(&sub.session) {
            cs.remove(canonical);
            if cs.is_empty() {
                self.forward.remove(&sub.session);
            }
        }

        let Some(set) = self.reverse.get_mut(canonical) else {
            return UnsubscribeOutcome::NotSubscribed;
        };
        set.remove(sub);
        if set.is_empty() {
            self.reverse.remove(canonical);
            self.grace.insert(canonical.to_owned(), now + GRACE_TTL_MS);
            UnsubscribeOutcome::EnteredGrace
        } else {
            UnsubscribeOutcome::StillLive
        }
    }

    /// Remove all of `session`'s subscriptions (disconnect). Returns canonicals
    /// that became empty and entered grace.
    pub fn drop_session(&mut self, session: &str, now: Millis) -> Vec<String> {
        let canonicals = self.forward.remove(session).unwrap_or_default();
        let mut graced = Vec::new();
        for canonical in canonicals {
            if let Some(set) = self.reverse.get_mut(&canonical) {
                set.retain(|s| s.session != session);
                if set.is_empty() {
                    self.reverse.remove(&canonical);
                    self.grace.insert(canonical.clone(), now + GRACE_TTL_MS);
                    graced.push(canonical);
                }
            }
        }
        graced.sort();
        graced
    }

    /// Subscribers of `canonical` for fan-out (`None` if none).
    pub fn subscribers_of(&self, canonical: &str) -> Option<&HashSet<Subscriber>> {
        self.reverse.get(canonical)
    }

    /// Evict grace entries whose TTL has elapsed at `now`. Returns canonicals to
    /// unmount + unwatch.
    pub fn reap(&mut self, now: Millis) -> Vec<String> {
        let mut expired: Vec<String> = self
            .grace
            .iter()
            .filter(|(_, evict_at)| **evict_at <= now)
            .map(|(c, _)| c.clone())
            .collect();
        for c in &expired {
            self.grace.remove(c);
        }
        expired.sort();
        expired
    }

    /// Enforce a warm-node budget: while total watched (live + grace) exceeds
    /// `max`, evict the oldest grace entry. Returns canonicals to unmount.
    pub fn enforce_watch_budget(&mut self, max: usize) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.watched_count() > max && !self.grace.is_empty() {
            // Oldest-entered = smallest evict_at (TTL is constant).
            let oldest = self
                .grace
                .iter()
                .min_by_key(|(c, evict_at)| (**evict_at, (*c).clone()))
                .map(|(c, _)| c.clone())
                .expect("grace non-empty");
            self.grace.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }

    /// Count of currently-watched canonicals (live + warm-in-grace).
    pub fn watched_count(&self) -> usize {
        self.reverse.len() + self.grace.len()
    }

    /// Whether `canonical` is currently watched: it has subscribers or is being
    /// kept warm in grace. This is the derived `watch ⟺ ...` predicate.
    pub fn is_watched(&self, canonical: &str) -> bool {
        self.reverse.contains_key(canonical) || self.grace.contains_key(canonical)
    }
}

#[cfg(test)]
#[path = "subscription_test.rs"]
mod subscription_test;
