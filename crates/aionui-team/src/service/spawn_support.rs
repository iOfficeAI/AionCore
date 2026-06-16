use super::*;
use aionui_api_types::BehaviorPolicy;
use aionui_common::AgentType;
use aionui_common::constants::{TEAM_CAPABLE_BACKENDS, has_mcp_capability};

use crate::provisioning::PersistSpawnedAgentRequest;

/// Known ACP vendor labels. Kept in lockstep with the `agent_metadata`
/// seed in `005_agent_metadata.sql` — a caller hitting an unknown
/// vendor should trigger a schema drift discussion, not silently fall
/// through.
const ACP_VENDOR_LABELS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "qwen",
    "codebuddy",
    "droid",
    "goose",
    "auggie",
    "kimi",
    "opencode",
    "copilot",
    "qoder",
    "vibe",
    "cursor",
    "kiro",
    "hermes",
    "snow",
];

const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";

pub(crate) fn parse_agent_type(backend: &str) -> Result<AgentType, TeamError> {
    // Any registered ACP vendor label collapses to `AgentType::Acp`.
    if ACP_VENDOR_LABELS.contains(&backend) {
        return Ok(AgentType::Acp);
    }
    // Otherwise interpret as a top-level `AgentType` (e.g. "acp",
    // "nanobot", "aionrs", "remote", "openclaw-gateway").
    let quoted = format!("\"{backend}\"");
    if let Ok(agent_type) = serde_json::from_str::<AgentType>(&quoted) {
        if agent_type.is_deprecated_runtime() {
            return Err(TeamError::InvalidRequest(DEPRECATED_AGENT_TYPE_MESSAGE.into()));
        }
        return Ok(agent_type);
    }
    Err(TeamError::InvalidRequest(format!("unsupported backend: {backend}")))
}

/// Resolve the most permissive session mode for a given backend string.
/// Reuses `AgentType::full_auto_mode_id` from aionui-common.
pub(crate) fn resolve_full_auto_mode(backend: &str) -> &'static str {
    let agent_type = if ACP_VENDOR_LABELS.contains(&backend) {
        AgentType::Acp
    } else {
        let quoted = format!("\"{backend}\"");
        serde_json::from_str::<AgentType>(&quoted).unwrap_or(AgentType::Acp)
    };
    agent_type.full_auto_mode_id(Some(backend))
}

impl TeamSessionService {
    /// Check if a backend is allowed to participate in team mode.
    /// Hard whitelist passes immediately; then checks behavior_policy.supports_team;
    /// finally queries persisted `agent_capabilities` for MCP transport declarations.
    pub(crate) async fn is_backend_team_capable(&self, backend: &str) -> bool {
        if TEAM_CAPABLE_BACKENDS.contains(&backend) {
            return true;
        }
        let Ok(Some(row)) = self.agent_metadata_repo.find_builtin_by_backend(backend).await else {
            return false;
        };
        let bp_supports = row
            .behavior_policy
            .as_deref()
            .and_then(|s| serde_json::from_str::<BehaviorPolicy>(s).ok())
            .is_some_and(|bp| bp.supports_team);
        if bp_supports {
            return true;
        }
        let caps = row
            .agent_capabilities
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        has_mcp_capability(caps.as_ref())
    }

    /// Return all backends currently team-capable (hard whitelist + behavior_policy + dynamically detected).
    /// Used to build the Lead prompt's `available_agent_types` list.
    pub(crate) async fn list_team_capable_backends(&self) -> Vec<(String, String)> {
        let Ok(rows) = self.agent_metadata_repo.list_all().await else {
            return TEAM_CAPABLE_BACKENDS
                .iter()
                .map(|b| (b.to_string(), capitalize(b)))
                .collect();
        };
        let mut result: Vec<(String, String)> = Vec::new();
        for row in &rows {
            if !row.enabled {
                continue;
            }
            // Use backend if present, otherwise agent_type as identifier
            let key = match row.backend.as_deref() {
                Some(b) => b.to_string(),
                None => row.agent_type.clone(),
            };

            // Check behavior_policy.supports_team (covers agents with backend=NULL like aionrs)
            let bp_supports = row
                .behavior_policy
                .as_deref()
                .and_then(|s| serde_json::from_str::<BehaviorPolicy>(s).ok())
                .is_some_and(|bp| bp.supports_team);
            if bp_supports {
                result.push((key, row.name.clone()));
                continue;
            }

            // Hard whitelist (only works when backend is present)
            if let Some(backend) = row.backend.as_deref()
                && TEAM_CAPABLE_BACKENDS.contains(&backend)
            {
                result.push((key, row.name.clone()));
                continue;
            }

            // Dynamic MCP detection
            let caps = row
                .agent_capabilities
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            if has_mcp_capability(caps.as_ref()) {
                result.push((key, row.name.clone()));
            }
        }
        // Ensure hard whitelist entries are present even if not in DB
        for &b in TEAM_CAPABLE_BACKENDS {
            if !result.iter().any(|(bk, _)| bk == b) {
                result.push((b.to_string(), capitalize(b)));
            }
        }
        result
    }

