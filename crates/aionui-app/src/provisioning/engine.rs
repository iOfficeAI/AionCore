//! In-process provisioning protocol engine (A0/A1 surface skeleton).
//!
//! Enforces scope separation, installation/profile binding, grant
//! expiry/revocation, conditional revision checks, Team roster invariants,
//! and exact readback shapes. Persistence into adopted-principal stores is
//! intentionally out of scope for this skeleton — zero-mutation fail-closed
//! paths and shape contracts are production-facing; durable writes are not
//! claimed complete.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use aionui_api_types::{
    AssistantDeleteRequest, AssistantDesiredState, AssistantGetRequest, AssistantReadback, AssistantReconcileRequest,
    DispositionOutcome, ManagedProvenance, McpDeleteRequest, McpDesiredState, McpGetRequest, McpReadback,
    McpReconcileRequest, PROVISION_PROTOCOL_VERSION, ProvisionAttestation, ProvisionAuthContext,
    ProvisionAuthorizeRequest, ProvisionBackendAvailability, ProvisionBackendState, ProvisionDiscoveryMethod,
    ProvisionErrorCode, ProvisionGrant, ProvisionScope, ProvisionSubject, ProvisionSubjectStatus, ResourceDisposition,
    SkillDeleteRequest, SkillDesiredState, SkillGetRequest, SkillReadback, SkillReconcileRequest, TeamAdjacency,
    TeamDefinitionDeleteRequest, TeamDefinitionDesiredState, TeamDefinitionGetRequest, TeamDefinitionReadback,
    TeamDefinitionUpsertRequest, TeamDeleteDisposition, TeamMemberReadback, TeamMemberRole, TeamRuntimeObservation,
    TeamRuntimeState, TeamWorkspacePolicy,
};

const DEFAULT_GRANT_TTL_SECONDS: u64 = 300;
const MAX_GRANT_TTL_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionEngineError {
    pub code: ProvisionErrorCode,
    pub message: &'static str,
    pub field: Option<&'static str>,
    pub zero_mutation: bool,
}

impl ProvisionEngineError {
    fn fail_closed(code: ProvisionErrorCode, message: &'static str) -> Self {
        Self {
            code,
            message,
            field: None,
            zero_mutation: true,
        }
    }

