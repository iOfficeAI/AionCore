//! Assistant service — unified built-in + user assistant CRUD, state
//! overlays, import, and source-dispatched rule/skill read/write helpers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    AgentManagementRow, AgentManagementStatus, AgentSource, AssistantAgentResponse, AssistantCapabilitiesResponse,
    AssistantDefaultListRequest, AssistantDefaultListResponse, AssistantDefaultScalarRequest,
    AssistantDefaultScalarResponse, AssistantDefaultsRequest, AssistantDefaultsResponse, AssistantDetailResponse,
    AssistantEngineResponse, AssistantPreferencesResponse, AssistantProfileResponse, AssistantPromptsResponse,
    AssistantResponse, AssistantRulesResponse, AssistantSource, AssistantStateResponse, CreateAssistantRequest,
    ImportAssistantsRequest, ImportAssistantsResult, ImportError, SetAssistantStateRequest, UpdateAssistantRequest,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    AssistantDefinitionRow, AssistantOverlayRow, AssistantRow, CreateAssistantParams, IAssistantDefinitionRepository,
    IAssistantOverlayRepository, IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository,
    IProviderRepository, SqlitePool, UpdateAssistantParams, UpsertAssistantDefinitionParams,
    UpsertAssistantOverlayParams, UpsertAssistantPreferenceParams, rebuild_legacy_assistant_mirror,
    resolve_agent_binding,
};
use aionui_extension::{AssistantClassifier, AssistantRuleDispatcher, ExtensionError};
use serde_json;
use tracing::{debug, warn};

use crate::agent_catalog::AssistantAgentCatalogPort;
#[cfg(test)]
use crate::builtin::BuiltinAssistant;
use crate::builtin::{AvatarAsset, BuiltinAssistantRegistry};
use crate::error::AssistantError;

/// Aggregated business logic for `/api/assistants/*` and rule/skill dispatch.
pub struct AssistantService {
    pool: SqlitePool,
    definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    state_repo: Arc<dyn IAssistantOverlayRepository>,
    preference_repo: Arc<dyn IAssistantPreferenceRepository>,
    repo: Arc<dyn IAssistantRepository>,
    override_repo: Arc<dyn IAssistantOverrideRepository>,
    /// Used to infer a sane `agent_id` default when the caller did not supply
    /// one. The historical default of `"gemini"` 400'd within
    /// 1 ms on machines without the Gemini CLI (ELECTRON-1J1 / 1KV); we now
    /// pick an agent that actually matches the configured provider list.
    provider_repo: Arc<dyn IProviderRepository>,
    builtin: Arc<BuiltinAssistantRegistry>,
    agent_catalog: Option<Arc<dyn AssistantAgentCatalogPort>>,
    /// Root directory holding user-authored rule/skill md files and avatars.
    /// Defaults to `~/.aionui/` but can be overridden for tests.
    user_data_dir: PathBuf,
}

pub struct AssistantServiceDeps {
    pub definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    pub state_repo: Arc<dyn IAssistantOverlayRepository>,
    pub preference_repo: Arc<dyn IAssistantPreferenceRepository>,
    pub repo: Arc<dyn IAssistantRepository>,
    pub override_repo: Arc<dyn IAssistantOverrideRepository>,
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub builtin: Arc<BuiltinAssistantRegistry>,
    pub agent_catalog: Option<Arc<dyn AssistantAgentCatalogPort>>,
}

impl AssistantService {
    /// Construct an `AssistantService` pinned to the runtime data directory.
    ///
    /// `user_data_dir` is the on-disk root for user-authored rule and skill
    /// `.md` files plus avatar uploads (`<user_data_dir>/assistant-rules/`,
    /// `<user_data_dir>/assistant-skills/`, `<user_data_dir>/assistant-avatars/`).
    /// Production code passes the same `services.data_dir` that the SQLite
    /// database lives under, so dev / packaged / multi-instance launches
    /// keep their rule files alongside the matching db. Tests pin a temp
    /// directory.
    ///
    /// There is no implicit `~/.aionui` fallback on purpose: an earlier
    /// version had one, and dev builds silently wrote rule files to the
    /// release directory while the db lived under `~/.aionui-dev/`,
    /// resulting in `read_rule` returning empty in dev mode. Forcing the
    /// caller to pass a path makes the wiring explicit.
    pub fn new(pool: SqlitePool, deps: AssistantServiceDeps, user_data_dir: PathBuf) -> Self {
        let AssistantServiceDeps {
            definition_repo,
            state_repo,
            preference_repo,
            repo,
            override_repo,
            provider_repo,
            builtin,
            agent_catalog,
        } = deps;
        Self {
            pool,
            definition_repo,
            state_repo,
            preference_repo,
            repo,
            override_repo,
            provider_repo,
            builtin,
            agent_catalog,
            user_data_dir,
        }
    }

    /// Bootstrap unified assistant storage from builtin assets and the
    /// legacy mirror tables.
    pub async fn bootstrap_assistant_storage(&self) -> Result<(), AssistantError> {
        self.materialize_builtin_definitions().await?;
        self.soft_delete_removed_builtin_definitions().await?;
        self.sync_legacy_user_assistants_to_new_tables().await?;
        self.sync_legacy_overrides_to_new_states().await?;
        self.reconcile_generated_assistants().await?;
        self.rebuild_legacy_mirror_from_new_tables().await?;
        Ok(())
    }

    /// Materialize builtin assistants into `assistant_definitions`.
    pub async fn materialize_builtin_definitions(&self) -> Result<(), AssistantError> {
        for builtin in self.builtin.all() {
            let recommended_prompts = serde_json::to_string(&builtin.prompts)
                .map_err(|e| AssistantError::Internal(format!("encode builtin prompts: {e}")))?;
            let recommended_prompts_i18n = serde_json::to_string(&builtin.prompts_i18n)
                .map_err(|e| AssistantError::Internal(format!("encode builtin prompts i18n: {e}")))?;
            let name_i18n = serde_json::to_string(&builtin.name_i18n)
                .map_err(|e| AssistantError::Internal(format!("encode builtin name_i18n: {e}")))?;
            let description_i18n = serde_json::to_string(&builtin.description_i18n)
                .map_err(|e| AssistantError::Internal(format!("encode builtin description_i18n: {e}")))?;
            let default_skill_ids = serde_json::to_string(&builtin.enabled_skills)
                .map_err(|e| AssistantError::Internal(format!("encode builtin skills: {e}")))?;
            let custom_skill_names = serde_json::to_string(&builtin.custom_skill_names)
                .map_err(|e| AssistantError::Internal(format!("encode builtin custom skills: {e}")))?;
            let default_disabled_builtin_skill_ids = serde_json::to_string(&builtin.disabled_builtin_skills)
                .map_err(|e| AssistantError::Internal(format!("encode builtin disabled skills: {e}")))?;
            let (avatar_type, avatar_value) = serialize_avatar("builtin", builtin.avatar.as_deref());
            let (definition_id, assistant_id) = self
                .resolve_definition_identity("builtin", Some(&builtin.id), &builtin.id)
                .await?;
            let agent_id = self.resolve_agent_id_for_agent_ref(&builtin.agent_ref).await?;

            self.definition_repo
                .upsert(&UpsertAssistantDefinitionParams {
                    id: &definition_id,
                    assistant_id: &assistant_id,
                    source: "builtin",
                    owner_type: "system",
                    source_ref: Some(&builtin.id),
                    source_version: None,
                    source_hash: None,
                    name: &builtin.name,
                    name_i18n: &name_i18n,
                    description: builtin.description.as_deref(),
                    description_i18n: &description_i18n,
                    avatar_type: &avatar_type,
                    avatar_value: avatar_value.as_deref(),
                    agent_id: &agent_id,
                    rule_resource_type: if builtin.rule_file.is_some() {
                        "builtin_asset"
                    } else {
                        "none"
                    },
                    rule_resource_ref: builtin.rule_file.as_ref().map(|_| builtin.id.as_str()),
                    rule_inline_content: None,
                    recommended_prompts: &recommended_prompts,
                    recommended_prompts_i18n: &recommended_prompts_i18n,
                    default_model_mode: "auto",
                    default_model_value: None,
                    default_permission_mode: "auto",
                    default_permission_value: None,
                    default_skills_mode: "fixed",
                    default_skill_ids: &default_skill_ids,
                    custom_skill_names: &custom_skill_names,
                    default_disabled_builtin_skill_ids: &default_disabled_builtin_skill_ids,
                    default_mcps_mode: "auto",
                    default_mcp_ids: "[]",
                })
                .await
                .map_err(|e| AssistantError::Internal(format!("upsert builtin definition: {e}")))?;
        }

        Ok(())
    }

    async fn soft_delete_removed_builtin_definitions(&self) -> Result<(), AssistantError> {
        let active_builtin_ids: HashSet<&str> = self.builtin.all().map(|builtin| builtin.id.as_str()).collect();

        for definition in self
            .definition_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant definitions: {e}")))?
        {
            if definition.source != "builtin" {
                continue;
            }

            let Some(source_ref) = definition.source_ref.as_deref() else {
                self.definition_repo
                    .soft_delete(&definition.id, now_ms())
                    .await
                    .map_err(|e| AssistantError::Internal(format!("soft-delete builtin definition: {e}")))?;
                continue;
            };

            if active_builtin_ids.contains(source_ref) {
                continue;
            }

            self.definition_repo
                .soft_delete(&definition.id, now_ms())
                .await
                .map_err(|e| AssistantError::Internal(format!("soft-delete builtin definition: {e}")))?;
        }

        Ok(())
    }

    async fn sync_legacy_user_assistants_to_new_tables(&self) -> Result<(), AssistantError> {
        for row in self.repo.list().await? {
            if self.builtin.has(&row.id) {
                continue;
            }
            self.upsert_definition_from_legacy_user_row(&row, None).await?;
        }
        Ok(())
    }

    async fn sync_legacy_overrides_to_new_states(&self) -> Result<(), AssistantError> {
        for override_row in self.override_repo.get_all().await? {
            let Some(definition) = self
                .definition_repo
                .get_by_assistant_id(&override_row.assistant_id)
                .await?
            else {
                warn!(
                    assistant_id = %override_row.assistant_id,
                    "skip syncing assistant override without unified definition"
                );
                continue;
            };

            self.state_repo
                .upsert(&UpsertAssistantOverlayParams {
                    assistant_definition_id: &definition.id,
                    enabled: override_row.enabled,
                    sort_order: override_row.sort_order,
                    agent_id_override: None,
                    last_used_at: override_row.last_used_at,
                })
                .await
                .map_err(|e| AssistantError::Internal(format!("upsert assistant overlay: {e}")))?;
        }

        Ok(())
    }

    async fn reconcile_generated_assistants(&self) -> Result<Vec<AgentManagementRow>, AssistantError> {
        let Some(agent_catalog) = &self.agent_catalog else {
            return Ok(Vec::new());
        };

        let rows = agent_catalog.list_management_agents().await?;
        let definitions = self.definition_repo.list().await.map_err(|e| {
            AssistantError::Internal(format!("list assistant definitions for generated reconcile: {e}"))
        })?;
        let generated_source_refs: HashSet<String> = definitions
            .iter()
            .filter(|definition| definition.source == "generated")
            .filter_map(|definition| definition.source_ref.clone())
            .collect();
        let has_existing_generated = !generated_source_refs.is_empty();
        let existing_min_sort_order = self
            .state_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant overlays for generated reconcile: {e}")))?
            .into_iter()
            .map(|state| state.sort_order)
            .min()
            .unwrap_or_default()
            .min(0);
        let generated_rows: Vec<&AgentManagementRow> = rows
            .iter()
            .filter(|row| {
                row.enabled
                    && row.agent_type.supports_new_conversation()
                    && matches!(row.status, AgentManagementStatus::Online)
            })
            .collect();
        let missing_generated_count = generated_rows
            .iter()
            .filter(|row| !generated_source_refs.contains(&row.id))
            .count();

        let mut missing_index = 0usize;
        for row in generated_rows {
            let existing_definition = definitions
                .iter()
                .find(|definition| {
                    definition.source == "generated" && definition.source_ref.as_deref() == Some(row.id.as_str())
                })
                .cloned();
            let is_missing = existing_definition.is_none();
            let assistant_id = format!("bare:{}", row.id);
            let (definition_id, assistant_id) = self
                .resolve_definition_identity("generated", Some(&row.id), &assistant_id)
                .await?;
            let avatar_value = row.icon.as_deref().filter(|value| !value.trim().is_empty());
            let (definition, should_upsert) = if let Some(mut definition) = existing_definition {
                let avatar_type = if avatar_value.is_some() { "emoji" } else { "none" };
                let should_upgrade_skill_defaults = definition.default_skills_mode == "auto"
                    && decode_str_list(Some(definition.default_skill_ids.as_str()))?.is_empty()
                    && decode_str_list(Some(definition.default_disabled_builtin_skill_ids.as_str()))?.is_empty();
                let identity_changed = definition.name != row.name
                    || definition.avatar_type != avatar_type
                    || definition.avatar_value.as_deref() != avatar_value
                    || definition.agent_id != row.id
                    || definition.source_ref.as_deref() != Some(row.id.as_str());

                definition.name = row.name.clone();
                definition.avatar_type = avatar_type.to_string();
                definition.avatar_value = avatar_value.map(ToOwned::to_owned);
                definition.agent_id = row.id.clone();
                definition.source_ref = Some(row.id.clone());
                if should_upgrade_skill_defaults {
                    definition.default_skills_mode = "fixed".into();
                }
                (definition, identity_changed || should_upgrade_skill_defaults)
            } else {
                (
                    AssistantDefinitionRow {
                        id: definition_id.clone(),
                        assistant_id: assistant_id.clone(),
                        source: "generated".into(),
                        owner_type: "system".into(),
                        source_ref: Some(row.id.clone()),
                        source_version: None,
                        source_hash: None,
                        name: row.name.clone(),
                        name_i18n: "{}".into(),
                        description: row.description.clone(),
                        description_i18n: "{}".into(),
                        avatar_type: if avatar_value.is_some() {
                            "emoji".into()
                        } else {
                            "none".into()
                        },
                        avatar_value: avatar_value.map(ToOwned::to_owned),
                        agent_id: row.id.clone(),
                        rule_resource_type: "none".into(),
                        rule_resource_ref: None,
                        rule_inline_content: None,
                        recommended_prompts: "[]".into(),
                        recommended_prompts_i18n: "{}".into(),
                        default_model_mode: "auto".into(),
                        default_model_value: None,
                        default_permission_mode: "auto".into(),
                        default_permission_value: None,
                        default_skills_mode: "fixed".into(),
                        default_skill_ids: "[]".into(),
                        custom_skill_names: "[]".into(),
                        default_disabled_builtin_skill_ids: "[]".into(),
                        default_mcps_mode: "auto".into(),
                        default_mcp_ids: "[]".into(),
                        created_at: 0,
                        updated_at: 0,
                        deleted_at: None,
                    },
                    true,
                )
            };

            if should_upsert {
                self.definition_repo
                    .upsert(&upsert_params_from_definition(&definition))
                    .await
                    .map_err(|e| AssistantError::Internal(format!("upsert generated assistant definition: {e}")))?;
            }

            if !is_missing {
                continue;
            }

            if self
                .state_repo
                .get(&definition_id)
                .await
                .map_err(|e| AssistantError::Internal(format!("get generated assistant overlay: {e}")))?
                .is_none()
            {
                let current_missing_index = missing_index;
                missing_index += 1;
                let initial_generated_sort_order = if !has_existing_generated && missing_generated_count > 0 {
                    existing_min_sort_order as i64 - missing_generated_count as i64 + current_missing_index as i64
                } else {
                    row.sort_order
                };
                self.state_repo
                    .upsert(&UpsertAssistantOverlayParams {
                        assistant_definition_id: &definition_id,
                        enabled: true,
                        sort_order: initial_generated_sort_order.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                        agent_id_override: None,
                        last_used_at: None,
                    })
                    .await
                    .map_err(|e| AssistantError::Internal(format!("upsert generated assistant overlay: {e}")))?;
            }
        }

        Ok(rows)
    }

