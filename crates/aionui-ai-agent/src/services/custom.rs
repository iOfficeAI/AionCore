//! Custom Agent business logic.
//!
//! Extends `AgentService` with CRUD for `agent_source = 'custom'` rows
//! in the `agent_metadata` catalog. Mirrors the frontend PRD
//! F-CAGENT-04 / -05 / -12 / -13 / -14 (create, edit, save, delete,
//! toggle enable).
//!
//! Test-on-save: create / update run `try_connect_custom_agent`
//! before hitting the DB. Failures become `AgentError::BadRequest` with
//! a prefixed marker (`cli_not_found:` / `acp_init_failed:`) that the
//! frontend maps back to the same three Alert states it shows for the
//! manual "Test connection" button.

use std::collections::HashMap;

use crate::error::AgentError;
use aionui_api_types::{
    AgentHandshake, AgentMetadata, CustomAgentUpsertRequest, TryConnectCustomAgentRequest,
    TryConnectCustomAgentResponse,
};
use aionui_common::generate_short_id;
use aionui_db::{AgentMetadataRow, UpsertAgentMetadataParams};
use tracing::warn;

use super::AgentService;
use crate::protocol::custom_agent_probe::{
    try_connect_custom_agent as probe, try_connect_custom_agent_with_catalog as probe_with_handshake,
};
use crate::runtime_status::custom_agent_runtime_reporter;

const CUSTOM_SORT_ORDER_DEFAULT: i64 = 1500;

impl AgentService {
    /// Public accessor for the probe — powers both
    /// `POST /api/agents/custom/try-connect` and the test-on-save path
    /// below.
    pub async fn try_connect_custom_agent(
        &self,
        user_id: &str,
        req: TryConnectCustomAgentRequest,
    ) -> Result<TryConnectCustomAgentResponse, AgentError> {
        if req.command.trim().is_empty() {
            return Err(AgentError::bad_request("command must not be empty"));
        }
        let reporter = req.runtime_scope_id.as_ref().map(|scope_id| {
            custom_agent_runtime_reporter(self.broadcaster().clone(), user_id.to_owned(), scope_id.clone())
        });
        Ok(probe(&req.command, &req.acp_args, &req.env, reporter.as_deref()).await)
    }

    pub async fn create_custom_agent(
        &self,
        user_id: &str,
        req: CustomAgentUpsertRequest,
    ) -> Result<AgentMetadata, AgentError> {
        validate_upsert(&req)?;
        let probed = probe_or_reject(&req).await?;

        let id = generate_short_id();
        self.upsert_custom_row(
            user_id,
            &id,
            &req,
            /* keep_enabled = */ true,
            None,
            probed.as_ref(),
        )
        .await
    }