    fn with_field(mut self, field: &'static str) -> Self {
        self.field = Some(field);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrantRecord {
    grant: ProvisionGrant,
    revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedAssistant {
    provenance: ManagedProvenance,
    desired: AssistantDesiredState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedMcp {
    provenance: ManagedProvenance,
    desired: McpDesiredState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedSkill {
    provenance: ManagedProvenance,
    desired: SkillDesiredState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedTeam {
    provenance: ManagedProvenance,
    name: String,
    members: Vec<TeamMemberReadback>,
    workspace_policy: TeamWorkspacePolicy,
    runtime: TeamRuntimeObservation,
}

/// Protocol engine used by the provision CLI and unit tests.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProvisionEngine {
    grants: HashMap<String, GrantRecord>,
    assistants: BTreeMap<String, ManagedAssistant>,
    mcps: BTreeMap<String, ManagedMcp>,
    skills: BTreeMap<String, ManagedSkill>,
    teams: BTreeMap<String, ManagedTeam>,
    /// Foreign (non-managed) resource ids that must never be mutated.
    foreign_mcp_ids: BTreeSet<String>,
    foreign_skill_ids: BTreeSet<String>,
    /// Simulated runtime busy team logical ids.
    busy_teams: BTreeSet<String>,
    /// Fixed clock override for deterministic tests.
    clock_ms: Option<i64>,
    grant_seq: u64,
    native_seq: u64,
}

impl ProvisionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock_ms(mut self, clock_ms: i64) -> Self {
        self.clock_ms = Some(clock_ms);
        self
    }

    /// Relative store path under a data-dir for durable protocol state.
    pub const STORE_RELATIVE_PATH: &'static str = "runtime/provision-engine-state.json";

    /// Load durable protocol state from the installation data-dir, or empty.
    pub fn load_from_data_dir(data_dir: &Path) -> Self {
        let path = data_dir.join(Self::STORE_RELATIVE_PATH);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist durable protocol state (grants + managed resources). Best-effort.
    pub fn save_to_data_dir(&self, data_dir: &Path) -> std::io::Result<()> {
        let path = data_dir.join(Self::STORE_RELATIVE_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, err)
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                data_dir.join(Self::STORE_RELATIVE_PATH),
                std::fs::Permissions::from_mode(0o600),
            );
        }
        Ok(())
    }

    pub fn mark_foreign_mcp(&mut self, id: impl Into<String>) {
        self.foreign_mcp_ids.insert(id.into());
    }

    pub fn mark_foreign_skill(&mut self, id: impl Into<String>) {
        self.foreign_skill_ids.insert(id.into());
    }

    pub fn mark_team_runtime_busy(&mut self, logical_id: impl Into<String>, state: TeamRuntimeState) {
        let id = logical_id.into();
        self.busy_teams.insert(id.clone());
        if let Some(team) = self.teams.get_mut(&id) {
            team.runtime.state = state;
        }
    }

    fn now(&self) -> i64 {
        self.clock_ms.unwrap_or_else(now_ms)
    }

    /// Authorize a short-lived grant bound to the attested installation/subject.
    ///
    /// The caller supplies installation/profile for binding checks only; the
    /// subject is taken from `attestation` and never from caller-selected ids.
    pub fn authorize(
        &mut self,
        request: ProvisionAuthorizeRequest,
        attestation: &ProvisionAttestation,
    ) -> Result<ProvisionGrant, ProvisionEngineError> {
        if request.protocol_version != PROVISION_PROTOCOL_VERSION {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::UnsupportedVersion,
                "unsupported protocol version",
            )
            .with_field("protocol_version"));
        }
        if request.installation_id != attestation.installation_id || request.profile_id != attestation.profile_id {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::WrongProfile,
                "installation or profile does not match attestation",
            ));
        }
        if attestation.subject.status != ProvisionSubjectStatus::Attested {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::AuthorityMissing,
                "no attested adopted principal; refusing anonymous authority",
            ));
        }
        if request.scopes.is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidPayload,
                "at least one scope is required",
            )
            .with_field("scopes"));
        }
        for scope in &request.scopes {
            if !attestation.capabilities.contains(scope) {
                return Err(ProvisionEngineError::fail_closed(
                    ProvisionErrorCode::ScopeMissing,
                    "requested scope is not advertised by this installation",
                )
                .with_field("scopes"));
            }
        }

        let ttl = request
            .ttl_seconds
            .unwrap_or(DEFAULT_GRANT_TTL_SECONDS)
            .min(MAX_GRANT_TTL_SECONDS);
        let now = self.now();
        self.grant_seq += 1;
        let grant = ProvisionGrant {
            grant_id: format!("grant_{}", self.grant_seq),
            protocol_version: PROVISION_PROTOCOL_VERSION,
            installation_id: attestation.installation_id.clone(),
            profile_id: attestation.profile_id.clone(),
            subject: attestation.subject.clone(),
            scopes: request.scopes.clone(),
            expires_at_ms: now.saturating_add((ttl as i64).saturating_mul(1000)),
            managed_by: request
                .managed_by
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "external-provisioner".to_owned()),
        };
        self.grants.insert(
            grant.grant_id.clone(),
            GrantRecord {
                grant: grant.clone(),
                revoked: false,
            },
        );
        Ok(grant)
    }

    pub fn revoke(&mut self, grant_id: &str) -> Result<(), ProvisionEngineError> {
        let record = self.grants.get_mut(grant_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::AuthorityUnknown, "unknown grant id")
                .with_field("grant_id")
        })?;
        record.revoked = true;
        Ok(())
    }

    /// Simulate account switch: revoke all grants and rebind subject.
    pub fn account_switch(&mut self, new_subject: ProvisionSubject) {
        for record in self.grants.values_mut() {
            record.revoked = true;
        }
        // Managed resources stay owned by prior subject at the protocol layer
        // until a new grant under the new subject is obtained; no rebinding.
        let _ = new_subject;
    }

    pub fn reconcile_assistant(
        &mut self,
        request: AssistantReconcileRequest,
    ) -> Result<AssistantReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::AssistantManagement)?;
        if request.logical_id.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidPayload,
                "logical_id is required",
            )
            .with_field("logical_id"));
        }
        if request.desired.name.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "assistant name is required",
            )
            .with_field("desired.name"));
        }

        let now = self.now();
        let managed_by = self.grant_managed_by(&request.auth)?;
        let entry = self.assistants.get(&request.logical_id);
        if let Some(expected) = request.expected_revision {
            match entry {
                Some(existing) if existing.provenance.revision == expected => {}
                Some(_) => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "assistant revision mismatch; refusing overwrite",
                    )
                    .with_field("expected_revision"));
                }
                None if expected == 0 => {}
                None => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "assistant does not exist at expected revision",
                    )
                    .with_field("expected_revision"));
                }
            }
        }

        let (revision, created_at_ms, native_id) = match entry {
            Some(existing) => (
                existing.provenance.revision.saturating_add(1),
                existing.provenance.created_at_ms,
                existing.provenance.native_id.clone(),
            ),
            None => {
                self.native_seq += 1;
                (1, now, Some(format!("native_asst_{}", self.native_seq)))
            }
        };

        let managed = ManagedAssistant {
            provenance: ManagedProvenance {
                logical_id: request.logical_id.clone(),
                native_id,
                revision,
                managed_by,
                created_at_ms,
                updated_at_ms: now,
            },
            desired: request.desired,
        };
        self.assistants.insert(request.logical_id.clone(), managed);
        self.assistant_readback(&request.logical_id)
    }

    pub fn get_assistant(&self, request: AssistantGetRequest) -> Result<AssistantReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::AssistantManagement)?;
        self.assistant_readback(&request.logical_id)
    }

    pub fn delete_assistant(&mut self, request: AssistantDeleteRequest) -> Result<(), ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::AssistantManagement)?;
        let existing = self.assistants.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed assistant not found")
                .with_field("logical_id")
        })?;
        if let Some(expected) = request.expected_revision
            && existing.provenance.revision != expected
        {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::ConcurrentConflict,
                "assistant revision mismatch; refusing delete",
            )
            .with_field("expected_revision"));
        }
        let refs = self.team_refs_for_assistant(&request.logical_id);
        if !refs.is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::TeamReferencedAssistant,
                "assistant is referenced by a Team definition; refuse delete",
            )
            .with_field("logical_id"));
        }
        self.assistants.remove(&request.logical_id);
        Ok(())
    }

    pub fn reconcile_mcp(&mut self, request: McpReconcileRequest) -> Result<McpReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::McpConfiguration)?;
        if self.foreign_mcp_ids.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "refusing to mutate foreign/user MCP resource",
            )
            .with_field("logical_id"));
        }
        self.conditional_upsert_mcp(request)
    }

    pub fn get_mcp(&self, request: McpGetRequest) -> Result<McpReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::McpConfiguration)?;
        let managed = self.mcps.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed MCP not found")
                .with_field("logical_id")
        })?;
        Ok(McpReadback {
            provenance: managed.provenance.clone(),
            name: managed.desired.name.clone(),
            enabled: managed.desired.enabled,
            transport: managed.desired.transport.clone(),
        })
    }

    pub fn delete_mcp(&mut self, request: McpDeleteRequest) -> Result<(), ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::McpConfiguration)?;
        if self.foreign_mcp_ids.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "refusing to delete foreign/user MCP resource",
            )
            .with_field("logical_id"));
        }
        let existing = self.mcps.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed MCP not found")
                .with_field("logical_id")
        })?;
        if let Some(expected) = request.expected_revision
            && existing.provenance.revision != expected
        {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::ConcurrentConflict,
                "MCP revision mismatch; refusing delete",
            )
            .with_field("expected_revision"));
        }
        // Refuse delete while any managed assistant still lists this MCP.
        for assistant in self.assistants.values() {
            if assistant.desired.mcps.iter().any(|id| id == &request.logical_id) {
                return Err(ProvisionEngineError::fail_closed(
                    ProvisionErrorCode::InvalidDesiredState,
                    "MCP is still referenced by a managed assistant",
                )
                .with_field("logical_id"));
            }
        }
        self.mcps.remove(&request.logical_id);
        Ok(())
    }

    pub fn reconcile_skill(&mut self, request: SkillReconcileRequest) -> Result<SkillReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::SkillRegistration)?;
        if self.foreign_skill_ids.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "refusing to mutate foreign/user skill resource",
            )
            .with_field("logical_id"));
        }
        self.conditional_upsert_skill(request)
    }

    pub fn get_skill(&self, request: SkillGetRequest) -> Result<SkillReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::SkillRegistration)?;
        let managed = self.skills.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed skill not found")
                .with_field("logical_id")
        })?;
        Ok(SkillReadback {
            provenance: managed.provenance.clone(),
            name: managed.desired.name.clone(),
            enabled: managed.desired.enabled,
            source_path: managed.desired.source_path.clone(),
        })
    }

    pub fn delete_skill(&mut self, request: SkillDeleteRequest) -> Result<(), ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::SkillRegistration)?;
        if self.foreign_skill_ids.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "refusing to delete foreign/user skill resource",
            )
            .with_field("logical_id"));
        }
        let existing = self.skills.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed skill not found")
                .with_field("logical_id")
        })?;
        if let Some(expected) = request.expected_revision
            && existing.provenance.revision != expected
        {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::ConcurrentConflict,
                "skill revision mismatch; refusing delete",
            )
            .with_field("expected_revision"));
        }
        for assistant in self.assistants.values() {
            if assistant.desired.skills.iter().any(|id| id == &request.logical_id) {
                return Err(ProvisionEngineError::fail_closed(
                    ProvisionErrorCode::InvalidDesiredState,
                    "skill is still referenced by a managed assistant",
                )
                .with_field("logical_id"));
            }
        }
        self.skills.remove(&request.logical_id);
        Ok(())
    }

    pub fn upsert_team(
        &mut self,
        request: TeamDefinitionUpsertRequest,
        create_only: bool,
    ) -> Result<TeamDefinitionReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::TeamDefinition)?;
        if self.busy_teams.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::RuntimeBusy,
                "team runtime is active or indeterminate; refusing definition mutation",
            )
            .with_field("logical_id"));
        }
        if let Some(existing) = self.teams.get(&request.logical_id)
            && matches!(
                existing.runtime.state,
                TeamRuntimeState::Active
                    | TeamRuntimeState::Starting
                    | TeamRuntimeState::Stopping
                    | TeamRuntimeState::Removing
                    | TeamRuntimeState::Unknown
            )
        {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::RuntimeBusy,
                "team runtime is not idle; refusing definition mutation",
            )
            .with_field("logical_id"));
        }
        if create_only && self.teams.contains_key(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "team logical id already exists",
            )
            .with_field("logical_id"));
        }

        let members = validate_team_members(&request.desired)?;
        let now = self.now();
        let managed_by = self.grant_managed_by(&request.auth)?;
        let entry = self.teams.get(&request.logical_id);
        if let Some(expected) = request.expected_revision {
            match entry {
                Some(existing) if existing.provenance.revision == expected => {}
                Some(_) => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "team revision mismatch; refusing overwrite",
                    )
                    .with_field("expected_revision"));
                }
                None if expected == 0 => {}
                None => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "team does not exist at expected revision",
                    )
                    .with_field("expected_revision"));
                }
            }
        } else if entry.is_some() && !create_only {
            // Update without expected_revision still works but is discouraged;
            // conditional semantics prefer explicit revision.
        }

        // Resolve assistant models from managed assistant store when present.
        let mut member_readbacks = Vec::with_capacity(members.len());
        for (index, member) in members.into_iter().enumerate() {
            let assistant = self.assistants.get(&member.assistant_logical_id);
            if let Some(expected) = member.assistant_revision {
                match assistant {
                    Some(asst) if asst.provenance.revision == expected => {}
                    Some(_) | None => {
                        return Err(ProvisionEngineError::fail_closed(
                            ProvisionErrorCode::InvalidDesiredState,
                            "assistant revision does not match managed assistant",
                        )
                        .with_field("desired.members.assistant_revision"));
                    }
                }
            }
            self.native_seq += 1;
            let slot_id = format!("slot_{}", self.native_seq);
            self.native_seq += 1;
            let conversation_id = format!("conv_{}", self.native_seq);
            member_readbacks.push(TeamMemberReadback {
                member_key: member.member_key,
                role: member.role,
                display_name: member.display_name,
                assistant_logical_id: member.assistant_logical_id.clone(),
                assistant_revision: member
                    .assistant_revision
                    .or_else(|| assistant.map(|a| a.provenance.revision)),
                native_slot_id: Some(slot_id),
                native_assistant_id: assistant.and_then(|a| a.provenance.native_id.clone()),
                native_conversation_id: Some(conversation_id),
                model: assistant.and_then(|a| a.desired.model.clone()),
            });
            // Stable order preserved; index only used for deterministic native ids.
            let _ = index;
        }

        let (revision, created_at_ms, native_id) = match entry {
            Some(existing) => (
                existing.provenance.revision.saturating_add(1),
                existing.provenance.created_at_ms,
                existing.provenance.native_id.clone(),
            ),
            None => {
                self.native_seq += 1;
                (1, now, Some(format!("native_team_{}", self.native_seq)))
            }
        };

        let managed = ManagedTeam {
            provenance: ManagedProvenance {
                logical_id: request.logical_id.clone(),
                native_id,
                revision,
                managed_by,
                created_at_ms,
                updated_at_ms: now,
            },
            name: request.desired.name,
            members: member_readbacks,
            workspace_policy: request.desired.workspace_policy,
            // Definition ops never start runtime.
            runtime: TeamRuntimeObservation {
                state: TeamRuntimeState::Idle,
                started_by_definition_ops: false,
            },
        };
        self.teams.insert(request.logical_id.clone(), managed);
        self.team_readback(&request.logical_id)
    }

    pub fn get_team(&self, request: TeamDefinitionGetRequest) -> Result<TeamDefinitionReadback, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::TeamDefinition)?;
        self.team_readback(&request.logical_id)
    }

    pub fn delete_team(
        &mut self,
        request: TeamDefinitionDeleteRequest,
    ) -> Result<TeamDeleteDisposition, ProvisionEngineError> {
        self.require_scope(&request.auth, ProvisionScope::TeamDefinition)?;
        if self.busy_teams.contains(&request.logical_id) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::RuntimeBusy,
                "team runtime is active or indeterminate; refusing delete",
            )
            .with_field("logical_id"));
        }
        let existing = self.teams.get(&request.logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed team not found")
                .with_field("logical_id")
        })?;
        if let Some(expected) = request.expected_revision
            && existing.provenance.revision != expected
        {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::ConcurrentConflict,
                "team revision mismatch; refusing delete",
            )
            .with_field("expected_revision"));
        }
        if matches!(
            existing.runtime.state,
            TeamRuntimeState::Active
                | TeamRuntimeState::Starting
                | TeamRuntimeState::Stopping
                | TeamRuntimeState::Removing
                | TeamRuntimeState::Unknown
        ) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::RuntimeBusy,
                "team runtime is not idle; refusing delete",
            )
            .with_field("logical_id"));
        }

        let conversations: Vec<ResourceDisposition> = existing
            .members
            .iter()
            .filter_map(|member| {
                member.native_conversation_id.as_ref().map(|id| ResourceDisposition {
                    resource_id: id.clone(),
                    outcome: DispositionOutcome::Deleted,
                })
            })
            .collect();
        let mailboxes = conversations
            .iter()
            .map(|c| ResourceDisposition {
                resource_id: format!("mailbox:{}", c.resource_id),
                outcome: DispositionOutcome::Deleted,
            })
            .collect::<Vec<_>>();
        let tasks = vec![ResourceDisposition {
            resource_id: format!("tasks:{}", request.logical_id),
            outcome: DispositionOutcome::Deleted,
        }];
        let history = vec![ResourceDisposition {
            resource_id: format!("history:{}", request.logical_id),
            outcome: DispositionOutcome::Deleted,
        }];

        // Refuse success if any disposition is unknown.
        for group in [&conversations, &mailboxes, &tasks, &history] {
            if group.iter().any(|d| d.outcome == DispositionOutcome::Unknown) {
                return Err(ProvisionEngineError::fail_closed(
                    ProvisionErrorCode::DispositionUnknown,
                    "cannot report successful delete while disposition is unknown",
                ));
            }
        }

        self.teams.remove(&request.logical_id);
        Ok(TeamDeleteDisposition {
            team_absent: true,
            conversations,
            mailboxes,
            tasks,
            history,
        })
    }

    fn conditional_upsert_mcp(&mut self, request: McpReconcileRequest) -> Result<McpReadback, ProvisionEngineError> {
        if request.desired.name.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "MCP name is required",
            )
            .with_field("desired.name"));
        }
        let now = self.now();
        let managed_by = self.grant_managed_by(&request.auth)?;
        let entry = self.mcps.get(&request.logical_id);
        if let Some(expected) = request.expected_revision {
            match entry {
                Some(existing) if existing.provenance.revision == expected => {}
                Some(_) | None => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "MCP revision mismatch; refusing overwrite",
                    )
                    .with_field("expected_revision"));
                }
            }
        }
        let (revision, created_at_ms, native_id) = match entry {
            Some(existing) => (
                existing.provenance.revision.saturating_add(1),
                existing.provenance.created_at_ms,
                existing.provenance.native_id.clone(),
            ),
            None => {
                self.native_seq += 1;
                (1, now, Some(format!("native_mcp_{}", self.native_seq)))
            }
        };
        let managed = ManagedMcp {
            provenance: ManagedProvenance {
                logical_id: request.logical_id.clone(),
                native_id,
                revision,
                managed_by,
                created_at_ms,
                updated_at_ms: now,
            },
            desired: request.desired,
        };
        let readback = McpReadback {
            provenance: managed.provenance.clone(),
            name: managed.desired.name.clone(),
            enabled: managed.desired.enabled,
            transport: managed.desired.transport.clone(),
        };
        self.mcps.insert(request.logical_id, managed);
        Ok(readback)
    }

    fn conditional_upsert_skill(
        &mut self,
        request: SkillReconcileRequest,
    ) -> Result<SkillReadback, ProvisionEngineError> {
        if request.desired.name.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "skill name is required",
            )
            .with_field("desired.name"));
        }
        let now = self.now();
        let managed_by = self.grant_managed_by(&request.auth)?;
        let entry = self.skills.get(&request.logical_id);
        if let Some(expected) = request.expected_revision {
            match entry {
                Some(existing) if existing.provenance.revision == expected => {}
                Some(_) | None => {
                    return Err(ProvisionEngineError::fail_closed(
                        ProvisionErrorCode::ConcurrentConflict,
                        "skill revision mismatch; refusing overwrite",
                    )
                    .with_field("expected_revision"));
                }
            }
        }
        let (revision, created_at_ms, native_id) = match entry {
            Some(existing) => (
                existing.provenance.revision.saturating_add(1),
                existing.provenance.created_at_ms,
                existing.provenance.native_id.clone(),
            ),
            None => {
                self.native_seq += 1;
                (1, now, Some(format!("native_skill_{}", self.native_seq)))
            }
        };
        let managed = ManagedSkill {
            provenance: ManagedProvenance {
                logical_id: request.logical_id.clone(),
                native_id,
                revision,
                managed_by,
                created_at_ms,
                updated_at_ms: now,
            },
            desired: request.desired,
        };
        let readback = SkillReadback {
            provenance: managed.provenance.clone(),
            name: managed.desired.name.clone(),
            enabled: managed.desired.enabled,
            source_path: managed.desired.source_path.clone(),
        };
        self.skills.insert(request.logical_id, managed);
        Ok(readback)
    }

    fn assistant_readback(&self, logical_id: &str) -> Result<AssistantReadback, ProvisionEngineError> {
        let managed = self.assistants.get(logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed assistant not found")
                .with_field("logical_id")
        })?;
        Ok(AssistantReadback {
            provenance: managed.provenance.clone(),
            name: managed.desired.name.clone(),
            enabled: managed.desired.enabled,
            rule: managed.desired.rule.clone(),
            model: managed.desired.model.clone(),
            permission: managed.desired.permission.clone(),
            thought_level: managed.desired.thought_level.clone(),
            skills: managed.desired.skills.clone(),
            mcps: managed.desired.mcps.clone(),
            placement: managed.desired.placement.clone(),
            team_adjacency: TeamAdjacency::Exact {
                team_logical_ids: self.team_refs_for_assistant(logical_id),
            },
        })
    }

    fn team_readback(&self, logical_id: &str) -> Result<TeamDefinitionReadback, ProvisionEngineError> {
        let managed = self.teams.get(logical_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::ResourceNotFound, "managed team not found")
                .with_field("logical_id")
        })?;
        Ok(TeamDefinitionReadback {
            provenance: managed.provenance.clone(),
            name: managed.name.clone(),
            members: managed.members.clone(),
            workspace_policy: managed.workspace_policy,
            runtime: managed.runtime.clone(),
        })
    }

    fn team_refs_for_assistant(&self, assistant_logical_id: &str) -> Vec<String> {
        self.teams
            .iter()
            .filter(|(_, team)| {
                team.members
                    .iter()
                    .any(|member| member.assistant_logical_id == assistant_logical_id)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn grant_managed_by(&self, auth: &ProvisionAuthContext) -> Result<String, ProvisionEngineError> {
        let grant = self.require_live_grant(auth)?;
        Ok(grant.managed_by.clone())
    }

    fn require_scope(
        &self,
        auth: &ProvisionAuthContext,
        scope: ProvisionScope,
    ) -> Result<ProvisionGrant, ProvisionEngineError> {
        let grant = self.require_live_grant(auth)?;
        if !grant.scopes.contains(&scope) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::ScopeMissing,
                "grant does not include the required scope",
            )
            .with_field("scope"));
        }
        Ok(grant)
    }

    fn require_live_grant(&self, auth: &ProvisionAuthContext) -> Result<ProvisionGrant, ProvisionEngineError> {
        if auth.grant_id.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::AuthorityMissing,
                "grant_id is required",
            )
            .with_field("grant_id"));
        }
        let record = self.grants.get(&auth.grant_id).ok_or_else(|| {
            ProvisionEngineError::fail_closed(ProvisionErrorCode::AuthorityUnknown, "unknown grant id")
                .with_field("grant_id")
        })?;
        if record.revoked {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::AuthorityRevoked,
                "grant has been revoked",
            ));
        }
        if record.grant.expires_at_ms <= self.now() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::AuthorityExpired,
                "grant has expired",
            ));
        }
        if record.grant.installation_id != auth.installation_id || record.grant.profile_id != auth.profile_id {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::WrongProfile,
                "grant is bound to a different installation/profile",
            ));
        }
        if record.grant.subject.status != ProvisionSubjectStatus::Attested {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::AuthorityMissing,
                "grant subject is not attested",
            ));
        }
        Ok(record.grant.clone())
    }
}