    /// Return the `team_list_models` response built from DB rows.
    /// Falls back to the hardcoded response if the DB query fails.
    /// For internal agents (like aionrs with backend=NULL), enriches
    /// with models from the providers table.
    pub(crate) async fn list_models_from_db(&self, agent_type_filter: Option<&str>) -> serde_json::Value {
        let Ok(rows) = self.agent_metadata_repo.list_all().await else {
            return crate::mcp::tools::handle_team_list_models(&serde_json::Value::Null);
        };
        let provider_models = self.collect_provider_models().await;
        crate::mcp::tools::build_list_models_from_rows(&rows, agent_type_filter, &provider_models)
    }

    /// Collect all enabled provider model IDs grouped by provider name.
    /// Returns a flat list of model IDs for use by internal agents (aionrs).
    async fn collect_provider_models(&self) -> Vec<String> {
        let Ok(providers) = self.provider_repo.list().await else {
            return vec![];
        };
        providers
            .into_iter()
            .filter(|p| p.enabled)
            .flat_map(|p| serde_json::from_str::<Vec<String>>(&p.models).unwrap_or_default())
            .collect()
    }

    pub(crate) async fn default_model_for_backend(&self, backend: &str) -> Option<String> {
        let row = self.agent_metadata_repo.find_builtin_by_backend(backend).await.ok()??;
        let json: serde_json::Value = serde_json::from_str(row.available_models.as_deref()?).ok()?;
        if let Some(id) = json.get("current_model_id").and_then(|v| v.as_str())
            && !id.is_empty()
        {
            return Some(id.to_owned());
        }
        let arr = json
            .get("available_models")
            .and_then(|v| v.as_array())
            .or_else(|| json.as_array())?;
        arr.first()
            .and_then(|e| e.get("id").and_then(|v| v.as_str()))
            .map(|s| s.to_owned())
    }

    pub async fn spawn_agent_in_session(
        &self,
        team_id: &str,
        caller_slot_id: &str,
        req: crate::session::SpawnAgentRequest,
    ) -> Result<TeamAgent, TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        entry.session.spawn_agent(caller_slot_id, req).await
    }

    pub fn dispose_all(&self) {
        let keys: Vec<String> = self.sessions.iter().map(|entry| entry.key().clone()).collect();
        for key in keys {
            self.stop_session_unchecked(&key);
        }
        info!("All team sessions disposed");
    }

    /// Create the conversation + persist the new agent slot for a spawn.
    ///
    /// Holds the per-team `add_agent` lock for the entirety of the
    /// read-modify-write on `teams.agents`, matching [`TeamSessionService::add_agent`]
    /// (W4-D23) so concurrent spawns cannot race and drop slots.
    ///
    /// The lock is *not* held across the process warmup step — callers
    /// (`TeamSession::spawn_agent`) wire that up separately so a slow
    /// `warmup` never stalls other spawns against the same team.
    pub(crate) async fn persist_spawned_agent(&self, req: PersistSpawnedAgentRequest) -> Result<TeamAgent, TeamError> {
        let lock = self
            .add_agent_locks
            .entry(req.team_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        self.provisioner().persist_spawned_agent(req).await
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::workspace_harness::{
        force_team_workspace, setup_with_factory_metadata_team_repo_and_conversation_repo, single_agent_team_request,
    };

    #[test]
    fn parse_agent_type_known_backends() {
        assert_eq!(parse_agent_type("acp").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("gemini").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("aionrs").unwrap(), AgentType::Aionrs);
    }

    #[test]
    fn parse_agent_type_rejects_deprecated_runtime_types() {
        for backend in ["nanobot", "remote", "openclaw-gateway"] {
            let err = parse_agent_type(backend).unwrap_err();
            assert!(matches!(err, TeamError::InvalidRequest(_)));
            assert!(
                err.to_string()
                    .contains("This agent type is no longer supported for new conversations."),
                "unexpected error for {backend}: {err}"
            );
        }
    }

    #[test]
    fn parse_agent_type_unknown_backend_returns_error() {
        let err = parse_agent_type("unknown").unwrap_err();
        assert!(matches!(err, TeamError::InvalidRequest(_)));
    }

    #[test]
    fn resolve_full_auto_mode_keeps_hermes_on_default() {
        assert_eq!(resolve_full_auto_mode("hermes"), "default");
    }

    #[tokio::test]
    async fn persist_spawned_agent_uses_team_workspace_resolver() {
        let (svc, team_repo, _, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user1", single_agent_team_request("Spawn Legacy"))
            .await
            .unwrap();
        let leader_workspace = conv_repo.get_extra(&created.agents[0].conversation_id).unwrap()["workspace"]
            .as_str()
            .unwrap()
            .to_owned();

        force_team_workspace(&team_repo, &created.id, "").await;

        let spawned = svc
            .persist_spawned_agent(PersistSpawnedAgentRequest {
                team_id: created.id.clone(),
                user_id: "user1".into(),
                slot_id: "spawn-slot-1".into(),
                name: "Spawned".into(),
                backend: "acp".into(),
                model: "claude".into(),
                custom_agent_id: None,
            })
            .await
            .unwrap();

        let got = svc.get_team("user1", &created.id).await.unwrap();
        assert_eq!(got.workspace, leader_workspace);
        let spawned_extra = conv_repo.get_extra(&spawned.conversation_id).unwrap();
        assert_eq!(
            spawned_extra.get("workspace").and_then(serde_json::Value::as_str),
            Some(leader_workspace.as_str())
        );
    }
}
