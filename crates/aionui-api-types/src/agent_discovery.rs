//! Unified agent metadata surfaced to the frontend.
//!
//! A single type replaces the previous split of `DetectedAgent` (API
//! response) and `AgentMetadata` (internal cache): the same shape is
//! stored in the `agent_metadata` table, cached in the process, and
//! returned over HTTP. The DB row feeds everything.
//!
//! Handshake-derived fields (`agent_capabilities` / `auth_methods` /
//! `config_options` / `available_modes` / `available_models` /
//! `available_commands`) stay as opaque JSON so this crate does not
//! depend on the ACP protocol SDK — the ai-agent crate typed-decodes
//! them when it needs to.

use aionui_common::{AgentType, TimestampMs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How an agent row was sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
    /// Ships with the backend binary (no CLI install required — e.g. `aionrs`).
    Internal,
    /// Seeded from the migration (ACP vendors, nanobot, openclaw).
    Builtin,
    /// Installed from the extension hub.
    Extension,
    /// User-defined row.
    Custom,
}

/// Environment variable entry passed to a spawned agent process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEnvEntry {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Source-specific bookkeeping (how to probe, how to upgrade, which Hub
/// package it came from).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSourceInfo {
    /// Primary CLI binary checked for availability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_name: Option<String>,
    /// Extra binary required when the row spawns via a bridge (e.g. `npx`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_binary: Option<String>,
    /// Hub package identifier when `agent_source = "extension"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_package_id: Option<String>,
    /// Version string for Hub or custom rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Adapter-side behaviour switches. These drive code branches that used
/// to be hardcoded per `AcpBackend`; new keys are added by extending
/// this struct — we deliberately avoid a free-form "extra" bag so every
/// flag is type-checked at its usage sites.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BehaviorPolicy {
    #[serde(default)]
    pub supports_side_question: bool,

    /// The agent's CLI bakes the model identity into its session
    /// system prompt at launch time and does not refresh it when
    /// `session/set_model` is called. Callers should inject a
    /// `<system-reminder>` before the next prompt so the model
    /// answers with the user-selected identity rather than the
    /// stale cached one.
    #[serde(default)]
    pub self_identity_sticky: bool,

    /// The agent does not implement the generic ACP `session/load`
    /// method. To resume, callers must call `session/new` again and
    /// pass the prior session id through a vendor-specific
    /// `_meta.<vendor>.options.resume` field.
    #[serde(default)]
    pub session_load_via_meta_field: bool,

    #[serde(default)]
    pub supports_team: bool,

    /// Explicitly override team-mode inference for protocol outliers.
    ///
    /// ACP normally requires stdio MCP support, so the presence of
    /// `mcp_capabilities` is enough to infer team support. Some adapters accept
    /// `mcpServers` but do not wire them into the wrapped agent; those adapters
    /// must set this to `Some(false)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_capable_override: Option<bool>,
}

/// Handshake-derived fields captured from the ACP init/session-response.
///
/// All fields are opaque JSON at this layer: they are passed through to
/// the frontend verbatim, and typed-decoded inside `aionui-ai-agent`
/// when the adapter needs them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentHandshake {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_modes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_models: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_commands: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentManagementStatus {
    Missing,
    Online,
    Offline,
    Unchecked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSnapshotCheckStatus {
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSnapshotCheckKind {
    Startup,
    Scheduled,
    Manual,
    Session,
}

/// A single `backend → logo URL` pair in the agent logo catalog.
///
/// Returned by `GET /api/agents/logos` so business surfaces can resolve
/// an agent logo from a backend identifier without owning a path map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLogoEntry {
    pub backend: String,
    pub logo: String,
}