fn validate_team_members(
    desired: &TeamDefinitionDesiredState,
) -> Result<Vec<aionui_api_types::TeamMemberDesired>, ProvisionEngineError> {
    if desired.name.trim().is_empty() {
        return Err(ProvisionEngineError::fail_closed(
            ProvisionErrorCode::InvalidDesiredState,
            "team name is required",
        )
        .with_field("desired.name"));
    }
    if !matches!(desired.workspace_policy, TeamWorkspacePolicy::VendorAutoShared) {
        return Err(ProvisionEngineError::fail_closed(
            ProvisionErrorCode::InvalidDesiredState,
            "only vendor_auto_shared workspace policy is accepted",
        )
        .with_field("desired.workspace_policy"));
    }
    if desired.members.is_empty() {
        return Err(ProvisionEngineError::fail_closed(
            ProvisionErrorCode::InvalidLeader,
            "team requires at least one member",
        )
        .with_field("desired.members"));
    }

    let mut keys = BTreeSet::new();
    let mut leaders = 0usize;
    for member in &desired.members {
        if member.member_key.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidMemberKey,
                "member_key must be non-empty",
            )
            .with_field("desired.members.member_key"));
        }
        if !keys.insert(member.member_key.clone()) {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidMemberKey,
                "member_key must be unique within the team",
            )
            .with_field("desired.members.member_key"));
        }
        if member.assistant_logical_id.trim().is_empty() {
            return Err(ProvisionEngineError::fail_closed(
                ProvisionErrorCode::InvalidDesiredState,
                "assistant_logical_id is required",
            )
            .with_field("desired.members.assistant_logical_id"));
        }
        if matches!(member.role, TeamMemberRole::Leader) {
            leaders += 1;
        }
    }
    if leaders != 1 {
        return Err(ProvisionEngineError::fail_closed(
            ProvisionErrorCode::InvalidLeader,
            "team requires exactly one leader",
        )
        .with_field("desired.members.role"));
    }
    Ok(desired.members.clone())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build a baseline attestation document from discovery + subject claims.
