//! Agent Center service — evolves Assistant with visibility / publish / run plan.
//!
//! Reuses [`AssistantService`] for identity/rules/defaults; stores Agent Center
//! fields in side tables so existing `/api/assistants` CRUD stays intact.

use std::sync::Arc;

use aionui_api_types::{
    AgentCenterDetailResponse, AgentCenterListItem, AgentCenterMeta, AgentCenterMetaPatch, AgentCenterRevisionResponse,
    AgentCenterRunPlanResponse, AgentMcpPolicy, AgentPublishStatus, AgentSkillRef, AgentVisibility,
    AssistantConversationOverridesRequest, AssistantDefaultListRequest, AssistantDefaultsRequest,
    CreateAgentCenterRequest, CreateConversationRequestWire, PublishAgentCenterRequest, SkillVersionPolicy,
    UpdateAgentCenterRequest, UpdateAssistantRequest,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateAssistantDefinitionRevisionParams, IAssistantAgentCenterRepository, IAssistantDefinitionRepository,
    IAssistantDefinitionRevisionRepository, UpsertAssistantAgentCenterParams,
};
use serde_json::json;

use crate::error::AssistantError;
use crate::service::AssistantService;

pub struct AgentCenterService {
    assistants: Arc<AssistantService>,
    definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    center_repo: Arc<dyn IAssistantAgentCenterRepository>,
    revision_repo: Arc<dyn IAssistantDefinitionRevisionRepository>,
}

impl AgentCenterService {
    pub fn new(
        assistants: Arc<AssistantService>,
        definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        center_repo: Arc<dyn IAssistantAgentCenterRepository>,
        revision_repo: Arc<dyn IAssistantDefinitionRevisionRepository>,
    ) -> Self {
        Self {
            assistants,
            definition_repo,
            center_repo,
            revision_repo,
        }
    }

    pub async fn list_for_user(
        &self,
        user_id: &str,
        scope: &str,
        team_id: Option<&str>,
    ) -> Result<Vec<AgentCenterListItem>, AssistantError> {
        let assistants = self.assistants.list_for_user(user_id).await?;
        let mut out = Vec::new();
        for assistant in assistants {
            // Skip builtins for Agent Center list (center is for productized agents).
            if matches!(assistant.source, aionui_api_types::AssistantSource::Builtin) {
                continue;
            }
            let Some(definition) = self
                .definition_repo
                .get_by_assistant_id_for_user(user_id, &assistant.id)
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?
            else {
                continue;
            };
            let meta = self.load_or_default_meta(&definition.id).await?;
            let include = match scope {
                "team" => {
                    meta.visibility == AgentVisibility::Team
                        && team_id.is_some_and(|tid| meta.team_id.as_deref() == Some(tid))
                }
                "enterprise" => meta.visibility == AgentVisibility::Enterprise,
                _ => {
                    // mine: private owned by user, or any draft/published the user owns (non-team listing)
                    meta.visibility == AgentVisibility::Private || definition.owner_type == "user"
                }
            };
            if include {
                out.push(AgentCenterListItem { assistant, meta });
            }
        }
        Ok(out)
    }

    pub async fn get_detail_for_user(
        &self,
        user_id: &str,
        id: &str,
        locale: Option<&str>,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let assistant = self.assistants.get_detail_for_user(user_id, id, locale).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let meta = self.load_or_default_meta(&definition.id).await?;
        Ok(AgentCenterDetailResponse { assistant, meta })
    }

    pub async fn create_for_user(
        &self,
        user_id: &str,
        req: CreateAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let created = self.assistants.create_for_user(user_id, req.assistant).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, &created.id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::Internal("created assistant missing definition".into()))?;

        let meta = self
            .upsert_meta(
                &definition.id,
                AgentCenterMeta::default(),
                &req.meta,
                /*keep_status*/ true,
            )
            .await?;

