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

const DEFAULT_USER_ID: &str = "system_default_user";

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
        self.list_for_user(DEFAULT_USER_ID).await
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AssistantRow>, DbError> {
        let rows =
            sqlx::query_as::<_, AssistantRow>("SELECT * FROM assistants WHERE user_id = ? ORDER BY updated_at DESC")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<AssistantRow>, DbError> {
        self.get_for_user(DEFAULT_USER_ID, id).await
    }

    async fn get_for_user(&self, user_id: &str, id: &str) -> Result<Option<AssistantRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantRow>("SELECT * FROM assistants WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn create(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError> {
        self.create_for_user(DEFAULT_USER_ID, params).await
    }

    async fn create_for_user(
        &self,
        user_id: &str,
        params: &CreateAssistantParams<'_>,
    ) -> Result<AssistantRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO assistants \
                (id, user_id, name, description, avatar, enabled_skills, \
                 custom_skill_names, disabled_builtin_skills, prompts, models, \
                 name_i18n, description_i18n, prompts_i18n, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id)
        .bind(user_id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.avatar)
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
        self.update_for_user(DEFAULT_USER_ID, id, params).await
    }

    async fn update_for_user(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateAssistantParams<'_>,
    ) -> Result<Option<AssistantRow>, DbError> {
        let Some(existing) = self.get_for_user(user_id, id).await? else {
            return Ok(None);
        };

        let merged = merge_update(existing, params);

        sqlx::query(
            "UPDATE assistants SET \
                name = ?, description = ?, avatar = ?, \
                enabled_skills = ?, custom_skill_names = ?, disabled_builtin_skills = ?, \
                prompts = ?, models = ?, name_i18n = ?, description_i18n = ?, \
                prompts_i18n = ?, updated_at = ? \
             WHERE user_id = ? AND id = ?",
        )
        .bind(&merged.name)
        .bind(&merged.description)
        .bind(&merged.avatar)
        .bind(&merged.enabled_skills)
        .bind(&merged.custom_skill_names)
        .bind(&merged.disabled_builtin_skills)
        .bind(&merged.prompts)
        .bind(&merged.models)
        .bind(&merged.name_i18n)
        .bind(&merged.description_i18n)
        .bind(&merged.prompts_i18n)
        .bind(merged.updated_at)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(Some(merged))
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        self.delete_for_user(DEFAULT_USER_ID, id).await
    }

    async fn delete_for_user(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistants WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError> {
        self.upsert_for_user(DEFAULT_USER_ID, params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &CreateAssistantParams<'_>,
    ) -> Result<AssistantRow, DbError> {
        let now = now_ms();

        let result = sqlx::query(
            "INSERT INTO assistants \
                (id, user_id, name, description, avatar, enabled_skills, \
                 custom_skill_names, disabled_builtin_skills, prompts, models, \
                 name_i18n, description_i18n, prompts_i18n, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, \
                description = excluded.description, \
                avatar = excluded.avatar, \
                enabled_skills = excluded.enabled_skills, \
                custom_skill_names = excluded.custom_skill_names, \
                disabled_builtin_skills = excluded.disabled_builtin_skills, \
                prompts = excluded.prompts, \
                models = excluded.models, \
                name_i18n = excluded.name_i18n, \
                description_i18n = excluded.description_i18n, \
                prompts_i18n = excluded.prompts_i18n, \
                updated_at = excluded.updated_at \
             WHERE assistants.user_id = excluded.user_id",
        )
        .bind(params.id)
        .bind(user_id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.avatar)
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

        if result.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Assistant with id '{}' already exists for another user",
                params.id
            )));
        }

        let row = self
            .get_for_user(user_id, params.id)
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

    async fn upsert_with_user_id(
        &self,
        user_id: Option<&str>,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        let now = now_ms();

        let result = sqlx::query(
            "INSERT INTO assistant_definitions (
                id, user_id, assistant_id, source, owner_type, source_ref,
                name, name_i18n, description, description_i18n, avatar_type, avatar_value,
                agent_id, rule_resource_type, rule_resource_ref,
                recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_model_value,
                default_permission_mode, default_permission_value,
                default_thought_level_mode, default_thought_level_value,
                default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
                default_mcps_mode, default_mcp_ids,
                created_at, updated_at, deleted_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
            ON CONFLICT(id) DO UPDATE SET
                assistant_id = excluded.assistant_id,
                source = excluded.source,
                owner_type = excluded.owner_type,
                source_ref = excluded.source_ref,
                name = excluded.name,
                name_i18n = excluded.name_i18n,
                description = excluded.description,
                description_i18n = excluded.description_i18n,
                avatar_type = excluded.avatar_type,
                avatar_value = excluded.avatar_value,
                agent_id = excluded.agent_id,
                rule_resource_type = excluded.rule_resource_type,
                rule_resource_ref = excluded.rule_resource_ref,
                recommended_prompts = excluded.recommended_prompts,
                recommended_prompts_i18n = excluded.recommended_prompts_i18n,
                default_model_mode = excluded.default_model_mode,
                default_model_value = excluded.default_model_value,
                default_permission_mode = excluded.default_permission_mode,
                default_permission_value = excluded.default_permission_value,
                default_thought_level_mode = excluded.default_thought_level_mode,
                default_thought_level_value = excluded.default_thought_level_value,
                default_skills_mode = excluded.default_skills_mode,
                default_skill_ids = excluded.default_skill_ids,
                custom_skill_names = excluded.custom_skill_names,
                default_disabled_builtin_skill_ids = excluded.default_disabled_builtin_skill_ids,
                default_mcps_mode = excluded.default_mcps_mode,
                default_mcp_ids = excluded.default_mcp_ids,
                updated_at = excluded.updated_at,
                deleted_at = NULL
             WHERE assistant_definitions.user_id IS excluded.user_id",
        )
        .bind(params.id)
        .bind(user_id)
        .bind(params.assistant_id)
        .bind(params.source)
        .bind(params.owner_type)
        .bind(params.source_ref)
        .bind(params.name)
        .bind(params.name_i18n)
        .bind(params.description)
        .bind(params.description_i18n)
        .bind(params.avatar_type)
        .bind(params.avatar_value)
        .bind(params.agent_id)
        .bind(params.rule_resource_type)
        .bind(params.rule_resource_ref)
        .bind(params.recommended_prompts)
        .bind(params.recommended_prompts_i18n)
        .bind(params.default_model_mode)
        .bind(params.default_model_value)
        .bind(params.default_permission_mode)
        .bind(params.default_permission_value)
        .bind(params.default_thought_level_mode)
        .bind(params.default_thought_level_value)
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

        if result.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Assistant definition with id '{}' already exists for another scope",
                params.id
            )));
        }

        let row = match user_id {
            Some(user_id) => self.get_by_id_for_user(user_id, params.id).await?,
            None => self.get_by_id(params.id).await?,
        }
        .ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce assistant definition row for id '{}'",
                params.id
            ))
        })?;
        Ok(row)
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
        self.get_for_user(DEFAULT_USER_ID, assistant_id).await
    }

    async fn get_for_user(&self, user_id: &str, assistant_id: &str) -> Result<Option<AssistantOverrideRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantOverrideRow>(
            "SELECT * FROM assistant_overrides WHERE user_id = ? AND assistant_id = ?",
        )
        .bind(user_id)
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_all(&self) -> Result<Vec<AssistantOverrideRow>, DbError> {
        self.get_all_for_user(DEFAULT_USER_ID).await
    }

    async fn get_all_for_user(&self, user_id: &str) -> Result<Vec<AssistantOverrideRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantOverrideRow>("SELECT * FROM assistant_overrides WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertOverrideParams<'_>) -> Result<AssistantOverrideRow, DbError> {
        self.upsert_for_user(DEFAULT_USER_ID, params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertOverrideParams<'_>,
    ) -> Result<AssistantOverrideRow, DbError> {
        let now = now_ms();
        let last_used_at: Option<TimestampMs> = params.last_used_at;

        sqlx::query(
            "INSERT INTO assistant_overrides \
                (user_id, assistant_id, enabled, sort_order, last_used_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, assistant_id) DO UPDATE SET \
                enabled = excluded.enabled, \
                sort_order = excluded.sort_order, \
                last_used_at = COALESCE(excluded.last_used_at, assistant_overrides.last_used_at), \
                updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(params.assistant_id)
        .bind(params.enabled)
        .bind(params.sort_order)
        .bind(last_used_at)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = self.get_for_user(user_id, params.assistant_id).await?.ok_or_else(|| {
            DbError::Init(format!(
                "upsert did not produce override row for id '{}'",
                params.assistant_id
            ))
        })?;
        Ok(row)
    }

    async fn delete(&self, assistant_id: &str) -> Result<bool, DbError> {
        self.delete_for_user(DEFAULT_USER_ID, assistant_id).await
    }

    async fn delete_for_user(&self, user_id: &str, assistant_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_overrides WHERE user_id = ? AND assistant_id = ?")
            .bind(user_id)
            .bind(assistant_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_orphans(&self, valid_ids: &[&str]) -> Result<u64, DbError> {
        self.delete_orphans_for_user(DEFAULT_USER_ID, valid_ids).await
    }

    async fn delete_orphans_for_user(&self, user_id: &str, valid_ids: &[&str]) -> Result<u64, DbError> {
        if valid_ids.is_empty() {
            let result = sqlx::query("DELETE FROM assistant_overrides WHERE user_id = ?")
                .bind(user_id)
                .execute(&self.pool)
                .await?;
            return Ok(result.rows_affected());
        }

        let placeholders = std::iter::repeat_n("?", valid_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM assistant_overrides WHERE user_id = ? AND assistant_id NOT IN ({placeholders})");
        let mut q = sqlx::query(&sql).bind(user_id);
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
        self.list_for_user(DEFAULT_USER_ID).await
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT *
             FROM assistant_definitions d
             WHERE d.user_id = ? AND d.deleted_at IS NULL
             UNION ALL
             SELECT *
             FROM assistant_definitions d
             WHERE d.user_id IS NULL
               AND d.deleted_at IS NULL
               AND NOT EXISTS (
                   SELECT 1
                   FROM assistant_definitions u
                   WHERE u.user_id = ?
                     AND (
                         u.assistant_id = d.assistant_id
                         OR (
                             u.source = d.source
                             AND u.source_ref IS NOT NULL
                             AND d.source_ref IS NOT NULL
                             AND u.source_ref = d.source_ref
                         )
                     )
               )
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_including_deleted(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list_including_deleted_for_user(DEFAULT_USER_ID).await
    }

    async fn list_including_deleted_for_user(&self, user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT *
             FROM assistant_definitions d
             WHERE d.user_id = ?
             UNION ALL
             SELECT *
             FROM assistant_definitions d
             WHERE d.user_id IS NULL
               AND NOT EXISTS (
                   SELECT 1
                   FROM assistant_definitions u
                   WHERE u.user_id = ?
                     AND (
                         u.assistant_id = d.assistant_id
                         OR (
                             u.source = d.source
                             AND u.source_ref IS NOT NULL
                             AND d.source_ref IS NOT NULL
                             AND u.source_ref = d.source_ref
                         )
                     )
               )
             ORDER BY updated_at DESC",
        )
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id_for_user(DEFAULT_USER_ID, assistant_id).await
    }

    async fn get_by_assistant_id_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE (user_id IS NULL OR user_id = ?) AND assistant_id = ? AND deleted_at IS NULL
             ORDER BY user_id IS NULL ASC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_assistant_id_including_deleted(
        &self,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id_including_deleted_for_user(DEFAULT_USER_ID, assistant_id)
            .await
    }

    async fn get_by_assistant_id_including_deleted_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE (user_id IS NULL OR user_id = ?) AND assistant_id = ?
             ORDER BY user_id IS NULL ASC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_id_for_user(DEFAULT_USER_ID, id).await
    }

    async fn get_by_id_for_user(&self, user_id: &str, id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE (user_id IS NULL OR user_id = ?) AND id = ? AND deleted_at IS NULL",
        )
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_source_ref(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref_for_user(DEFAULT_USER_ID, source, source_ref)
            .await
    }

    async fn get_by_source_ref_for_user(
        &self,
        user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE (user_id IS NULL OR user_id = ?) AND source = ? AND source_ref = ? AND deleted_at IS NULL
             ORDER BY user_id IS NULL ASC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(source)
        .bind(source_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_by_source_ref_including_deleted(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref_including_deleted_for_user(DEFAULT_USER_ID, source, source_ref)
            .await
    }

    async fn get_by_source_ref_including_deleted_for_user(
        &self,
        user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE (user_id IS NULL OR user_id = ?) AND source = ? AND source_ref = ?
             ORDER BY user_id IS NULL ASC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(source)
        .bind(source_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_global_by_source_ref_including_deleted(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE user_id IS NULL AND source = ? AND source_ref = ?
             LIMIT 1",
        )
        .bind(source)
        .bind(source_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn get_global_by_assistant_id_including_deleted(
        &self,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "SELECT * FROM assistant_definitions
             WHERE user_id IS NULL AND assistant_id = ?
             LIMIT 1",
        )
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        if params.source == "builtin" && params.owner_type == "system" {
            self.upsert_global(params).await
        } else {
            self.upsert_for_user(DEFAULT_USER_ID, params).await
        }
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        self.upsert_with_user_id(Some(user_id), params).await
    }

    async fn upsert_global(
        &self,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        self.upsert_with_user_id(None, params).await
    }

    async fn update_avatar_fields_preserving_deleted(
        &self,
        id: &str,
        avatar_type: &str,
        avatar_value: Option<&str>,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantDefinitionRow>(
            "UPDATE assistant_definitions
             SET avatar_type = ?, avatar_value = ?, updated_at = ?
             WHERE id = ?
             RETURNING *",
        )
        .bind(avatar_type)
        .bind(avatar_value)
        .bind(now_ms())
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn soft_delete(&self, id: &str, deleted_at: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE assistant_definitions
             SET deleted_at = ?, updated_at = ?
             WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(deleted_at)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn soft_delete_for_user(&self, user_id: &str, id: &str, deleted_at: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE assistant_definitions
             SET deleted_at = ?, updated_at = ?
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(deleted_at)
        .bind(now_ms())
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl IAssistantOverlayRepository for SqliteAssistantOverlayRepository {
    async fn get(&self, assistant_definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        self.get_for_user(DEFAULT_USER_ID, assistant_definition_id).await
    }

    async fn get_for_user(
        &self,
        user_id: &str,
        assistant_definition_id: &str,
    ) -> Result<Option<AssistantOverlayRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantOverlayRow>(
            "SELECT * FROM assistant_overlays WHERE user_id = ? AND assistant_definition_id = ?",
        )
        .bind(user_id)
        .bind(assistant_definition_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        self.list_for_user(DEFAULT_USER_ID).await
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<AssistantOverlayRow>, DbError> {
        let rows = sqlx::query_as::<_, AssistantOverlayRow>(
            "SELECT * FROM assistant_overlays WHERE user_id = ? ORDER BY sort_order, updated_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        self.upsert_for_user(DEFAULT_USER_ID, params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAssistantOverlayParams<'_>,
    ) -> Result<AssistantOverlayRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_overlays (
                user_id, assistant_definition_id, enabled, sort_order, agent_id_override,
                last_used_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id, assistant_definition_id) DO UPDATE SET
                enabled = excluded.enabled,
                sort_order = excluded.sort_order,
                agent_id_override = excluded.agent_id_override,
                last_used_at = excluded.last_used_at,
                updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(params.assistant_definition_id)
        .bind(params.enabled)
        .bind(params.sort_order)
        .bind(params.agent_id_override)
        .bind(params.last_used_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get_for_user(user_id, params.assistant_definition_id)
            .await?
            .ok_or_else(|| {
                DbError::Init(format!(
                    "upsert did not produce overlay row for assistant_definition_id '{}'",
                    params.assistant_definition_id
                ))
            })
    }

    async fn delete(&self, assistant_definition_id: &str) -> Result<bool, DbError> {
        self.delete_for_user(DEFAULT_USER_ID, assistant_definition_id).await
    }

    async fn delete_for_user(&self, user_id: &str, assistant_definition_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_overlays WHERE user_id = ? AND assistant_definition_id = ?")
            .bind(user_id)
            .bind(assistant_definition_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl IAssistantPreferenceRepository for SqliteAssistantPreferenceRepository {
    async fn get(&self, assistant_definition_id: &str) -> Result<Option<AssistantPreferenceRow>, DbError> {
        self.get_for_user(DEFAULT_USER_ID, assistant_definition_id).await
    }

    async fn get_for_user(
        &self,
        user_id: &str,
        assistant_definition_id: &str,
    ) -> Result<Option<AssistantPreferenceRow>, DbError> {
        let row = sqlx::query_as::<_, AssistantPreferenceRow>(
            "SELECT * FROM assistant_preferences WHERE user_id = ? AND assistant_definition_id = ?",
        )
        .bind(user_id)
        .bind(assistant_definition_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertAssistantPreferenceParams<'_>) -> Result<AssistantPreferenceRow, DbError> {
        self.upsert_for_user(DEFAULT_USER_ID, params).await
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAssistantPreferenceParams<'_>,
    ) -> Result<AssistantPreferenceRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO assistant_preferences (
                user_id, assistant_definition_id, last_model_id, last_permission_value, last_thought_level_value, last_skill_ids,
                last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_id, assistant_definition_id) DO UPDATE SET
                last_model_id = excluded.last_model_id,
                last_permission_value = excluded.last_permission_value,
                last_thought_level_value = excluded.last_thought_level_value,
                last_skill_ids = excluded.last_skill_ids,
                last_disabled_builtin_skill_ids = excluded.last_disabled_builtin_skill_ids,
                last_mcp_ids = excluded.last_mcp_ids,
                updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(params.assistant_definition_id)
        .bind(params.last_model_id)
        .bind(params.last_permission_value)
        .bind(params.last_thought_level_value)
        .bind(params.last_skill_ids)
        .bind(params.last_disabled_builtin_skill_ids)
        .bind(params.last_mcp_ids)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get_for_user(user_id, params.assistant_definition_id)
            .await?
            .ok_or_else(|| {
                DbError::Init(format!(
                    "upsert did not produce preference row for assistant_definition_id '{}'",
                    params.assistant_definition_id
                ))
            })
    }

    async fn delete(&self, assistant_definition_id: &str) -> Result<bool, DbError> {
        self.delete_for_user(DEFAULT_USER_ID, assistant_definition_id).await
    }

    async fn delete_for_user(&self, user_id: &str, assistant_definition_id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM assistant_preferences WHERE user_id = ? AND assistant_definition_id = ?")
            .bind(user_id)
            .bind(assistant_definition_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "user_b";

    async fn setup() -> (
        SqliteAssistantRepository,
        SqliteAssistantOverrideRepository,
        crate::Database,
    ) {
        let db = init_database_memory().await.unwrap();
        let a = SqliteAssistantRepository::new(db.pool().clone());
        let o = SqliteAssistantOverrideRepository::new(db.pool().clone());
        ensure_user(&db, USER_B).await;
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
        ensure_user(&db, USER_B).await;
        (d, s, p, db)
    }

    async fn ensure_user(db: &crate::Database, user_id: &str) {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    fn params<'a>(id: &'a str, name: &'a str) -> CreateAssistantParams<'a> {
        CreateAssistantParams {
            id,
            name,
            description: Some("desc"),
            avatar: None,
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
        definition_params_with_id("asstdef_u1", id, Some(id), name)
    }

    fn definition_params_with_id<'a>(
        definition_id: &'a str,
        assistant_id: &'a str,
        source_ref: Option<&'a str>,
        name: &'a str,
    ) -> UpsertAssistantDefinitionParams<'a> {
        UpsertAssistantDefinitionParams {
            id: definition_id,
            assistant_id,
            source: "user",
            owner_type: "user",
            source_ref,
            name,
            name_i18n: r#"{"zh-CN":"助手"}"#,
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_id: "gemini",
            rule_resource_type: "user_file",
            rule_resource_ref: None,
            recommended_prompts: r#"["hello"]"#,
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "fixed",
            default_permission_value: Some("workspace-write"),
            default_thought_level_mode: "auto",
            default_thought_level_value: None,
            default_skills_mode: "fixed",
            default_skill_ids: r#"["pdf","cron"]"#,
            custom_skill_names: r#"["my-custom-skill"]"#,
            default_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        }
    }

    fn builtin_definition_params<'a>(
        definition_id: &'a str,
        assistant_id: &'a str,
        source_ref: Option<&'a str>,
        name: &'a str,
    ) -> UpsertAssistantDefinitionParams<'a> {
        UpsertAssistantDefinitionParams {
            source: "builtin",
            owner_type: "system",
            ..definition_params_with_id(definition_id, assistant_id, source_ref, name)
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
        p.description = Some("updated");
        let second = a.upsert(&p).await.unwrap();
        assert_eq!(second.name, "second");
        assert_eq!(second.description.as_deref(), Some("updated"));

        let list = a.list().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn assistants_are_scoped_by_user() {
        let (a, _o, _db) = setup().await;
        a.create_for_user(USER_A, &params("a1", "User A Assistant"))
            .await
            .unwrap();
        a.create_for_user(USER_B, &params("b1", "User B Assistant"))
            .await
            .unwrap();

        assert!(a.get_for_user(USER_A, "a1").await.unwrap().is_some());
        assert!(a.get_for_user(USER_A, "b1").await.unwrap().is_none());
        assert!(a.get_for_user(USER_B, "a1").await.unwrap().is_none());
        assert!(a.get_for_user(USER_B, "b1").await.unwrap().is_some());

        let user_a_ids: Vec<String> = a
            .list_for_user(USER_A)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        let user_b_ids: Vec<String> = a
            .list_for_user(USER_B)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(user_a_ids, vec!["a1"]);
        assert_eq!(user_b_ids, vec!["b1"]);
    }

    #[tokio::test]
    async fn assistant_upsert_rejects_cross_user_id_takeover() {
        let (a, _o, _db) = setup().await;
        a.upsert_for_user(USER_A, &params("shared", "User A Assistant"))
            .await
            .unwrap();

        let err = a
            .upsert_for_user(USER_B, &params("shared", "User B Assistant"))
            .await
            .expect_err("cross-user upsert must not take over an existing assistant id");
        assert!(matches!(err, DbError::Conflict(_)));

        let user_a = a.get_for_user(USER_A, "shared").await.unwrap().unwrap();
        assert_eq!(user_a.name, "User A Assistant");
        assert!(a.get_for_user(USER_B, "shared").await.unwrap().is_none());
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
        })
        .await
        .unwrap();

        let updated = o
            .upsert(&UpsertOverrideParams {
                assistant_id: "u1",
                enabled: false,
                sort_order: 3,
                last_used_at: None,
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
        })
        .await
        .unwrap();
        o.upsert(&UpsertOverrideParams {
            assistant_id: "u2",
            enabled: false,
            sort_order: 1,
            last_used_at: None,
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
        })
        .await
        .unwrap();
        let removed = o.delete_orphans(&[]).await.unwrap();
        assert_eq!(removed, 1);
        assert!(o.get_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn overrides_are_scoped_by_user() {
        let (_a, o, _db) = setup().await;
        o.upsert_for_user(
            USER_A,
            &UpsertOverrideParams {
                assistant_id: "shared",
                enabled: true,
                sort_order: 1,
                last_used_at: Some(100),
            },
        )
        .await
        .unwrap();
        o.upsert_for_user(
            USER_B,
            &UpsertOverrideParams {
                assistant_id: "shared",
                enabled: false,
                sort_order: 9,
                last_used_at: Some(900),
            },
        )
        .await
        .unwrap();

        let user_a = o.get_for_user(USER_A, "shared").await.unwrap().unwrap();
        let user_b = o.get_for_user(USER_B, "shared").await.unwrap().unwrap();
        assert!(user_a.enabled);
        assert_eq!(user_a.sort_order, 1);
        assert!(!user_b.enabled);
        assert_eq!(user_b.sort_order, 9);
        assert_eq!(o.get_all_for_user(USER_A).await.unwrap().len(), 1);
        assert_eq!(o.get_all_for_user(USER_B).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn definition_upsert_then_get() {
        let (d, _s, _p, _db) = setup_v2().await;
        let row = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        assert_eq!(row.assistant_id, "u1");
        assert_eq!(row.id, "asstdef_u1");
        assert_eq!(row.source, "user");
        assert_eq!(row.default_permission_mode, "fixed");

        let fetched = d.get_by_assistant_id("u1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "User One");
        assert_eq!(fetched.rule_resource_type, "user_file");
        assert_eq!(fetched.avatar_type, "emoji");
        assert_eq!(fetched.avatar_value.as_deref(), Some("🤖"));
    }

    #[tokio::test]
    async fn definitions_with_same_refs_are_isolated_by_user() {
        let (d, _s, _p, _db) = setup_v2().await;
        d.upsert_for_user(
            USER_A,
            &definition_params_with_id("def_a", "shared-assistant", Some("shared-ref"), "User A Definition"),
        )
        .await
        .unwrap();
        d.upsert_for_user(
            USER_B,
            &definition_params_with_id("def_b", "shared-assistant", Some("shared-ref"), "User B Definition"),
        )
        .await
        .unwrap();

        let user_a = d
            .get_by_source_ref_for_user(USER_A, "user", "shared-ref")
            .await
            .unwrap()
            .unwrap();
        let user_b = d
            .get_by_source_ref_for_user(USER_B, "user", "shared-ref")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_a.id, "def_a");
        assert_eq!(user_a.name, "User A Definition");
        assert_eq!(user_b.id, "def_b");
        assert_eq!(user_b.name, "User B Definition");
    }

    #[tokio::test]
    async fn definition_upsert_rejects_cross_scope_id_takeover() {
        let (d, _s, _p, _db) = setup_v2().await;
        d.upsert_for_user(
            USER_A,
            &definition_params_with_id("shared_def", "assistant-a", Some("ref-a"), "User A Definition"),
        )
        .await
        .unwrap();

        let err = d
            .upsert_for_user(
                USER_B,
                &definition_params_with_id("shared_def", "assistant-b", Some("ref-b"), "User B Definition"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));

        let user_a = d.get_by_id_for_user(USER_A, "shared_def").await.unwrap().unwrap();
        assert_eq!(user_a.assistant_id, "assistant-a");
        assert_eq!(user_a.name, "User A Definition");
        assert!(d.get_by_id_for_user(USER_B, "shared_def").await.unwrap().is_none());

        d.upsert_global(&builtin_definition_params(
            "builtin_shared_def",
            "builtin-a",
            Some("builtin-a"),
            "Builtin Definition",
        ))
        .await
        .unwrap();
        d.upsert_global(&builtin_definition_params(
            "builtin_shared_def",
            "builtin-a",
            Some("builtin-a"),
            "Builtin Definition Updated",
        ))
        .await
        .unwrap();

        let err = d
            .upsert_for_user(
                USER_A,
                &definition_params_with_id("builtin_shared_def", "user-a", Some("user-a"), "User Definition"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));

        let global = d.get_by_id("builtin_shared_def").await.unwrap().unwrap();
        assert_eq!(global.assistant_id, "builtin-a");
        assert_eq!(global.name, "Builtin Definition Updated");
    }

    #[tokio::test]
    async fn global_builtin_definition_is_visible_to_all_users() {
        let (d, _s, _p, _db) = setup_v2().await;
        d.upsert_global(&builtin_definition_params(
            "builtin_def",
            "builtin-assistant",
            Some("builtin-ref"),
            "Builtin Definition",
        ))
        .await
        .unwrap();

        let user_a = d
            .get_by_assistant_id_for_user(USER_A, "builtin-assistant")
            .await
            .unwrap();
        let user_b = d
            .get_by_assistant_id_for_user(USER_B, "builtin-assistant")
            .await
            .unwrap();
        assert_eq!(user_a.unwrap().id, "builtin_def");
        assert_eq!(user_b.unwrap().id, "builtin_def");
    }

    #[tokio::test]
    async fn user_definition_overrides_global_definition() {
        let (d, _s, _p, _db) = setup_v2().await;
        d.upsert_global(&builtin_definition_params(
            "global_def",
            "shared-assistant",
            Some("shared-ref"),
            "Global Definition",
        ))
        .await
        .unwrap();
        d.upsert_for_user(
            USER_B,
            &definition_params_with_id("user_def", "shared-assistant", Some("shared-ref"), "User Definition"),
        )
        .await
        .unwrap();

        let user_a = d
            .get_by_assistant_id_for_user(USER_A, "shared-assistant")
            .await
            .unwrap()
            .unwrap();
        let user_b_by_assistant = d
            .get_by_assistant_id_for_user(USER_B, "shared-assistant")
            .await
            .unwrap()
            .unwrap();
        let user_b_by_ref = d
            .get_by_source_ref_for_user(USER_B, "user", "shared-ref")
            .await
            .unwrap()
            .unwrap();
        let user_b_list = d.list_for_user(USER_B).await.unwrap();

        assert_eq!(user_a.id, "global_def");
        assert_eq!(user_b_by_assistant.id, "user_def");
        assert_eq!(user_b_by_ref.id, "user_def");
        assert_eq!(user_b_list.len(), 1);
        assert_eq!(user_b_list[0].id, "user_def");
    }

    #[tokio::test]
    async fn state_upsert_then_list() {
        let (d, s, _p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        s.upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: &definition.id,
            enabled: false,
            sort_order: 9,
            agent_id_override: Some("claude"),
            last_used_at: Some(1234),
        })
        .await
        .unwrap();

        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].assistant_definition_id, definition.id);
        assert!(!list[0].enabled);
        assert_eq!(list[0].sort_order, 9);
        assert_eq!(list[0].agent_id_override.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn overlays_are_scoped_by_user() {
        let (d, s, _p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        s.upsert_for_user(
            USER_A,
            &UpsertAssistantOverlayParams {
                assistant_definition_id: &definition.id,
                enabled: true,
                sort_order: 1,
                agent_id_override: Some("agent-a"),
                last_used_at: Some(10),
            },
        )
        .await
        .unwrap();
        s.upsert_for_user(
            USER_B,
            &UpsertAssistantOverlayParams {
                assistant_definition_id: &definition.id,
                enabled: false,
                sort_order: 7,
                agent_id_override: Some("agent-b"),
                last_used_at: Some(70),
            },
        )
        .await
        .unwrap();

        let user_a = s.get_for_user(USER_A, &definition.id).await.unwrap().unwrap();
        let user_b = s.get_for_user(USER_B, &definition.id).await.unwrap().unwrap();
        assert!(user_a.enabled);
        assert_eq!(user_a.agent_id_override.as_deref(), Some("agent-a"));
        assert!(!user_b.enabled);
        assert_eq!(user_b.agent_id_override.as_deref(), Some("agent-b"));
        assert_eq!(s.list_for_user(USER_A).await.unwrap().len(), 1);
        assert_eq!(s.list_for_user(USER_B).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn preference_upsert_then_get() {
        let (d, _s, p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        let row = p
            .upsert(&UpsertAssistantPreferenceParams {
                assistant_definition_id: &definition.id,
                last_model_id: Some("gpt-4.1"),
                last_permission_value: Some("workspace-write"),
                last_thought_level_value: Some("high"),
                last_skill_ids: r#"["pdf"]"#,
                last_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
                last_mcp_ids: r#"["mcp-1"]"#,
            })
            .await
            .unwrap();
        assert_eq!(row.last_model_id.as_deref(), Some("gpt-4.1"));
        assert_eq!(row.last_thought_level_value.as_deref(), Some("high"));

        let fetched = p.get(&definition.id).await.unwrap().unwrap();
        assert_eq!(fetched.last_skill_ids, r#"["pdf"]"#);
        assert_eq!(fetched.last_thought_level_value.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn preferences_are_scoped_by_user() {
        let (d, _s, p, _db) = setup_v2().await;
        let definition = d.upsert(&definition_params("u1", "User One")).await.unwrap();
        p.upsert_for_user(
            USER_A,
            &UpsertAssistantPreferenceParams {
                assistant_definition_id: &definition.id,
                last_model_id: Some("model-a"),
                last_permission_value: Some("read-only"),
                last_thought_level_value: Some("low"),
                last_skill_ids: r#"["a"]"#,
                last_disabled_builtin_skill_ids: "[]",
                last_mcp_ids: "[]",
            },
        )
        .await
        .unwrap();
        p.upsert_for_user(
            USER_B,
            &UpsertAssistantPreferenceParams {
                assistant_definition_id: &definition.id,
                last_model_id: Some("model-b"),
                last_permission_value: Some("workspace-write"),
                last_thought_level_value: Some("high"),
                last_skill_ids: r#"["b"]"#,
                last_disabled_builtin_skill_ids: "[]",
                last_mcp_ids: "[]",
            },
        )
        .await
        .unwrap();

        let user_a = p.get_for_user(USER_A, &definition.id).await.unwrap().unwrap();
        let user_b = p.get_for_user(USER_B, &definition.id).await.unwrap().unwrap();
        assert_eq!(user_a.last_model_id.as_deref(), Some("model-a"));
        assert_eq!(user_a.last_skill_ids, r#"["a"]"#);
        assert_eq!(user_b.last_model_id.as_deref(), Some("model-b"));
        assert_eq!(user_b.last_skill_ids, r#"["b"]"#);
    }
}