/// The unified, decoded view of an `agent_metadata` row.
///
/// This remains the refresh/logos/custom-agent CRUD read model for the
/// legacy agent catalog, even though business surfaces now consume
/// assistants instead of `GET /api/agents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetadata {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_i18n: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_i18n: Option<serde_json::Value>,

    /// Vendor label (e.g. "claude"). `None` for agents without vendor
    /// grouping (remote / internal / nanobot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub agent_type: AgentType,
    pub agent_source: AgentSource,
    #[serde(default)]
    pub agent_source_info: AgentSourceInfo,

    pub enabled: bool,

    /// Whether the spawn command was resolvable on `$PATH` at hydrate time.
    ///
    /// Derived at discovery time — not a persisted column. Serialized so
    /// the frontend can show "installed / missing" status without a
    /// second round-trip.
    pub available: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Absolute path to the spawn command, resolved via `which()` at
    /// hydrate time. `Some` iff `available` is `true`. Server-internal:
    /// the frontend only cares about `available`, so this field is
    /// never serialized over the wire.
    #[serde(default, skip)]
    pub resolved_command: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skills_dirs: Option<Vec<String>>,

    #[serde(default)]
    pub behavior_policy: BehaviorPolicy,

    /// Native mode id that AionUi's legacy `yolo` / `yoloNoSandbox`
    /// aliases resolve to before calling `session/set_mode`. `None`
    /// means the backend has no "yolo" equivalent and the alias should
    /// pass through unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_id: Option<String>,

    /// Display ordering key — smaller values appear first. The range
    /// scheme is documented in `007_agent_metadata_sort_order.sql`.
    pub sort_order: i64,

    /// Whether this agent supports team mode. Derived at hydrate time from
    /// the behavior policy, hard whitelist, and persisted `agent_capabilities`
    /// MCP declarations. Not a persisted column.
    #[serde(default)]
    pub team_capable: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_status: Option<AgentSnapshotCheckStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_kind: Option<AgentSnapshotCheckKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_guidance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_latency_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<TimestampMs>,

    #[serde(default)]
    pub handshake: AgentHandshake,

    /// Internal carrier: whether the agent row has a command override set.
    /// Computed in decode_row and projected to `AgentManagementRow`.
    #[serde(skip)]
    pub has_command_override: bool,
    /// Internal carrier: count of non-blocked env override keys.
    /// Computed in decode_row and projected to `AgentManagementRow`.
    #[serde(skip)]
    pub env_override_key_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManagementRow {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_i18n: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_i18n: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub agent_type: AgentType,
    pub agent_source: AgentSource,
    #[serde(default)]
    pub agent_source_info: AgentSourceInfo,
    pub enabled: bool,
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skills_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub behavior_policy: BehaviorPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_modes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_models: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_commands: Option<serde_json::Value>,
    pub sort_order: i64,
    #[serde(default)]
    pub team_capable: bool,
    pub status: AgentManagementStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_status: Option<AgentSnapshotCheckStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_kind: Option<AgentSnapshotCheckKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_error_details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_guidance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_latency_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<TimestampMs>,
    #[serde(default)]
    pub has_command_override: bool,
    #[serde(default)]
    pub env_override_key_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_source_serde_roundtrip() {
        for (variant, expected) in [
            (AgentSource::Internal, "internal"),
            (AgentSource::Builtin, "builtin"),
            (AgentSource::Extension, "extension"),
            (AgentSource::Custom, "custom"),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, format!("\"{expected}\""));
            let parsed: AgentSource = serde_json::from_str(&s).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn agent_metadata_skips_empty_fields() {
        let meta = AgentMetadata {
            id: "abc12345".into(),
            icon: None,
            name: "Claude".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude".into()),
            agent_type: AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 3100,
            team_capable: true,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["id"], "abc12345");
        // Server-internal fields are stripped from the wire form.
        assert!(v.get("resolved_command").is_none());
        assert_eq!(v["backend"], "claude");
        assert_eq!(v["available"], true);
        assert_eq!(v["team_capable"], true);
        assert!(v.get("command").is_none());
        assert!(v.get("icon").is_none());
    }

    #[test]
    fn agent_metadata_deserializes_minimal_payload() {
        let payload = json!({
            "id": "x",
            "name": "y",
            "agent_type": "acp",
            "agent_source": "custom",
            "enabled": true,
            "available": false,
            "sort_order": 1100,
        });
        let meta: AgentMetadata = serde_json::from_value(payload).unwrap();
        assert_eq!(meta.agent_type, AgentType::Acp);
        assert_eq!(meta.agent_source, AgentSource::Custom);
        assert!(!meta.available);
        assert!(!meta.behavior_policy.supports_side_question);
        assert!(meta.last_check_status.is_none());
        assert!(meta.handshake.agent_capabilities.is_none());
    }

    #[test]
    fn agent_management_status_serializes_snake_case() {
        let value = serde_json::to_value(AgentManagementStatus::Offline).unwrap();
        assert_eq!(value, json!("offline"));
        let value = serde_json::to_value(AgentManagementStatus::Unchecked).unwrap();
        assert_eq!(value, json!("unchecked"));
    }
}

#[cfg(test)]
mod behavior_policy_tests {
    use super::BehaviorPolicy;

    #[test]
    fn deserializes_new_capability_flags() {
        let json = serde_json::json!({
            "supports_side_question": true,
            "self_identity_sticky": true,
            "session_load_via_meta_field": true,
        });
        let policy: BehaviorPolicy = serde_json::from_value(json).unwrap();
        assert!(policy.supports_side_question);
        assert!(policy.self_identity_sticky);
        assert!(policy.session_load_via_meta_field);
    }

    #[test]
    fn defaults_to_false_when_flags_omitted() {
        let policy: BehaviorPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!policy.supports_side_question);
        assert!(!policy.self_identity_sticky);
        assert!(!policy.session_load_via_meta_field);
        assert!(!policy.supports_team);
    }

    #[test]
    fn supports_team_defaults_false_and_roundtrips() {
        let empty: BehaviorPolicy = serde_json::from_str("{}").unwrap();
        assert!(!empty.supports_team);
        assert_eq!(empty.team_capable_override, None);

        let with_team: BehaviorPolicy = serde_json::from_str(r#"{"supports_team":true}"#).unwrap();
        assert!(with_team.supports_team);

        let serialized = serde_json::to_string(&with_team).unwrap();
        assert!(serialized.contains("\"supports_team\":true"));
        assert!(!serialized.contains("team_capable_override"));
    }

    #[test]
    fn team_capable_override_roundtrips_explicit_false() {
        let policy: BehaviorPolicy = serde_json::from_str(r#"{"team_capable_override":false}"#).unwrap();
        assert_eq!(policy.team_capable_override, Some(false));

        let serialized = serde_json::to_string(&policy).unwrap();
        assert!(serialized.contains("\"team_capable_override\":false"));
    }
}