    async fn upsert_definition_from_legacy_user_row(
        &self,
        row: &AssistantRow,
        requested_agent_id: Option<&str>,
    ) -> Result<(), AssistantError> {
        // User-defined assistants do not expose locale-aware editing in the
        // current product. Keep the unified definition canonical fields as the
        // single source of truth and leave *_i18n empty for user rows.
        let name_i18n = "{}".to_string();
        let description_i18n = "{}".to_string();
        let recommended_prompts = normalize_json_array_string(row.prompts.as_deref(), "prompts")?;
        let recommended_prompts_i18n = "{}".to_string();
        let default_skill_ids = normalize_json_array_string(row.enabled_skills.as_deref(), "enabled_skills")?;
        let custom_skill_names = normalize_json_array_string(row.custom_skill_names.as_deref(), "custom_skill_names")?;
        let default_disabled_builtin_skill_ids =
            normalize_json_array_string(row.disabled_builtin_skills.as_deref(), "disabled_builtin_skills")?;
        let (avatar_type, avatar_value) = serialize_avatar("user", row.avatar.as_deref());
        let (definition_id, assistant_id) = self.resolve_definition_identity("user", Some(&row.id), &row.id).await?;
        let existing_definition = self.definition_repo.get_by_assistant_id(&assistant_id).await?;
        let agent_id = match requested_agent_id {
            Some(agent_id) => agent_id.to_string(),
            None => match existing_definition {
                Some(definition) => definition.agent_id,
                None => self.resolve_default_agent_id().await?,
            },
        };
        self.resolve_runtime_backend_for_agent_id(&agent_id).await?;

        self.definition_repo
            .upsert(&UpsertAssistantDefinitionParams {
                id: &definition_id,
                assistant_id: &assistant_id,
                source: "user",
                owner_type: "user",
                source_ref: Some(&row.id),
                source_version: None,
                source_hash: None,
                name: &row.name,
                name_i18n: &name_i18n,
                description: row.description.as_deref(),
                description_i18n: &description_i18n,
                avatar_type: &avatar_type,
                avatar_value: avatar_value.as_deref(),
                agent_id: &agent_id,
                rule_resource_type: "user_file",
                rule_resource_ref: Some(&row.id),
                rule_inline_content: None,
                recommended_prompts: &recommended_prompts,
                recommended_prompts_i18n: &recommended_prompts_i18n,
                default_model_mode: "auto",
                default_model_value: None,
                default_permission_mode: "auto",
                default_permission_value: None,
                default_skills_mode: "fixed",
                default_skill_ids: &default_skill_ids,
                custom_skill_names: &custom_skill_names,
                default_disabled_builtin_skill_ids: &default_disabled_builtin_skill_ids,
                default_mcps_mode: "auto",
                default_mcp_ids: "[]",
            })
            .await
            .map_err(|e| AssistantError::Internal(format!("upsert user definition: {e}")))?;

        Ok(())
    }

    async fn apply_detail_overrides(
        &self,
        assistant_id: &str,
        overrides: SerializedDetailOverrides,
        reset_model_and_permission: bool,
    ) -> Result<(), AssistantError> {
        if !overrides.has_changes() && !reset_model_and_permission {
            return Ok(());
        }

        let Some(existing) = self
            .definition_repo
            .get_by_assistant_id(assistant_id)
            .await
            .map_err(|e| AssistantError::Internal(format!("get assistant definition: {e}")))?
        else {
            return Ok(());
        };

        let mut patched = existing.clone();
        apply_detail_patch_to_definition(&mut patched, &overrides, reset_model_and_permission);

        let patched = self
            .definition_repo
            .upsert(&upsert_params_from_definition(&patched))
            .await
            .map_err(|e| AssistantError::Internal(format!("upsert patched assistant definition: {e}")))?;

        let state = self
            .state_repo
            .get(&patched.id)
            .await
            .map_err(|e| AssistantError::Internal(format!("get assistant overlay: {e}")))?;
        rebuild_legacy_assistant_mirror(&self.pool, &patched, state.as_ref())
            .await
            .map_err(|e| AssistantError::Internal(format!("rebuild legacy mirror: {e}")))?;

        Ok(())
    }