pub fn attestation_from_parts(
    installation_id: String,
    profile_id: String,
    identity_mode: String,
    aioncore_version: String,
    aionui_version: Option<String>,
    subject: ProvisionSubject,
    backend: ProvisionBackendState,
) -> ProvisionAttestation {
    ProvisionAttestation {
        protocol_version: PROVISION_PROTOCOL_VERSION,
        schema_version: aionui_api_types::PROVISION_SCHEMA_VERSION,
        installation_id,
        profile_id,
        identity_mode,
        aioncore_version,
        aionui_version,
        subject,
        backend,
        capabilities: ProvisionScope::ALL.to_vec(),
    }
}

pub fn closed_backend_state() -> ProvisionBackendState {
    ProvisionBackendState {
        state: ProvisionBackendAvailability::Closed,
        pid: None,
        base_url: None,
        discovery: ProvisionDiscoveryMethod::DataDirEndpointFile,
    }
}

pub fn running_backend_state(pid: u32, base_url: String) -> ProvisionBackendState {
    ProvisionBackendState {
        state: ProvisionBackendAvailability::Running,
        pid: Some(pid),
        base_url: Some(base_url),
        discovery: ProvisionDiscoveryMethod::DataDirEndpointFile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{AssistantPlacement, TeamMemberDesired, TeamMemberRole, TeamWorkspacePolicy};
    use serde_json::json;

    fn attested_subject() -> ProvisionSubject {
        ProvisionSubject {
            subject_id: Some("user_adopted_1".into()),
            user_type: Some("aionpro".into()),
            session_generation: Some(3),
            status: ProvisionSubjectStatus::Attested,
        }
    }

    fn sample_attestation() -> ProvisionAttestation {
        attestation_from_parts(
            "inst_abc".into(),
            "prof_abc".into(),
            "aionpro".into(),
            "0.1.62".into(),
            Some("2.1.52".into()),
            attested_subject(),
            running_backend_state(9, "http://127.0.0.1:25808".into()),
        )
    }

    fn auth_for(grant: &ProvisionGrant) -> ProvisionAuthContext {
        ProvisionAuthContext {
            grant_id: grant.grant_id.clone(),
            installation_id: grant.installation_id.clone(),
            profile_id: grant.profile_id.clone(),
        }
    }

    fn authorize_scopes(engine: &mut ProvisionEngine, scopes: Vec<ProvisionScope>) -> ProvisionGrant {
        let attestation = sample_attestation();
        engine
            .authorize(
                ProvisionAuthorizeRequest {
                    protocol_version: PROVISION_PROTOCOL_VERSION,
                    installation_id: attestation.installation_id.clone(),
                    profile_id: attestation.profile_id.clone(),
                    scopes,
                    managed_by: Some("pc-tools".into()),
                    ttl_seconds: Some(600),
                },
                &attestation,
            )
            .unwrap()
    }

    #[test]
    fn authorize_binds_subject_from_attestation_not_caller() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::AssistantManagement]);
        assert_eq!(grant.subject.subject_id.as_deref(), Some("user_adopted_1"));
        assert!(grant.scopes.contains(&ProvisionScope::AssistantManagement));
        assert!(!grant.scopes.contains(&ProvisionScope::TeamDefinition));
    }

    #[test]
    fn wrong_profile_fails_before_write() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let attestation = sample_attestation();
        let err = engine
            .authorize(
                ProvisionAuthorizeRequest {
                    protocol_version: PROVISION_PROTOCOL_VERSION,
                    installation_id: "inst_other".into(),
                    profile_id: attestation.profile_id.clone(),
                    scopes: vec![ProvisionScope::AssistantManagement],
                    managed_by: None,
                    ttl_seconds: None,
                },
                &attestation,
            )
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::WrongProfile);
        assert!(err.zero_mutation);
    }

    #[test]
    fn missing_subject_is_not_anonymous_authority() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let mut attestation = sample_attestation();
        attestation.subject = ProvisionSubject {
            subject_id: None,
            user_type: None,
            session_generation: None,
            status: ProvisionSubjectStatus::Unknown,
        };
        let err = engine
            .authorize(
                ProvisionAuthorizeRequest {
                    protocol_version: PROVISION_PROTOCOL_VERSION,
                    installation_id: attestation.installation_id.clone(),
                    profile_id: attestation.profile_id.clone(),
                    scopes: vec![ProvisionScope::AssistantManagement],
                    managed_by: None,
                    ttl_seconds: None,
                },
                &attestation,
            )
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::AuthorityMissing);
        assert!(err.zero_mutation);
    }

    #[test]
    fn scope_separation_blocks_cross_domain_writes() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::AssistantManagement]);
        let auth = auth_for(&grant);

        let err = engine
            .reconcile_mcp(McpReconcileRequest {
                auth: auth.clone(),
                logical_id: "mcp-1".into(),
                expected_revision: None,
                desired: McpDesiredState {
                    name: "m".into(),
                    enabled: true,
                    transport: json!({"type": "stdio"}),
                },
            })
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::ScopeMissing);

        let err = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth,
                    logical_id: "team-1".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "T".into(),
                        members: vec![TeamMemberDesired {
                            member_key: "lead".into(),
                            role: TeamMemberRole::Leader,
                            display_name: "L".into(),
                            assistant_logical_id: "a1".into(),
                            assistant_revision: None,
                        }],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::ScopeMissing);
    }

    #[test]
    fn assistant_reconcile_is_conditional_and_reports_team_adjacency() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(
            &mut engine,
            vec![ProvisionScope::AssistantManagement, ProvisionScope::TeamDefinition],
        );
        let auth = auth_for(&grant);

        let created = engine
            .reconcile_assistant(AssistantReconcileRequest {
                auth: auth.clone(),
                logical_id: "asst-1".into(),
                expected_revision: None,
                desired: AssistantDesiredState {
                    name: "Helper".into(),
                    enabled: false,
                    rule: Some("be careful".into()),
                    model: Some("mock".into()),
                    permission: Some("default".into()),
                    thought_level: Some("medium".into()),
                    skills: vec![],
                    mcps: vec![],
                    placement: Some(AssistantPlacement { sort_order: 5 }),
                },
            })
            .unwrap();
        assert_eq!(created.provenance.revision, 1);
        assert!(!created.enabled);
        assert!(matches!(created.team_adjacency, TeamAdjacency::Exact { .. }));

        let conflict = engine
            .reconcile_assistant(AssistantReconcileRequest {
                auth: auth.clone(),
                logical_id: "asst-1".into(),
                expected_revision: Some(99),
                desired: AssistantDesiredState {
                    name: "Helper".into(),
                    enabled: true,
                    rule: None,
                    model: None,
                    permission: None,
                    thought_level: None,
                    skills: vec![],
                    mcps: vec![],
                    placement: None,
                },
            })
            .unwrap_err();
        assert_eq!(conflict.code, ProvisionErrorCode::ConcurrentConflict);
        assert!(conflict.zero_mutation);

        // Create team referencing assistant, then delete must refuse.
        let team = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth: auth.clone(),
                    logical_id: "team-1".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "Ops".into(),
                        members: vec![
                            TeamMemberDesired {
                                member_key: "lead".into(),
                                role: TeamMemberRole::Leader,
                                display_name: "Lead".into(),
                                assistant_logical_id: "asst-1".into(),
                                assistant_revision: Some(1),
                            },
                            TeamMemberDesired {
                                member_key: "mate".into(),
                                role: TeamMemberRole::Teammate,
                                display_name: "Mate".into(),
                                assistant_logical_id: "asst-1".into(),
                                assistant_revision: Some(1),
                            },
                        ],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap();
        assert_eq!(team.members.len(), 2);
        assert!(!team.runtime.started_by_definition_ops);
        assert_eq!(team.runtime.state, TeamRuntimeState::Idle);

        let blocked = engine
            .delete_assistant(AssistantDeleteRequest {
                auth: auth.clone(),
                logical_id: "asst-1".into(),
                expected_revision: Some(1),
            })
            .unwrap_err();
        assert_eq!(blocked.code, ProvisionErrorCode::TeamReferencedAssistant);
    }

    #[test]
    fn expired_and_revoked_grants_perform_zero_mutation() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::SkillRegistration]);
        let auth = auth_for(&grant);

        engine.clock_ms = Some(1_000_000 + 601_000);
        let expired = engine
            .reconcile_skill(SkillReconcileRequest {
                auth: auth.clone(),
                logical_id: "skill-1".into(),
                expected_revision: None,
                desired: SkillDesiredState {
                    name: "s".into(),
                    enabled: true,
                    source_path: None,
                },
            })
            .unwrap_err();
        assert_eq!(expired.code, ProvisionErrorCode::AuthorityExpired);
        assert!(engine.skills.is_empty());

        engine.clock_ms = Some(1_000_000);
        let grant2 = authorize_scopes(&mut engine, vec![ProvisionScope::SkillRegistration]);
        let auth2 = auth_for(&grant2);
        engine.revoke(&grant2.grant_id).unwrap();
        let revoked = engine
            .reconcile_skill(SkillReconcileRequest {
                auth: auth2,
                logical_id: "skill-1".into(),
                expected_revision: None,
                desired: SkillDesiredState {
                    name: "s".into(),
                    enabled: true,
                    source_path: None,
                },
            })
            .unwrap_err();
        assert_eq!(revoked.code, ProvisionErrorCode::AuthorityRevoked);
        assert!(engine.skills.is_empty());
    }

    #[test]
    fn foreign_mcp_is_preserved() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        engine.mark_foreign_mcp("user-mcp");
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::McpConfiguration]);
        let err = engine
            .reconcile_mcp(McpReconcileRequest {
                auth: auth_for(&grant),
                logical_id: "user-mcp".into(),
                expected_revision: None,
                desired: McpDesiredState {
                    name: "hijack".into(),
                    enabled: true,
                    transport: json!({}),
                },
            })
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::InvalidDesiredState);
    }

    #[test]
    fn team_requires_exactly_one_leader_and_unique_keys() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::TeamDefinition]);
        let auth = auth_for(&grant);

        let no_leader = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth: auth.clone(),
                    logical_id: "team-x".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "X".into(),
                        members: vec![TeamMemberDesired {
                            member_key: "a".into(),
                            role: TeamMemberRole::Teammate,
                            display_name: "A".into(),
                            assistant_logical_id: "asst".into(),
                            assistant_revision: None,
                        }],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap_err();
        assert_eq!(no_leader.code, ProvisionErrorCode::InvalidLeader);

        let dup_key = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth,
                    logical_id: "team-y".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "Y".into(),
                        members: vec![
                            TeamMemberDesired {
                                member_key: "same".into(),
                                role: TeamMemberRole::Leader,
                                display_name: "L".into(),
                                assistant_logical_id: "asst".into(),
                                assistant_revision: None,
                            },
                            TeamMemberDesired {
                                member_key: "same".into(),
                                role: TeamMemberRole::Teammate,
                                display_name: "T".into(),
                                assistant_logical_id: "asst".into(),
                                assistant_revision: None,
                            },
                        ],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap_err();
        assert_eq!(dup_key.code, ProvisionErrorCode::InvalidMemberKey);
    }

    #[test]
    fn runtime_busy_fails_closed() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(
            &mut engine,
            vec![ProvisionScope::AssistantManagement, ProvisionScope::TeamDefinition],
        );
        let auth = auth_for(&grant);
        engine
            .reconcile_assistant(AssistantReconcileRequest {
                auth: auth.clone(),
                logical_id: "asst-1".into(),
                expected_revision: None,
                desired: AssistantDesiredState {
                    name: "A".into(),
                    enabled: true,
                    rule: None,
                    model: Some("m".into()),
                    permission: None,
                    thought_level: None,
                    skills: vec![],
                    mcps: vec![],
                    placement: None,
                },
            })
            .unwrap();
        engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth: auth.clone(),
                    logical_id: "team-1".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "T".into(),
                        members: vec![TeamMemberDesired {
                            member_key: "lead".into(),
                            role: TeamMemberRole::Leader,
                            display_name: "L".into(),
                            assistant_logical_id: "asst-1".into(),
                            assistant_revision: Some(1),
                        }],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap();
        engine.mark_team_runtime_busy("team-1", TeamRuntimeState::Active);

        let err = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth: auth.clone(),
                    logical_id: "team-1".into(),
                    expected_revision: Some(1),
                    desired: TeamDefinitionDesiredState {
                        name: "T2".into(),
                        members: vec![TeamMemberDesired {
                            member_key: "lead".into(),
                            role: TeamMemberRole::Leader,
                            display_name: "L".into(),
                            assistant_logical_id: "asst-1".into(),
                            assistant_revision: Some(1),
                        }],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                false,
            )
            .unwrap_err();
        assert_eq!(err.code, ProvisionErrorCode::RuntimeBusy);
        assert!(err.zero_mutation);
    }

    #[test]
    fn team_delete_reports_disposition() {
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(
            &mut engine,
            vec![ProvisionScope::AssistantManagement, ProvisionScope::TeamDefinition],
        );
        let auth = auth_for(&grant);
        engine
            .reconcile_assistant(AssistantReconcileRequest {
                auth: auth.clone(),
                logical_id: "asst-1".into(),
                expected_revision: None,
                desired: AssistantDesiredState {
                    name: "A".into(),
                    enabled: true,
                    rule: None,
                    model: None,
                    permission: None,
                    thought_level: None,
                    skills: vec![],
                    mcps: vec![],
                    placement: None,
                },
            })
            .unwrap();
        let team = engine
            .upsert_team(
                TeamDefinitionUpsertRequest {
                    auth: auth.clone(),
                    logical_id: "team-1".into(),
                    expected_revision: None,
                    desired: TeamDefinitionDesiredState {
                        name: "T".into(),
                        members: vec![TeamMemberDesired {
                            member_key: "lead".into(),
                            role: TeamMemberRole::Leader,
                            display_name: "L".into(),
                            assistant_logical_id: "asst-1".into(),
                            assistant_revision: Some(1),
                        }],
                        workspace_policy: TeamWorkspacePolicy::VendorAutoShared,
                    },
                },
                true,
            )
            .unwrap();
        let disposition = engine
            .delete_team(TeamDefinitionDeleteRequest {
                auth,
                logical_id: "team-1".into(),
                expected_revision: Some(team.provenance.revision),
            })
            .unwrap();
        assert!(disposition.team_absent);
        assert!(!disposition.conversations.is_empty());
        assert!(
            disposition
                .conversations
                .iter()
                .all(|d| d.outcome == DispositionOutcome::Deleted)
        );
    }

    #[test]
    fn durable_store_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = ProvisionEngine::new().with_clock_ms(1_000_000);
        let grant = authorize_scopes(&mut engine, vec![ProvisionScope::AssistantManagement]);
        let auth = auth_for(&grant);
        engine
            .reconcile_assistant(AssistantReconcileRequest {
                auth: auth.clone(),
                logical_id: "asst-persist".into(),
                expected_revision: None,
                desired: AssistantDesiredState {
                    name: "Persist".into(),
                    enabled: false,
                    rule: Some("r".into()),
                    model: Some("m".into()),
                    permission: Some("default".into()),
                    thought_level: Some("low".into()),
                    skills: vec![],
                    mcps: vec![],
                    placement: None,
                },
            })
            .unwrap();
        engine.save_to_data_dir(dir.path()).unwrap();
        let reloaded = ProvisionEngine::load_from_data_dir(dir.path());
        let got = reloaded
            .get_assistant(AssistantGetRequest {
                auth,
                logical_id: "asst-persist".into(),
            })
            .unwrap();
        assert_eq!(got.name, "Persist");
        assert_eq!(got.provenance.revision, 1);
    }
}