        // Apply MCP allowlist into assistant defaults when provided.
        if let Some(mcp_ids) = req.meta.mcp_ids.as_ref() {
            self.apply_mcp_defaults(user_id, &created.id, meta.mcp_policy, mcp_ids.clone())
                .await?;
        } else if matches!(meta.mcp_policy, AgentMcpPolicy::Allowlist) {
            // Empty allowlist = mount no MCP
            self.apply_mcp_defaults(user_id, &created.id, AgentMcpPolicy::Allowlist, Vec::new())
                .await?;
        }

        // Map skill_refs → default skill ids when provided.
        if let Some(refs) = req.meta.skill_refs.as_ref() {
            self.apply_skill_defaults(user_id, &created.id, refs).await?;
        }

        self.get_detail_for_user(user_id, &created.id, None).await
    }

    pub async fn update_for_user(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let _ = self.assistants.update_for_user(user_id, id, req.assistant).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let current = self.load_or_default_meta(&definition.id).await?;
        if current.status == AgentPublishStatus::Archived {
            return Err(AssistantError::Conflict("archived agents cannot be edited".into()));
        }
        let meta = self.upsert_meta(&definition.id, current, &req.meta, true).await?;
        if let Some(mcp_ids) = req.meta.mcp_ids.as_ref() {
            self.apply_mcp_defaults(user_id, id, meta.mcp_policy, mcp_ids.clone())
                .await?;
        }
        if let Some(refs) = req.meta.skill_refs.as_ref() {
            self.apply_skill_defaults(user_id, id, refs).await?;
        }
        self.get_detail_for_user(user_id, id, None).await
    }

    pub async fn publish_for_user(
        &self,
        user_id: &str,
        id: &str,
        req: PublishAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let detail = self.get_detail_for_user(user_id, id, None).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;

        let mut meta = detail.meta.clone();
        if req.pin_skills_on_publish {
            for skill in &mut meta.skill_refs {
                if skill.version_policy == SkillVersionPolicy::Latest {
                    // Publish-time nail: keep key; mark as pin without inventing a version string
                    // when registry resolution is unavailable in this crate.
                    skill.version_policy = SkillVersionPolicy::Pin;
                }
            }
        }

        let next_revision = meta.version + 1;
        let revision_id = generate_prefixed_id("arev");
        let snapshot = json!({
            "assistant_id": id,
            "assistant": detail.assistant,
            "meta": meta,
            "published_at_ms": now_ms(),
            "revision": next_revision,
        });
        let snapshot_json = serde_json::to_string(&snapshot)
            .map_err(|e| AssistantError::Internal(format!("snapshot serialize: {e}")))?;

        self.revision_repo
            .create(&CreateAssistantDefinitionRevisionParams {
                id: &revision_id,
                assistant_definition_id: &definition.id,
                revision: next_revision,
                snapshot_json: &snapshot_json,
                changelog: req.changelog.as_deref(),
                created_by: Some(user_id),
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let patch = AgentCenterMetaPatch {
            skill_refs: Some(meta.skill_refs.clone()),
            ..AgentCenterMetaPatch::default()
        };
        let mut base = meta;
        base.status = AgentPublishStatus::Published;
        base.version = next_revision;
        base.published_revision_id = Some(revision_id);
        let _ = self.upsert_meta_full(&definition.id, &mut base, &patch).await?;

        self.get_detail_for_user(user_id, id, None).await
    }

    pub async fn list_versions_for_user(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Vec<AgentCenterRevisionResponse>, AssistantError> {
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let rows = self
            .revision_repo
            .list(&definition.id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|row| AgentCenterRevisionResponse {
                id: row.id,
                revision: row.revision,
                changelog: row.changelog,
                created_by: row.created_by,
                created_at: row.created_at,
                snapshot: None,
            })
            .collect())
    }

    /// Build a `POST /api/conversations` payload from the published (or current draft) snapshot.
    pub async fn run_plan_for_user(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<AgentCenterRunPlanResponse, AssistantError> {
        let detail = self.get_detail_for_user(user_id, id, None).await?;
        if detail.meta.status == AgentPublishStatus::Archived {
            return Err(AssistantError::Conflict("archived agents cannot be run".into()));
        }

        let skill_ids: Vec<String> = if !detail.meta.skill_refs.is_empty() {
            detail.meta.skill_refs.iter().map(|s| s.skill_key.clone()).collect()
        } else {
            detail.assistant.defaults.skills.value.clone()
        };

        let mcp_ids = match detail.meta.mcp_policy {
            AgentMcpPolicy::Allowlist => Some(detail.assistant.defaults.mcps.value.clone()),
            AgentMcpPolicy::InheritUserEnabled => None, // conversation path inherits when overrides omit mcp_ids
        };

        let overrides = AssistantConversationOverridesRequest {
            model: detail.assistant.defaults.model.value.clone(),
            permission: detail.assistant.defaults.permission.value.clone(),
            thought_level: detail.assistant.defaults.thought_level.value.clone(),
            skill_ids: Some(skill_ids),
            disabled_builtin_skill_ids: Some(detail.assistant.capabilities.default_disabled_builtin_skill_ids.clone()),
            mcp_ids,
        };

        Ok(AgentCenterRunPlanResponse {
            assistant_id: id.to_owned(),
            revision_id: detail.meta.published_revision_id.clone(),
            revision: detail.meta.version,
            create_conversation: CreateConversationRequestWire::for_assistant(id, Some(overrides)),
        })
    }

    async fn load_or_default_meta(&self, definition_id: &str) -> Result<AgentCenterMeta, AssistantError> {
        match self
            .center_repo
            .get(definition_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
        {
            Some(row) => Ok(row_to_meta(&row)?),
            None => Ok(AgentCenterMeta::default()),
        }
    }

    async fn upsert_meta(
        &self,
        definition_id: &str,
        mut base: AgentCenterMeta,
        patch: &AgentCenterMetaPatch,
        _keep_status: bool,
    ) -> Result<AgentCenterMeta, AssistantError> {
        self.upsert_meta_full(definition_id, &mut base, patch).await
    }

    async fn upsert_meta_full(
        &self,
        definition_id: &str,
        base: &mut AgentCenterMeta,
        patch: &AgentCenterMetaPatch,
    ) -> Result<AgentCenterMeta, AssistantError> {
        if let Some(v) = patch.visibility {
            base.visibility = v;
        }
        if let Some(ref team) = patch.team_id {
            base.team_id = team.clone();
        }
        if let Some(ref ent) = patch.enterprise_id {
            base.enterprise_id = ent.clone();
        }
        if let Some(ref scopes) = patch.knowledge_scopes {
            base.knowledge_scopes = scopes.clone();
        }
        if let Some(ref refs) = patch.skill_refs {
            base.skill_refs = refs.clone();
        }
        if let Some(p) = patch.mcp_policy {
            base.mcp_policy = p;
        }
        if let Some(ref roles) = patch.role_bindings {
            base.role_bindings = roles.clone();
        }

        let knowledge_scopes =
            serde_json::to_string(&base.knowledge_scopes).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let skill_refs =
            serde_json::to_string(&base.skill_refs).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let role_bindings =
            serde_json::to_string(&base.role_bindings).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let visibility = visibility_str(base.visibility);
        let status = status_str(base.status);
        let mcp_policy = mcp_policy_str(base.mcp_policy);

        let row = self
            .center_repo
            .upsert(&UpsertAssistantAgentCenterParams {
                assistant_definition_id: definition_id,
                visibility,
                team_id: base.team_id.as_deref(),
                enterprise_id: base.enterprise_id.as_deref(),
                status,
                version: base.version,
                published_revision_id: base.published_revision_id.as_deref(),
                knowledge_scopes: &knowledge_scopes,
                skill_refs: &skill_refs,
                mcp_policy,
                role_bindings: &role_bindings,
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        row_to_meta(&row)
    }

    async fn apply_mcp_defaults(
        &self,
        user_id: &str,
        assistant_id: &str,
        policy: AgentMcpPolicy,
        mcp_ids: Vec<String>,
    ) -> Result<(), AssistantError> {
        let mode = match policy {
            AgentMcpPolicy::Allowlist => "fixed",
            AgentMcpPolicy::InheritUserEnabled => "auto",
        };
        let update = UpdateAssistantRequest {
            defaults: Some(AssistantDefaultsRequest {
                mcps: Some(AssistantDefaultListRequest {
                    mode: mode.to_owned(),
                    value: mcp_ids,
                }),
                ..AssistantDefaultsRequest::default()
            }),
            ..UpdateAssistantRequest::default()
        };
        let _ = self.assistants.update_for_user(user_id, assistant_id, update).await?;
        Ok(())
    }

    async fn apply_skill_defaults(
        &self,
        user_id: &str,
        assistant_id: &str,
        refs: &[AgentSkillRef],
    ) -> Result<(), AssistantError> {
        let ids: Vec<String> = refs.iter().map(|r| r.skill_key.clone()).collect();
        let update = UpdateAssistantRequest {
            defaults: Some(AssistantDefaultsRequest {
                skills: Some(AssistantDefaultListRequest {
                    mode: "fixed".to_owned(),
                    value: ids.clone(),
                }),
                ..AssistantDefaultsRequest::default()
            }),
            enabled_skills: Some(ids),
            ..UpdateAssistantRequest::default()
        };
        let _ = self.assistants.update_for_user(user_id, assistant_id, update).await?;
        Ok(())
    }
}

fn row_to_meta(row: &aionui_db::AssistantAgentCenterRow) -> Result<AgentCenterMeta, AssistantError> {
    Ok(AgentCenterMeta {
        visibility: parse_visibility(&row.visibility)?,
        team_id: row.team_id.clone(),
        enterprise_id: row.enterprise_id.clone(),
        status: parse_status(&row.status)?,
        version: row.version,
        published_revision_id: row.published_revision_id.clone(),
        knowledge_scopes: serde_json::from_str(&row.knowledge_scopes).unwrap_or_default(),
        skill_refs: serde_json::from_str(&row.skill_refs).unwrap_or_default(),
        mcp_policy: parse_mcp_policy(&row.mcp_policy)?,
        role_bindings: serde_json::from_str(&row.role_bindings).unwrap_or_default(),
    })
}

fn parse_visibility(s: &str) -> Result<AgentVisibility, AssistantError> {
    match s {
        "private" => Ok(AgentVisibility::Private),
        "team" => Ok(AgentVisibility::Team),
        "enterprise" => Ok(AgentVisibility::Enterprise),
        other => Err(AssistantError::Internal(format!("bad visibility: {other}"))),
    }
}

fn parse_status(s: &str) -> Result<AgentPublishStatus, AssistantError> {
    match s {
        "draft" => Ok(AgentPublishStatus::Draft),
        "published" => Ok(AgentPublishStatus::Published),
        "archived" => Ok(AgentPublishStatus::Archived),
        other => Err(AssistantError::Internal(format!("bad status: {other}"))),
    }
}

fn parse_mcp_policy(s: &str) -> Result<AgentMcpPolicy, AssistantError> {
    match s {
        "allowlist" => Ok(AgentMcpPolicy::Allowlist),
        "inherit_user_enabled" => Ok(AgentMcpPolicy::InheritUserEnabled),
        other => Err(AssistantError::Internal(format!("bad mcp_policy: {other}"))),
    }
}

fn visibility_str(v: AgentVisibility) -> &'static str {
    match v {
        AgentVisibility::Private => "private",
        AgentVisibility::Team => "team",
        AgentVisibility::Enterprise => "enterprise",
    }
}

fn status_str(s: AgentPublishStatus) -> &'static str {
    match s {
        AgentPublishStatus::Draft => "draft",
        AgentPublishStatus::Published => "published",
        AgentPublishStatus::Archived => "archived",
    }
}

fn mcp_policy_str(p: AgentMcpPolicy) -> &'static str {
    match p {
        AgentMcpPolicy::Allowlist => "allowlist",
        AgentMcpPolicy::InheritUserEnabled => "inherit_user_enabled",
    }
}
