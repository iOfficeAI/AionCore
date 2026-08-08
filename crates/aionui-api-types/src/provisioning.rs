//! Trusted local provisioning protocol surface (A0/A1).
//!
//! Conversation-independent, least-privilege contracts for pc-tools and other
//! non-agent callers. This module defines the versioned wire shapes, scopes,
//! managed provenance, and stable error codes. Runtime discovery and mutation
//! engines live in `aionui-app`.
//!
//! Contract ownership: iOfficeAI/AionCore#795 (A0) and #798 (A1).

use serde::{Deserialize, Serialize};

/// Current protocol major version advertised by this build.
pub const PROVISION_PROTOCOL_VERSION: u32 = 1;

/// Schema version for capability/discovery documents.
pub const PROVISION_SCHEMA_VERSION: u32 = 1;

/// Vendor-supported local provisioning contract name.
pub const PROVISION_CONTRACT: &str = "trusted-local-provisioning";

/// Separately revocable capability scopes.
///
/// Possession of one scope must not authorize another. `TeamDefinition` is the
/// A1 capability and remains independent of assistant/MCP/skill management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionScope {
    AssistantManagement,
    McpConfiguration,
    SkillRegistration,
    TeamDefinition,
}

impl ProvisionScope {
    pub const ALL: [Self; 4] = [
        Self::AssistantManagement,
        Self::McpConfiguration,
        Self::SkillRegistration,
        Self::TeamDefinition,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AssistantManagement => "assistant_management",
            Self::McpConfiguration => "mcp_configuration",
            Self::SkillRegistration => "skill_registration",
            Self::TeamDefinition => "team_definition",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "assistant_management" => Some(Self::AssistantManagement),
            "mcp_configuration" => Some(Self::McpConfiguration),
            "skill_registration" => Some(Self::SkillRegistration),
            "team_definition" => Some(Self::TeamDefinition),
            _ => None,
        }
    }
}

/// Stable, bounded error vocabulary for provisioning callers.
///
/// Codes are intentional public contract — do not rename without a protocol
/// version bump. Wire form is always the `PROVISION_*` string from [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvisionErrorCode {
    /// Caller targeted a different installation/profile than the grant attests.
    WrongProfile,
    /// Grant lifetime elapsed.
    AuthorityExpired,
    /// Grant was explicitly revoked (account switch, logout, or revoke).
    AuthorityRevoked,
    /// Conditional write lost a revision race with UI/agent/provisioner.
    ConcurrentConflict,
    /// Team/runtime is active/starting/stopping/removing; fail closed.
    RuntimeBusy,
    /// Requested operation needs a scope the grant does not hold.
    ScopeMissing,
    /// JSON payload missing or structurally invalid.
    InvalidPayload,
    /// No discoverable installation under the resolved data-dir.
    InstallationNotFound,
    /// Backend is not running (closed-app) or endpoint advertisement is stale.
    BackendClosed,
    /// Backend is running but unreachable without implying local-default identity.
    BackendUnavailable,
    /// Grant subject does not match the currently attested principal.
    SubjectMismatch,
    /// Caller requested an unsupported protocol version.
    UnsupportedVersion,
    /// Assistant is referenced by a Team and cannot be deleted yet.
    TeamReferencedAssistant,
    /// Team roster has zero or multiple leaders.
    InvalidLeader,
    /// Duplicate or empty member_key in Team roster.
    InvalidMemberKey,
    /// Grant identity/handle is unknown to this installation.
    AuthorityUnknown,
    /// Operation refused because authority is anonymous/missing.
    AuthorityMissing,
    /// Resource logical id not found under managed provenance.
    ResourceNotFound,
    /// Desired state violates a protocol invariant.
    InvalidDesiredState,
    /// Delete disposition for an owned resource is unknown — cannot claim success.
    DispositionUnknown,
}