    /// Rebuild downgrade-compatibility mirror rows from the new assistant tables.
    pub async fn rebuild_legacy_mirror_from_new_tables(&self) -> Result<(), AssistantError> {
        let states = self
            .state_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant overlays: {e}")))?;
        let state_map: HashMap<String, aionui_db::AssistantOverlayRow> = states
            .into_iter()
            .map(|state| (state.assistant_definition_id.clone(), state))
            .collect();

        for definition in self
            .definition_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant definitions: {e}")))?
        {
            rebuild_legacy_assistant_mirror(&self.pool, &definition, state_map.get(&definition.id))
                .await
                .map_err(|e| AssistantError::Internal(format!("rebuild legacy mirror: {e}")))?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Classification
    // -----------------------------------------------------------------------

    /// Classify an assistant id into its source.
    pub async fn classify_source(&self, id: &str) -> AssistantSource {
        if self.builtin.has(id) {
            return AssistantSource::Builtin;
        }
        if let Ok(Some(definition)) = self.definition_repo.get_by_assistant_id(id).await {
            return match definition.source.as_str() {
                "builtin" => AssistantSource::Builtin,
                "generated" => AssistantSource::Generated,
                _ => AssistantSource::User,
            };
        }
        AssistantSource::User
    }

    // -----------------------------------------------------------------------
    // List / Get
    // -----------------------------------------------------------------------

    /// Unified assistant list (built-in + user) with per-assistant overlay
    /// application. Also performs opportunistic orphan cleanup on the
    /// overrides table.
    pub async fn list(&self) -> Result<Vec<AssistantResponse>, AssistantError> {
        let projections = self.reconcile_generated_assistants().await?;
        let definitions = self
            .definition_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant definitions: {e}")))?;
        let states = self
            .state_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("list assistant overlays: {e}")))?;
        let state_map: HashMap<String, AssistantOverlayRow> = states
            .into_iter()
            .map(|state| (state.assistant_definition_id.clone(), state))
            .collect();

        let mut result = Vec::new();

        for definition in &definitions {
            let projection = self
                .project_definition(definition, state_map.get(&definition.id), &projections)
                .await?;
            result.push(definition_to_response(
                definition,
                state_map.get(&definition.id),
                &projection,
            )?);
        }

        // Sort by sort_order asc, then last_used_at desc (newer first).
        result.sort_by(|a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| b.last_used_at.cmp(&a.last_used_at))
        });

        // Opportunistic orphan cleanup: any override row whose assistant_id no
        // longer appears in the merged list is stale.
        let valid_ids: Vec<&str> = result.iter().map(|a| a.id.as_str()).collect();
        if let Err(e) = self.override_repo.delete_orphans(&valid_ids).await {
            warn!("override orphan cleanup failed: {e}");
        }

        Ok(result)
    }

    pub async fn get(&self, id: &str) -> Result<AssistantResponse, AssistantError> {
        let projections = self.reconcile_generated_assistants().await?;
        if let Some(definition) = self.definition_repo.get_by_assistant_id(id).await? {
            let state = self.state_repo.get(&definition.id).await?;
            let projection = self
                .project_definition(&definition, state.as_ref(), &projections)
                .await?;
            return definition_to_response(&definition, state.as_ref(), &projection);
        }

        Err(AssistantError::NotFound(format!("assistant '{id}' not found")))
    }

    pub async fn get_detail(&self, id: &str, locale: Option<&str>) -> Result<AssistantDetailResponse, AssistantError> {
        let projections = self.reconcile_generated_assistants().await?;
        if let Some(definition) = self.definition_repo.get_by_assistant_id(id).await? {
            let state = self.state_repo.get(&definition.id).await?;
            let preference = self.preference_repo.get(&definition.id).await?;
            let rules_content = self.read_rule(id, locale).await?;
            let projection = self
                .project_definition(&definition, state.as_ref(), &projections)
                .await?;
            return definition_to_detail_response(
                &definition,
                state.as_ref(),
                preference.as_ref(),
                &rules_content,
                &projection,
            );
        }

        Err(AssistantError::NotFound(format!("assistant '{id}' not found")))
    }

    // -----------------------------------------------------------------------
    // Default-agent inference
    // -----------------------------------------------------------------------

    /// Pick a sane `agent_id` default for newly created / imported assistants
    /// when the caller did not supply one.
    ///
    /// Inference rule (ELECTRON-1J1 / 1KV):
    /// 1. If any enabled provider exists (Anthropic, OpenAI, custom,
    ///    Bedrock, Vertex, …), return `"aionrs"`. AionRS speaks both
    ///    OpenAI-compatible and Anthropic-protocol APIs over the
    ///    user-configured base URL and does not require any third-party
    ///    CLI to be installed. CLI-based agents (`claude`, `gemini`)
    ///    must be opted into explicitly via `agent_id` because
    ///    the presence of an Anthropic API key does not imply that the
    ///    Claude Code CLI is on `PATH`.
    /// 2. Otherwise (no providers configured), return a `BadRequest`
    ///    error. The previous code silently fell back to `"gemini"`,
    ///    which on machines without the Gemini CLI 400'd within 1 ms
    ///    with `Agent 'Gemini CLI' CLI not found in PATH`.
    pub async fn resolve_default_agent_id(&self) -> Result<String, AssistantError> {
        let providers = self
            .provider_repo
            .list()
            .await
            .map_err(|e| AssistantError::Internal(format!("failed to list providers: {e}")))?;

        if providers.iter().any(|p| p.enabled) {
            self.resolve_agent_id_for_agent_ref("aionrs").await
        } else {
            Err(AssistantError::BadRequest(
                "Cannot create assistant: no providers configured. Add a provider before creating an assistant, \
                 or pass an explicit `agent_id` in the request body."
                    .into(),
            ))
        }
    }

    async fn resolve_runtime_backend_for_agent_id(&self, agent_id: &str) -> Result<String, AssistantError> {
        let trimmed = agent_id.trim();
        if trimmed.is_empty() {
            return Err(AssistantError::BadRequest("agent_id is required".into()));
        }
        let Some(binding) = resolve_agent_binding(&self.pool, trimmed)
            .await
            .map_err(|e| AssistantError::Internal(format!("resolve agent binding: {e}")))?
        else {
            return Err(AssistantError::BadRequest(format!("Unknown agent_id '{trimmed}'")));
        };
        Ok(binding.runtime_backend)
    }

    async fn resolve_agent_id_for_agent_ref(&self, agent_ref: &str) -> Result<String, AssistantError> {
        let trimmed = agent_ref.trim();
        let Some(binding) = resolve_agent_binding(&self.pool, trimmed)
            .await
            .map_err(|e| AssistantError::Internal(format!("resolve agent binding: {e}")))?
        else {
            return Err(AssistantError::BadRequest(format!("Unknown agent_ref '{trimmed}'")));
        };
        Ok(binding.agent_id)
    }

    async fn project_definition(
        &self,
        definition: &AssistantDefinitionRow,
        state: Option<&AssistantOverlayRow>,
        agent_rows: &[AgentManagementRow],
    ) -> Result<AssistantRuntimeProjection, AssistantError> {
        let effective_agent_id = effective_agent_id_for_definition(definition, state);
        let runtime_backend = resolve_agent_binding(&self.pool, effective_agent_id)
            .await
            .map_err(|e| AssistantError::Internal(format!("resolve agent binding: {e}")))?
            .map(|binding| binding.runtime_backend);
        Ok(assistant_projection_for_definition(
            definition,
            state,
            agent_rows,
            runtime_backend.as_deref(),
        ))
    }

    // -----------------------------------------------------------------------
    // Create / Update / Delete
    // -----------------------------------------------------------------------

    pub async fn create(&self, req: CreateAssistantRequest) -> Result<AssistantResponse, AssistantError> {
        let name = req.name.trim().to_string();
        if name.is_empty() {
            return Err(AssistantError::BadRequest("name is required".into()));
        }

        let id = match req.id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => generate_user_id(),
        };

        // Reject id collisions with built-ins.
        if self.builtin.has(&id) {
            return Err(AssistantError::BadRequest(
                "Id conflicts with built-in assistant".into(),
            ));
        }

        let serialized = SerializedFields::from_create(&req)?;
        let detail_overrides = SerializedDetailOverrides::from_create(&req)?;
        // Resolve the default agent id from the configured provider list when
        // the caller did not supply one. Avoids the historical
        // `"gemini"` fallback that 400'd within 1 ms on machines without
        // the Gemini CLI (ELECTRON-1J1, ELECTRON-1KV).
        let resolved_agent_id = match req.agent_id.as_deref() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => self.resolve_default_agent_id().await?,
        };
        self.resolve_runtime_backend_for_agent_id(&resolved_agent_id).await?;
        let avatar = self.normalize_user_avatar_input(&id, req.avatar.as_deref())?;
        let params = CreateAssistantParams {
            id: &id,
            name: &name,
            description: req.description.as_deref(),
            avatar: avatar.as_deref(),
            enabled_skills: serialized.enabled_skills.as_deref(),
            custom_skill_names: serialized.custom_skill_names.as_deref(),
            disabled_builtin_skills: serialized.disabled_builtin_skills.as_deref(),
            prompts: serialized.prompts.as_deref(),
            models: serialized.models.as_deref(),
            name_i18n: serialized.name_i18n.as_deref(),
            description_i18n: serialized.description_i18n.as_deref(),
            prompts_i18n: serialized.prompts_i18n.as_deref(),
        };

        let row = self.repo.create(&params).await?;
        self.upsert_definition_from_legacy_user_row(&row, Some(&resolved_agent_id))
            .await?;
        self.apply_detail_overrides(&row.id, detail_overrides, false).await?;
        if let Some(definition) = self.definition_repo.get_by_assistant_id(&row.id).await? {
            self.sync_preferences_from_defaults_request(&definition, None, req.defaults.as_ref())
                .await?;
        }
        self.get(&id).await
    }

    pub async fn update(&self, id: &str, req: UpdateAssistantRequest) -> Result<AssistantResponse, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                let detail_overrides = SerializedDetailOverrides::from_update(&req)?;
                let builtin_defaults_forbidden = req
                    .defaults
                    .as_ref()
                    .is_some_and(|defaults| defaults.skills.is_some() || defaults.mcps.is_some());

                // Built-in rows are sourced from the embedded bundle and can't
                // be mutated. Users may still override `agent_id`, and
                // product-defined governance allows model/permission defaults
                // to vary per built-in assistant. Any other field on the
                // request is rejected so callers don't silently lose data.
                if req.name.is_some()
                    || req.description.is_some()
                    || req.avatar.is_some()
                    || req.enabled_skills.is_some()
                    || req.custom_skill_names.is_some()
                    || req.disabled_builtin_skills.is_some()
                    || req.prompts.is_some()
                    || req.models.is_some()
                    || req.name_i18n.is_some()
                    || req.description_i18n.is_some()
                    || req.prompts_i18n.is_some()
                    || req.recommended_prompts.is_some()
                    || req.recommended_prompts_i18n.is_some()
                    || builtin_defaults_forbidden
                {
                    return Err(AssistantError::Forbidden(
                        "Only 'agent_id', 'defaults.model', and 'defaults.permission' can be overridden on built-in assistants".into(),
                    ));
                }

                let definition = self
                    .definition_repo
                    .get_by_assistant_id(id)
                    .await?
                    .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;

                let existing = self.override_repo.get(id).await?;
                let enabled = existing.as_ref().is_none_or(|o| o.enabled);
                let sort_order = existing.as_ref().map(|o| o.sort_order).unwrap_or(0);
                let last_used_at = existing.as_ref().and_then(|o| o.last_used_at);
                let requested_agent_id = req.agent_id.as_deref().map(|agent_id| agent_id.trim().to_string());
                let current_agent_id = self
                    .state_repo
                    .get(&definition.id)
                    .await
                    .map_err(|e| AssistantError::Internal(format!("get assistant overlay: {e}")))?
                    .and_then(|row| row.agent_id_override)
                    .unwrap_or_else(|| definition.agent_id.clone());
                let reset_model_and_permission = requested_agent_id
                    .as_deref()
                    .is_some_and(|agent_id| agent_id != current_agent_id);
                if let Some(requested_agent_id) = requested_agent_id.as_deref() {
                    self.resolve_runtime_backend_for_agent_id(requested_agent_id).await?;
                    self.state_repo
                        .upsert(&UpsertAssistantOverlayParams {
                            assistant_definition_id: &definition.id,
                            enabled,
                            sort_order,
                            agent_id_override: Some(requested_agent_id),
                            last_used_at,
                        })
                        .await
                        .map_err(|e| AssistantError::Internal(format!("upsert assistant overlay: {e}")))?;
                }
                self.apply_detail_overrides(id, detail_overrides, reset_model_and_permission)
                    .await?;
                let definition = self
                    .definition_repo
                    .get_by_assistant_id(id)
                    .await?
                    .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;
                self.sync_preferences_from_defaults_request(&definition, Some(&definition), req.defaults.as_ref())
                    .await?;
                let state = self.state_repo.get(&definition.id).await?;
                rebuild_legacy_assistant_mirror(&self.pool, &definition, state.as_ref())
                    .await
                    .map_err(|e| AssistantError::Internal(format!("rebuild legacy mirror: {e}")))?;
                return self.get(id).await;
            }
            AssistantSource::Generated => {
                if req.name.is_some()
                    || req.name_i18n.is_some()
                    || req.avatar.is_some()
                    || req.agent_id.is_some()
                    || req.models.is_some()
                {
                    return Err(AssistantError::Forbidden(
                        "Generated assistant identity fields cannot be edited".into(),
                    ));
                }

                let mut req = req;
                if req.recommended_prompts.is_none() {
                    req.recommended_prompts = req.prompts.clone();
                }

                let serialized = SerializedFields::from_update(&req)?;
                let detail_overrides = SerializedDetailOverrides::from_update(&req)?;
                let current_definition = self
                    .definition_repo
                    .get_by_assistant_id(id)
                    .await?
                    .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;
                let mut patched = current_definition.clone();

                if let Some(description) = req.description {
                    patched.description = Some(description);
                }
                if let Some(value) = serialized.description_i18n {
                    patched.description_i18n = value;
                }
                if let Some(value) = serialized.prompts_i18n {
                    patched.recommended_prompts_i18n = value;
                }
                if let Some(value) = serialized.enabled_skills {
                    patched.default_skills_mode = "fixed".to_string();
                    patched.default_skill_ids = value;
                }
                if let Some(value) = serialized.custom_skill_names {
                    patched.custom_skill_names = value;
                }
                if let Some(value) = serialized.disabled_builtin_skills {
                    patched.default_disabled_builtin_skill_ids = value;
                }
                apply_detail_patch_to_definition(&mut patched, &detail_overrides, false);

                let patched = self
                    .definition_repo
                    .upsert(&upsert_params_from_definition(&patched))
                    .await
                    .map_err(|e| AssistantError::Internal(format!("upsert generated assistant definition: {e}")))?;
                self.sync_preferences_from_defaults_request(&patched, Some(&current_definition), req.defaults.as_ref())
                    .await?;
                let state = self.state_repo.get(&patched.id).await?;
                rebuild_legacy_assistant_mirror(&self.pool, &patched, state.as_ref())
                    .await
                    .map_err(|e| AssistantError::Internal(format!("rebuild legacy mirror: {e}")))?;
                return self.get(id).await;
            }
            AssistantSource::User => {}
        }

        let serialized = SerializedFields::from_update(&req)?;
        let detail_overrides = SerializedDetailOverrides::from_update(&req)?;
        let current_definition = self
            .definition_repo
            .get_by_assistant_id(id)
            .await?
            .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;
        let requested_agent_id = match req.agent_id.as_deref() {
            Some(agent_id) if !agent_id.trim().is_empty() => Some(agent_id.trim().to_string()),
            Some(_) => return Err(AssistantError::BadRequest("agent_id is required".into())),
            None => None,
        };
        if let Some(agent_id) = requested_agent_id.as_deref() {
            self.resolve_runtime_backend_for_agent_id(agent_id).await?;
        }
        let reset_model_and_permission = requested_agent_id
            .as_deref()
            .is_some_and(|agent_id| agent_id != current_definition.agent_id);
        let normalized_avatar = if req.avatar.is_some() {
            Some(self.normalize_user_avatar_input(id, req.avatar.as_deref())?)
        } else {
            None
        };
        let params = UpdateAssistantParams {
            name: req.name.as_deref(),
            description: req.description.as_ref().map(|s| Some(s.as_str())),
            avatar: normalized_avatar.as_ref().map(|value| value.as_deref()),
            enabled_skills: serialized.enabled_skills.as_ref().map(|s| Some(s.as_str())),
            custom_skill_names: serialized.custom_skill_names.as_ref().map(|s| Some(s.as_str())),
            disabled_builtin_skills: serialized.disabled_builtin_skills.as_ref().map(|s| Some(s.as_str())),
            prompts: serialized.prompts.as_ref().map(|s| Some(s.as_str())),
            models: serialized.models.as_ref().map(|s| Some(s.as_str())),
            name_i18n: serialized.name_i18n.as_ref().map(|s| Some(s.as_str())),
            description_i18n: serialized.description_i18n.as_ref().map(|s| Some(s.as_str())),
            prompts_i18n: serialized.prompts_i18n.as_ref().map(|s| Some(s.as_str())),
        };

        let row = self
            .repo
            .update(id, &params)
            .await?
            .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;
        self.upsert_definition_from_legacy_user_row(&row, requested_agent_id.as_deref())
            .await?;
        self.apply_detail_overrides(id, detail_overrides, reset_model_and_permission)
            .await?;
        if let Some(definition) = self.definition_repo.get_by_assistant_id(id).await? {
            self.sync_preferences_from_defaults_request(&definition, Some(&current_definition), req.defaults.as_ref())
                .await?;
        }
        self.get(id).await
    }

    async fn sync_preferences_from_defaults_request(
        &self,
        definition: &AssistantDefinitionRow,
        previous_definition: Option<&AssistantDefinitionRow>,
        defaults: Option<&AssistantDefaultsRequest>,
    ) -> Result<(), AssistantError> {
        let Some(defaults) = defaults else {
            return Ok(());
        };

        let existing = self
            .preference_repo
            .get(&definition.id)
            .await
            .map_err(|e| AssistantError::Internal(format!("get assistant preference: {e}")))?;

        let mut last_model_id = existing.as_ref().and_then(|row| row.last_model_id.clone());
        let mut last_permission_value = existing.as_ref().and_then(|row| row.last_permission_value.clone());
        let mut last_skill_ids = existing
            .as_ref()
            .map(|row| decode_str_list(Some(row.last_skill_ids.as_str())))
            .transpose()?
            .unwrap_or_default();
        let mut last_disabled_builtin_skill_ids = existing
            .as_ref()
            .map(|row| decode_str_list(Some(row.last_disabled_builtin_skill_ids.as_str())))
            .transpose()?
            .unwrap_or_default();
        let mut last_mcp_ids = existing
            .as_ref()
            .map(|row| decode_str_list(Some(row.last_mcp_ids.as_str())))
            .transpose()?
            .unwrap_or_default();

        if let Some(model) = defaults.model.as_ref() {
            match model.mode.as_str() {
                "fixed" => {
                    last_model_id = model.value.clone().filter(|value| !value.trim().is_empty());
                }
                "auto" => {
                    if previous_definition.is_some_and(|current| current.default_model_mode == "fixed") {
                        last_model_id = None;
                    }
                }
                other => {
                    return Err(AssistantError::BadRequest(format!(
                        "defaults.model.mode must be 'auto' or 'fixed', got '{other}'"
                    )));
                }
            }
        }

        if let Some(permission) = defaults.permission.as_ref() {
            match permission.mode.as_str() {
                "fixed" => {
                    last_permission_value = permission.value.clone().filter(|value| !value.trim().is_empty());
                }
                "auto" => {
                    if previous_definition.is_some_and(|current| current.default_permission_mode == "fixed") {
                        last_permission_value = None;
                    }
                }
                other => {
                    return Err(AssistantError::BadRequest(format!(
                        "defaults.permission.mode must be 'auto' or 'fixed', got '{other}'"
                    )));
                }
            }
        }

        if let Some(skills) = defaults.skills.as_ref() {
            match skills.mode.as_str() {
                "fixed" => {
                    last_skill_ids = skills.value.clone();
                    last_disabled_builtin_skill_ids.clear();
                }
                "auto" => {
                    if previous_definition.is_some_and(|current| current.default_skills_mode == "fixed") {
                        last_skill_ids.clear();
                        last_disabled_builtin_skill_ids.clear();
                    }
                }
                other => {
                    return Err(AssistantError::BadRequest(format!(
                        "defaults.skills.mode must be 'auto' or 'fixed', got '{other}'"
                    )));
                }
            }
        }

        if let Some(mcps) = defaults.mcps.as_ref() {
            match mcps.mode.as_str() {
                "fixed" => {
                    last_mcp_ids = mcps.value.clone();
                }
                "auto" => {
                    if previous_definition.is_some_and(|current| current.default_mcps_mode == "fixed") {
                        last_mcp_ids.clear();
                    }
                }
                other => {
                    return Err(AssistantError::BadRequest(format!(
                        "defaults.mcps.mode must be 'auto' or 'fixed', got '{other}'"
                    )));
                }
            }
        }

        if last_model_id.is_none()
            && last_permission_value.is_none()
            && last_skill_ids.is_empty()
            && last_disabled_builtin_skill_ids.is_empty()
            && last_mcp_ids.is_empty()
        {
            if existing.is_some() {
                self.preference_repo
                    .delete(&definition.id)
                    .await
                    .map_err(|e| AssistantError::Internal(format!("delete assistant preference: {e}")))?;
            }
            return Ok(());
        }

        let last_skill_ids_json = serde_json::to_string(&last_skill_ids)
            .map_err(|e| AssistantError::Internal(format!("encode assistant skills preference: {e}")))?;
        let last_disabled_builtin_skill_ids_json = serde_json::to_string(&last_disabled_builtin_skill_ids)
            .map_err(|e| AssistantError::Internal(format!("encode disabled assistant skills preference: {e}")))?;
        let last_mcp_ids_json = serde_json::to_string(&last_mcp_ids)
            .map_err(|e| AssistantError::Internal(format!("encode assistant mcp preference: {e}")))?;

        self.preference_repo
            .upsert(&UpsertAssistantPreferenceParams {
                assistant_definition_id: &definition.id,
                last_model_id: last_model_id.as_deref(),
                last_permission_value: last_permission_value.as_deref(),
                last_skill_ids: &last_skill_ids_json,
                last_disabled_builtin_skill_ids: &last_disabled_builtin_skill_ids_json,
                last_mcp_ids: &last_mcp_ids_json,
            })
            .await
            .map_err(|e| AssistantError::Internal(format!("upsert assistant preference: {e}")))?;

        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<(), AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                return Err(AssistantError::Forbidden("Cannot delete built-in assistant".into()));
            }
            AssistantSource::Generated => {
                return Err(AssistantError::Forbidden("Cannot delete generated assistant".into()));
            }
            AssistantSource::User => {}
        }

        let removed = self.repo.delete(id).await?;
        if !removed {
            return Err(AssistantError::NotFound(format!("assistant '{id}' not found")));
        }

        // Drop the override row (best-effort).
        if let Err(e) = self.override_repo.delete(id).await {
            warn!("failed to remove override for deleted assistant '{id}': {e}");
        }
        if let Some(definition) = self.definition_repo.get_by_assistant_id(id).await? {
            if let Err(e) = self.state_repo.delete(&definition.id).await {
                warn!("failed to remove assistant overlay for deleted assistant '{id}': {e}");
            }
            if let Err(e) = self.preference_repo.delete(&definition.id).await {
                warn!("failed to remove assistant preferences for deleted assistant '{id}': {e}");
            }
            if let Err(e) = self.definition_repo.soft_delete(&definition.id, now_ms()).await {
                warn!("failed to soft-delete assistant definition for deleted assistant '{id}': {e}");
            }
        }

        // Best-effort filesystem cleanup.
        self.cleanup_user_assets(id);

        Ok(())
    }

    pub async fn set_state(
        &self,
        id: &str,
        req: SetAssistantStateRequest,
    ) -> Result<AssistantResponse, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin | AssistantSource::Generated => {}
            AssistantSource::User => {
                // Confirm the user row exists (otherwise 404).
                if self.repo.get(id).await?.is_none() {
                    return Err(AssistantError::NotFound(format!("assistant '{id}' not found")));
                }
            }
        }

        // Merge with existing state/override to preserve fields not in this request.
        let definition = self
            .definition_repo
            .get_by_assistant_id(id)
            .await?
            .ok_or_else(|| AssistantError::NotFound(format!("assistant '{id}' not found")))?;
        let existing_state = self.state_repo.get(&definition.id).await?;
        let existing = self.override_repo.get(id).await?;
        let enabled = req.enabled.unwrap_or_else(|| {
            existing_state
                .as_ref()
                .map(|state| state.enabled)
                .unwrap_or_else(|| existing.as_ref().is_none_or(|o| o.enabled))
        });
        let sort_order = req
            .sort_order
            .or_else(|| existing_state.as_ref().map(|state| state.sort_order))
            .or_else(|| existing.as_ref().map(|o| o.sort_order))
            .unwrap_or(0);
        let last_used_at = req
            .last_used_at
            .or_else(|| existing_state.as_ref().and_then(|state| state.last_used_at))
            .or_else(|| existing.as_ref().and_then(|o| o.last_used_at));
        let agent_id_override = existing_state
            .as_ref()
            .and_then(|state| state.agent_id_override.clone());
        let state = self
            .state_repo
            .upsert(&UpsertAssistantOverlayParams {
                assistant_definition_id: &definition.id,
                enabled,
                sort_order,
                agent_id_override: agent_id_override.as_deref(),
                last_used_at,
            })
            .await
            .map_err(|e| AssistantError::Internal(format!("upsert assistant overlay: {e}")))?;
        rebuild_legacy_assistant_mirror(&self.pool, &definition, Some(&state))
            .await
            .map_err(|e| AssistantError::Internal(format!("rebuild legacy mirror: {e}")))?;

        self.get(id).await
    }

    // -----------------------------------------------------------------------
    // Import (insert-only, idempotent)
    // -----------------------------------------------------------------------

    /// Bulk insert-only import of legacy Electron config rows. Skip on
    /// built-in id collision or already-imported user-id collision.
    /// Never overwrites an existing user row.
    pub async fn import(&self, req: ImportAssistantsRequest) -> Result<ImportAssistantsResult, AssistantError> {
        let mut result = ImportAssistantsResult::default();

        // Resolved-once cache for the inferred default agent id. We only
        // hit the provider repo when at least one row in the batch omits
        // `agent_id` AND has cleared all the other skip conditions.
        let mut cached_default_agent_id: Option<String> = None;

        for entry in req.assistants {
            let id = entry
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(generate_user_id);

            if self.builtin.has(&id) {
                result.skipped += 1;
                continue;
            }
            match self.repo.get(&id).await {
                Ok(Some(_)) => {
                    result.skipped += 1;
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id: id.clone(),
                        error: e.to_string(),
                    });
                    continue;
                }
            }

            let name = entry.name.trim().to_string();
            if name.is_empty() {
                result.failed += 1;
                result.errors.push(ImportError {
                    id,
                    error: "name is required".into(),
                });
                continue;
            }

            let serialized = match SerializedFields::from_create(&entry) {
                Ok(s) => s,
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: e.to_string(),
                    });
                    continue;
                }
            };

            // Mirror the create() path: prefer the caller-supplied agent id;
            // otherwise infer from the configured provider list.
            let resolved_agent_id = match entry.agent_id.as_deref() {
                Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                Some(_) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: "agent_id is required".into(),
                    });
                    continue;
                }
                _ => match cached_default_agent_id.as_deref() {
                    Some(v) => v.to_string(),
                    None => match self.resolve_default_agent_id().await {
                        Ok(v) => {
                            cached_default_agent_id = Some(v.clone());
                            v
                        }
                        Err(e) => {
                            result.failed += 1;
                            result.errors.push(ImportError {
                                id,
                                error: e.to_string(),
                            });
                            continue;
                        }
                    },
                },
            };
            if let Err(e) = self.resolve_runtime_backend_for_agent_id(&resolved_agent_id).await {
                result.failed += 1;
                result.errors.push(ImportError {
                    id,
                    error: e.to_string(),
                });
                continue;
            }

            let avatar = match self.normalize_user_avatar_input(&id, entry.avatar.as_deref()) {
                Ok(value) => value,
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: e.to_string(),
                    });
                    continue;
                }
            };

            let params = CreateAssistantParams {
                id: &id,
                name: &name,
                description: entry.description.as_deref(),
                avatar: avatar.as_deref(),
                enabled_skills: serialized.enabled_skills.as_deref(),
                custom_skill_names: serialized.custom_skill_names.as_deref(),
                disabled_builtin_skills: serialized.disabled_builtin_skills.as_deref(),
                prompts: serialized.prompts.as_deref(),
                models: serialized.models.as_deref(),
                name_i18n: serialized.name_i18n.as_deref(),
                description_i18n: serialized.description_i18n.as_deref(),
                prompts_i18n: serialized.prompts_i18n.as_deref(),
            };

            match self.repo.create(&params).await {
                Ok(row) => {
                    self.upsert_definition_from_legacy_user_row(&row, Some(&resolved_agent_id))
                        .await?;
                    result.imported += 1;
                }
                Err(aionui_db::DbError::Conflict(_)) => {
                    // Someone raced us into the table — treat as skip to
                    // keep import idempotent across retries.
                    result.skipped += 1;
                }
                Err(e) => {
                    result.failed += 1;
                    result.errors.push(ImportError {
                        id,
                        error: e.to_string(),
                    });
                }
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Rule / skill dispatch helpers
    // -----------------------------------------------------------------------

    /// Read an assistant rule file, dispatching by source.
    pub async fn read_rule(&self, id: &str, locale: Option<&str>) -> Result<String, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => {
                let locale = locale.unwrap_or("");
                Ok(self
                    .builtin
                    .rule_bytes(id, locale)
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_default())
            }
            AssistantSource::Generated | AssistantSource::User => {
                let path = self.user_rule_path(id, locale);
                Ok(read_file_or_empty(&path))
            }
        }
    }

    /// Write an assistant rule file. User-authored and generated assistants
    /// keep editable configuration in the local profile; built-ins reject.
    pub async fn write_rule(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AssistantError::BadRequest(
                "Cannot write rule for built-in assistant".into(),
            )),
            AssistantSource::Generated | AssistantSource::User => {
                let path = self.user_rule_path(id, locale);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| AssistantError::Internal(format!("create dir failed: {e}")))?;
                }
                std::fs::write(&path, content).map_err(|e| AssistantError::Internal(format!("write failed: {e}")))?;
                Ok(())
            }
        }
    }

    /// Delete all locale versions of an assistant rule.
    pub async fn delete_rule(&self, id: &str) -> Result<bool, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AssistantError::BadRequest(
                "Cannot delete rule for built-in assistant".into(),
            )),
            AssistantSource::Generated | AssistantSource::User => {
                Ok(remove_assistant_md_files(&self.user_rules_dir(), id))
            }
        }
    }

    pub async fn read_skill(&self, id: &str, locale: Option<&str>) -> Result<String, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Ok(String::new()),
            AssistantSource::Generated | AssistantSource::User => {
                let path = self.user_skill_path(id, locale);
                Ok(read_file_or_empty(&path))
            }
        }
    }

    pub async fn write_skill(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AssistantError::BadRequest(
                "Cannot write skill for built-in assistant".into(),
            )),
            AssistantSource::Generated | AssistantSource::User => {
                let path = self.user_skill_path(id, locale);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| AssistantError::Internal(format!("create dir failed: {e}")))?;
                }
                std::fs::write(&path, content).map_err(|e| AssistantError::Internal(format!("write failed: {e}")))?;
                Ok(())
            }
        }
    }

    pub async fn delete_skill(&self, id: &str) -> Result<bool, AssistantError> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => Err(AssistantError::BadRequest(
                "Cannot delete skill for built-in assistant".into(),
            )),
            AssistantSource::Generated | AssistantSource::User => {
                Ok(remove_assistant_md_files(&self.user_skills_dir(), id))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Avatar helpers
    // -----------------------------------------------------------------------

    /// Resolve the avatar bytes for an assistant together with its file
    /// extension (for `Content-Type` inference).
    ///
    /// - Built-in source → read from the embedded bundle (or the disk
    ///   override when `AIONUI_BUILTIN_ASSISTANTS_PATH` is set).
    /// - User source → scan the user-writable avatars directory for a file
    ///   whose stem equals `id`.
    ///
    /// Built-ins whose manifest `avatar` field is an inline emoji (and thus
    /// has no on-disk file) also return `None`; clients fall back to the
    /// text avatar for those.
    pub async fn avatar_asset(&self, id: &str) -> Option<AvatarAsset> {
        match self.classify_source(id).await {
            AssistantSource::Builtin => self.builtin.avatar_asset(id),
            AssistantSource::Generated | AssistantSource::User => {
                let dir = self.user_avatars_dir();
                let entries = std::fs::read_dir(&dir).ok()?;
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if let Some(stem) = name.split('.').next()
                        && stem == id
                    {
                        let bytes = std::fs::read(entry.path()).ok()?;
                        let extension = std::path::Path::new(name.as_ref())
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.to_ascii_lowercase());
                        return Some(AvatarAsset { bytes, extension });
                    }
                }
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-rules")
    }

    fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-skills")
    }

    fn user_avatars_dir(&self) -> PathBuf {
        self.user_data_dir.join("assistant-avatars")
    }

    fn normalize_user_avatar_input(&self, id: &str, avatar: Option<&str>) -> Result<Option<String>, AssistantError> {
        let Some(value) = avatar.map(str::trim).filter(|value| !value.is_empty()) else {
            remove_assistant_avatar_files(&self.user_avatars_dir(), id);
            return Ok(None);
        };

        if !looks_like_avatar_asset(value) {
            remove_assistant_avatar_files(&self.user_avatars_dir(), id);
            return Ok(Some(value.to_string()));
        }

        if let Some(source_assistant_id) = parse_assistant_avatar_route(value) {
            if let Some(existing_avatar_path) = self.find_existing_user_avatar_file(&source_assistant_id) {
                if source_assistant_id == id {
                    return Ok(Some(existing_avatar_path.to_string_lossy().to_string()));
                }
                return self.persist_user_avatar_file(id, &existing_avatar_path).map(Some);
            }
            if let Some(builtin_avatar) = self.builtin.avatar_asset(&source_assistant_id) {
                return self
                    .persist_user_avatar_bytes(id, &builtin_avatar.bytes, builtin_avatar.extension.as_deref())
                    .map(Some);
            }
            return Ok(Some(value.to_string()));
        }

        if let Some(source_path) = parse_local_avatar_path(value) {
            return self.persist_user_avatar_file(id, &source_path).map(Some);
        }

        remove_assistant_avatar_files(&self.user_avatars_dir(), id);
        Ok(Some(value.to_string()))
    }

    fn persist_user_avatar_file(&self, id: &str, source_path: &Path) -> Result<String, AssistantError> {
        let extension = source_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .ok_or_else(|| AssistantError::BadRequest("assistant avatar must have a file extension".into()))?;

        if !is_supported_avatar_extension(&extension) {
            return Err(AssistantError::BadRequest(format!(
                "unsupported assistant avatar format: .{extension}"
            )));
        }

        let destination_dir = self.user_avatars_dir();
        std::fs::create_dir_all(&destination_dir)
            .map_err(|e| AssistantError::Internal(format!("create assistant avatar directory: {e}")))?;
        remove_assistant_avatar_files(&destination_dir, id);

        let destination = destination_dir.join(format!("{id}.{extension}"));
        std::fs::copy(source_path, &destination).map_err(|e| {
            AssistantError::Internal(format!(
                "copy assistant avatar from '{}' to '{}': {e}",
                source_path.display(),
                destination.display()
            ))
        })?;

        Ok(destination.to_string_lossy().to_string())
    }

    fn persist_user_avatar_bytes(
        &self,
        id: &str,
        bytes: &[u8],
        extension: Option<&str>,
    ) -> Result<String, AssistantError> {
        let extension = extension
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| AssistantError::BadRequest("assistant avatar must have a file extension".into()))?;

        if !is_supported_avatar_extension(&extension) {
            return Err(AssistantError::BadRequest(format!(
                "unsupported assistant avatar format: .{extension}"
            )));
        }

        let destination_dir = self.user_avatars_dir();
        std::fs::create_dir_all(&destination_dir)
            .map_err(|e| AssistantError::Internal(format!("create assistant avatar directory: {e}")))?;
        remove_assistant_avatar_files(&destination_dir, id);

        let destination = destination_dir.join(format!("{id}.{extension}"));
        std::fs::write(&destination, bytes).map_err(|e| {
            AssistantError::Internal(format!("write assistant avatar to '{}': {e}", destination.display()))
        })?;

        Ok(destination.to_string_lossy().to_string())
    }

    fn find_existing_user_avatar_file(&self, id: &str) -> Option<PathBuf> {
        let entries = std::fs::read_dir(self.user_avatars_dir()).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let file_stem = path.file_stem().and_then(|stem| stem.to_str());
            if file_stem == Some(id) {
                return Some(path);
            }
        }
        None
    }

    fn user_rule_path(&self, id: &str, locale: Option<&str>) -> PathBuf {
        assistant_md_path(&self.user_rules_dir(), id, locale)
    }

    fn user_skill_path(&self, id: &str, locale: Option<&str>) -> PathBuf {
        assistant_md_path(&self.user_skills_dir(), id, locale)
    }

    async fn resolve_definition_identity(
        &self,
        source: &str,
        source_ref: Option<&str>,
        assistant_id: &str,
    ) -> Result<(String, String), AssistantError> {
        if let Some(source_ref) = source_ref
            && let Some(existing) = self
                .definition_repo
                .get_by_source_ref(source, source_ref)
                .await
                .map_err(|e| AssistantError::Internal(format!("get assistant definition by source_ref: {e}")))?
        {
            return Ok((existing.id, existing.assistant_id));
        }

        if let Some(existing) = self
            .definition_repo
            .get_by_assistant_id(assistant_id)
            .await
            .map_err(|e| AssistantError::Internal(format!("get assistant definition by key: {e}")))?
        {
            return Ok((existing.id, existing.assistant_id));
        }

        Ok((generate_prefixed_id("asstdef"), assistant_id.to_string()))
    }

    fn cleanup_user_assets(&self, id: &str) {
        remove_assistant_md_files(&self.user_rules_dir(), id);
        remove_assistant_md_files(&self.user_skills_dir(), id);
        remove_assistant_avatar_files(&self.user_avatars_dir(), id);
    }
}

