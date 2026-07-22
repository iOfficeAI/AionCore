//! Router state for the Memory domain.

use std::sync::Arc;

use crate::evidence::EvidenceBuilder;

/// Dependencies supplied by application composition when Memory routes are added.
#[derive(Clone)]
pub struct MemoryRouterState {
    pub evidence_builder: Arc<EvidenceBuilder>,
}

impl Default for MemoryRouterState {
    fn default() -> Self {
        Self {
            evidence_builder: Arc::new(EvidenceBuilder),
        }
    }
}