impl ProvisionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WrongProfile => "PROVISION_WRONG_PROFILE",
            Self::AuthorityExpired => "PROVISION_AUTHORITY_EXPIRED",
            Self::AuthorityRevoked => "PROVISION_AUTHORITY_REVOKED",
            Self::ConcurrentConflict => "PROVISION_CONCURRENT_CONFLICT",
            Self::RuntimeBusy => "PROVISION_RUNTIME_BUSY",
            Self::ScopeMissing => "PROVISION_SCOPE_MISSING",
            Self::InvalidPayload => "PROVISION_INVALID_PAYLOAD",
            Self::InstallationNotFound => "PROVISION_INSTALLATION_NOT_FOUND",
            Self::BackendClosed => "PROVISION_BACKEND_CLOSED",
            Self::BackendUnavailable => "PROVISION_BACKEND_UNAVAILABLE",
            Self::SubjectMismatch => "PROVISION_SUBJECT_MISMATCH",
            Self::UnsupportedVersion => "PROVISION_UNSUPPORTED_VERSION",
            Self::TeamReferencedAssistant => "PROVISION_TEAM_REFERENCED_ASSISTANT",
            Self::InvalidLeader => "PROVISION_INVALID_LEADER",
            Self::InvalidMemberKey => "PROVISION_INVALID_MEMBER_KEY",
            Self::AuthorityUnknown => "PROVISION_AUTHORITY_UNKNOWN",
            Self::AuthorityMissing => "PROVISION_AUTHORITY_MISSING",
            Self::ResourceNotFound => "PROVISION_RESOURCE_NOT_FOUND",
            Self::InvalidDesiredState => "PROVISION_INVALID_DESIRED_STATE",
            Self::DispositionUnknown => "PROVISION_DISPOSITION_UNKNOWN",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "PROVISION_WRONG_PROFILE" => Some(Self::WrongProfile),
            "PROVISION_AUTHORITY_EXPIRED" => Some(Self::AuthorityExpired),
            "PROVISION_AUTHORITY_REVOKED" => Some(Self::AuthorityRevoked),
            "PROVISION_CONCURRENT_CONFLICT" => Some(Self::ConcurrentConflict),
            "PROVISION_RUNTIME_BUSY" => Some(Self::RuntimeBusy),
            "PROVISION_SCOPE_MISSING" => Some(Self::ScopeMissing),
            "PROVISION_INVALID_PAYLOAD" => Some(Self::InvalidPayload),
            "PROVISION_INSTALLATION_NOT_FOUND" => Some(Self::InstallationNotFound),
            "PROVISION_BACKEND_CLOSED" => Some(Self::BackendClosed),
            "PROVISION_BACKEND_UNAVAILABLE" => Some(Self::BackendUnavailable),
            "PROVISION_SUBJECT_MISMATCH" => Some(Self::SubjectMismatch),
            "PROVISION_UNSUPPORTED_VERSION" => Some(Self::UnsupportedVersion),
            "PROVISION_TEAM_REFERENCED_ASSISTANT" => Some(Self::TeamReferencedAssistant),
            "PROVISION_INVALID_LEADER" => Some(Self::InvalidLeader),
            "PROVISION_INVALID_MEMBER_KEY" => Some(Self::InvalidMemberKey),
            "PROVISION_AUTHORITY_UNKNOWN" => Some(Self::AuthorityUnknown),
            "PROVISION_AUTHORITY_MISSING" => Some(Self::AuthorityMissing),
            "PROVISION_RESOURCE_NOT_FOUND" => Some(Self::ResourceNotFound),
            "PROVISION_INVALID_DESIRED_STATE" => Some(Self::InvalidDesiredState),
            "PROVISION_DISPOSITION_UNKNOWN" => Some(Self::DispositionUnknown),
            _ => None,
        }
    }
}

impl Serialize for ProvisionErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProvisionErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::unknown_variant(&raw, &["PROVISION_*"]))
    }
}

/// Vendor-native managed provenance attached to provisioned resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProvenance {
    /// Stable caller logical identity (opaque to AionUi UI codes).
    pub logical_id: String,
    /// Optional native AionUi id after materialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    /// Monotonic whole-resource revision for conditional writes.
    pub revision: u64,
    /// Manager label (e.g. `pc-tools`); not an authority claim.
    pub managed_by: String,
    /// Unix-ms when the managed record was first created.
    pub created_at_ms: i64,
    /// Unix-ms when the managed record was last reconciled.
    pub updated_at_ms: i64,
}

/// Attested installation / profile / subject claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionAttestation {
    pub protocol_version: u32,
    pub schema_version: u32,
    pub installation_id: String,
    pub profile_id: String,
    pub identity_mode: String,
    pub aioncore_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aionui_version: Option<String>,
    pub subject: ProvisionSubject,
    pub backend: ProvisionBackendState,
    pub capabilities: Vec<ProvisionScope>,
}

/// Principal currently bound to the installation/profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionSubject {
    /// Opaque subject id. Never caller-selected as authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_generation: Option<i64>,
    /// `unknown` until a principal is attested; never silently `system_default_user`.
    pub status: ProvisionSubjectStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionSubjectStatus {
    Unknown,
    Attested,
    Absent,
}

