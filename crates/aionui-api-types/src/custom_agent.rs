//! Request/response types for Custom Agent CRUD endpoints.
//!
//! Custom agents are user-defined rows in the `agent_metadata` table.
//! They share the same storage and spawn path as builtin agents, but are
//! owned/edited via `/api/agents/custom/*` endpoints exposed to the
//! settings UI (F-CAGENT-04 / -05 / -12 / -13 / -14 in the frontend
//! PRD).

use serde::{Deserialize, Serialize};

use crate::agent_discovery::{AgentEnvEntry, BehaviorPolicy};

/// Request body for `PUT /api/agents/{id}/overrides`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetAgentOverridesRequest {
    #[serde(default)]
    pub command_override: Option<String>,
    #[serde(default)]
    pub env_override: Option<Vec<AgentEnvEntry>>,
}

/// Response body for `GET /api/agents/{id}/overrides`.
#[derive(Debug, Clone, Serialize)]
pub struct AgentOverridesResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_override: Option<String>,
    pub env_override: Vec<AgentEnvEntry>,
}

/// Payload shared by `POST /api/agents/custom` and
/// `PUT  /api/agents/custom/{id}`.
///
/// Field coverage matches the frontend editor (F-CAGENT-07/-08/-09/-10):
/// name/command required; icon/args/env optional; `advanced` carries the
/// subset of `AgentMetadata` columns exposed via the JSON advanced panel.
/// Unknown keys inside `advanced` are silently dropped (serde default),
/// mirroring `handleSubmit` in `InlineAgentEditor.tsx`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAgentUpsertRequest {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced: Option<CustomAgentAdvancedOverrides>,
}

/// Optional overrides exposed through the JSON advanced editor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomAgentAdvancedOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skills_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior_policy: Option<BehaviorPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Request body for `PATCH /api/agents/{id}/enabled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEnabledRequest {
    pub enabled: bool,
}

/// Response body for `DELETE /api/agents/custom/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteCustomAgentResponse {
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn advanced_silently_drops_unknown_keys() {
        let payload = json!({
            "yolo_id": "bypassPermissions",
            "unknown_field": 42,
            "another": "ignored"
        });
        let parsed: CustomAgentAdvancedOverrides = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.yolo_id.as_deref(), Some("bypassPermissions"));
        let roundtrip = serde_json::to_value(&parsed).unwrap();
        assert!(roundtrip.get("unknown_field").is_none());
        assert!(roundtrip.get("another").is_none());
    }

    /// `skill_delivery` must NOT be settable through the custom-agent overrides.
    ///
    /// The AionUi editor parses this struct with a per-field WHITELIST, so adding
    /// a field here without the matching frontend change means a user can type it
    /// in and have it silently discarded — worse than it being unsupported,
    /// because there is no feedback. It is also a vendor capability declaration
    /// that belongs to the registry/probe, not a user preference: a custom agent
    /// with no declaration falls to `injected`, which works.
    ///
    /// Opening this up must be a single change that touches both repositories.
    #[test]
    fn skill_delivery_is_not_a_custom_agent_override() {
        let payload = json!({
            "yolo_id": "bypassPermissions",
            "skill_delivery": { "mode": "argv", "args": ["--plugin-dir", "/tmp/x"] }
        });
        let parsed: CustomAgentAdvancedOverrides = serde_json::from_value(payload).unwrap();
        let roundtrip = serde_json::to_value(&parsed).unwrap();
        assert!(
            roundtrip.get("skill_delivery").is_none(),
            "skill_delivery must not round-trip through the custom-agent overrides: {roundtrip}"
        );
    }

    #[test]
    fn upsert_request_minimal_payload() {
        let payload = json!({
            "name": "My Agent",
            "command": "my-cli"
        });
        let req: CustomAgentUpsertRequest = serde_json::from_value(payload).unwrap();
        assert_eq!(req.name, "My Agent");
        assert_eq!(req.command, "my-cli");
        assert!(req.args.is_empty());
        assert!(req.env.is_empty());
        assert!(req.advanced.is_none());
    }
}