#[async_trait::async_trait]
impl AssistantClassifier for AssistantService {
    async fn classify(&self, id: &str) -> AssistantSource {
        self.classify_source(id).await
    }
}

#[async_trait::async_trait]
impl AssistantRuleDispatcher for AssistantService {
    async fn read_rule(&self, id: &str, locale: Option<&str>) -> Result<String, ExtensionError> {
        AssistantService::read_rule(self, id, locale)
            .await
            .map_err(assistant_error_to_extension_error)
    }

    async fn write_rule(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), ExtensionError> {
        AssistantService::write_rule(self, id, locale, content)
            .await
            .map_err(assistant_error_to_extension_error)
    }

    async fn delete_rule(&self, id: &str) -> Result<bool, ExtensionError> {
        AssistantService::delete_rule(self, id)
            .await
            .map_err(assistant_error_to_extension_error)
    }

    async fn read_skill(&self, id: &str, locale: Option<&str>) -> Result<String, ExtensionError> {
        AssistantService::read_skill(self, id, locale)
            .await
            .map_err(assistant_error_to_extension_error)
    }

    async fn write_skill(&self, id: &str, locale: Option<&str>, content: &str) -> Result<(), ExtensionError> {
        AssistantService::write_skill(self, id, locale, content)
            .await
            .map_err(assistant_error_to_extension_error)
    }

    async fn delete_skill(&self, id: &str) -> Result<bool, ExtensionError> {
        AssistantService::delete_skill(self, id)
            .await
            .map_err(assistant_error_to_extension_error)
    }
}

