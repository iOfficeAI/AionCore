use std::path::PathBuf;
use std::sync::Arc;

use crate::conversion::ConversionService;
use crate::proxy::ProxyService;
use crate::watch_manager::OfficecliWatchManager;

#[derive(Clone)]
pub struct OfficeRouterState {
    pub watch_manager: Arc<OfficecliWatchManager>,
    pub conversion_service: Arc<ConversionService>,
    pub proxy_service: Arc<ProxyService>,
    pub allowed_roots: Vec<PathBuf>,
    /// Whether a request-supplied `workspace` may widen `allowed_roots`
    /// (desktop: `true`; hosted/multi-user: `false`). See `aionui_file::FileService`.
    pub trust_client_roots: bool,
    /// Resolves a `ChatFileRef` preview target to an absolute path server-side
    /// (`start_preview`), so pe→path resolution stays on the backend.
    pub project: Arc<aionui_project::ProjectService>,
}
