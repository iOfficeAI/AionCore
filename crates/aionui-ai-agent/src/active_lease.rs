use aionui_common::{TimestampMs, now_ms};
use dashmap::DashMap;

pub const ACTIVE_LEASE_TTL_MS: TimestampMs = 90_000;

#[derive(Debug)]
pub struct ActiveLeaseRegistry {
    leases: DashMap<String, TimestampMs>,
    ttl_ms: TimestampMs,
}

impl ActiveLeaseRegistry {
    pub fn new() -> Self {
        Self::with_ttl_ms(ACTIVE_LEASE_TTL_MS)
    }

    pub fn with_ttl_ms(ttl_ms: TimestampMs) -> Self {
        Self {
            leases: DashMap::new(),
            ttl_ms,
        }
    }

    pub fn renew(&self, conversation_id: &str) -> TimestampMs {
        let expires_at = now_ms().saturating_add(self.ttl_ms);
        self.leases.insert(conversation_id.to_owned(), expires_at);
        expires_at
    }

    pub fn renew_many<'a>(&self, conversation_ids: impl IntoIterator<Item = &'a str>) -> (usize, TimestampMs) {
        let expires_at = now_ms().saturating_add(self.ttl_ms);
        let mut count = 0;
        for conversation_id in conversation_ids {
            self.leases.insert(conversation_id.to_owned(), expires_at);
            count += 1;
        }
        (count, expires_at)
    }

    pub fn active_until(&self, conversation_id: &str) -> Option<TimestampMs> {
        let expires_at = *self.leases.get(conversation_id)?;
        if expires_at > now_ms() {
            Some(expires_at)
        } else {
            self.leases.remove(conversation_id);
            None
        }
    }

    pub fn is_active(&self, conversation_id: &str) -> bool {
        self.active_until(conversation_id).is_some()
    }
}

impl Default for ActiveLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn renew_sets_active_lease() {
        let registry = ActiveLeaseRegistry::new();

        let expires_at = registry.renew("conv-1");

        assert_eq!(registry.active_until("conv-1"), Some(expires_at));
        assert!(registry.is_active("conv-1"));
    }

    #[test]
    fn renew_many_sets_all_leases_and_returns_count() {
        let registry = ActiveLeaseRegistry::new();

        let (count, expires_at) = registry.renew_many(["conv-1", "conv-2"]);

        assert_eq!(count, 2);
        assert_eq!(registry.active_until("conv-1"), Some(expires_at));
        assert_eq!(registry.active_until("conv-2"), Some(expires_at));
    }

    #[test]
    fn active_lookup_returns_none_for_missing_lease() {
        let registry = ActiveLeaseRegistry::new();

        assert_eq!(registry.active_until("missing"), None);
        assert!(!registry.is_active("missing"));
    }

    #[test]
    fn active_lookup_lazily_removes_expired_lease() {
        let registry = ActiveLeaseRegistry::with_ttl_ms(1);
        registry.renew("conv-1");

        std::thread::sleep(Duration::from_millis(5));

        assert_eq!(registry.active_until("conv-1"), None);
        assert!(!registry.leases.contains_key("conv-1"));
    }
}
