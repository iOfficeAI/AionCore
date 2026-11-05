//! In-memory epoch/sequence event ring for headless Host reconnect (E14).
//!
//! Brand: CSBU WorkMate. No AGPL. Bounded RAM only — not durable journal replay.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_api_types::{EventCursor, WebSocketMessage};
use serde_json::Value;

/// Default ring capacity (recent live events only).
pub const DEFAULT_EVENT_LOG_CAPACITY: usize = 512;

/// Outcome of a resume request against the in-memory ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStatus {
    /// Same epoch; events with `sequence > after.sequence` were available.
    Ok,
    /// Client cursor epoch differs from the Host (restart / rotation).
    Gap,
}

#[derive(Debug, Clone)]
pub struct ResumeSlice {
    pub status: ResumeStatus,
    pub current: EventCursor,
    pub events: Vec<WebSocketMessage<Value>>,
}

/// Process-local ring of recently broadcast WebSocket envelopes, stamped with
/// [`EventCursor`]. Epoch is fixed for the Host process lifetime (bumps only
/// when a new [`EventLog`] is constructed — i.e. Host restart).
pub struct EventLog {
    epoch: u64,
    next_sequence: AtomicU64,
    capacity: usize,
    ring: Mutex<VecDeque<WebSocketMessage<Value>>>,
}

impl EventLog {
    /// Create a log with `capacity` slots. Epoch starts at `1` (0 reserved for
    /// "unset / legacy client").
    pub fn new(capacity: usize) -> Self {
        Self::with_epoch(1, capacity)
    }

    pub fn with_epoch(epoch: u64, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            epoch: epoch.max(1),
            next_sequence: AtomicU64::new(1),
            capacity,
            ring: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Last assigned cursor (sequence 0 when nothing has been published yet).
    pub fn current_cursor(&self) -> EventCursor {
        let seq = self.next_sequence.load(Ordering::Relaxed).saturating_sub(1);
        EventCursor::new(self.epoch, seq)
    }

    /// Stamp `event` with the next Host cursor (always Host-owned), append a
    /// clone to the ring, and return the stamped envelope for broadcast.
    pub fn record(&self, mut event: WebSocketMessage<Value>) -> WebSocketMessage<Value> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        event.cursor = Some(EventCursor::new(self.epoch, sequence));
        {
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= self.capacity {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        event
    }

    /// Events strictly after `after` in the current epoch, or a gap slice when
    /// epochs differ or the ring evicted events between `after` and the oldest
    /// retained entry (replay from the oldest retained of this epoch).
    pub fn events_after(&self, after: EventCursor) -> ResumeSlice {
        let current = self.current_cursor();
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());

        if after.epoch != self.epoch {
            let events: Vec<_> = ring.iter().cloned().collect();
            return ResumeSlice {
                status: ResumeStatus::Gap,
                current,
                events,
            };
        }

        // Eviction gap: oldest retained sequence is more than one past the
        // client's cursor, so at least one event was dropped from the ring.
        let mut status = ResumeStatus::Ok;
        if let Some(oldest) = ring.front().and_then(|ev| ev.cursor) {
            if after.sequence.saturating_add(1) < oldest.sequence {
                status = ResumeStatus::Gap;
            }
        } else if after.sequence < current.sequence {
            // Everything after `after` was published then evicted (capacity
            // edge) — treat as gap with empty replay.
            status = ResumeStatus::Gap;
        }

        let events: Vec<_> = ring
            .iter()
            .filter(|ev| match ev.cursor {
                Some(c) => after.precedes(c),
                None => false,
            })
            .cloned()
            .collect();

        ResumeSlice {
            status,
            current,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_stamps_monotonic_sequence() {
        let log = EventLog::new(8);
        let a = log.record(WebSocketMessage::new("a", json!({})));
        let b = log.record(WebSocketMessage::new("b", json!({})));
        assert_eq!(a.cursor, Some(EventCursor::new(1, 1)));
        assert_eq!(b.cursor, Some(EventCursor::new(1, 2)));
        assert_eq!(log.current_cursor(), EventCursor::new(1, 2));
    }

    #[test]
    fn events_after_same_epoch_filters() {
        let log = EventLog::new(8);
        log.record(WebSocketMessage::new("a", json!({})));
        log.record(WebSocketMessage::new("b", json!({})));
        log.record(WebSocketMessage::new("c", json!({})));
        let slice = log.events_after(EventCursor::new(1, 1));
        assert_eq!(slice.status, ResumeStatus::Ok);
        assert_eq!(slice.events.len(), 2);
        assert_eq!(slice.events[0].name, "b");
        assert_eq!(slice.events[1].name, "c");
    }

    #[test]
    fn epoch_mismatch_is_gap_and_replays_retained() {
        let log = EventLog::with_epoch(3, 8);
        log.record(WebSocketMessage::new("x", json!({})));
        let slice = log.events_after(EventCursor::new(1, 99));
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert_eq!(slice.events.len(), 1);
        assert_eq!(slice.current.epoch, 3);
    }

    #[test]
    fn ring_evicts_oldest() {
        let log = EventLog::new(2);
        log.record(WebSocketMessage::new("1", json!({})));
        log.record(WebSocketMessage::new("2", json!({})));
        log.record(WebSocketMessage::new("3", json!({})));
        let slice = log.events_after(EventCursor::default());
        assert_eq!(slice.status, ResumeStatus::Gap);
        assert_eq!(slice.events.len(), 2);
        assert_eq!(slice.events[0].name, "2");
        assert_eq!(slice.events[1].name, "3");
    }

    #[test]
    fn eviction_gap_when_cursor_behind_oldest() {
        let log = EventLog::new(2);
        log.record(WebSocketMessage::new("1", json!({})));
        log.record(WebSocketMessage::new("2", json!({})));
        log.record(WebSocketMessage::new("3", json!({})));
        // Client last saw seq 1; ring now holds 2,3 — contiguous after 1 → Ok
        let contiguous = log.events_after(EventCursor::new(1, 1));
        assert_eq!(contiguous.status, ResumeStatus::Ok);
        assert_eq!(contiguous.events.len(), 2);
        // Client still at default/0 while oldest is 2 → gap (seq 1 dropped)
        let gapped = log.events_after(EventCursor::new(1, 0));
        assert_eq!(gapped.status, ResumeStatus::Gap);
    }

    #[test]
    fn overwrites_pre_stamped_cursor_with_host_epoch() {
        let log = EventLog::new(4);
        let stamped = WebSocketMessage::new("pre", json!({})).with_cursor(EventCursor::new(9, 7));
        let out = log.record(stamped);
        assert_eq!(out.cursor, Some(EventCursor::new(1, 1)));
        let auto = log.record(WebSocketMessage::new("auto", json!({})));
        assert_eq!(auto.cursor, Some(EventCursor::new(1, 2)));
    }
}
