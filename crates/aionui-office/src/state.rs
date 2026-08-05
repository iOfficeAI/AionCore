use std::path::PathBuf;
use std::sync::Arc;

use crate::conversion::ConversionService;
use crate::proxy::ProxyService;
use crate::snapshot::SnapshotService;
use crate::watch_manager::OfficecliWatchManager;

#[derive(Clone)]
pub struct OfficeRouterState {
    pub watch_manager: Arc<OfficecliWatchManager>,
    pub snapshot_service: Arc<SnapshotService>,
    pub conversion_service: Arc<ConversionService>,
    pub proxy_service: Arc<ProxyService>,
    pub allowed_roots: Vec<PathBuf>,
    /// Resolves a `ChatFileRef` preview target to an absolute path server-side
    /// (`start_preview`), so pe→path resolution stays on the backend.
    pub project: Arc<aionui_project::ProjectService>,
}