fn assistant_error_to_extension_error(error: AssistantError) -> ExtensionError {
    match error {
        AssistantError::BadRequest(message) => ExtensionError::InvalidRequest(message),
        AssistantError::NotFound(message) => ExtensionError::NotFound(message),
        AssistantError::Internal(message) => ExtensionError::Internal(message),
        other => ExtensionError::Internal(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Response conversion
// ---------------------------------------------------------------------------

fn avatar_display_value(definition: &AssistantDefinitionRow) -> Option<String> {
    match definition.avatar_type.as_str() {
        "builtin_asset" | "user_asset" => definition.avatar_value.as_deref().map(|value| {
            if is_direct_avatar_url(value) {
                value.to_string()
            } else {
                format!("/api/assistants/{}/avatar", definition.assistant_id)
            }
        }),
        _ => definition.avatar_value.clone(),
    }
}

fn serialize_avatar(source: &str, avatar: Option<&str>) -> (String, Option<String>) {
    let Some(value) = avatar.map(str::trim).filter(|value| !value.is_empty()) else {
        return ("none".to_string(), None);
    };

    let avatar_type = if looks_like_avatar_asset(value) {
        match source {
            "builtin" => "builtin_asset",
            _ => "user_asset",
        }
    } else {
        "emoji"
    };

    (avatar_type.to_string(), Some(value.to_string()))
}

fn looks_like_avatar_asset(value: &str) -> bool {
    value.contains('/') || (std::path::Path::new(value).extension().is_some() && !value.starts_with('.'))
}

fn parse_local_avatar_path(value: &str) -> Option<PathBuf> {
    let path = value
        .strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(value));
    path.is_file().then_some(path)
}

fn is_supported_avatar_extension(extension: &str) -> bool {
    matches!(extension, "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg")
}

fn is_direct_avatar_url(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("data:")
        || value.starts_with("file://")
        || value.starts_with("/api/assistants/")
}

fn parse_assistant_avatar_route(value: &str) -> Option<String> {
    let prefix = "/api/assistants/";
    let suffix = "/avatar";
    let route = value
        .strip_prefix(prefix)
        .map(|rest| format!("{prefix}{rest}"))
        .or_else(|| value.find(prefix).map(|index| value[index..].to_string()))?;
    let id = route.strip_prefix(prefix)?.strip_suffix(suffix)?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn definition_to_response(
    definition: &AssistantDefinitionRow,
    state: Option<&AssistantOverlayRow>,
    projection: &AssistantRuntimeProjection,
) -> Result<AssistantResponse, AssistantError> {
    let source = match definition.source.as_str() {
        "builtin" => AssistantSource::Builtin,
        "generated" => AssistantSource::Generated,
        _ => AssistantSource::User,
    };
    let models = match (
        definition.default_model_mode.as_str(),
        definition.default_model_value.as_deref(),
    ) {
        ("fixed", Some(model)) => vec![model.to_string()],
        _ => Vec::new(),
    };

    Ok(AssistantResponse {
        id: definition.assistant_id.clone(),
        source,
        name: definition.name.clone(),
        name_i18n: decode_str_map(Some(definition.name_i18n.as_str()))?,
        description: definition.description.clone(),
        description_i18n: decode_str_map(Some(definition.description_i18n.as_str()))?,
        avatar: avatar_display_value(definition),
        enabled: state.is_none_or(|row| row.enabled),
        sort_order: state.map(|row| row.sort_order).unwrap_or(0),
        agent_id: projection.agent_id.clone(),
        agent: projection.agent.clone(),
        enabled_skills: decode_str_list(Some(definition.default_skill_ids.as_str()))?,
        custom_skill_names: decode_str_list(Some(definition.custom_skill_names.as_str()))?,
        disabled_builtin_skills: decode_str_list(Some(definition.default_disabled_builtin_skill_ids.as_str()))?,
        context: None,
        context_i18n: HashMap::new(),
        prompts: decode_str_list(Some(definition.recommended_prompts.as_str()))?,
        prompts_i18n: decode_list_map(Some(definition.recommended_prompts_i18n.as_str()))?,
        models,
        last_used_at: state.and_then(|row| row.last_used_at),
        agent_status: projection.agent_status,
        agent_status_message: projection.agent_status_message.clone(),
        team_selectable: projection.team_selectable,
        team_block_reason: projection.team_block_reason.clone(),
        deletable: projection.deletable,
    })
}

fn definition_to_detail_response(
    definition: &AssistantDefinitionRow,
    state: Option<&AssistantOverlayRow>,
    preference: Option<&aionui_db::AssistantPreferenceRow>,
    rules_content: &str,
    projection: &AssistantRuntimeProjection,
) -> Result<AssistantDetailResponse, AssistantError> {
    let default_skill_ids = decode_str_list(Some(definition.default_skill_ids.as_str()))?;
    let custom_skill_names = decode_str_list(Some(definition.custom_skill_names.as_str()))?;
    let default_disabled_builtin_skill_ids =
        decode_str_list(Some(definition.default_disabled_builtin_skill_ids.as_str()))?;
    let default_mcp_ids = decode_str_list(Some(definition.default_mcp_ids.as_str()))?;
    let last_skill_ids = preference
        .map(|row| decode_str_list(Some(row.last_skill_ids.as_str())))
        .transpose()?
        .unwrap_or_default();
    let last_disabled_builtin_skill_ids = preference
        .map(|row| decode_str_list(Some(row.last_disabled_builtin_skill_ids.as_str())))
        .transpose()?
        .unwrap_or_default();
    let last_mcp_ids = preference
        .map(|row| decode_str_list(Some(row.last_mcp_ids.as_str())))
        .transpose()?
        .unwrap_or_default();

    Ok(AssistantDetailResponse {
        id: definition.assistant_id.clone(),
        source: match definition.source.as_str() {
            "builtin" => AssistantSource::Builtin,
            "generated" => AssistantSource::Generated,
            _ => AssistantSource::User,
        },
        agent_status: projection.agent_status,
        agent_status_message: projection.agent_status_message.clone(),
        team_selectable: projection.team_selectable,
        team_block_reason: projection.team_block_reason.clone(),
        deletable: projection.deletable,
        profile: AssistantProfileResponse {
            name: definition.name.clone(),
            name_i18n: decode_str_map(Some(definition.name_i18n.as_str()))?,
            description: definition.description.clone(),
            description_i18n: decode_str_map(Some(definition.description_i18n.as_str()))?,
            avatar: avatar_display_value(definition),
        },
        state: AssistantStateResponse {
            enabled: state.map(|row| row.enabled).unwrap_or(true),
            sort_order: state.map(|row| row.sort_order).unwrap_or_default(),
            last_used_at: state.and_then(|row| row.last_used_at),
        },
        engine: AssistantEngineResponse {
            agent_id: projection.agent_id.clone(),
            agent: projection.agent.clone(),
        },
        rules: AssistantRulesResponse {
            content: if rules_content.is_empty() {
                definition.rule_inline_content.clone().unwrap_or_default()
            } else {
                rules_content.to_owned()
            },
            storage_mode: definition.rule_resource_type.clone(),
        },
        prompts: AssistantPromptsResponse {
            recommended: decode_str_list(Some(definition.recommended_prompts.as_str()))?,
            recommended_i18n: decode_list_map(Some(definition.recommended_prompts_i18n.as_str()))?,
        },
        defaults: AssistantDefaultsResponse {
            model: AssistantDefaultScalarResponse {
                mode: definition.default_model_mode.clone(),
                value: definition.default_model_value.clone(),
            },
            permission: AssistantDefaultScalarResponse {
                mode: definition.default_permission_mode.clone(),
                value: definition.default_permission_value.clone(),
            },
            skills: AssistantDefaultListResponse {
                mode: definition.default_skills_mode.clone(),
                value: default_skill_ids.clone(),
            },
            mcps: AssistantDefaultListResponse {
                mode: definition.default_mcps_mode.clone(),
                value: default_mcp_ids,
            },
        },
        capabilities: AssistantCapabilitiesResponse {
            default_skill_ids,
            custom_skill_names,
            default_disabled_builtin_skill_ids,
        },
        preferences: AssistantPreferencesResponse {
            last_model_id: preference.and_then(|row| row.last_model_id.clone()),
            last_permission_value: preference.and_then(|row| row.last_permission_value.clone()),
            last_skill_ids,
            last_disabled_builtin_skill_ids,
            last_mcp_ids,
        },
    })
}

#[derive(Debug, Clone)]
struct AssistantRuntimeProjection {
    agent_id: String,
    agent: Option<AssistantAgentResponse>,
    agent_status: AgentManagementStatus,
    agent_status_message: Option<String>,
    team_selectable: bool,
    team_block_reason: Option<String>,
    deletable: bool,
}

fn assistant_projection_for_definition(
    definition: &AssistantDefinitionRow,
    state: Option<&AssistantOverlayRow>,
    agent_rows: &[AgentManagementRow],
    resolved_runtime_backend: Option<&str>,
) -> AssistantRuntimeProjection {
    let enabled = state.is_none_or(|row| row.enabled);
    let source = match definition.source.as_str() {
        "builtin" => AssistantSource::Builtin,
        "generated" => AssistantSource::Generated,
        _ => AssistantSource::User,
    };
    let effective_agent_id = effective_agent_id_for_definition(definition, state);
    let fallback_runtime_backend = resolved_runtime_backend.unwrap_or(effective_agent_id);

    // An agent row identifies its runtime key by `backend` for vendor ACP
    // agents, but aionrs (the built-in Rust agent) has a NULL `backend` and is
    // keyed by its `agent_type` ("aionrs") instead. Match on either so aionrs
    // assistants resolve to the aionrs row rather than falling back to Missing.
    let row_matches_backend = |row: &&AgentManagementRow| {
        row.backend.as_deref() == Some(effective_agent_id)
            || row.agent_type.serde_name() == effective_agent_id
            || row.backend.as_deref() == Some(fallback_runtime_backend)
            || row.agent_type.serde_name() == fallback_runtime_backend
    };

    let agent_row = if matches!(source, AssistantSource::Generated) {
        agent_rows.iter().find(|row| row.id == effective_agent_id).or_else(|| {
            definition
                .source_ref
                .as_deref()
                .and_then(|source_ref| agent_rows.iter().find(|row| row.id == source_ref))
        })
    } else {
        agent_rows
            .iter()
            .find(|row| row.id == effective_agent_id)
            .or_else(|| {
                agent_rows
                    .iter()
                    .find(|row| row_matches_backend(row) && row.agent_source != AgentSource::Custom)
            })
            .or_else(|| agent_rows.iter().find(row_matches_backend))
    };
    let agent_id = agent_row
        .map(|row| row.id.clone())
        .unwrap_or_else(|| effective_agent_id.to_owned());
    let agent = agent_row.map(|row| AssistantAgentResponse {
        r#type: row.agent_type,
        source: row.agent_source,
        acp_backend: row.backend.clone(),
    });

    let agent_status = agent_row
        .map(|row| row.status)
        .unwrap_or(AgentManagementStatus::Missing);
    let agent_status_message = agent_row.and_then(|row| {
        row.last_check_error_message
            .clone()
            .or_else(|| row.last_check_guidance.clone())
    });
    let team_block_reason = if !enabled {
        Some("Assistant is disabled.".to_string())
    } else {
        match agent_row {
            Some(row) if matches!(row.status, AgentManagementStatus::Missing) => {
                Some("This assistant's agent is not installed.".to_string())
            }
            Some(row) if matches!(row.status, AgentManagementStatus::Offline) => Some(
                row.last_check_error_message
                    .clone()
                    .or_else(|| row.last_check_guidance.clone())
                    .unwrap_or_else(|| "This assistant's agent is unavailable.".to_string()),
            ),
            Some(row) if !row.team_capable => Some("This assistant's agent does not support team mode.".to_string()),
            None => Some("This assistant's agent could not be resolved.".to_string()),
            _ => None,
        }
    };

    AssistantRuntimeProjection {
        agent_id,
        agent,
        agent_status,
        agent_status_message,
        team_selectable: enabled
            && agent_row.is_some_and(|row| matches!(row.status, AgentManagementStatus::Online) && row.team_capable),
        team_block_reason,
        deletable: matches!(source, AssistantSource::User),
    }
}

fn effective_agent_id_for_definition<'a>(
    definition: &'a AssistantDefinitionRow,
    state: Option<&'a AssistantOverlayRow>,
) -> &'a str {
    state
        .and_then(|row| row.agent_id_override.as_deref())
        .unwrap_or(definition.agent_id.as_str())
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Serialized-JSON fragments for a single user-authored assistant row,
/// produced from either a create or update request.
struct SerializedFields {
    enabled_skills: Option<String>,
    custom_skill_names: Option<String>,
    disabled_builtin_skills: Option<String>,
    prompts: Option<String>,
    models: Option<String>,
    name_i18n: Option<String>,
    description_i18n: Option<String>,
    prompts_i18n: Option<String>,
}

impl SerializedFields {
    fn from_create(req: &CreateAssistantRequest) -> Result<Self, AssistantError> {
        Ok(Self {
            enabled_skills: encode_str_list(req.enabled_skills.as_deref())?,
            custom_skill_names: encode_str_list(req.custom_skill_names.as_deref())?,
            disabled_builtin_skills: encode_str_list(req.disabled_builtin_skills.as_deref())?,
            prompts: encode_str_list(req.prompts.as_deref())?,
            models: encode_str_list(req.models.as_deref())?,
            name_i18n: encode_str_map(req.name_i18n.as_ref())?,
            description_i18n: encode_str_map(req.description_i18n.as_ref())?,
            prompts_i18n: encode_list_map(req.prompts_i18n.as_ref())?,
        })
    }

    fn from_update(req: &UpdateAssistantRequest) -> Result<Self, AssistantError> {
        Ok(Self {
            enabled_skills: encode_str_list(req.enabled_skills.as_deref())?,
            custom_skill_names: encode_str_list(req.custom_skill_names.as_deref())?,
            disabled_builtin_skills: encode_str_list(req.disabled_builtin_skills.as_deref())?,
            prompts: encode_str_list(req.prompts.as_deref())?,
            models: encode_str_list(req.models.as_deref())?,
            name_i18n: encode_str_map(req.name_i18n.as_ref())?,
            description_i18n: encode_str_map(req.description_i18n.as_ref())?,
            prompts_i18n: encode_list_map(req.prompts_i18n.as_ref())?,
        })
    }
}

#[derive(Default)]
struct SerializedDetailOverrides {
    recommended_prompts: Option<String>,
    recommended_prompts_i18n: Option<String>,
    default_model_mode: Option<String>,
    default_model_value: Option<Option<String>>,
    default_permission_mode: Option<String>,
    default_permission_value: Option<Option<String>>,
    default_skills_mode: Option<String>,
    default_skill_ids: Option<String>,
    default_mcps_mode: Option<String>,
    default_mcp_ids: Option<String>,
}

impl SerializedDetailOverrides {
    fn from_create(req: &CreateAssistantRequest) -> Result<Self, AssistantError> {
        Self::from_parts(
            req.recommended_prompts.as_deref(),
            req.recommended_prompts_i18n.as_ref(),
            req.defaults.as_ref(),
        )
    }

    fn from_update(req: &UpdateAssistantRequest) -> Result<Self, AssistantError> {
        Self::from_parts(
            req.recommended_prompts.as_deref(),
            req.recommended_prompts_i18n.as_ref(),
            req.defaults.as_ref(),
        )
    }

    fn from_parts(
        recommended_prompts: Option<&[String]>,
        _recommended_prompts_i18n: Option<&HashMap<String, Vec<String>>>,
        defaults: Option<&AssistantDefaultsRequest>,
    ) -> Result<Self, AssistantError> {
        let mut result = Self {
            recommended_prompts: encode_str_list(recommended_prompts)?,
            // User-defined assistants currently have no locale-aware editor.
            // Keep unified storage canonical-only until product exposes it.
            recommended_prompts_i18n: None,
            ..Default::default()
        };

        if let Some(defaults) = defaults {
            if let Some(model) = defaults.model.as_ref() {
                let (mode, value) = validate_scalar_default(model, "defaults.model")?;
                result.default_model_mode = Some(mode);
                result.default_model_value = Some(value);
            }
            if let Some(permission) = defaults.permission.as_ref() {
                let (mode, value) = validate_scalar_default(permission, "defaults.permission")?;
                result.default_permission_mode = Some(mode);
                result.default_permission_value = Some(value);
            }
            if let Some(skills) = defaults.skills.as_ref() {
                let (mode, value) = validate_list_default(skills, "defaults.skills")?;
                result.default_skills_mode = Some(mode);
                result.default_skill_ids = Some(value);
            }
            if let Some(mcps) = defaults.mcps.as_ref() {
                let (mode, value) = validate_list_default(mcps, "defaults.mcps")?;
                result.default_mcps_mode = Some(mode);
                result.default_mcp_ids = Some(value);
            }
        }

        Ok(result)
    }

    fn has_changes(&self) -> bool {
        self.recommended_prompts.is_some()
            || self.recommended_prompts_i18n.is_some()
            || self.default_model_mode.is_some()
            || self.default_model_value.is_some()
            || self.default_permission_mode.is_some()
            || self.default_permission_value.is_some()
            || self.default_skills_mode.is_some()
            || self.default_skill_ids.is_some()
            || self.default_mcps_mode.is_some()
            || self.default_mcp_ids.is_some()
    }
}

fn apply_detail_patch_to_definition(
    definition: &mut AssistantDefinitionRow,
    overrides: &SerializedDetailOverrides,
    reset_model_and_permission: bool,
) {
    if reset_model_and_permission {
        definition.default_model_mode = "auto".to_string();
        definition.default_model_value = None;
        definition.default_permission_mode = "auto".to_string();
        definition.default_permission_value = None;
    }
    if let Some(value) = overrides.recommended_prompts.as_deref() {
        definition.recommended_prompts = value.to_string();
    }
    if let Some(value) = overrides.recommended_prompts_i18n.as_deref() {
        definition.recommended_prompts_i18n = value.to_string();
    }
    if let Some(value) = overrides.default_model_mode.as_deref() {
        definition.default_model_mode = value.to_string();
    }
    if let Some(value) = overrides.default_model_value.as_ref() {
        definition.default_model_value = value.clone();
    }
    if let Some(value) = overrides.default_permission_mode.as_deref() {
        definition.default_permission_mode = value.to_string();
    }
    if let Some(value) = overrides.default_permission_value.as_ref() {
        definition.default_permission_value = value.clone();
    }
    if let Some(value) = overrides.default_skills_mode.as_deref() {
        definition.default_skills_mode = value.to_string();
    }
    if let Some(value) = overrides.default_skill_ids.as_deref() {
        definition.default_skill_ids = value.to_string();
    }
    if let Some(value) = overrides.default_mcps_mode.as_deref() {
        definition.default_mcps_mode = value.to_string();
    }
    if let Some(value) = overrides.default_mcp_ids.as_deref() {
        definition.default_mcp_ids = value.to_string();
    }
}

fn upsert_params_from_definition(definition: &AssistantDefinitionRow) -> UpsertAssistantDefinitionParams<'_> {
    UpsertAssistantDefinitionParams {
        id: &definition.id,
        assistant_id: &definition.assistant_id,
        source: &definition.source,
        owner_type: &definition.owner_type,
        source_ref: definition.source_ref.as_deref(),
        source_version: definition.source_version.as_deref(),
        source_hash: definition.source_hash.as_deref(),
        name: &definition.name,
        name_i18n: &definition.name_i18n,
        description: definition.description.as_deref(),
        description_i18n: &definition.description_i18n,
        avatar_type: &definition.avatar_type,
        avatar_value: definition.avatar_value.as_deref(),
        agent_id: &definition.agent_id,
        rule_resource_type: &definition.rule_resource_type,
        rule_resource_ref: definition.rule_resource_ref.as_deref(),
        rule_inline_content: definition.rule_inline_content.as_deref(),
        recommended_prompts: &definition.recommended_prompts,
        recommended_prompts_i18n: &definition.recommended_prompts_i18n,
        default_model_mode: &definition.default_model_mode,
        default_model_value: definition.default_model_value.as_deref(),
        default_permission_mode: &definition.default_permission_mode,
        default_permission_value: definition.default_permission_value.as_deref(),
        default_skills_mode: &definition.default_skills_mode,
        default_skill_ids: &definition.default_skill_ids,
        custom_skill_names: &definition.custom_skill_names,
        default_disabled_builtin_skill_ids: &definition.default_disabled_builtin_skill_ids,
        default_mcps_mode: &definition.default_mcps_mode,
        default_mcp_ids: &definition.default_mcp_ids,
    }
}

fn encode_str_list(value: Option<&[String]>) -> Result<Option<String>, AssistantError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AssistantError::Internal(format!("encode list: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn validate_scalar_default(
    value: &AssistantDefaultScalarRequest,
    field_name: &str,
) -> Result<(String, Option<String>), AssistantError> {
    match value.mode.as_str() {
        "auto" => Ok(("auto".into(), None)),
        "fixed" => {
            let fixed = value.value.clone().filter(|v| !v.trim().is_empty()).ok_or_else(|| {
                AssistantError::BadRequest(format!("{field_name}.value is required when mode='fixed'"))
            })?;
            Ok(("fixed".into(), Some(fixed)))
        }
        other => Err(AssistantError::BadRequest(format!(
            "{field_name}.mode must be 'auto' or 'fixed', got '{other}'"
        ))),
    }
}

fn validate_list_default(
    value: &AssistantDefaultListRequest,
    field_name: &str,
) -> Result<(String, String), AssistantError> {
    match value.mode.as_str() {
        "auto" => Ok(("auto".into(), "[]".into())),
        "fixed" => Ok((
            "fixed".into(),
            serde_json::to_string(&value.value)
                .map_err(|e| AssistantError::Internal(format!("encode {field_name}: {e}")))?,
        )),
        other => Err(AssistantError::BadRequest(format!(
            "{field_name}.mode must be 'auto' or 'fixed', got '{other}'"
        ))),
    }
}

fn encode_str_map(value: Option<&HashMap<String, String>>) -> Result<Option<String>, AssistantError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AssistantError::Internal(format!("encode map: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn encode_list_map(value: Option<&HashMap<String, Vec<String>>>) -> Result<Option<String>, AssistantError> {
    match value {
        Some(v) => Ok(Some(
            serde_json::to_string(v).map_err(|e| AssistantError::Internal(format!("encode map: {e}")))?,
        )),
        None => Ok(None),
    }
}

fn decode_str_list(raw: Option<&str>) -> Result<Vec<String>, AssistantError> {
    match raw {
        Some(s) if !s.is_empty() => {
            serde_json::from_str(s).map_err(|e| AssistantError::Internal(format!("decode list: {e}")))
        }
        _ => Ok(Vec::new()),
    }
}

fn decode_str_map(raw: Option<&str>) -> Result<HashMap<String, String>, AssistantError> {
    match raw {
        Some(s) if !s.is_empty() => {
            serde_json::from_str(s).map_err(|e| AssistantError::Internal(format!("decode map: {e}")))
        }
        _ => Ok(HashMap::new()),
    }
}

fn decode_list_map(raw: Option<&str>) -> Result<HashMap<String, Vec<String>>, AssistantError> {
    match raw {
        Some(s) if !s.is_empty() => {
            serde_json::from_str(s).map_err(|e| AssistantError::Internal(format!("decode map: {e}")))
        }
        _ => Ok(HashMap::new()),
    }
}

fn normalize_json_array_string(raw: Option<&str>, field: &str) -> Result<String, AssistantError> {
    serde_json::to_string(&decode_str_list(raw)?).map_err(|e| AssistantError::Internal(format!("encode {field}: {e}")))
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn assistant_md_path(dir: &Path, id: &str, locale: Option<&str>) -> PathBuf {
    let filename = match locale {
        Some(loc) if !loc.is_empty() => format!("{id}.{loc}.md"),
        _ => format!("{id}.md"),
    };
    dir.join(filename)
}

fn read_file_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Remove every `{id}*.md` file in `dir`. Returns `true` if any file was
/// deleted.
fn remove_assistant_md_files(dir: &Path, id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut deleted = false;
    let prefix = format!("{id}.");
    let exact = format!("{id}.md");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == exact || (name.starts_with(&prefix) && name.ends_with(".md")) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!("failed to remove {}: {e}", entry.path().display());
                continue;
            }
            deleted = true;
        }
    }
    deleted
}

fn remove_assistant_avatar_files(dir: &Path, id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut deleted = false;
    let prefix = format!("{id}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!("failed to remove {}: {e}", entry.path().display());
                continue;
            }
            deleted = true;
        }
    }
    deleted
}

