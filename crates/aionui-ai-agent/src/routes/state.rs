use std::sync::Arc;

use crate::{AgentRegistry, AgentService, RemoteAgentService};

/// Router state for remote agent routes.
#[derive(Clone)]
pub struct RemoteAgentRouterState {
    pub service: Arc<RemoteAgentService>,
    /// Require a live site administrator for operations that create, mutate,
    /// or connect remote agents. Enabled for every hosted identity mode.
    pub require_host_admin: bool,
}

#[derive(Clone)]
pub struct AgentRouterState {
    pub agent_registry: Arc<AgentRegistry>,
    pub service: Arc<AgentService>,
    /// Require a live site administrator for host-process discovery and
    /// custom-agent management. Enabled for every hosted identity mode.
    pub require_host_admin: bool,
}
