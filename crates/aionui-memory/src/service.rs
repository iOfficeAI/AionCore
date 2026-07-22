//! Memory domain business operations.

use std::sync::Arc;

use aionui_api_types::MemoryUpdateInput;

use crate::{EvidenceBuildRequest, EvidenceBuilder, MemoryError};

/// Domain service that owns Memory business-operation entry points.
#[derive(Clone)]
pub struct MemoryService {
    evidence_builder: Arc<EvidenceBuilder>,
}

impl MemoryService {
    /// Creates a service with dependencies supplied by application composition.
    pub fn new(evidence_builder: Arc<EvidenceBuilder>) -> Self {
        Self { evidence_builder }
    }

    /// Reconstructs validated, sanitized evidence for the registered Memory task.
    pub fn build_evidence(&self, request: EvidenceBuildRequest) -> Result<MemoryUpdateInput, MemoryError> {
        self.evidence_builder.build(request)
    }
}