/// Whether the backend process is running and how the CLI discovered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionBackendState {
    pub state: ProvisionBackendAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Discovery method — never port scan.
    pub discovery: ProvisionDiscoveryMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionBackendAvailability {
    Running,
    Closed,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionDiscoveryMethod {
    /// Endpoint advertisement file under the installation data-dir.
    DataDirEndpointFile,
}

/// Endpoint advertisement written by a running aioncore into its data-dir.
///
/// Callers must not supply a port; the CLI resolves this file from `--data-dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalProvisionEndpoint {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub installation_id: String,
    pub profile_id: String,
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub base_url: String,
    pub identity_mode: String,
    pub aioncore_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aionui_version: Option<String>,
    pub started_at_ms: i64,
    pub capabilities: Vec<ProvisionScope>,
}

/// Request a short-lived scoped grant after attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionAuthorizeRequest {
    pub protocol_version: u32,
    pub installation_id: String,
    pub profile_id: String,
    /// Scopes requested. Unknown scopes fail closed.
    pub scopes: Vec<ProvisionScope>,
    /// Optional manager label recorded on grants and provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    /// Optional TTL seconds; server clamps to a safe maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// Opaque grant handle returned by authorize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionGrant {
    pub grant_id: String,
    pub protocol_version: u32,
    pub installation_id: String,
    pub profile_id: String,
    pub subject: ProvisionSubject,
    pub scopes: Vec<ProvisionScope>,
    pub expires_at_ms: i64,
    pub managed_by: String,
}

/// Common request envelope for mutating operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionAuthContext {
    pub grant_id: String,
    pub installation_id: String,
    pub profile_id: String,
}

/// Conditional whole-assistant reconcile (A0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantReconcileRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    /// When set, must match current managed revision or the write fails with
    /// [`ProvisionErrorCode::ConcurrentConflict`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub desired: AssistantDesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDesiredState {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<AssistantPlacement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantPlacement {
    pub sort_order: i32,
}

