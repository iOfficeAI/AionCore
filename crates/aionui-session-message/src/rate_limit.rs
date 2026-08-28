//! Loop and storm protection: two in-memory sliding windows.
//!
//! Delivery starts a turn, and whether to reply is the recipient agent's own
//! decision — so A→B, B replies, A receives and starts a turn, A's agent says
//! "thanks, got it"… two agents can be polite at each other indefinitely, and
//! every round is real token spend. The TTL and queue caps bound a SINGLE
//! message; they do not bound CHAIN LENGTH.
//!
//! Not hop counting: a hop count needs the backend to know which inbound
//! message caused this outbound one — stateful causal tracking, requiring a
//! causal-chain id persisted across turns, or a guess. Rate gates throttle the
//! ping-pong's FREQUENCY directly with two counters, and incidentally cover
//! storms that have nothing to do with loops (one turn fanning out 50
//! messages).
//!
//! State is in memory and resets on restart, same as the queue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_api_types::SessionRateGate;

use crate::queue::Clock;

pub const WINDOW_MS: i64 = 10 * 60 * 1000;
/// Per-conversation outbound cap within the window.
pub const OUTBOUND_LIMIT: u32 = 20;
/// Per-(from, to) cap within the window.
pub const PAIR_LIMIT: u32 = 10;

#[derive(Debug, PartialEq, Eq)]
pub enum RateVerdict {
    Allowed,
    Tripped { gate: SessionRateGate, window_count: u32 },
}

#[derive(Default)]
struct Windows {
    outbound: HashMap<String, Vec<i64>>,
    pair: HashMap<(String, String), Vec<i64>>,
}

pub struct RateLimiter {
    windows: Mutex<Windows>,
    clock: Arc<dyn Clock>,
}

impl RateLimiter {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            windows: Mutex::new(Windows::default()),
            clock,
        }
    }

    /// Check both gates and, only if allowed, record the send.
    ///
    /// A tripped check must NOT consume a slot: otherwise a caller that keeps
    /// retrying would keep pushing its own window forward and never recover.
    pub fn check_and_record(&self, from: &str, to: &str) -> RateVerdict {
        let now = self.clock.now_ms();
        let cutoff = now - WINDOW_MS;
        let mut windows = self.windows.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let outbound = windows.outbound.entry(from.to_owned()).or_default();
        outbound.retain(|at| *at > cutoff);
        let outbound_count = outbound.len() as u32;

        let pair = windows.pair.entry((from.to_owned(), to.to_owned())).or_default();
        pair.retain(|at| *at > cutoff);
        let pair_count = pair.len() as u32;

        // Pair gate first: it is the tighter of the two and the one that
        // actually describes a two-agent loop, so it gives the more accurate
        // signal when both would trip.
        if pair_count >= PAIR_LIMIT {
            return RateVerdict::Tripped {
                gate: SessionRateGate::Pair,
                window_count: pair_count,
            };
        }
        if outbound_count >= OUTBOUND_LIMIT {
            return RateVerdict::Tripped {
                gate: SessionRateGate::Outbound,
                window_count: outbound_count,
            };
        }

        pair.push(now);
        windows
            .outbound
            .get_mut(from)
            .expect("outbound entry was just created")
            .push(now);
        RateVerdict::Allowed
    }
}

#[cfg(test)]
#[path = "rate_limit_test.rs"]
mod rate_limit_test;