    pub async fn update_custom_agent(
        &self,
        user_id: &str,
        id: &str,
        req: CustomAgentUpsertRequest,
    ) -> Result<AgentMetadata, AgentError> {
        validate_upsert(&req)?;
        let existing = self
            .registry()
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AgentError::forbidden(
                "Only custom agents can be edited via this endpoint",
            ));
        }
        let probed = probe_or_reject(&req).await?;

        let keep_enabled = existing.enabled;
        self.upsert_custom_row(user_id, id, &req, keep_enabled, Some(&existing), probed.as_ref())
            .await
    }

    pub async fn delete_custom_agent(&self, user_id: &str, id: &str) -> Result<(), AgentError> {
        let existing = self
            .registry()
            .repo_handle()
            .get_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.get_for_user: {e}")))?
            .ok_or_else(|| AgentError::not_found(format!("Agent '{id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AgentError::forbidden(
                "Only custom agents can be deleted via this endpoint",
            ));
        }
        let removed = self
            .registry()
            .repo_handle()
            .delete_for_user(user_id, id)
            .await
            .map_err(|e| AgentError::internal(format!("repo.delete_for_user: {e}")))?;
        if !removed {
            return Err(AgentError::not_found(format!("Agent '{id}' not found")));
        }
        // Agents are machine-level, so the machine cache must be refreshed
        // regardless of which user triggered the change. reload_one is a safe
        // no-op for rows the machine cache never held (a non-default user's own
        // custom agent).
        if let Err(err) = self.registry().reload_one(id).await {
            warn!(agent_id = %id, error = %err, "registry reload failed after delete_custom_agent");
        }
        Ok(())
    }

    pub async fn set_agent_enabled(&self, user_id: &str, id: &str, enabled: bool) -> Result<AgentMetadata, AgentError> {
        let updated = self
            .registry()
            .repo_handle()
            .set_enabled_for_user(user_id, id, enabled)
            .await
            .map_err(|e| AgentError::internal(format!("repo.set_enabled_for_user: {e}")))?;
        if !updated {
            return Err(AgentError::not_found(format!("Agent '{id}' not found")));
        }
        // enabled is machine-level and gates the registry's runtime start, so
        // any user's toggle must refresh the machine cache — not just the
        // default user's. (Previously gated on SYSTEM_DEFAULT_USER_ID, which
        // left a builtin toggled by another user stale in the cache.)
        if let Err(err) = self.registry().reload_one(id).await {
            warn!(agent_id = %id, error = %err, "registry reload failed after set_agent_enabled");
        }
        self.registry()
            .get_for_user(user_id, id)
            .await?
            .ok_or_else(|| AgentError::internal(format!("Agent '{id}' not visible after enable toggle")))
    }

    async fn upsert_custom_row(
        &self,
        user_id: &str,
        id: &str,
        req: &CustomAgentUpsertRequest,
        enabled: bool,
        existing: Option<&AgentMetadataRow>,
        probed: Option<&AgentHandshake>,
    ) -> Result<AgentMetadata, AgentError> {
        let advanced = req.advanced.clone().unwrap_or_default();

        // Handshake snapshot. The upsert rewrites every column, so a value we
        // don't pass here is cleared. Take the capabilities the save-time probe
        // just observed; otherwise keep what an earlier probe or session stored,
        // so editing a custom agent no longer wipes its snapshot. Capabilities
        // are written in the same statement (not via the async catalog channel)
        // so they are visible as soon as the create/update response returns:
        // a team attach right after creation must already see them.
        let encode = |v: Option<&serde_json::Value>, field: &str| -> Result<Option<String>, AgentError> {
            v.map(|v| serde_json::to_string(v).map_err(|e| AgentError::internal(format!("encode {field}: {e}"))))
                .transpose()
        };
        let keep = |pick: fn(&AgentMetadataRow) -> &Option<String>| existing.and_then(|row| pick(row).clone());
        let agent_capabilities = encode(probed.and_then(|h| h.agent_capabilities.as_ref()), "agent_capabilities")?
            .or_else(|| keep(|r| &r.agent_capabilities));
        let auth_methods =
            encode(probed.and_then(|h| h.auth_methods.as_ref()), "auth_methods")?.or_else(|| keep(|r| &r.auth_methods));
        let config_options = keep(|r| &r.config_options);
        let available_modes = keep(|r| &r.available_modes);
        let available_models = keep(|r| &r.available_models);
        let available_commands = keep(|r| &r.available_commands);

        let args_json =
            serde_json::to_string(&req.args).map_err(|e| AgentError::internal(format!("encode args: {e}")))?;
        let env_json = serde_json::to_string(&req.env).map_err(|e| AgentError::internal(format!("encode env: {e}")))?;
        let native_skills_dirs_json = advanced
            .native_skills_dirs
            .as_ref()
            .map(|v| {
                serde_json::to_string(v).map_err(|e| AgentError::internal(format!("encode native_skills_dirs: {e}")))
            })
            .transpose()?;
        let behavior_policy_json = advanced
            .behavior_policy
            .as_ref()
            .map(|v| serde_json::to_string(v).map_err(|e| AgentError::internal(format!("encode behavior_policy: {e}"))))
            .transpose()?;

        let source_info = serde_json::json!({
            "binary_name": first_token(&req.command),
        });
        let source_info_json = source_info.to_string();

        let params = UpsertAgentMetadataParams {
            id,
            icon: req.icon.as_deref(),
            name: req.name.trim(),
            name_i18n: None,
            description: advanced.description.as_deref(),
            description_i18n: None,
            backend: None,
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(&source_info_json),
            enabled,
            command: Some(req.command.trim()),
            args: Some(&args_json),
            env: Some(&env_json),
            native_skills_dirs: native_skills_dirs_json.as_deref(),
            skill_delivery: None,
            behavior_policy: behavior_policy_json.as_deref(),
            yolo_id: advanced.yolo_id.as_deref(),
            agent_capabilities: agent_capabilities.as_deref(),
            auth_methods: auth_methods.as_deref(),
            config_options: config_options.as_deref(),
            available_modes: available_modes.as_deref(),
            available_models: available_models.as_deref(),
            available_commands: available_commands.as_deref(),
            sort_order: CUSTOM_SORT_ORDER_DEFAULT,
        };

        self.registry()
            .repo_handle()
            .upsert_for_user(user_id, &params)
            .await
            .map_err(|e| AgentError::internal(format!("repo.upsert_for_user: {e}")))?;

        // Machine-level cache refresh; harmless no-op when the row is a
        // non-default user's own custom agent (never in the machine cache).
        self.registry()
            .reload_one(id)
            .await
            .map_err(|e| AgentError::internal(format!("registry reload: {e}")))?;

        // The probe's catalog (modes / models / config options) goes through
        // the same serialized channel a live session uses, so it gets the same
        // config-option merge and enrichment. Additive: `None` fields are skipped.
        if let Some(handshake) = probed {
            self.registry()
                .catalog_sender()
                .send_partial(user_id.to_owned(), id.to_owned(), handshake.clone());
        }

        self.registry()
            .get_for_user(user_id, id)
            .await?
            .ok_or_else(|| AgentError::internal(format!("Agent '{id}' not visible after upsert")))
    }
}