/// Generate a new user-authored assistant id with millisecond-resolution
/// timestamp + 4 hex chars of randomness.
pub fn generate_user_id() -> String {
    // Use time + a pseudo-random 16-bit value (sufficient for collision-free
    // ids within the same millisecond for any realistic UI workflow).
    let ms = now_ms();
    // Best-effort 16-bit random: hash the current nanos.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let hex = format!("{:04x}", (nanos as u16) ^ 0xA5A5);
    debug!("generated user assistant id: custom-{ms}-{hex}");
    format!("custom-{ms}-{hex}")
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{
        CreateProviderParams, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
        SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
        SqliteProviderRepository, init_database_memory,
    };
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct Fixture {
        service: AssistantService,
        definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        state_repo: Arc<dyn IAssistantOverlayRepository>,
        preference_repo: Arc<dyn IAssistantPreferenceRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        agent_rows: Arc<Mutex<Vec<aionui_api_types::AgentManagementRow>>>,
        _tmp: TempDir,
        _db: aionui_db::Database,
    }

    #[derive(Clone, Default)]
    struct StubAgentCatalog {
        rows: Arc<Mutex<Vec<aionui_api_types::AgentManagementRow>>>,
    }

    #[async_trait::async_trait]
    impl AssistantAgentCatalogPort for StubAgentCatalog {
        async fn list_management_agents(&self) -> Result<Vec<aionui_api_types::AgentManagementRow>, AssistantError> {
            Ok(self.rows.lock().expect("agent rows lock poisoned").clone())
        }
    }

    /// Default fixture: seeded with a single OpenAI-compatible provider so
    /// `resolve_default_agent_type` returns `"aionrs"`. Tests that need to
    /// exercise the no-provider or anthropic-only branches construct their
    /// own fixture via [`fixture_with_options`].
    async fn fixture() -> Fixture {
        fixture_with_options(FixtureOpts::default()).await
    }

    async fn fixture_with_builtins(builtins: Vec<BuiltinAssistant>) -> Fixture {
        fixture_with_options(FixtureOpts {
            builtins,
            ..Default::default()
        })
        .await
    }

    #[derive(Default)]
    struct FixtureOpts {
        builtins: Vec<BuiltinAssistant>,
        /// When `true`, no provider is seeded — used by the test that
        /// asserts the no-provider error path.
        no_default_provider: bool,
        /// When set, the seeded provider's `platform` is overridden.
        /// Defaults to `"openai"` so existing tests get an `"aionrs"`
        /// default agent type.
        seed_platform: Option<&'static str>,
        agent_rows: Vec<aionui_api_types::AgentManagementRow>,
    }

    async fn fixture_with_options(opts: FixtureOpts) -> Fixture {
        let tmp = TempDir::new().unwrap();
        let db = init_database_memory().await.unwrap();
        let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
            Arc::new(SqliteAssistantDefinitionRepository::new(db.pool().clone()));
        let state_repo: Arc<dyn IAssistantOverlayRepository> =
            Arc::new(SqliteAssistantOverlayRepository::new(db.pool().clone()));
        let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
            Arc::new(SqliteAssistantPreferenceRepository::new(db.pool().clone()));
        let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(db.pool().clone()));
        let orepo: Arc<dyn IAssistantOverrideRepository> =
            Arc::new(SqliteAssistantOverrideRepository::new(db.pool().clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));

        if !opts.no_default_provider {
            seed_provider(&*provider_repo, opts.seed_platform.unwrap_or("openai")).await;
        }

        // Write a manifest into a temp dir and load from it.
        let assets_dir = tmp.path().join("assets");
        std::fs::create_dir_all(&assets_dir).unwrap();
        let manifest_json = serde_json::json!({
            "version": "1.0.0",
            "assistants": opts
                .builtins
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "id": b.id,
                        "name": b.name,
                        "agent_ref": b.agent_ref,
                        "rule_file": b.rule_file,
                    })
                })
                .collect::<Vec<_>>()
        });
        std::fs::write(
            assets_dir.join("assistants.json"),
            serde_json::to_string(&manifest_json).unwrap(),
        )
        .unwrap();
        let builtin_reg = Arc::new(BuiltinAssistantRegistry::load_from_dir(assets_dir));

        let agent_rows = Arc::new(Mutex::new(opts.agent_rows.clone()));
        let service = AssistantService::new(
            db.pool().clone(),
            AssistantServiceDeps {
                definition_repo: definition_repo.clone(),
                state_repo: state_repo.clone(),
                preference_repo: preference_repo.clone(),
                repo,
                override_repo: orepo,
                provider_repo: provider_repo.clone(),
                builtin: builtin_reg,
                agent_catalog: Some(Arc::new(StubAgentCatalog {
                    rows: agent_rows.clone(),
                })),
            },
            tmp.path().to_path_buf(),
        );
        service.bootstrap_assistant_storage().await.unwrap();

        Fixture {
            service,
            definition_repo,
            state_repo,
            preference_repo,
            provider_repo,
            agent_rows,
            _tmp: tmp,
            _db: db,
        }
    }

    async fn seed_provider(repo: &dyn IProviderRepository, platform: &str) {
        repo.create(CreateProviderParams {
            id: None,
            platform,
            name: "Test Provider",
            base_url: "https://example.invalid",
            api_key_encrypted: "stub",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("seed provider");
    }

    fn mk_builtin(id: &str, name: &str) -> BuiltinAssistant {
        BuiltinAssistant {
            id: id.into(),
            name: name.into(),
            name_i18n: HashMap::new(),
            description: None,
            description_i18n: HashMap::new(),
            avatar: None,
            agent_ref: "gemini".into(),
            enabled_skills: Vec::new(),
            custom_skill_names: Vec::new(),
            disabled_builtin_skills: Vec::new(),
            rule_file: None,
            prompts: Vec::new(),
            prompts_i18n: HashMap::new(),
            models: Vec::new(),
        }
    }

    fn mk_agent_row(
        id: &str,
        backend: &str,
        status: aionui_api_types::AgentManagementStatus,
    ) -> aionui_api_types::AgentManagementRow {
        aionui_api_types::AgentManagementRow {
            id: id.into(),
            icon: Some(format!("/api/assets/{backend}.svg")),
            name: format!("{backend} agent"),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some(backend.into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            installed: true,
            command: Some(backend.into()),
            args: Vec::new(),
            env: Vec::new(),
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy {
                supports_team: true,
                ..Default::default()
            },
            yolo_id: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            sort_order: 3100,
            team_capable: true,
            status,
            last_check_status: Some(aionui_api_types::AgentSnapshotCheckStatus::Online),
            last_check_kind: Some(aionui_api_types::AgentSnapshotCheckKind::Manual),
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: Some(42),
            last_check_at: Some(1_750_000_000_000),
            last_success_at: Some(1_750_000_000_000),
            last_failure_at: None,
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[tokio::test]
    async fn list_empty_is_empty() {
        let fx = fixture().await;
        let list = fx.service.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn list_includes_builtin_and_user() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;

        let created = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap();
        assert_eq!(created.source, AssistantSource::User);

        let list = fx.service.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|a| a.id == "builtin-office"));
        assert!(list.iter().any(|a| a.id == "u1"));
    }

    #[tokio::test]
    async fn list_maps_generated_definition_to_generated_source() {
        let fx = fixture().await;
        fx.definition_repo
            .upsert(&UpsertAssistantDefinitionParams {
                id: "asstdef-generated",
                assistant_id: "bare:claude",
                source: "generated",
                owner_type: "system",
                source_ref: Some("agent-claude"),
                source_version: None,
                source_hash: None,
                name: "Claude",
                name_i18n: "{}",
                description: None,
                description_i18n: "{}",
                avatar_type: "none",
                avatar_value: None,
                agent_id: "agent-claude",
                rule_resource_type: "none",
                rule_resource_ref: None,
                rule_inline_content: None,
                recommended_prompts: "[]",
                recommended_prompts_i18n: "{}",
                default_model_mode: "auto",
                default_model_value: None,
                default_permission_mode: "auto",
                default_permission_value: None,
                default_skills_mode: "auto",
                default_skill_ids: "[]",
                custom_skill_names: "[]",
                default_disabled_builtin_skill_ids: "[]",
                default_mcps_mode: "auto",
                default_mcp_ids: "[]",
            })
            .await
            .unwrap();
        fx.state_repo
            .upsert(&UpsertAssistantOverlayParams {
                assistant_definition_id: "asstdef-generated",
                enabled: true,
                sort_order: 3,
                agent_id_override: None,
                last_used_at: None,
            })
            .await
            .unwrap();

        let list = fx.service.list().await.unwrap();
        let generated = list.iter().find(|assistant| assistant.id == "bare:claude").unwrap();
        assert_eq!(generated.source, AssistantSource::Generated);
    }

    #[tokio::test]
    async fn bootstrap_materializes_generated_assistant_from_available_agent() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        let list = fx.service.list().await.unwrap();
        let bare = list
            .iter()
            .find(|assistant| assistant.id == "bare:agent-claude")
            .unwrap();
        assert_eq!(bare.source, AssistantSource::Generated);
        assert_eq!(bare.agent_id, "agent-claude");
        assert_eq!(bare.agent_status, aionui_api_types::AgentManagementStatus::Online);
        assert!(bare.team_selectable);
        assert!(!bare.deletable);

        let detail = fx.service.get_detail("bare:agent-claude", Some("en-US")).await.unwrap();
        assert_eq!(detail.defaults.skills.mode, "fixed");
        assert!(detail.defaults.skills.value.is_empty());
        assert!(detail.capabilities.default_disabled_builtin_skill_ids.is_empty());
    }

    #[tokio::test]
    async fn reconcile_upgrades_empty_generated_auto_skill_defaults_to_fixed() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        let definition = fx
            .definition_repo
            .get_by_assistant_id("bare:agent-claude")
            .await
            .unwrap()
            .expect("generated assistant definition should exist after bootstrap");
        let mut legacy_definition = definition.clone();
        legacy_definition.default_skills_mode = "auto".into();
        legacy_definition.default_skill_ids = "[]".into();
        legacy_definition.default_disabled_builtin_skill_ids = "[]".into();
        fx.definition_repo
            .upsert(&upsert_params_from_definition(&legacy_definition))
            .await
            .unwrap();

        let detail = fx.service.get_detail("bare:agent-claude", Some("en-US")).await.unwrap();

        assert_eq!(detail.defaults.skills.mode, "fixed");
        assert!(detail.defaults.skills.value.is_empty());
        assert!(detail.capabilities.default_disabled_builtin_skill_ids.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_materializes_generated_assistant_from_available_custom_agent() {
        let mut custom_row = mk_agent_row(
            "custom-agent-1",
            "custom",
            aionui_api_types::AgentManagementStatus::Online,
        );
        custom_row.name = "Custom ACP Agent".into();
        custom_row.agent_source = aionui_api_types::AgentSource::Custom;

        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![custom_row],
            ..Default::default()
        })
        .await;

        let list = fx.service.list().await.unwrap();
        let bare = list
            .iter()
            .find(|assistant| assistant.id == "bare:custom-agent-1")
            .expect("available custom agent should be materialized as a generated assistant");
        assert_eq!(bare.source, AssistantSource::Generated);
        assert_eq!(bare.name, "Custom ACP Agent");
        assert_eq!(bare.agent_id, "custom-agent-1");
        assert_eq!(bare.agent_status, aionui_api_types::AgentManagementStatus::Online);
        assert!(bare.team_selectable);
        assert!(!bare.deletable);
    }

    #[tokio::test]
    async fn bootstrap_falls_back_to_agent_type_when_backend_is_empty() {
        // Engines like Aion CLI carry their identity in `agent_type` and leave
        // `backend` empty (it is an ACP-vendor label). The generated assistant must
        // still expose the concrete agent id so the frontend does not bind it
        // through an overloaded runtime backend label.
        let mut agent_row = mk_agent_row(
            "agent-aionrs",
            "aionrs",
            aionui_api_types::AgentManagementStatus::Online,
        );
        agent_row.backend = None;
        agent_row.agent_type = aionui_common::AgentType::Aionrs;

        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![agent_row],
            ..Default::default()
        })
        .await;

        let list = fx.service.list().await.unwrap();
        let bare = list
            .iter()
            .find(|assistant| assistant.id == "bare:agent-aionrs")
            .unwrap();
        assert_eq!(bare.agent_id, "agent-aionrs");
    }

    #[tokio::test]
    async fn aionrs_assistant_resolves_agent_status_via_agent_type_not_backend() {
        // Regression: an assistant whose engine is aionrs must match the aionrs
        // agent row by `agent_type` ("aionrs"), since that row's `backend` is
        // NULL. Matching on `backend` alone left the row unresolved and
        // mislabelled every aionrs assistant as Missing/unavailable.
        let mut aionrs_row = mk_agent_row(
            "agent-aionrs",
            "aionrs",
            aionui_api_types::AgentManagementStatus::Online,
        );
        aionrs_row.backend = None;
        aionrs_row.agent_type = aionui_common::AgentType::Aionrs;

        let mut builtin = mk_builtin("builtin-aionrs", "Aion Assistant");
        builtin.agent_ref = "aionrs".into();

        let fx = fixture_with_options(FixtureOpts {
            builtins: vec![builtin],
            agent_rows: vec![aionrs_row],
            ..Default::default()
        })
        .await;

        let list = fx.service.list().await.unwrap();
        let assistant = list
            .iter()
            .find(|assistant| assistant.id == "builtin-aionrs")
            .expect("aionrs builtin assistant should be listed");
        assert_eq!(
            assistant.agent_status,
            aionui_api_types::AgentManagementStatus::Online,
            "aionrs assistant should resolve to the online aionrs agent row, not Missing"
        );
    }

    #[tokio::test]
    async fn bootstrap_places_new_generated_assistants_before_existing_assistants() {
        let fx = fixture_with_options(FixtureOpts {
            builtins: vec![mk_builtin("builtin-office", "Office")],
            agent_rows: vec![
                mk_agent_row(
                    "agent-claude",
                    "claude",
                    aionui_api_types::AgentManagementStatus::Online,
                ),
                mk_agent_row("agent-codex", "codex", aionui_api_types::AgentManagementStatus::Online),
            ],
            ..Default::default()
        })
        .await;

        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap();

        let list = fx.service.list().await.unwrap();
        let ordered_ids: Vec<&str> = list.iter().map(|assistant| assistant.id.as_str()).collect();

        assert_eq!(ordered_ids[0..2], ["bare:agent-claude", "bare:agent-codex"]);
        assert!(ordered_ids[2..].contains(&"builtin-office"));
        assert!(ordered_ids[2..].contains(&"u1"));
    }

    #[tokio::test]
    async fn reconcile_generated_assistants_preserves_existing_user_sort_order() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        let first = fx.service.list().await.unwrap();
        let bare = first
            .iter()
            .find(|assistant| assistant.id == "bare:agent-claude")
            .expect("generated assistant should exist after first reconcile");
        assert_eq!(bare.sort_order, -1);

        fx.service
            .set_state(
                "bare:agent-claude",
                SetAssistantStateRequest {
                    sort_order: Some(9000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let second = fx.service.list().await.unwrap();
        let bare_after_reconcile = second
            .iter()
            .find(|assistant| assistant.id == "bare:agent-claude")
            .expect("generated assistant should still exist");
        assert_eq!(bare_after_reconcile.sort_order, 9000);
    }

    #[tokio::test]
    async fn bootstrap_materializes_builtin_and_syncs_legacy_rows() {
        let mut builtin = mk_builtin("builtin-office", "Office");
        builtin.rule_file = Some("rules/builtin-office.{locale}.md".into());
        let fx = fixture_with_builtins(vec![builtin]).await;

        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap();
        fx.service
            .set_state(
                "builtin-office",
                SetAssistantStateRequest {
                    enabled: Some(false),
                    sort_order: Some(9),
                    last_used_at: Some(1234),
                },
            )
            .await
            .unwrap();

        fx.service.bootstrap_assistant_storage().await.unwrap();

        let builtin = fx
            .definition_repo
            .get_by_assistant_id("builtin-office")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(builtin.source, "builtin");
        assert_eq!(builtin.rule_resource_type, "builtin_asset");
        assert_eq!(builtin.rule_resource_ref.as_deref(), Some("builtin-office"));
        let user = fx.definition_repo.get_by_assistant_id("u1").await.unwrap().unwrap();
        assert_eq!(user.source, "user");
        let builtin_state = fx.state_repo.get(&builtin.id).await.unwrap().unwrap();
        assert!(!builtin_state.enabled);
        assert_eq!(builtin_state.sort_order, 9);
        assert_eq!(builtin_state.last_used_at, Some(1234));
    }

    #[tokio::test]
    async fn bootstrap_soft_deletes_builtin_removed_from_manifest() {
        let mut fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;

        let original = fx
            .definition_repo
            .get_by_assistant_id("builtin-office")
            .await
            .unwrap()
            .unwrap();
        fx.service.builtin = Arc::new(BuiltinAssistantRegistry::empty());

        fx.service.bootstrap_assistant_storage().await.unwrap();

        assert!(
            fx.definition_repo
                .get_by_assistant_id("builtin-office")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fx.service
                .list()
                .await
                .unwrap()
                .iter()
                .all(|assistant| assistant.id != "builtin-office")
        );
        assert!(fx.definition_repo.get_by_id(&original.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_user_definition_ignores_i18n_payloads_in_unified_storage() {
        let fx = fixture().await;
        let mut name_i18n = HashMap::new();
        name_i18n.insert("zh-CN".into(), "中文名".into());
        let mut description_i18n = HashMap::new();
        description_i18n.insert("zh-CN".into(), "中文描述".into());
        let mut prompts_i18n = HashMap::new();
        prompts_i18n.insert("zh-CN".into(), vec!["中文提示词".into()]);
        let mut recommended_prompts_i18n = HashMap::new();
        recommended_prompts_i18n.insert("zh-CN".into(), vec!["推荐提示词".into()]);

        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                description: Some("desc".into()),
                name_i18n: Some(name_i18n),
                description_i18n: Some(description_i18n),
                prompts_i18n: Some(prompts_i18n),
                recommended_prompts_i18n: Some(recommended_prompts_i18n),
                ..req_default()
            })
            .await
            .unwrap();

        let definition = fx.definition_repo.get_by_assistant_id("u1").await.unwrap().unwrap();
        assert_eq!(definition.name_i18n, "{}");
        assert_eq!(definition.description_i18n, "{}");
        assert_eq!(definition.recommended_prompts_i18n, "{}");
    }

    #[tokio::test]
    async fn create_rejects_empty_name() {
        let fx = fixture().await;
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "   ".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_rejects_builtin_id_collision() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("builtin-office".into()),
                name: "Mine".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_rejects_duplicate_user_id() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        let err = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "B".into(),
                ..req_default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::Conflict(_)));
    }

    #[tokio::test]
    async fn update_rejects_builtin_non_preset_fields() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    name: Some("New".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::Forbidden(_)));
    }

    #[tokio::test]
    async fn update_builtin_agent_id_writes_override() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let updated = fx
            .service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    agent_id: Some("2d23ff1c".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.source, AssistantSource::Builtin);
        assert_eq!(updated.agent_id, "2d23ff1c");
        // List view must reflect the override too.
        let listed = fx
            .service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.id == "builtin-office")
            .unwrap();
        assert_eq!(listed.agent_id, "2d23ff1c");
    }

    #[tokio::test]
    async fn update_builtin_allows_agent_model_and_permission_overrides() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let updated = fx
            .service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    agent_id: Some("cc126dd5".into()),
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("gemini-2.5-pro".into()),
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("default".into()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.source, AssistantSource::Builtin);
        assert_eq!(updated.agent_id, "cc126dd5");

        let detail = fx.service.get_detail("builtin-office", Some("en-US")).await.unwrap();
        assert_eq!(detail.defaults.model.mode, "fixed");
        assert_eq!(detail.defaults.model.value.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(detail.defaults.permission.mode, "fixed");
        assert_eq!(detail.defaults.permission.value.as_deref(), Some("default"));
    }

    #[tokio::test]
    async fn update_generated_rejects() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        let err = fx
            .service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    name: Some("Nope".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::Forbidden(_)));
    }

    #[tokio::test]
    async fn update_generated_persists_editable_fields() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        fx.service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    description: Some("local cli description".into()),
                    enabled_skills: Some(vec!["skill-a".into()]),
                    custom_skill_names: Some(vec!["custom skill".into()]),
                    disabled_builtin_skills: Some(vec!["builtin-off".into()]),
                    recommended_prompts: Some(vec!["Start locally".into()]),
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("openai/gpt-5".into()),
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("strict".into()),
                        }),
                        skills: Some(AssistantDefaultListRequest {
                            mode: "fixed".into(),
                            value: vec!["skill-a".into()],
                        }),
                        mcps: Some(AssistantDefaultListRequest {
                            mode: "fixed".into(),
                            value: vec!["mcp-a".into()],
                        }),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let detail = fx.service.get_detail("bare:agent-claude", Some("en-US")).await.unwrap();
        assert_eq!(detail.profile.description.as_deref(), Some("local cli description"));
        assert_eq!(detail.prompts.recommended, vec!["Start locally"]);
        assert_eq!(detail.defaults.model.mode, "fixed");
        assert_eq!(detail.defaults.model.value.as_deref(), Some("openai/gpt-5"));
        assert_eq!(detail.defaults.permission.mode, "fixed");
        assert_eq!(detail.defaults.permission.value.as_deref(), Some("strict"));
        assert_eq!(detail.defaults.skills.mode, "fixed");
        assert_eq!(detail.defaults.skills.value, vec!["skill-a"]);
        assert_eq!(detail.defaults.mcps.mode, "fixed");
        assert_eq!(detail.defaults.mcps.value, vec!["mcp-a"]);
        assert_eq!(detail.capabilities.default_skill_ids, vec!["skill-a"]);
        assert_eq!(detail.capabilities.custom_skill_names, vec!["custom skill"]);
        assert_eq!(
            detail.capabilities.default_disabled_builtin_skill_ids,
            vec!["builtin-off"]
        );
    }

    #[tokio::test]
    async fn update_generated_rejects_identity_fields() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        let name_err = fx
            .service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    name: Some("Renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(name_err, AssistantError::Forbidden(_)));

        let avatar_err = fx
            .service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    avatar: Some("🙂".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(avatar_err, AssistantError::Forbidden(_)));

        let agent_err = fx
            .service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    agent_id: Some("agent-codex".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(agent_err, AssistantError::Forbidden(_)));
    }

    #[tokio::test]
    async fn reconcile_generated_assistant_refreshes_identity_without_overwriting_edits() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;

        fx.service
            .update(
                "bare:agent-claude",
                UpdateAssistantRequest {
                    description: Some("user edited description".into()),
                    recommended_prompts: Some(vec!["Keep this prompt".into()]),
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("openai/gpt-5".into()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        {
            let mut rows = fx.agent_rows.lock().expect("agent rows lock poisoned");
            rows[0].name = "Claude Code renamed".into();
            rows[0].icon = Some("🤖".into());
            rows[0].description = Some("agent supplied description".into());
        }

        let listed = fx.service.list().await.unwrap();
        let assistant = listed
            .iter()
            .find(|assistant| assistant.id == "bare:agent-claude")
            .expect("generated assistant remains listed");
        assert_eq!(assistant.name, "Claude Code renamed");
        assert_eq!(assistant.avatar.as_deref(), Some("🤖"));
        assert_eq!(assistant.description.as_deref(), Some("user edited description"));

        let detail = fx.service.get_detail("bare:agent-claude", Some("en-US")).await.unwrap();
        assert_eq!(detail.profile.name, "Claude Code renamed");
        assert_eq!(detail.profile.avatar.as_deref(), Some("🤖"));
        assert_eq!(detail.profile.description.as_deref(), Some("user edited description"));
        assert_eq!(detail.prompts.recommended, vec!["Keep this prompt"]);
        assert_eq!(detail.defaults.model.mode, "fixed");
        assert_eq!(detail.defaults.model.value.as_deref(), Some("openai/gpt-5"));
    }

    #[tokio::test]
    async fn update_builtin_changing_agent_without_defaults_clears_model_and_permission() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        fx.service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    agent_id: Some("cc126dd5".into()),
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("gemini-2.5-pro".into()),
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("default".into()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        fx.service
            .update(
                "builtin-office",
                UpdateAssistantRequest {
                    agent_id: Some("2d23ff1c".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let detail = fx.service.get_detail("builtin-office", Some("en-US")).await.unwrap();
        assert_eq!(detail.engine.agent_id, "2d23ff1c");
        assert_eq!(detail.defaults.model.mode, "auto");
        assert_eq!(detail.defaults.model.value, None);
        assert_eq!(detail.defaults.permission.mode, "auto");
        assert_eq!(detail.defaults.permission.value, None);
    }

    #[tokio::test]
    async fn builtin_detail_defaults_start_auto_for_model_permission_and_mcps() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;

        let detail = fx.service.get_detail("builtin-office", Some("en-US")).await.unwrap();
        assert_eq!(detail.defaults.model.mode, "auto");
        assert_eq!(detail.defaults.model.value, None);
        assert_eq!(detail.defaults.permission.mode, "auto");
        assert_eq!(detail.defaults.permission.value, None);
        assert_eq!(detail.defaults.mcps.mode, "auto");
        assert!(detail.defaults.mcps.value.is_empty());
    }

    #[tokio::test]
    async fn update_user_partial_preserves_other_fields() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "original".into(),
                description: Some("desc".into()),
                ..req_default()
            })
            .await
            .unwrap();
        let updated = fx
            .service
            .update(
                "u1",
                UpdateAssistantRequest {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.description.as_deref(), Some("desc"));
    }

    #[tokio::test]
    async fn update_user_changing_agent_without_defaults_clears_model_and_permission() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                defaults: Some(AssistantDefaultsRequest {
                    model: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("openai/gpt-5".into()),
                    }),
                    permission: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("default".into()),
                    }),
                    ..Default::default()
                }),
                ..req_default()
            })
            .await
            .unwrap();

        fx.service
            .update(
                "u1",
                UpdateAssistantRequest {
                    agent_id: Some("8e1acf31".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let detail = fx.service.get_detail("u1", Some("en-US")).await.unwrap();
        assert_eq!(detail.engine.agent_id, "8e1acf31");
        assert_eq!(detail.defaults.model.mode, "auto");
        assert_eq!(detail.defaults.model.value, None);
        assert_eq!(detail.defaults.permission.mode, "auto");
        assert_eq!(detail.defaults.permission.value, None);
    }

    #[tokio::test]
    async fn create_user_without_governance_defaults_starts_auto() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                ..req_default()
            })
            .await
            .unwrap();

        let detail = fx.service.get_detail("u1", Some("en-US")).await.unwrap();
        assert_eq!(detail.defaults.model.mode, "auto");
        assert_eq!(detail.defaults.permission.mode, "auto");
        assert_eq!(detail.defaults.mcps.mode, "auto");
    }

    #[tokio::test]
    async fn create_persists_detail_defaults_and_recommended_prompts() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                recommended_prompts: Some(vec!["Write a plan".into(), "Summarize risks".into()]),
                defaults: Some(AssistantDefaultsRequest {
                    model: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("openai/gpt-5".into()),
                    }),
                    permission: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("default".into()),
                    }),
                    skills: Some(AssistantDefaultListRequest {
                        mode: "fixed".into(),
                        value: vec!["skill-a".into(), "skill-b".into()],
                    }),
                    mcps: Some(AssistantDefaultListRequest {
                        mode: "fixed".into(),
                        value: vec!["mcp-a".into()],
                    }),
                }),
                ..req_default()
            })
            .await
            .unwrap();

        let detail = fx.service.get_detail("u1", Some("en-US")).await.unwrap();
        assert_eq!(detail.prompts.recommended, vec!["Write a plan", "Summarize risks"]);
        assert_eq!(detail.defaults.model.mode, "fixed");
        assert_eq!(detail.defaults.model.value.as_deref(), Some("openai/gpt-5"));
        assert_eq!(detail.defaults.permission.mode, "fixed");
        assert_eq!(detail.defaults.permission.value.as_deref(), Some("default"));
        assert_eq!(detail.defaults.skills.mode, "fixed");
        assert_eq!(detail.defaults.skills.value, vec!["skill-a", "skill-b"]);
        assert_eq!(detail.defaults.mcps.mode, "fixed");
        assert_eq!(detail.defaults.mcps.value, vec!["mcp-a"]);
    }

    #[tokio::test]
    async fn update_persists_detail_defaults_and_recommended_prompts() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                ..req_default()
            })
            .await
            .unwrap();

        fx.service
            .update(
                "u1",
                UpdateAssistantRequest {
                    recommended_prompts: Some(vec!["Start here".into()]),
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "auto".into(),
                            value: None,
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("strict".into()),
                        }),
                        skills: Some(AssistantDefaultListRequest {
                            mode: "fixed".into(),
                            value: vec!["skill-z".into()],
                        }),
                        mcps: Some(AssistantDefaultListRequest {
                            mode: "auto".into(),
                            value: vec![],
                        }),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let detail = fx.service.get_detail("u1", Some("en-US")).await.unwrap();
        assert_eq!(detail.prompts.recommended, vec!["Start here"]);
        assert_eq!(detail.defaults.model.mode, "auto");
        assert_eq!(detail.defaults.model.value, None);
        assert_eq!(detail.defaults.permission.mode, "fixed");
        assert_eq!(detail.defaults.permission.value.as_deref(), Some("strict"));
        assert_eq!(detail.defaults.skills.mode, "fixed");
        assert_eq!(detail.defaults.skills.value, vec!["skill-z"]);
        assert_eq!(detail.defaults.mcps.mode, "auto");
        assert!(detail.defaults.mcps.value.is_empty());
    }

    #[tokio::test]
    async fn update_switching_defaults_to_fixed_seeds_preferences() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                ..req_default()
            })
            .await
            .unwrap();

        fx.service
            .update(
                "u1",
                UpdateAssistantRequest {
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("openai/gpt-5".into()),
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "fixed".into(),
                            value: Some("strict".into()),
                        }),
                        skills: Some(AssistantDefaultListRequest {
                            mode: "fixed".into(),
                            value: vec!["skill-z".into()],
                        }),
                        mcps: Some(AssistantDefaultListRequest {
                            mode: "fixed".into(),
                            value: vec!["mcp-z".into()],
                        }),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let definition = fx.definition_repo.get_by_assistant_id("u1").await.unwrap().unwrap();
        let pref = fx.preference_repo.get(&definition.id).await.unwrap().unwrap();
        assert_eq!(pref.last_model_id.as_deref(), Some("openai/gpt-5"));
        assert_eq!(pref.last_permission_value.as_deref(), Some("strict"));
        assert_eq!(pref.last_skill_ids, r#"["skill-z"]"#);
        assert_eq!(pref.last_mcp_ids, r#"["mcp-z"]"#);
    }

    #[tokio::test]
    async fn update_switching_defaults_from_fixed_to_auto_clears_preferences() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Planner".into(),
                defaults: Some(AssistantDefaultsRequest {
                    model: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("openai/gpt-5".into()),
                    }),
                    permission: Some(AssistantDefaultScalarRequest {
                        mode: "fixed".into(),
                        value: Some("strict".into()),
                    }),
                    skills: Some(AssistantDefaultListRequest {
                        mode: "fixed".into(),
                        value: vec!["skill-z".into()],
                    }),
                    mcps: Some(AssistantDefaultListRequest {
                        mode: "fixed".into(),
                        value: vec!["mcp-z".into()],
                    }),
                }),
                ..req_default()
            })
            .await
            .unwrap();

        fx.service
            .update(
                "u1",
                UpdateAssistantRequest {
                    defaults: Some(AssistantDefaultsRequest {
                        model: Some(AssistantDefaultScalarRequest {
                            mode: "auto".into(),
                            value: None,
                        }),
                        permission: Some(AssistantDefaultScalarRequest {
                            mode: "auto".into(),
                            value: None,
                        }),
                        skills: Some(AssistantDefaultListRequest {
                            mode: "auto".into(),
                            value: vec![],
                        }),
                        mcps: Some(AssistantDefaultListRequest {
                            mode: "auto".into(),
                            value: vec![],
                        }),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let definition = fx.definition_repo.get_by_assistant_id("u1").await.unwrap().unwrap();
        assert!(fx.preference_repo.get(&definition.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_user_removes_row_and_override() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        fx.service
            .set_state(
                "u1",
                SetAssistantStateRequest {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        fx.service.delete("u1").await.unwrap();
        // list now empty
        let list = fx.service.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_builtin_rejects() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx.service.delete("builtin-office").await.unwrap_err();
        assert!(matches!(err, AssistantError::Forbidden(_)));
    }

    #[tokio::test]
    async fn set_state_builtin_writes_override() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let resp = fx
            .service
            .set_state(
                "builtin-office",
                SetAssistantStateRequest {
                    enabled: Some(false),
                    sort_order: Some(7),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!resp.enabled);
        assert_eq!(resp.sort_order, 7);
        assert_eq!(resp.source, AssistantSource::Builtin);
    }

    #[tokio::test]
    async fn set_state_user_404_when_missing() {
        let fx = fixture().await;
        let err = fx
            .service
            .set_state(
                "unknown",
                SetAssistantStateRequest {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::NotFound(_)));
    }

    #[tokio::test]
    async fn import_happy_path() {
        let fx = fixture().await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![
                    CreateAssistantRequest {
                        id: Some("u1".into()),
                        name: "A".into(),
                        ..req_default()
                    },
                    CreateAssistantRequest {
                        id: Some("u2".into()),
                        name: "B".into(),
                        ..req_default()
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 2);
        assert_eq!(res.skipped, 0);
        assert_eq!(res.failed, 0);
    }

    #[tokio::test]
    async fn import_skips_builtin_collision() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("builtin-office".into()),
                    name: "spoof".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 0);
        assert_eq!(res.skipped, 1);
    }

    #[tokio::test]
    async fn import_retry_is_idempotent() {
        let fx = fixture().await;
        let first = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "A".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(first.imported, 1);

        let second = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "A".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);
    }

    #[tokio::test]
    async fn import_fails_on_empty_name() {
        let fx = fixture().await;
        let res = fx
            .service
            .import(ImportAssistantsRequest {
                assistants: vec![CreateAssistantRequest {
                    id: Some("u1".into()),
                    name: "  ".into(),
                    ..req_default()
                }],
            })
            .await
            .unwrap();
        assert_eq!(res.imported, 0);
        assert_eq!(res.failed, 1);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].id, "u1");
    }

    #[tokio::test]
    async fn read_rule_user_returns_empty_when_missing() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        let content = fx.service.read_rule("u1", Some("en-US")).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn write_rule_user_then_read_returns_same() {
        let fx = fixture().await;
        fx.service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "A".into(),
                ..req_default()
            })
            .await
            .unwrap();
        fx.service.write_rule("u1", Some("en-US"), "rule body").await.unwrap();
        let content = fx.service.read_rule("u1", Some("en-US")).await.unwrap();
        assert_eq!(content, "rule body");
    }

    #[tokio::test]
    async fn write_rule_builtin_rejects() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        let err = fx
            .service
            .write_rule("builtin-office", Some("en-US"), "x")
            .await
            .unwrap_err();
        assert!(matches!(err, AssistantError::BadRequest(_)));
    }

    #[tokio::test]
    async fn write_rule_generated_then_read_returns_same() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;
        fx.service
            .write_rule("bare:agent-claude", Some("en-US"), "rule body")
            .await
            .unwrap();
        let content = fx.service.read_rule("bare:agent-claude", Some("en-US")).await.unwrap();
        assert_eq!(content, "rule body");
    }

    #[tokio::test]
    async fn delete_rule_generated_removes_local_rule() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;
        fx.service
            .write_rule("bare:agent-claude", Some("en-US"), "rule body")
            .await
            .unwrap();
        assert!(fx.service.delete_rule("bare:agent-claude").await.unwrap());
        let content = fx.service.read_rule("bare:agent-claude", Some("en-US")).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn write_skill_generated_then_read_returns_same() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;
        fx.service
            .write_skill("bare:agent-claude", Some("en-US"), "skill body")
            .await
            .unwrap();
        let content = fx.service.read_skill("bare:agent-claude", Some("en-US")).await.unwrap();
        assert_eq!(content, "skill body");
    }

    #[tokio::test]
    async fn delete_skill_generated_removes_local_skill() {
        let fx = fixture_with_options(FixtureOpts {
            agent_rows: vec![mk_agent_row(
                "agent-claude",
                "claude",
                aionui_api_types::AgentManagementStatus::Online,
            )],
            ..Default::default()
        })
        .await;
        fx.service
            .write_skill("bare:agent-claude", Some("en-US"), "skill body")
            .await
            .unwrap();
        assert!(fx.service.delete_skill("bare:agent-claude").await.unwrap());
        let content = fx.service.read_skill("bare:agent-claude", Some("en-US")).await.unwrap();
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn read_rule_builtin_dispatches_to_manifest() {
        let tmp = TempDir::new().unwrap();
        let db = init_database_memory().await.unwrap();

        let assets_dir = tmp.path().join("assets");
        let rules_dir = assets_dir.join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("office.en-US.md"), "office rules").unwrap();
        let manifest = serde_json::json!({
            "assistants": [{
                "id": "builtin-office",
                "name": "Office",
                "agent_ref": "gemini",
                "rule_file": "rules/office.{locale}.md",
            }]
        });
        std::fs::write(
            assets_dir.join("assistants.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let builtin_reg = Arc::new(BuiltinAssistantRegistry::load_from_dir(assets_dir));

        let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
            Arc::new(SqliteAssistantDefinitionRepository::new(db.pool().clone()));
        let state_repo: Arc<dyn IAssistantOverlayRepository> =
            Arc::new(SqliteAssistantOverlayRepository::new(db.pool().clone()));
        let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
            Arc::new(SqliteAssistantPreferenceRepository::new(db.pool().clone()));
        let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(db.pool().clone()));
        let orepo: Arc<dyn IAssistantOverrideRepository> =
            Arc::new(SqliteAssistantOverrideRepository::new(db.pool().clone()));
        let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let service = AssistantService::new(
            db.pool().clone(),
            AssistantServiceDeps {
                definition_repo,
                state_repo,
                preference_repo,
                repo,
                override_repo: orepo,
                provider_repo,
                builtin: builtin_reg,
                agent_catalog: None,
            },
            tmp.path().to_path_buf(),
        );
        let content = service.read_rule("builtin-office", Some("en-US")).await.unwrap();
        assert_eq!(content, "office rules");
    }

    #[tokio::test]
    async fn classify_falls_back_to_user() {
        let fx = fixture().await;
        assert_eq!(fx.service.classify_source("ghost").await, AssistantSource::User);
    }

    #[tokio::test]
    async fn classify_builtin_wins() {
        let fx = fixture_with_builtins(vec![mk_builtin("builtin-office", "Office")]).await;
        assert_eq!(
            fx.service.classify_source("builtin-office").await,
            AssistantSource::Builtin
        );
    }

    // -----------------------------------------------------------------------
    // Default agent inference (ELECTRON-1J1 / 1KV regression coverage)
    // -----------------------------------------------------------------------

    /// Anthropic provider routes to AionRS, not the Claude Code CLI:
    /// having an Anthropic API key does not imply the user has
    /// `claude` on `PATH`. CLI-based agents must be opted into
    /// explicitly.
    #[tokio::test]
    async fn resolve_default_agent_id_routes_anthropic_provider_to_aionrs() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("anthropic"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_id().await.unwrap();
        assert_eq!(resolved, "632f31d2");
    }

    /// OpenAI / custom provider falls back to AionRS, the only AionUI
    /// agent that doesn't require a third-party CLI.
    #[tokio::test]
    async fn resolve_default_agent_id_falls_back_to_aionrs_for_openai_provider() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("openai"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_id().await.unwrap();
        assert_eq!(resolved, "632f31d2");
    }

    /// Custom (non-anthropic, non-openai) platform also routes to AionRS,
    /// which handles OpenAI-compatible custom URLs.
    #[tokio::test]
    async fn resolve_default_agent_id_handles_custom_platform_as_aionrs() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("custom"),
            ..Default::default()
        })
        .await;
        let resolved = fx.service.resolve_default_agent_id().await.unwrap();
        assert_eq!(resolved, "632f31d2");
    }

    /// No providers → loud BadRequest with actionable text. Crucially,
    /// this no longer silently falls through to `"gemini"`.
    #[tokio::test]
    async fn resolve_default_agent_id_errors_when_no_providers() {
        let fx = fixture_with_options(FixtureOpts {
            no_default_provider: true,
            ..Default::default()
        })
        .await;
        let err = fx.service.resolve_default_agent_id().await.unwrap_err();
        match err {
            AssistantError::BadRequest(msg) => {
                assert!(
                    msg.to_lowercase().contains("no providers"),
                    "unexpected error message: {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("gemini"),
                    "error message must not mention gemini: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    /// Disabled providers do not satisfy the inference; the resolver
    /// must treat them as if they were absent.
    #[tokio::test]
    async fn resolve_default_agent_id_ignores_disabled_providers() {
        let fx = fixture_with_options(FixtureOpts {
            no_default_provider: true,
            ..Default::default()
        })
        .await;

        // Seed a *disabled* provider directly via the repo; resolution
        // must still error out because no enabled provider exists.
        fx.provider_repo
            .create(CreateProviderParams {
                id: None,
                platform: "anthropic",
                name: "Disabled",
                base_url: "https://example.invalid",
                api_key_encrypted: "stub",
                models: "[]",
                enabled: false,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();

        let err = fx.service.resolve_default_agent_id().await.unwrap_err();
        assert!(matches!(err, AssistantError::BadRequest(_)));
    }

    /// End-to-end regression for ELECTRON-1J1 / 1KV: creating an
    /// assistant with no `agent_id` and no Gemini CLI installed
    /// must NOT default to `"gemini"`. Any enabled provider — Anthropic
    /// or otherwise — should resolve to `"aionrs"`, the only built-in
    /// agent that doesn't depend on a third-party CLI being on `PATH`.
    #[tokio::test]
    async fn create_without_agent_id_does_not_default_to_gemini_when_provider_exists() {
        for platform in ["anthropic", "openai"] {
            let fx = fixture_with_options(FixtureOpts {
                seed_platform: Some(platform),
                ..Default::default()
            })
            .await;
            let created = fx
                .service
                .create(CreateAssistantRequest {
                    id: Some(format!("u-{platform}")),
                    name: "Mine".into(),
                    ..req_default()
                })
                .await
                .unwrap();
            assert_ne!(
                created.agent_id, "gemini",
                "Gemini default would 400 within 1ms on machines without the CLI"
            );
            assert_eq!(
                created.agent_id, "632f31d2",
                "{platform} provider should resolve to aionrs"
            );
        }
    }

    /// Explicit `agent_id` in the request body wins over the inferred default.
    #[tokio::test]
    async fn create_respects_explicit_agent_id() {
        let fx = fixture_with_options(FixtureOpts {
            seed_platform: Some("anthropic"),
            ..Default::default()
        })
        .await;
        let created = fx
            .service
            .create(CreateAssistantRequest {
                id: Some("u1".into()),
                name: "Mine".into(),
                agent_id: Some("8e1acf31".into()),
                ..req_default()
            })
            .await
            .unwrap();
        assert_eq!(created.agent_id, "8e1acf31");
    }

    fn req_default() -> CreateAssistantRequest {
        CreateAssistantRequest {
            id: None,
            name: String::new(),
            description: None,
            avatar: None,
            agent_id: None,
            enabled_skills: None,
            custom_skill_names: None,
            disabled_builtin_skills: None,
            prompts: None,
            models: None,
            name_i18n: None,
            description_i18n: None,
            prompts_i18n: None,
            recommended_prompts: None,
            recommended_prompts_i18n: None,
            defaults: None,
        }
    }
}
