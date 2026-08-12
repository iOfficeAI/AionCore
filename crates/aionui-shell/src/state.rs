use std::sync::Arc;

use aionui_system::ClientPrefService;

use crate::shell::ShellService;
use crate::stt::SttService;

#[derive(Clone)]
pub struct ShellRouterState {
    pub shell_service: Arc<ShellService>,
    pub stt_service: Arc<SttService>,
    pub client_pref_service: ClientPrefService,
    /// In hosted identity modes, require a live site administrator for
    /// operating host applications and restrict member STT to official endpoints.
    pub require_host_admin: bool,
}