fn validate_upsert(req: &CustomAgentUpsertRequest) -> Result<(), AgentError> {
    if req.name.trim().is_empty() {
        return Err(AgentError::bad_request("name must not be empty"));
    }
    if req.command.trim().is_empty() {
        return Err(AgentError::bad_request("command must not be empty"));
    }
    Ok(())
}

/// Test-on-save. On success, returns the handshake the probe observed (if the
/// agent advertised anything) so the caller can persist it.
async fn probe_or_reject(req: &CustomAgentUpsertRequest) -> Result<Option<AgentHandshake>, AgentError> {
    // Test-only bypass — real probe spawns a child process and relies
    // on a working ACP CLI on PATH, which is not present in CI.
    // Gated behind cfg(test) / the `test-support` feature so production
    // builds cannot be tricked into skipping the probe via env var.
    #[cfg(any(test, feature = "test-support"))]
    if std::env::var("AIONUI_BYPASS_PROBE").is_ok() {
        tracing::warn!("AIONUI_BYPASS_PROBE set — skipping custom agent probe. Test-only.");
        return Ok(None);
    }

    let env_map: HashMap<String, String> = req.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect();
    match probe_with_handshake(&req.command, &req.args, &env_map, None).await {
        (TryConnectCustomAgentResponse::Success, handshake) => Ok(handshake.map(|h| *h)),
        // Reachable but not authorized is a valid agent the user simply hasn't
        // logged into yet — accept the save so it lands in the list (offline,
        // needs-login), where a later "test connection" confirms recovery.
        (TryConnectCustomAgentResponse::FailAuth { error }, _) => {
            tracing::info!(%error, "custom agent reachable but requires auth; accepting save");
            Ok(None)
        }
        (TryConnectCustomAgentResponse::FailCli { error }, _) => {
            Err(AgentError::bad_request(format!("cli_not_found: {error}")))
        }
        (TryConnectCustomAgentResponse::FailAcp { error }, _) => {
            Err(AgentError::bad_request(format!("acp_init_failed: {error}")))
        }
    }
}

fn first_token(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
}
