//! SQLite-backed assistant repositories.

use aionui_common::{TimestampMs, now_ms};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow, AssistantRow,
    CreateAssistantParams, UpdateAssistantParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertOverrideParams,
};
use crate::repository::assistant::{
    IAssistantDefinitionRepository, IAssistantOverlayRepository, IAssistantOverrideRepository,
    IAssistantPreferenceRepository, IAssistantRepository,
};

/// SQLite-backed implementation of [`IAssistantRepository`].
#[derive(Clone, Debug)]
pub struct SqliteAssistantRepository {
    pool: SqlitePool,
}

impl SqliteAssistantRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

#[async_trait::async_trait]
impl IAssistantRepository for SqliteAssistantRepository {
    async fn list(&self) -> Result<Vec<AssistantRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantRow>("SELECT * FROM assistants ORDER BY updated_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<AssistantRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantRow>("SELECT * FROM assistants WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn create(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO assistants \
                (id, name, description, avatar, preset_agent_type, enabled_skills, \
                 custom_skill_names, disabled_builtin_skills, prompts, models, \
                 name_i18n, description_i18n, prompts_i18n, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.avatar)
        .bind(params.preset_agent_type)
        .bind(params.enabled_skills)
        .bind(params.custom_skill_names)
        .bind(params.disabled_builtin_skills)
        .bind(params.prompts)
        .bind(params.models)
        .bind(params.name_i18n)
        .bind(params.description_i18n)
        .bind(params.prompts_i18n)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Assistant with id '{}' already exists", params.id))
            }
            _ => DbError::Query(e),
        })?;

        Ok(AssistantRow {
            id: params.id.to_string(),
            name: params.name.to_string(),
            description: params.description.map(String::from),
            avatar: params.avatar.map(String::from),
            preset_agent_type: params.preset_agent_type.to_string(),
            enabled_skills: params.enabled_skills.map(String::from),
            custom_skill_names: params.custom_skill_names.map(String::from),
            disabled_builtin_skills: params.disabled_builtin_skills.map(String::from),
            prompts: params.prompts.map(String::from),
            models: params.models.map(String::from),
            name_i18n: params.name_i18n.map(String::from),
            description_i18n: params.description_i18n.map(String::from),
            prompts_i18n: params.prompts_i18n.map(String::from),
            created_at: now,
            updated_at: now,
        })
    }

    async fn update(&self, id: &str, params: &UpdateAssistantParams<'_>) -> Result<Option<AssistantRow>, DbError> {
        let Some(existing) = self.get(id).await? else {
            return Ok(None);
        };

        let merged = merge_update(existing, params);

        sqlx::query(
            "UPDATE assistants SET \
                name = ?, description = ?, avatar = ?, preset_agent_type = ?, \
                enabled_skills = ?, custom_skill_names = ?, disabled_builtin_skills = ?, \
                prompts = ?, models = ?, name_i18n = ?, description_i18n = ?, \
                prompts_i18n = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(&merged.name)
        .bind(&merged.description)
        .bind(&merged.avatar)
        .bind(&merged.preset_agent_type)
        .bind(&merged.enabled_skills)
        .bind(&merged.custom_skill_names)
        .bind(&merged.disabled_builtin_skills)
        .bind(&merged.prompts)
        .bind(&merged.models)
        .bind(&merged.name_i18n)
        .bind(&merged.description_i18n)
        .bind(&merged.prompts_i18n)
        .bind(merged.updated_at)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(Some(merged))
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistants WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO assistants \
                (id, name, description, avatar, preset_agent_type, enabled_skills, \
                 custom_skill_names, disabled_builtin_skills, prompts, models, \
                 name_i18n, description_i18n, prompts_i18n, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, \
                description = excluded.description, \
                avatar = excluded.avatar, \
                preset_agent_type = excluded.preset_agent_type, \
                enabled_skills = excluded.enabled_skills, \
                custom_skill_names = excluded.custom_skill_names, \
                disabled_builtin_skills = excluded.disabled_builtin_skills, \
                prompts = excluded.prompts, \
                models = excluded.models, \
                name_i18n = excluded.name_i18n, \
                description_i18n = excluded.description_i18n, \
                prompts_i18n = excluded.prompts_i18n, \
                updated_at = excluded.updated_at",
        )
        .bind(params.id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.avatar)
        .bind(params.preset_agent_type)
        .bind(params.enabled_skills)
        .bind(params.custom_skill_names)
        .bind(params.disabled_builtin_skills)
        .bind(params.prompts)
        .bind(params.models)
        .bind(params.name_i18n)
        .bind(params.description_i18n)
        .bind(params.prompts_i18n)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = self
            .get(params.id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert did not produce row for id '{}'", params.id)))?;
        Ok(row)
    }
}

fn merge_update(existing: AssistantRow, params: &UpdateAssistantParams<'_>) -> AssistantRow {
    let now = now_ms();
    AssistantRow {
        id: existing.id,
        name: params.name.map(String::from).unwrap_or(existing.name),
        description: params.description.map_or(existing.description, |v| v.map(String::from)),
        avatar: params.avatar.map_or(existing.avatar, |v| v.map(String::from)),
        preset_agent_type: params
            .preset_agent_type
            .map(String::from)
            .unwrap_or(existing.preset_agent_type),
        enabled_skills: params
            .enabled_skills
            .map_or(existing.enabled_skills, |v| v.map(String::from)),
        custom_skill_names: params
            .custom_skill_names
            .map_or(existing.custom_skill_names, |v| v.map(String::from)),
        disabled_builtin_skills: params
            .disabled_builtin_skills
            .map_or(existing.disabled_builtin_skills, |v| v.map(String::from)),
        prompts: params.prompts.map_or(existing.prompts, |v| v.map(String::from)),
        models: params.models.map_or(existing.models, |v| v.map(String::from)),
        name_i18n: params.name_i18n.map_or(existing.name_i18n, |v| v.map(String::from)),
        description_i18n: params
            .description_i18n
            .map_or(existing.description_i18n, |v| v.map(String::from)),
        prompts_i18n: params
            .prompts_i18n
            .map_or(existing.prompts_i18n, |v| v.map(String::from)),
        created_at: existing.created_at,
        updated_at: now,
    }
}

/// SQLite-backed implementation of [`IAssistantOverrideRepository`].
#[derive(Clone, Debug)]
pub struct SqliteAssistantOverrideRepository {
    pool: SqlitePool,
}

/// SQLite-backed implementation of [`IAssistantDefinitionRepository`].
#[derive(Clone, Debug)]
pub struct SqliteAssistantDefinitionRepository {
    pool: SqlitePool,
}

impl SqliteAssistantDefinitionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// SQLite-backed implementation of [`IAssistantOverlayRepository`].
#[derive(Clone, Debug)]
pub struct SqliteAssistantOverlayRepository {
    pool: SqlitePool,
}

impl SqliteAssistantOverlayRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// SQLite-backed implementation of [`IAssistantPreferenceRepository`].
#[derive(Clone, Debug)]
pub struct SqliteAssistantPreferenceRepository {
    pool: SqlitePool,
}

impl SqliteAssistantPreferenceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl SqliteAssistantOverrideRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IAssistantOverrideRepository for SqliteAssistantOverrideRepository {
    async fn get(&self, assistant_id: &str) -> Result<Option<AssistantOverrideRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantOverrideRow>("SELECT * FROM assistant_overrides WHERE assistant_id = ?")
            .bind(assistant_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn get_all(&self) -> Result<Vec<AssistantOverrideRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantOverrideRow>("SELECT * FROM assistant_overrides")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertOverrideParams<'_>) -> Result<AssistantOverrideRow, DbError> {
        let now = now_ms();
        let last_used_at: Option<TimestampMs> = params.last_used_at;

        // `preset_agent_type` has three-way semantics in the params struct
        // (see `UpsertOverrideParams`). At the SQL layer we flatten it into a
        // `(write?, value)` pair: on CONFLICT, if the caller did not specify
        // a new value, `COALESCE(new_flag, 0)` keeps the existing column.
        let (pat_write, pat_value): (bool, Option<&str>) = match params.preset_agent_type {
            Some(v) => (true, v),
            None => (false, None),
        };

        sqlx::query(
            "INSERT INTO assistant_overrides \
                (assistant_id, enabled, sort_order, last_used_at, preset_agent_type, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(assistant_id) DO UPDATE SET \
                enabled = excluded.enabled, \
                sort_order = excluded.sort_order, \
                last_used_at = COALESCE(excluded.last_used_at, assistant_overrides.last_used_at), \
                preset_agent_type = CASE WHEN ? THEN ? ELSE assistant_overrides.preset_agent_type END, \
                updated_at = excluded.updated_at",
        )
        .bind(params.assistant_id)
        .bind(params.enabled)
        .bind(params.sort_order)
        .bind(last_used_at)
        .bind(pat_value)
        .bind(now)
        .bind(pat_write)
        .bind(pat_value)
        .execute(&self.pool)
        .await?;

        let row = self.get(params.assistant_id).await?.ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce override row for id '{}'",
                params.assistant_id
            ))
        })?;
        Ok(row)
    }

    async fn delete(&self, assistant_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_overrides WHERE assistant_id = ?")
            .bind(assistant_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_orphans(&self, valid_ids: &[&str]) -> Result<u64, DbError> {
        if valid_ids.is_empty() {
            let result = sqlx::query("DELETE FROM assistant_overrides")
                .execute(&self.pool)
                .await?;
            return Ok(result.rows_affected());
        }

        let placeholders = std::iter::repeat_n("?", valid_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM assistant_overrides WHERE assistant_id NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in valid_ids {
            q = q.bind(*id);
        }
        let result = q.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }
}

#[async_trait::async_trait]
impl IAssistantDefinitionRepository for SqliteAssistantDefinitionRepository {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions WHERE deleted_at IS NULL ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_by_key(&self, assistant_key: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions WHERE assistant_key = ? AND deleted_at IS NULL",
        )
        .bind(assistant_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_definition_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions WHERE definition_id = ? AND deleted_at IS NULL",
        )
        .bind(definition_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_source_ref(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions WHERE source = ? AND source_ref = ? AND deleted_at IS NULL",
        )
        .bind(source)
        .bind(source_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO assistant_definitions (
                definition_id, assistant_key, source, owner_type, source_ref, source_version, source_hash,
                name, name_i18n, description, description_i18n, avatar_type, avatar_value,
                agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
                recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_model_value,
                default_permission_mode, default_permission_value,
                default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
                default_mcps_mode, default_mcp_ids,
                created_at, updated_at, deleted_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
            ON CONFLICT(definition_id) DO UPDATE SET
                assistant_key = excluded.assistant_key,
                source = excluded.source,
                owner_type = excluded.owner_type,
                source_ref = excluded.source_ref,
                source_version = excluded.source_version,
                source_hash = excluded.source_hash,
                name = excluded.name,
                name_i18n = excluded.name_i18n,
                description = excluded.description,
                description_i18n = excluded.description_i18n,
                avatar_type = excluded.avatar_type,
                avatar_value = excluded.avatar_value,
                agent_backend = excluded.agent_backend,
                rule_resource_type = excluded.rule_resource_type,
                rule_resource_ref = excluded.rule_resource_ref,
                rule_inline_content = excluded.rule_inline_content,
                recommended_prompts = excluded.recommended_prompts,
                recommended_prompts_i18n = excluded.recommended_prompts_i18n,
                default_model_mode = excluded.default_model_mode,
                default_model_value = excluded.default_model_value,
                default_permission_mode = excluded.default_permission_mode,
                default_permission_value = excluded.default_permission_value,
                default_skills_mode = excluded.default_skills_mode,
                default_skill_ids = excluded.default_skill_ids,
                custom_skill_names = excluded.custom_skill_names,
                default_disabled_builtin_skill_ids = excluded.default_disabled_builtin_skill_ids,
                default_mcps_mode = excluded.default_mcps_mode,
                default_mcp_ids = excluded.default_mcp_ids,
                updated_at = excluded.updated_at,
                deleted_at = NULL",
        )
        .bind(params.definition_id)
        .bind(params.assistant_key)
        .bind(params.source)
        .bind(params.owner_type)
        .bind(params.source_ref)
        .bind(params.source_version)
        .bind(params.source_hash)
        .bind(params.name)
        .bind(params.name_i18n)
        .bind(params.description)
        .bind(params.description_i18n)
        .bind(params.avatar_type)
        .bind(params.avatar_value)
        .bind(params.agent_backend)
        .bind(params.rule_resource_type)
        .bind(params.rule_resource_ref)
        .bind(params.rule_inline_content)
        .bind(params.recommended_prompts)
        .bind(params.recommended_prompts_i18n)
        .bind(params.default_model_mode)
        .bind(params.default_model_value)
        .bind(params.default_permission_mode)
        .bind(params.default_permission_value)
        .bind(params.default_skills_mode)
        .bind(params.default_skill_ids)
        .bind(params.custom_skill_names)
        .bind(params.default_disabled_builtin_skill_ids)
        .bind(params.default_mcps_mode)
        .bind(params.default_mcp_ids)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get_by_definition_id(params.definition_id).await?.ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce definition row for definition_id '{}'",
                params.definition_id
            ))
        })
    }

    async fn soft_delete(&self, definition_id: &str, deleted_at: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE assistant_definitions
             SET deleted_at = ?, updated_at = ?
             WHERE definition_id = ? AND deleted_at IS NULL",
        )
        .bind(deleted_at)
        .bind(now_ms())
        .bind(definition_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl IAssistantOverlayRepository for SqliteAssistantOverlayRepository {
    async fn get(&self, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantOverlayRow>("SELECT * FROM assistant_overlays WHERE definition_id = ?")
            .bind(definition_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantOverlayRow>(
            "SELECT * FROM assistant_overlays ORDER BY sort_order, updated_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_overlays (
                definition_id, enabled, sort_order, agent_backend_override, last_used_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(definition_id) DO UPDATE SET
                enabled = excluded.enabled,
                sort_order = excluded.sort_order,
                agent_backend_override = excluded.agent_backend_override,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at",
        )
        .bind(params.definition_id)
        .bind(params.enabled)
        .bind(params.sort_order)
        .bind(params.agent_backend_override)
        .bind(params.last_used_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.definition_id).await?.ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce state row for definition_id '{}'",
                params.definition_id
            ))
        })
    }

    async fn delete(&self, definition_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_overlays WHERE definition_id = ?")
            .bind(definition_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl IAssistantPreferenceRepository for SqliteAssistantPreferenceRepository {
    async fn get(&self, definition_id: &str) -> Result<Option<AssistantPreferenceRow>, DbError> {
        let row =
            sqlx::query_as::<_, AssistantPreferenceRow>("SELECT * FROM assistant_preferences WHERE definition_id = ?")
                .bind(definition_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertAssistantPreferenceParams<'_>) -> Result<AssistantPreferenceRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_preferences (
                definition_id, last_model_id, last_permission_value, last_skill_ids,
                last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(definition_id) DO UPDATE SET
                last_model_id = excluded.last_model_id,
                last_permission_value = excluded.last_permission_value,
                last_skill_ids = excluded.last_skill_ids,
                last_disabled_builtin_skill_ids = excluded.last_disabled_builtin_skill_ids,
                last_mcp_ids = excluded.last_mcp_ids,
                updated_at = excluded.updated_at",
        )
        .bind(params.definition_id)
        .bind(params.last_model_id)
        .bind(params.last_permission_value)
        .bind(params.last_skill_ids)
        .bind(params.last_disabled_builtin_skill_ids)
        .bind(params.last_mcp_ids)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.definition_id).await?.ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce preference row for definition_id '{}'",
                params.definition_id
            ))
        })
    }

    async fn delete(&self, definition_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_preferences WHERE definition_id = ?")
            .bind(definition_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

pub async fn rebuild_legacy_assistant_mirror(
    pool: &SqlitePool,
    definition: &AssistantDefinitionRow,
    state: Option<&AssistantOverlayRow>,
) -> Result<(), DbError> {
    let prompts = normalize_json_array(Some(definition.recommended_prompts.as_str()));
    let default_skills = normalize_json_array(Some(definition.default_skill_ids.as_str()));
    let custom_skill_names = normalize_json_array(Some(definition.custom_skill_names.as_str()));
    let disabled_builtin = normalize_json_array(Some(definition.default_disabled_builtin_skill_ids.as_str()));
    let models = match (
        definition.default_model_mode.as_str(),
        definition.default_model_value.as_deref(),
    ) {
        ("fixed", Some(model)) => serde_json::to_string(&vec![model]).unwrap_or_else(|_| "[]".to_string()),
        _ => "[]".to_string(),
    };

    if definition.source == "user" {
        sqlx::query(
            "INSERT INTO assistants (
                id, name, description, avatar, preset_agent_type, enabled_skills,
                custom_skill_names, disabled_builtin_skills, prompts, models,
                name_i18n, description_i18n, prompts_i18n, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                avatar = excluded.avatar,
                preset_agent_type = excluded.preset_agent_type,
                enabled_skills = excluded.enabled_skills,
                custom_skill_names = excluded.custom_skill_names,
                disabled_builtin_skills = excluded.disabled_builtin_skills,
                prompts = excluded.prompts,
                models = excluded.models,
                name_i18n = excluded.name_i18n,
                description_i18n = excluded.description_i18n,
                prompts_i18n = excluded.prompts_i18n,
                updated_at = excluded.updated_at",
        )
        .bind(&definition.assistant_key)
        .bind(&definition.name)
        .bind(&definition.description)
        .bind(&definition.avatar_value)
        .bind(&definition.agent_backend)
        .bind(&default_skills)
        .bind(&custom_skill_names)
        .bind(&disabled_builtin)
        .bind(&prompts)
        .bind(&models)
        .bind(&definition.name_i18n)
        .bind(&definition.description_i18n)
        .bind(&definition.recommended_prompts_i18n)
        .bind(definition.created_at)
        .bind(definition.updated_at)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM assistants WHERE id = ?")
            .bind(&definition.assistant_key)
            .execute(pool)
            .await?;
    }

    let enabled = state.map(|row| row.enabled).unwrap_or(true);
    let sort_order = state.map(|row| row.sort_order).unwrap_or_default();
    let agent_backend_override = state.and_then(|row| row.agent_backend_override.clone());
    let last_used_at = state.and_then(|row| row.last_used_at);

    sqlx::query(
        "INSERT INTO assistant_overrides (assistant_id, enabled, sort_order, preset_agent_type, last_used_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(assistant_id) DO UPDATE SET
            enabled = excluded.enabled,
            sort_order = excluded.sort_order,
            preset_agent_type = excluded.preset_agent_type,
            last_used_at = excluded.last_used_at,
            updated_at = excluded.updated_at",
    )
    .bind(&definition.assistant_key)
    .bind(enabled)
    .bind(sort_order)
    .bind(agent_backend_override)
    .bind(last_used_at)
    .bind(state.map(|row| row.updated_at).unwrap_or(definition.updated_at))
    .execute(pool)
    .await?;

    Ok(())
}

fn normalize_json_array(raw: Option<&str>) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => "[]".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (
        SqliteAssistantRepository,
        SqliteAssistantOverrideRepository,
        crate::Database,
    ) {
        let db = init_database_memory().await.unwrap();
        let a = SqliteAssistantRepository::new(db.pool().clone());
        let o = SqliteAssistantOverrideRepository::new(db.pool().clone());
        (a, o, db)
    }

    async fn setup_v2() -> (
        SqliteAssistantDefinitionRepository,
        SqliteAssistantOverlayRepository,
        SqliteAssistantPreferenceRepository,
        crate::Database,
    ) {
        let db = init_database_memory().await.unwrap();
        let d = SqliteAssistantDefinitionRepository::new(db.pool().clone());
        let s = SqliteAssistantOverlayRepository::new(db.pool().clone());
        let p = SqliteAssistantPreferenceRepository::new(db.pool().clone());
        (d, s, p, db)
    }

    fn params<'a>(id: &'a str, name: &'a str) -> CreateAssistantParams<'a> {
        CreateAssistantParams {
            id,
            name,
            description: Some("desc"),
            avatar: None,
            preset_agent_type: "gemini",
            enabled_skills: Some(r#"["skill-a"]"#),
            custom_skill_names: None,
            disabled_builtin_skills: None,
            prompts: Some(r#"["hello"]"#),
            models: None,
            name_i18n: Some(r#"{"zh-CN":"助手"}"#),
            description_i18n: None,
            prompts_i18n: None,
        }
    }

    fn definition_params<'a>(id: &'a str, name: &'a str) -> UpsertAssistantDefinitionParams<'a> {
        UpsertAssistantDefinitionParams {
            definition_id: "asstdef_u1",
            assistant_key: id,
            source: "user",
            owner_type: "user",
            source_ref: Some(id),
            source_version: None,
            source_hash: None,
            name,
            name_i18n: r#"{"zh-CN":"助手"}"#,
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_backend: "gemini",
            rule_resource_type: "inline",
            rule_resource_ref: None,
            rule_inline_content: Some("# rule"),
            recommended_prompts: r#"["hello"]"#,
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "fixed",
            default_permission_value: Some("workspace-write"),
            default_skills_mode: "fixed",
            default_skill_ids: r#"["pdf","cron"]"#,
            custom_skill_names: r#"["my-custom-skill"]"#,
            default_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        }
    }

    #[tokio::test]
    async fn assistant_list_empty() {
        let (a, _o, _db) = setup().await;
        assert!(a.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn assistant_create_then_get() {
        let (a, _o, _db) = setup().await;
        let row = a.create(&params("u1", "User One")).await.unwrap();
        assert_eq!(row.id, "u1");
        assert_eq!(row.name, "User One");
        assert_eq!(row.preset_agent_type, "gemini");
        assert_eq!(row.enabled_skills.as_deref(), Some(r#"["skill-a"]"#));
        assert!(row.created_at > 0);
        assert_eq!(row.created_at, row.updated_at);

        let fetched = a.get("u1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "User One");
    }

    #[tokio::test]
    async fn assistant_create_duplicate_id_returns_conflict() {
        let (a, _o, _db) = setup().await;
        a.create(&params("u1", "A")).await.unwrap();
        let err = a.create(&params("u1", "B")).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn assistant_get_missing_returns_none() {
        let (a, _o, _db) = setup().await;
        assert!(a.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn assistant_list_orders_by_updated_at_desc() {
        let (a, _o, _db) = setup().await;
        a.create(&params("u1", "first")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        a.create(&params("u2", "second")).await.unwrap();

        let list = a.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "u2");
        assert_eq!(list[1].id, "u1");
    }

    #[tokio::test]
    async fn assistant_update_partial_keeps_other_fields() {
        let (a, _o, _db) = setup().await;
        a.create(&params("u1", "original")).await.unwrap();

        let upd = UpdateAssistantParams {
            name: Some("renamed"),
            ..Default::default()
        };
        let updated = a.update("u1", &upd).await.unwrap().unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.preset_agent_type, "gemini");
        assert_eq!(updated.description.as_deref(), Some("desc"));
        assert_eq!(updated.enabled_skills.as_deref(), Some(r#"["skill-a"]"#));
        assert!(updated.updated_at >= updated.created_at);
    }

    #[tokio::test]
    async fn assistant_update_clears_nullable_with_some_none() {
        let (a, _o, _db) = setup().await;
        a.create(&params("u1", "has-desc")).await.unwrap();

        let upd = UpdateAssistantParams {
            description: Some(None),
            ..Default::default()
        };
        let updated = a.update("u1", &upd).await.unwrap().unwrap();
        assert!(updated.description.is_none());
    }

    #[tokio::test]
    async fn assistant_update_nonexistent_returns_none() {
        let (a, _o, _db) = setup().await;
        let res = a
            .update(
                "nope",
                &UpdateAssistantParams {
                    name: Some("x"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn assistant_delete_existing_returns_true() {
        let (a, _o, _db) = setup().await;
        a.create(&params("u1", "x")).await.unwrap();
        assert!(a.delete("u1").await.unwrap());
        assert!(a.get("u1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn assistant_delete_missing_returns_false() {
        let (a, _o, _db) = setup().await;
        assert!(!a.delete("nope").await.unwrap());
    }

    #[tokio::test]
    async fn assistant_upsert_inserts_then_updates() {
        let (a, _o, _db) = setup().await;
        let first = a.upsert(&params("u1", "first")).await.unwrap();
        assert_eq!(first.name, "first");

        let mut p = params("u1", "second");
        p.preset_agent_type = "claude";
        let second = a.upsert(&p).await.unwrap();
        assert_eq!(second.name, "second");
        assert_eq!(second.preset_agent_type, "claude");

        let list = a.list().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn override_get_missing_returns_none() {
        let (_a, o, _db) = setup().await;
        assert!(o.get("u1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn override_upsert_inserts_row() {
        let (_a, o, _db) = setup().await;
        let row = o
            .upsert(&UpsertOverrideParams {
                assistant_id: "u1",
                enabled: false,
                sort_order: 5,
                last_used_at: Some(1000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(row.assistant_id, "u1");
        assert!(!row.enabled);
        assert_eq!(row.sort_order, 5);
        assert_eq!(row.last_used_at, Some(1000));
    }

    #[tokio::test]
    async fn override_upsert_updates_existing() {
        let (_a, o, _db) = setup().await;
        o.upsert(&UpsertOverrideParams {
            assistant_id: "u1",
            enabled: true,
            sort_order: 0,
            last_used_at: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();

        let updated = o
            .upsert(&UpsertOverrideParams {
                assistant_id: "u1",
                enabled: false,
                sort_order: 3,
                last_used_at: None,
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(!updated.enabled);
        assert_eq!(updated.sort_order, 3);
        // last_used_at None does not overwrite previous value (COALESCE)
        assert_eq!(updated.last_used_at, Some(1000));
    }

    #[tokio::test]
    async fn override_get_all_returns_rows() {
        let (_a, o, _db) = setup().await;
        o.upsert(&UpsertOverrideParams {
            assistant_id: "u1",
            enabled: true,
            sort_order: 0,
            last_used_at: None,
            ..Default::default()
        })
        .await
        .unwrap();
        o.upsert(&UpsertOverrideParams {
            assistant_id: "u2",
            enabled: false,
            sort_order: 1,
            last_used_at: None,
            ..Default::default()
        })
        .await
        .unwrap();

        let all = o.get_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn override_delete() {
        let (_a, o, _db) = setup().await;
        o.upsert(&UpsertOverrideParams {
            assistant_id: "u1",
            enabled: true,
            sort_order: 0,
            last_used_at: None,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(o.delete("u1").await.unwrap());
        assert!(!o.delete("u1").await.unwrap());
    }

    #[tokio::test]
    async fn override_delete_orphans_removes_only_absent() {
        let (_a, o, _db) = setup().await;
        for id in ["a", "b", "c"] {
            o.upsert(&UpsertOverrideParams {
                assistant_id: id,
                enabled: true,
                sort_order: 0,
                last_used_at: None,
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let removed = o.delete_orphans(&["a", "c"]).await.unwrap();
        assert_eq!(removed, 1);
        let remaining: Vec<String> = o.get_all().await.unwrap().into_iter().map(|r| r.assistant_id).collect();
        assert!(remaining.contains(&"a".to_string()));
        assert!(remaining.contains(&"c".to_string()));
        assert!(!remaining.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn override_delete_orphans_empty_valid_ids_clears_table() {
        let (_a, o, _db) = setup().await;
        o.upsert(&UpsertOverrideParams {
            assistant_id: "a",
            enabled: true,
            sort_order: 0,
            last_used_at: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let removed = o.delete_orphans(&[]).await.unwrap();
        assert_eq!(removed, 1);
        assert!(o.get_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn definition_upsert_then_get() {
        let (d, _s, _p, _db) = setup_v2().await;
        let row = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        assert_eq!(row.assistant_key, "u1");
        assert_eq!(row.definition_id, "asstdef_u1");
        assert_eq!(row.source, "user");
        assert_eq!(row.default_permission_mode, "fixed");

        let fetched = d.get_by_key("u1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "User One");
        assert_eq!(fetched.rule_inline_content.as_deref(), Some("# rule"));
        assert_eq!(fetched.avatar_type, "emoji");
        assert_eq!(fetched.avatar_value.as_deref(), Some("🤖"));
    }

    #[tokio::test]
    async fn state_upsert_then_list() {
        let (d, s, _p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        s.upsert(&UpsertAssistantOverlayParams {
            definition_id: &definition.definition_id,
            enabled: false,
            sort_order: 9,
            agent_backend_override: Some("claude"),
            last_used_at: Some(1234),
        })
        .await
        .unwrap();

        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].definition_id, definition.definition_id);
        assert!(!list[0].enabled);
        assert_eq!(list[0].sort_order, 9);
        assert_eq!(list[0].agent_backend_override.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn preference_upsert_then_get() {
        let (d, _s, p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        let row = p
            .upsert(&UpsertAssistantPreferenceParams {
                definition_id: &definition.definition_id,
                last_model_id: Some("gpt-4.1"),
                last_permission_value: Some("workspace-write"),
                last_skill_ids: r#"["pdf"]"#,
                last_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
                last_mcp_ids: r#"["mcp-1"]"#,
            })
            .await
            .unwrap();
        assert_eq!(row.last_model_id.as_deref(), Some("gpt-4.1"));

        let fetched = p.get(&definition.definition_id).await.unwrap().unwrap();
        assert_eq!(fetched.last_skill_ids, r#"["pdf"]"#);
    }

    #[tokio::test]
    async fn rebuild_legacy_mirror_creates_legacy_rows() {
        let (d, s, _p, db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        let state = s
            .upsert(&UpsertAssistantOverlayParams {
                definition_id: &definition.definition_id,
                enabled: false,
                sort_order: 7,
                agent_backend_override: Some("claude"),
                last_used_at: Some(999),
            })
            .await
            .unwrap();

        rebuild_legacy_assistant_mirror(db.pool(), &definition, Some(&state))
            .await
            .unwrap();

        let legacy_assistant = sqlx::query_as::<_, AssistantRow>("SELECT * FROM assistants WHERE id = 'u1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(legacy_assistant.preset_agent_type, "gemini");
        assert_eq!(legacy_assistant.avatar.as_deref(), Some("🤖"));
        assert_eq!(legacy_assistant.enabled_skills.as_deref(), Some(r#"["pdf","cron"]"#));

        let legacy_override =
            sqlx::query_as::<_, AssistantOverrideRow>("SELECT * FROM assistant_overrides WHERE assistant_id = 'u1'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(!legacy_override.enabled);
        assert_eq!(legacy_override.sort_order, 7);
        assert_eq!(legacy_override.last_used_at, Some(999));
    }

    #[tokio::test]
    async fn rebuild_legacy_mirror_skips_builtin_assistant_rows() {
        let (d, s, _p, db) = setup_v2().await;
        let definition = d
            .upsert(&UpsertAssistantDefinitionParams {
                definition_id: "asstdef_builtin_office",
                assistant_key: "builtin-office",
                source: "builtin",
                owner_type: "system",
                source_ref: Some("builtin-office"),
                source_version: None,
                source_hash: None,
                name: "Office",
                name_i18n: r#"{"zh-CN":"Office"}"#,
                description: Some("builtin"),
                description_i18n: "{}",
                avatar_type: "builtin_asset",
                avatar_value: Some("office.svg"),
                agent_backend: "aionrs",
                rule_resource_type: "builtin_asset",
                rule_resource_ref: Some("builtin-office"),
                rule_inline_content: None,
                recommended_prompts: r#"["hello"]"#,
                recommended_prompts_i18n: "{}",
                default_model_mode: "auto",
                default_model_value: None,
                default_permission_mode: "auto",
                default_permission_value: None,
                default_skills_mode: "fixed",
                default_skill_ids: "[]",
                custom_skill_names: "[]",
                default_disabled_builtin_skill_ids: "[]",
                default_mcps_mode: "auto",
                default_mcp_ids: "[]",
            })
            .await
            .unwrap();
        let state = s
            .upsert(&UpsertAssistantOverlayParams {
                definition_id: &definition.definition_id,
                enabled: false,
                sort_order: 3,
                agent_backend_override: Some("claude"),
                last_used_at: Some(42),
            })
            .await
            .unwrap();

        rebuild_legacy_assistant_mirror(db.pool(), &definition, Some(&state))
            .await
            .unwrap();

        let legacy_assistant_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM assistants WHERE id = 'builtin-office'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(legacy_assistant_count, 0);

        let legacy_override = sqlx::query_as::<_, AssistantOverrideRow>(
            "SELECT * FROM assistant_overrides WHERE assistant_id = 'builtin-office'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(!legacy_override.enabled);
        assert_eq!(legacy_override.sort_order, 3);
        assert_eq!(legacy_override.preset_agent_type.as_deref(), Some("claude"));
    }
}