/// Exact assistant readback after reconcile/get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantReadback {
    pub provenance: ManagedProvenance,
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<AssistantPlacement>,
    /// Team adjacency: exact list, or unknown when not queryable.
    pub team_adjacency: TeamAdjacency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamAdjacency {
    Exact { team_logical_ids: Vec<String> },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantGetRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDeleteRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Conditional MCP reconcile (A0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpReconcileRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub desired: McpDesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpDesiredState {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    /// Opaque transport document; secrets must not be logged.
    pub transport: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpReadback {
    pub provenance: ManagedProvenance,
    pub name: String,
    pub enabled: bool,
    pub transport: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpGetRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpDeleteRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Conditional skill registration/activation (A0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillReconcileRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub desired: SkillDesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDesiredState {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillReadback {
    pub provenance: ManagedProvenance,
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillGetRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDeleteRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// A1 Team-definition create/update desired state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDefinitionUpsertRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub desired: TeamDefinitionDesiredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDefinitionDesiredState {
    pub name: String,
    /// Ordered roster; leader first is recommended but not required —
    /// exactly one member must have role `leader`.
    pub members: Vec<TeamMemberDesired>,
    /// Only `vendor_auto_shared` is accepted; callers supply no local path.
    #[serde(default = "default_workspace_policy")]
    pub workspace_policy: TeamWorkspacePolicy,
}

fn default_workspace_policy() -> TeamWorkspacePolicy {
    TeamWorkspacePolicy::VendorAutoShared
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkspacePolicy {
    VendorAutoShared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberDesired {
    /// Stable caller logical key unique within the team.
    pub member_key: String,
    pub role: TeamMemberRole,
    pub display_name: String,
    /// Logical managed assistant id (+ optional expected revision).
    pub assistant_logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberRole {
    Leader,
    Teammate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDefinitionReadback {
    pub provenance: ManagedProvenance,
    pub name: String,
    pub members: Vec<TeamMemberReadback>,
    pub workspace_policy: TeamWorkspacePolicy,
    /// Runtime is never started by definition ops; always report observed state.
    pub runtime: TeamRuntimeObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberReadback {
    pub member_key: String,
    pub role: TeamMemberRole,
    pub display_name: String,
    pub assistant_logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_assistant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRuntimeObservation {
    pub state: TeamRuntimeState,
    /// Definition ops must not start runtime; true only if pre-existing activity.
    pub started_by_definition_ops: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRuntimeState {
    Idle,
    Starting,
    Active,
    Stopping,
    Removing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDefinitionGetRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDefinitionDeleteRequest {
    pub auth: ProvisionAuthContext,
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// Exact resource disposition reported by Team delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDeleteDisposition {
    pub team_absent: bool,
    pub conversations: Vec<ResourceDisposition>,
    pub mailboxes: Vec<ResourceDisposition>,
    pub tasks: Vec<ResourceDisposition>,
    pub history: Vec<ResourceDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDisposition {
    pub resource_id: String,
    pub outcome: DispositionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionOutcome {
    Deleted,
    Preserved,
    Unknown,
}

/// Structured protocol error body (stdout JSON when CLI fails closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionErrorBody {
    pub code: ProvisionErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// True when the engine guarantees zero mutation occurred.
    pub zero_mutation: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_roundtrip_snake_case() {
        for scope in ProvisionScope::ALL {
            let raw = serde_json::to_string(&scope).unwrap();
            assert_eq!(raw, format!("\"{}\"", scope.as_str()));
            let parsed: ProvisionScope = serde_json::from_str(&raw).unwrap();
            assert_eq!(parsed, scope);
            assert_eq!(ProvisionScope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(ProvisionScope::parse("not_a_scope"), None);
    }

    #[test]
    fn error_codes_are_stable_strings() {
        assert_eq!(ProvisionErrorCode::WrongProfile.as_str(), "PROVISION_WRONG_PROFILE");
        assert_eq!(
            ProvisionErrorCode::ConcurrentConflict.as_str(),
            "PROVISION_CONCURRENT_CONFLICT"
        );
        assert_eq!(ProvisionErrorCode::RuntimeBusy.as_str(), "PROVISION_RUNTIME_BUSY");
        assert_eq!(
            ProvisionErrorCode::AuthorityExpired.as_str(),
            "PROVISION_AUTHORITY_EXPIRED"
        );
        assert_eq!(
            ProvisionErrorCode::AuthorityRevoked.as_str(),
            "PROVISION_AUTHORITY_REVOKED"
        );
        let wire = serde_json::to_string(&ProvisionErrorCode::AuthorityMissing).unwrap();
        assert_eq!(wire, "\"PROVISION_AUTHORITY_MISSING\"");
        let parsed: ProvisionErrorCode = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, ProvisionErrorCode::AuthorityMissing);
    }

    #[test]
    fn assistant_reconcile_request_parses_expected_revision() {
        let raw = r#"{
            "auth": {
                "grant_id": "g1",
                "installation_id": "inst",
                "profile_id": "prof"
            },
            "logical_id": "asst-1",
            "expected_revision": 3,
            "desired": {
                "name": "Helper",
                "enabled": false,
                "rule": "be helpful",
                "model": "mock",
                "permission": "default",
                "thought_level": "medium",
                "skills": ["s1"],
                "mcps": ["m1"],
                "placement": { "sort_order": 10 }
            }
        }"#;
        let req: AssistantReconcileRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.expected_revision, Some(3));
        assert!(!req.desired.enabled);
        assert_eq!(req.desired.placement.unwrap().sort_order, 10);
    }

    #[test]
    fn team_definition_requires_member_keys_and_roles() {
        let raw = r#"{
            "auth": {
                "grant_id": "g1",
                "installation_id": "inst",
                "profile_id": "prof"
            },
            "logical_id": "team-1",
            "desired": {
                "name": "Ops",
                "members": [
                    {
                        "member_key": "lead",
                        "role": "leader",
                        "display_name": "Lead",
                        "assistant_logical_id": "asst-1"
                    },
                    {
                        "member_key": "mate",
                        "role": "teammate",
                        "display_name": "Mate",
                        "assistant_logical_id": "asst-1",
                        "assistant_revision": 2
                    }
                ]
            }
        }"#;
        let req: TeamDefinitionUpsertRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.desired.members.len(), 2);
        assert_eq!(req.desired.workspace_policy, TeamWorkspacePolicy::VendorAutoShared);
        assert_eq!(req.desired.members[0].role, TeamMemberRole::Leader);
    }

    #[test]
    fn team_adjacency_and_runtime_enums_serialize_stably() {
        let adjacency = TeamAdjacency::Exact {
            team_logical_ids: vec!["t1".into()],
        };
        let json = serde_json::to_value(&adjacency).unwrap();
        assert_eq!(json["kind"], "exact");
        assert_eq!(json["team_logical_ids"][0], "t1");

        let runtime = TeamRuntimeState::Idle;
        assert_eq!(serde_json::to_string(&runtime).unwrap(), "\"idle\"");
    }
}
