//! SQLite-backed agent metadata repository.

use aionui_common::now_ms;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use tracing::warn;

use crate::error::DbError;
use crate::models::{
    AgentMetadataRow, UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpsertAgentMetadataParams,
};
use crate::repository::agent_metadata::IAgentMetadataRepository;

#[derive(Clone, Debug)]
pub struct SqliteAgentMetadataRepository {
    pool: SqlitePool,
}

const DEFAULT_USER_ID: &str = "system_default_user";

/// Projection over the single catalog row (`am`). An agent is a machine-level
/// resource: its identity, CLI-login-derived capabilities/models, availability
/// probe, command/env overrides AND its enable/disable state all live on this
/// one row and are shared by every user on the device. There is no per-user
/// overlay — the acting-user filter only scopes row *visibility* (a custom
/// agent's owner), never field values.
const AGENT_METADATA_SAFE_COLUMNS: &str = "\
    am.agent_id AS id, am.user_id, am.icon, am.name, am.name_i18n, am.description, am.description_i18n, \
    am.backend, am.agent_type, am.agent_source, am.agent_source_info, \
    am.enabled, am.command, am.args, am.env, am.native_skills_dirs, \
    am.behavior_policy, am.yolo_id, \
    CAST(am.agent_capabilities AS BLOB) AS agent_capabilities, \
    CAST(am.auth_methods AS BLOB) AS auth_methods, \
    CAST(am.config_options AS BLOB) AS config_options, \
    CAST(am.available_modes AS BLOB) AS available_modes, \
    CAST(am.available_models AS BLOB) AS available_models, \
    CAST(am.available_commands AS BLOB) AS available_commands, \
    am.sort_order, \
    am.last_check_status, am.last_check_kind, am.last_check_error_code, am.last_check_error_message, \
    am.last_check_guidance, am.last_check_latency_ms, am.last_check_at, am.last_success_at, am.last_failure_at, \
    am.command_override, am.env_override, \
    am.created_at, am.updated_at";

/// FROM clause for the catalog. No per-user join — every field is machine-level.
const AGENT_METADATA_FROM: &str = "FROM agent_metadata am";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentMetadataCacheField {
    AgentCapabilities,
    AuthMethods,
    ConfigOptions,
    AvailableModes,
    AvailableModels,
    AvailableCommands,
}

impl AgentMetadataCacheField {
    fn column_name(self) -> &'static str {
        match self {
            Self::AgentCapabilities => "agent_capabilities",
            Self::AuthMethods => "auth_methods",
            Self::ConfigOptions => "config_options",
            Self::AvailableModes => "available_modes",
            Self::AvailableModels => "available_models",
            Self::AvailableCommands => "available_commands",
        }
    }
}

#[derive(Debug)]
struct AgentMetadataSafeRow {
    id: String,
    user_id: Option<String>,
    icon: Option<String>,
    name: String,
    name_i18n: Option<String>,
    description: Option<String>,
    description_i18n: Option<String>,
    backend: Option<String>,
    agent_type: String,
    agent_source: String,
    agent_source_info: Option<String>,
    enabled: bool,
    command: Option<String>,
    args: Option<String>,
    env: Option<String>,
    native_skills_dirs: Option<String>,
    behavior_policy: Option<String>,
    yolo_id: Option<String>,
    agent_capabilities: Option<Vec<u8>>,
    auth_methods: Option<Vec<u8>>,
    config_options: Option<Vec<u8>>,
    available_modes: Option<Vec<u8>>,
    available_models: Option<Vec<u8>>,
    available_commands: Option<Vec<u8>>,
    sort_order: i64,
    last_check_status: Option<String>,
    last_check_kind: Option<String>,
    last_check_error_code: Option<String>,
    last_check_error_message: Option<String>,
    last_check_guidance: Option<String>,
    last_check_latency_ms: Option<i64>,
    last_check_at: Option<aionui_common::TimestampMs>,
    last_success_at: Option<aionui_common::TimestampMs>,
    last_failure_at: Option<aionui_common::TimestampMs>,
    command_override: Option<String>,
    env_override: Option<String>,
    created_at: aionui_common::TimestampMs,
    updated_at: aionui_common::TimestampMs,
}

impl AgentMetadataSafeRow {
    fn from_sqlite_row(row: SqliteRow) -> Result<Self, DbError> {
        Ok(Self {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            icon: row.try_get("icon")?,
            name: row.try_get("name")?,
            name_i18n: row.try_get("name_i18n")?,
            description: row.try_get("description")?,
            description_i18n: row.try_get("description_i18n")?,
            backend: row.try_get("backend")?,
            agent_type: row.try_get("agent_type")?,
            agent_source: row.try_get("agent_source")?,
            agent_source_info: row.try_get("agent_source_info")?,
            enabled: row.try_get("enabled")?,
            command: row.try_get("command")?,
            args: row.try_get("args")?,
            env: row.try_get("env")?,
            native_skills_dirs: row.try_get("native_skills_dirs")?,
            behavior_policy: row.try_get("behavior_policy")?,
            yolo_id: row.try_get("yolo_id")?,
            agent_capabilities: row.try_get("agent_capabilities")?,
            auth_methods: row.try_get("auth_methods")?,
            config_options: row.try_get("config_options")?,
            available_modes: row.try_get("available_modes")?,
            available_models: row.try_get("available_models")?,
            available_commands: row.try_get("available_commands")?,
            sort_order: row.try_get("sort_order")?,
            last_check_status: row.try_get("last_check_status")?,
            last_check_kind: row.try_get("last_check_kind")?,
            last_check_error_code: row.try_get("last_check_error_code")?,
            last_check_error_message: row.try_get("last_check_error_message")?,
            last_check_guidance: row.try_get("last_check_guidance")?,
            last_check_latency_ms: row.try_get("last_check_latency_ms")?,
            last_check_at: row.try_get("last_check_at")?,
            last_success_at: row.try_get("last_success_at")?,
            last_failure_at: row.try_get("last_failure_at")?,
            command_override: row.try_get("command_override")?,
            env_override: row.try_get("env_override")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    fn into_model(self) -> (AgentMetadataRow, Vec<AgentMetadataCacheField>) {
        let mut invalid_fields = Vec::new();
        let agent_capabilities = decode_cache_field(
            self.agent_capabilities,
            AgentMetadataCacheField::AgentCapabilities,
            &mut invalid_fields,
        );
        let auth_methods = decode_cache_field(
            self.auth_methods,
            AgentMetadataCacheField::AuthMethods,
            &mut invalid_fields,
        );
        let config_options = decode_cache_field(
            self.config_options,
            AgentMetadataCacheField::ConfigOptions,
            &mut invalid_fields,
        );
        let available_modes = decode_cache_field(
            self.available_modes,
            AgentMetadataCacheField::AvailableModes,
            &mut invalid_fields,
        );
        let available_models = decode_cache_field(
            self.available_models,
            AgentMetadataCacheField::AvailableModels,
            &mut invalid_fields,
        );
        let available_commands = decode_cache_field(
            self.available_commands,
            AgentMetadataCacheField::AvailableCommands,
            &mut invalid_fields,
        );

        (
            AgentMetadataRow {
                id: self.id,
                user_id: self.user_id,
                icon: self.icon,
                name: self.name,
                name_i18n: self.name_i18n,
                description: self.description,
                description_i18n: self.description_i18n,
                backend: self.backend,
                agent_type: self.agent_type,
                agent_source: self.agent_source,
                agent_source_info: self.agent_source_info,
                enabled: self.enabled,
                command: self.command,
                args: self.args,
                env: self.env,
                native_skills_dirs: self.native_skills_dirs,
                behavior_policy: self.behavior_policy,
                yolo_id: self.yolo_id,
                agent_capabilities,
                auth_methods,
                config_options,
                available_modes,
                available_models,
                available_commands,
                sort_order: self.sort_order,
                last_check_status: self.last_check_status,
                last_check_kind: self.last_check_kind,
                last_check_error_code: self.last_check_error_code,
                last_check_error_message: self.last_check_error_message,
                last_check_guidance: self.last_check_guidance,
                last_check_latency_ms: self.last_check_latency_ms,
                last_check_at: self.last_check_at,
                last_success_at: self.last_success_at,
                last_failure_at: self.last_failure_at,
                command_override: self.command_override,
                env_override: self.env_override,
                created_at: self.created_at,
                updated_at: self.updated_at,
            },
            invalid_fields,
        )
    }
}

fn decode_cache_field(
    value: Option<Vec<u8>>,
    field: AgentMetadataCacheField,
    invalid_fields: &mut Vec<AgentMetadataCacheField>,
) -> Option<String> {
    let bytes = value?;
    match String::from_utf8(bytes) {
        Ok(value) => Some(value),
        Err(_) => {
            invalid_fields.push(field);
            None
        }
    }
}

impl SqliteAgentMetadataRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn fetch_optional_safe_three_binds(
        &self,
        sql: &str,
        first: &str,
        second: &str,
        third: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        let row = sqlx::query(sql)
            .bind(first)
            .bind(second)
            .bind(third)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(self.decode_and_repair(row).await?)),
            None => Ok(None),
        }
    }

    async fn fetch_optional_safe_two_binds(
        &self,
        sql: &str,
        first: &str,
        second: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        let row = sqlx::query(sql)
            .bind(first)
            .bind(second)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(self.decode_and_repair(row).await?)),
            None => Ok(None),
        }
    }

    /// Resolve the viewer id that makes the catalog row for `id` visible
    /// through the owner-scoped merged read: the row's own `user_id`, or the
    /// default user for globally-owned (NULL) rows. Returns `None` when no
    /// catalog row exists for `id`. Machine-level writes (handshake,
    /// availability) use this so they can read back a row regardless of which
    /// user owns it.
    async fn catalog_viewer(&self, id: &str) -> Result<Option<String>, DbError> {
        let owner: Option<Option<String>> = sqlx::query_scalar("SELECT user_id FROM agent_metadata WHERE agent_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(owner.map(|user_id| user_id.unwrap_or_else(|| DEFAULT_USER_ID.to_string())))
    }

    async fn decode_and_repair(&self, row: SqliteRow) -> Result<AgentMetadataRow, DbError> {
        let safe = AgentMetadataSafeRow::from_sqlite_row(row)?;
        let user_id = safe.user_id.clone();
        let (model, invalid_fields) = safe.into_model();
        if !invalid_fields.is_empty() {
            self.clear_invalid_utf8_cache_fields(&model.id, user_id.as_deref(), &invalid_fields)
                .await;
        }
        Ok(model)
    }

    async fn clear_invalid_utf8_cache_fields(
        &self,
        id: &str,
        user_id: Option<&str>,
        fields: &[AgentMetadataCacheField],
    ) {
        for field in fields {
            warn!(
                table = "agent_metadata",
                row_id = %id,
                user_id = user_id,
                field = field.column_name(),
                action = "clear_invalid_utf8",
                "Clearing invalid UTF-8 from rebuildable agent metadata cache field"
            );
        }

        for field in fields {
            // Rebuildable cache data is machine-level and lives only on the
            // catalog row — the next handshake repopulates it.
            let _ = user_id;
            let catalog_sql = format!(
                "UPDATE agent_metadata SET {} = NULL, updated_at = ? WHERE agent_id = ?",
                field.column_name()
            );
            let catalog = sqlx::query(&catalog_sql)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await;

            if let Err(err) = catalog {
                warn!(
                    table = "agent_metadata",
                    row_id = %id,
                    user_id = user_id,
                    field = field.column_name(),
                    action = "clear_invalid_utf8_failed",
                    error = %err,
                    "Failed to persist invalid UTF-8 cache-field repair"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for SqliteAgentMetadataRepository {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        self.list_all_for_user(DEFAULT_USER_ID).await
    }

    async fn list_all_for_user(&self, user_id: &str) -> Result<Vec<AgentMetadataRow>, DbError> {
        let sql = format!(
            "SELECT {AGENT_METADATA_SAFE_COLUMNS} {AGENT_METADATA_FROM}
             WHERE am.user_id IS NULL OR am.user_id = ?
             ORDER BY am.sort_order ASC, am.name ASC"
        );
        let rows = sqlx::query(&sql).bind(user_id).fetch_all(&self.pool).await?;
        let mut decoded = Vec::with_capacity(rows.len());
        for row in rows {
            decoded.push(self.decode_and_repair(row).await?);
        }
        Ok(decoded)
    }

    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.get_for_user(DEFAULT_USER_ID, id).await
    }

    async fn get_for_user(&self, user_id: &str, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe_two_binds(
            &format!(
                "SELECT {AGENT_METADATA_SAFE_COLUMNS} {AGENT_METADATA_FROM}
                 WHERE am.agent_id = ? AND (am.user_id IS NULL OR am.user_id = ?)"
            ),
            id,
            user_id,
        )
        .await
    }

    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.find_by_source_and_name_for_user(DEFAULT_USER_ID, agent_source, name)
            .await
    }

    async fn find_by_source_and_name_for_user(
        &self,
        user_id: &str,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe_three_binds(
            &format!(
                "SELECT {AGENT_METADATA_SAFE_COLUMNS} {AGENT_METADATA_FROM}
                 WHERE am.agent_source = ? AND am.name = ? AND (am.user_id IS NULL OR am.user_id = ?)
                 ORDER BY am.sort_order ASC, am.name ASC
                 LIMIT 1"
            ),
            agent_source,
            name,
            user_id,
        )
        .await
    }

    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.find_builtin_by_backend_for_user(DEFAULT_USER_ID, backend).await
    }

    async fn find_builtin_by_backend_for_user(
        &self,
        user_id: &str,
        backend: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe_two_binds(
            &format!(
                "SELECT {AGENT_METADATA_SAFE_COLUMNS} {AGENT_METADATA_FROM}
                 WHERE am.agent_source = 'builtin'
                   AND am.backend = ?
                   AND (am.user_id IS NULL OR am.user_id = ?)
                 ORDER BY am.sort_order ASC, am.name ASC
                 LIMIT 1"
            ),
            backend,
            user_id,
        )
        .await
    }

    async fn upsert(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        if matches!(params.agent_source, "builtin" | "internal") {
            self.upsert_global(params).await
        } else {
            self.upsert_for_user(DEFAULT_USER_ID, params).await
        }
    }

    async fn upsert_for_user(
        &self,
        user_id: &str,
        params: &UpsertAgentMetadataParams<'_>,
    ) -> Result<AgentMetadataRow, DbError> {
        self.upsert_with_user_id(Some(user_id), params).await
    }

    async fn upsert_global(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        self.upsert_with_user_id(None, params).await
    }

    async fn apply_handshake(
        &self,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        // Machine-level handshake (no acting user): update the single catalog
        // row in place regardless of which user owns it. Never creates rows.
        let Some(viewer) = self.catalog_viewer(id).await? else {
            return Ok(None);
        };
        let Some(existing) = self.get_for_user(&viewer, id).await? else {
            return Ok(None);
        };

        let now = now_ms();
        let agent_capabilities = params
            .agent_capabilities
            .map_or(existing.agent_capabilities, |v| v.map(String::from));
        let auth_methods = params
            .auth_methods
            .map_or(existing.auth_methods, |v| v.map(String::from));
        let config_options = params
            .config_options
            .map_or(existing.config_options, |v| v.map(String::from));
        let available_modes = params
            .available_modes
            .map_or(existing.available_modes, |v| v.map(String::from));
        let available_models = params
            .available_models
            .map_or(existing.available_models, |v| v.map(String::from));
        let available_commands = params
            .available_commands
            .map_or(existing.available_commands, |v| v.map(String::from));

        sqlx::query(
            "UPDATE agent_metadata SET \
                agent_capabilities = ?, \
                auth_methods = ?, \
                config_options = ?, \
                available_modes = ?, \
                available_models = ?, \
                available_commands = ?, \
                updated_at = ? \
             WHERE agent_id = ?",
        )
        .bind(&agent_capabilities)
        .bind(&auth_methods)
        .bind(&config_options)
        .bind(&available_modes)
        .bind(&available_models)
        .bind(&available_commands)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        self.get_for_user(&viewer, id).await
    }

    async fn apply_handshake_for_user(
        &self,
        _user_id: &str,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        // Handshake capabilities come from the agent CLI's own login and are
        // machine-level: the acting user is irrelevant, so this writes the
        // single catalog row exactly like the no-user variant.
        self.apply_handshake(id, params).await
    }

    async fn update_availability_snapshot(
        &self,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        // Machine-level probe result (startup `--version` / PATH checks):
        // update the single catalog row in place. Never creates rows.
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE agent_metadata SET \
                last_check_status = ?, \
                last_check_kind = ?, \
                last_check_error_code = ?, \
                last_check_error_message = ?, \
                last_check_guidance = ?, \
                last_check_latency_ms = ?, \
                last_check_at = ?, \
                last_success_at = COALESCE(?, last_success_at), \
                last_failure_at = COALESCE(?, last_failure_at), \
                updated_at = ? \
             WHERE agent_id = ?",
        )
        .bind(params.last_check_status)
        .bind(params.last_check_kind)
        .bind(params.last_check_error_code)
        .bind(params.last_check_error_message)
        .bind(params.last_check_guidance)
        .bind(params.last_check_latency_ms)
        .bind(params.last_check_at)
        .bind(params.last_success_at)
        .bind(params.last_failure_at)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }
        let viewer = self
            .catalog_viewer(id)
            .await?
            .unwrap_or_else(|| DEFAULT_USER_ID.to_string());
        self.get_for_user(&viewer, id).await
    }

    async fn update_availability_snapshot_for_user(
        &self,
        _user_id: &str,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        // Availability reflects whether this machine's agent binary can run,
        // which is machine-level regardless of the acting user: write the
        // single catalog row exactly like the no-user variant.
        self.update_availability_snapshot(id, params).await
    }

    async fn update_agent_overrides(
        &self,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        // Overrides select which binary / environment this machine runs the
        // agent with — machine-level, so they live on the catalog row.
        let now = now_ms();
        sqlx::query(
            "UPDATE agent_metadata SET command_override = ?, env_override = ?, updated_at = ? WHERE agent_id = ?",
        )
        .bind(command_override)
        .bind(env_override)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(DbError::Query)?;
        Ok(())
    }

    async fn update_agent_overrides_for_user(
        &self,
        _user_id: &str,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        // Machine-level: the acting user does not change which binary runs.
        self.update_agent_overrides(id, command_override, env_override).await
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, DbError> {
        self.set_enabled_for_user(DEFAULT_USER_ID, id, enabled).await
    }

    async fn set_enabled_for_user(&self, user_id: &str, id: &str, enabled: bool) -> Result<bool, DbError> {
        // enabled is machine-level: it gates whether the registry starts the
        // agent at all, so it lives on the catalog row and is shared by every
        // user. The user_id only scopes visibility (a custom agent's owner) —
        // the toggle itself is not per-user.
        if self.get_for_user(user_id, id).await?.is_none() {
            return Ok(false);
        }
        let now = now_ms();
        let result = sqlx::query("UPDATE agent_metadata SET enabled = ?, updated_at = ? WHERE agent_id = ?")
            .bind(enabled)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        self.delete_for_user(DEFAULT_USER_ID, id).await
    }

    async fn delete_for_user(&self, user_id: &str, id: &str) -> Result<bool, DbError> {
        // Only the owner may delete a catalog row (builtin rows have no owner
        // and are never deletable through this path).
        let result = sqlx::query("DELETE FROM agent_metadata WHERE agent_id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

impl SqliteAgentMetadataRepository {
    async fn upsert_with_user_id(
        &self,
        user_id: Option<&str>,
        params: &UpsertAgentMetadataParams<'_>,
    ) -> Result<AgentMetadataRow, DbError> {
        let now = now_ms();

        // Single-row catalog: one row per agent_id, whoever owns it. The
        // ownership guard on the conflict clause stops a different owner from
        // hijacking an existing agent_id (`IS` treats two NULLs as equal).
        let conflict_target = "ON CONFLICT(agent_id) DO UPDATE SET";

        let sql = format!(
            "INSERT INTO agent_metadata \
                (id, agent_id, user_id, icon, name, name_i18n, description, description_i18n, \
                 backend, agent_type, agent_source, agent_source_info, \
                 enabled, command, args, env, native_skills_dirs, \
                 behavior_policy, yolo_id, \
                 agent_capabilities, auth_methods, config_options, \
                 available_modes, available_models, available_commands, \
                 sort_order, created_at, updated_at) \
             VALUES (lower(hex(randomblob(16))), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             {conflict_target} \
                icon = excluded.icon, \
                name = excluded.name, \
                name_i18n = excluded.name_i18n, \
                description = excluded.description, \
                description_i18n = excluded.description_i18n, \
                backend = excluded.backend, \
                agent_type = excluded.agent_type, \
                agent_source = excluded.agent_source, \
                agent_source_info = excluded.agent_source_info, \
                enabled = excluded.enabled, \
                command = excluded.command, \
                args = excluded.args, \
                env = excluded.env, \
                native_skills_dirs = excluded.native_skills_dirs, \
                behavior_policy = excluded.behavior_policy, \
                yolo_id = excluded.yolo_id, \
                agent_capabilities = excluded.agent_capabilities, \
                auth_methods = excluded.auth_methods, \
                config_options = excluded.config_options, \
                available_modes = excluded.available_modes, \
                available_models = excluded.available_models, \
                available_commands = excluded.available_commands, \
                sort_order = excluded.sort_order, \
                updated_at = excluded.updated_at \
             WHERE agent_metadata.user_id IS excluded.user_id"
        );

        sqlx::query(&sql)
            .bind(params.id)
            .bind(user_id)
            .bind(params.icon)
            .bind(params.name)
            .bind(params.name_i18n)
            .bind(params.description)
            .bind(params.description_i18n)
            .bind(params.backend)
            .bind(params.agent_type)
            .bind(params.agent_source)
            .bind(params.agent_source_info)
            .bind(params.enabled)
            .bind(params.command)
            .bind(params.args)
            .bind(params.env)
            .bind(params.native_skills_dirs)
            .bind(params.behavior_policy)
            .bind(params.yolo_id)
            .bind(params.agent_capabilities)
            .bind(params.auth_methods)
            .bind(params.config_options)
            .bind(params.available_modes)
            .bind(params.available_models)
            .bind(params.available_commands)
            .bind(params.sort_order)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await?;

        let viewer = user_id.unwrap_or(DEFAULT_USER_ID);
        let row = self
            .get_for_user(viewer, params.id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert did not produce row for id '{}'", params.id)))?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "agent_user_b";

    async fn setup() -> (SqliteAgentMetadataRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
        ensure_user(&db, USER_B).await;
        (repo, db)
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

    async fn corrupt_cache_field(db: &crate::Database, id: &str, field: &str, bytes_hex: &str) {
        assert!(
            matches!(
                field,
                "agent_capabilities"
                    | "auth_methods"
                    | "config_options"
                    | "available_modes"
                    | "available_models"
                    | "available_commands"
            ),
            "test helper only corrupts rebuildable handshake/cache fields"
        );
        let sql = format!("UPDATE agent_metadata SET {field} = CAST(x'{bytes_hex}' AS TEXT) WHERE agent_id = ?");
        sqlx::query(&sql).bind(id).execute(db.pool()).await.unwrap();
    }

    async fn cache_field_blob(db: &crate::Database, id: &str, field: &str) -> Option<Vec<u8>> {
        assert!(
            matches!(
                field,
                "agent_capabilities"
                    | "auth_methods"
                    | "config_options"
                    | "available_modes"
                    | "available_models"
                    | "available_commands"
            ),
            "test helper only reads rebuildable handshake/cache fields"
        );
        let sql = format!("SELECT CAST({field} AS BLOB) FROM agent_metadata WHERE agent_id = ?");
        sqlx::query_scalar::<_, Option<Vec<u8>>>(&sql)
            .bind(id)
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    fn custom_params<'a>(id: &'a str, name: &'a str) -> UpsertAgentMetadataParams<'a> {
        UpsertAgentMetadataParams {
            id,
            icon: None,
            name,
            name_i18n: None,
            description: Some("a custom agent"),
            description_i18n: None,
            backend: Some("claude"),
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(r#"{"binary_name":"claude"}"#),
            enabled: true,
            command: Some("claude"),
            args: Some("[]"),
            env: Some("[]"),
            native_skills_dirs: Some(r#"[".claude/skills"]"#),
            behavior_policy: Some(r#"{"supports_side_question":true}"#),
            yolo_id: Some("bypassPermissions"),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: 1100,
        }
    }

    #[tokio::test]
    async fn seed_rows_populated_after_migrations() {
        let (repo, _db) = setup().await;
        let rows = repo.list_all().await.unwrap();
        // 39 ACP vendors + 2 non-ACP builtins + 1 internal = 42.
        assert_eq!(rows.len(), 42);
        assert!(
            rows.iter()
                .any(|r| r.name == "Claude Code" && r.agent_source == "builtin")
        );
        assert!(
            rows.iter()
                .any(|r| r.name == "Aion CLI" && r.agent_source == "internal")
        );
        // Nanobot and OpenClaw are builtin (not internal).
        assert!(rows.iter().any(|r| r.name == "Nanobot" && r.agent_source == "builtin"));
        assert!(rows.iter().any(|r| r.name == "OpenClaw"
            && r.agent_type == "acp"
            && r.backend.as_deref() == Some("openclaw")
            && r.agent_source == "builtin"));
        assert!(
            rows.iter()
                .any(|r| r.name == "OpenClaw" && r.agent_type == "openclaw-gateway" && r.agent_source == "builtin")
        );
        let hermes = rows
            .iter()
            .find(|r| r.name == "Hermes" && r.agent_source == "builtin")
            .expect("seeded hermes row");
        assert_eq!(hermes.yolo_id, None);
        let codex = rows
            .iter()
            .find(|r| r.name == "Codex CLI" && r.backend.as_deref() == Some("codex") && r.agent_source == "builtin")
            .expect("seeded codex row");
        assert_eq!(codex.yolo_id.as_deref(), Some("agent-full-access"));
        let pi = rows
            .iter()
            .find(|r| r.name == "Pi" && r.backend.as_deref() == Some("pi") && r.agent_source == "builtin")
            .expect("seeded Pi ACP row");
        assert_eq!(pi.command.as_deref(), Some("npx"));
        assert_eq!(pi.args.as_deref(), Some(r#"["-y","pi-acp"]"#));
        assert_eq!(
            pi.agent_source_info.as_deref(),
            Some(r#"{"binary_name":"pi","bridge_binary":"npx"}"#)
        );
    }

    #[tokio::test]
    async fn list_all_clears_invalid_utf8_cache_field_and_keeps_row() {
        let (repo, db) = setup().await;
        corrupt_cache_field(&db, "2d23ff1c", "config_options", "FF").await;

        let rows = repo.list_all().await.unwrap();
        let claude = rows
            .iter()
            .find(|row| row.id == "2d23ff1c")
            .expect("corrupted cache field must not remove the row");

        assert!(claude.config_options.is_none());
        assert_eq!(claude.name, "Claude Code");
        assert_eq!(cache_field_blob(&db, "2d23ff1c", "config_options").await, None);
    }

    #[tokio::test]
    async fn get_clears_invalid_utf8_from_all_rebuildable_cache_fields() {
        let (repo, db) = setup().await;
        for field in [
            "agent_capabilities",
            "auth_methods",
            "config_options",
            "available_modes",
            "available_models",
            "available_commands",
        ] {
            corrupt_cache_field(&db, "2d23ff1c", field, "C3").await;
        }

        let row = repo.get("2d23ff1c").await.unwrap().expect("seed row");

        assert!(row.agent_capabilities.is_none());
        assert!(row.auth_methods.is_none());
        assert!(row.config_options.is_none());
        assert!(row.available_modes.is_none());
        assert!(row.available_models.is_none());
        assert!(row.available_commands.is_none());
        for field in [
            "agent_capabilities",
            "auth_methods",
            "config_options",
            "available_modes",
            "available_models",
            "available_commands",
        ] {
            assert_eq!(
                cache_field_blob(&db, "2d23ff1c", field).await,
                None,
                "{field} should be cleared"
            );
        }
    }

    #[tokio::test]
    async fn find_by_source_and_name_hits_seed_row() {
        let (repo, _db) = setup().await;
        let row = repo
            .find_by_source_and_name("builtin", "Claude Code")
            .await
            .unwrap()
            .expect("seeded claude row");
        assert_eq!(row.backend.as_deref(), Some("claude"));
        assert_eq!(row.agent_type, "acp");
    }

    #[tokio::test]
    async fn seed_rows_include_icon_backfill() {
        let (repo, _db) = setup().await;

        let claude = repo.get("2d23ff1c").await.unwrap().expect("seeded claude row");
        assert_eq!(claude.icon.as_deref(), Some("/api/assets/logos/ai-major/claude.svg"));

        let rows = repo.list_all().await.unwrap();
        let aionrs = rows
            .iter()
            .find(|row| row.agent_type == "aionrs" && row.agent_source == "internal")
            .expect("seeded aion cli row");
        assert_eq!(aionrs.icon.as_deref(), Some("/api/assets/logos/brand/aion.svg"));
        let aionrs_modes: serde_json::Value =
            serde_json::from_str(aionrs.available_modes.as_deref().expect("aionrs modes catalog")).unwrap();
        assert_eq!(aionrs_modes["current_mode_id"].as_str(), Some("default"));
        assert_eq!(
            aionrs_modes["available_modes"]
                .as_array()
                .expect("aionrs available modes")
                .iter()
                .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>(),
            vec!["default", "auto_edit", "yolo"]
        );
        let aionrs_config_options: serde_json::Value =
            serde_json::from_str(aionrs.config_options.as_deref().expect("aionrs config options")).unwrap();
        assert_eq!(
            aionrs_config_options["config_options"][0]["options"][1]["value"].as_str(),
            Some("auto_edit")
        );

        let kiro = repo.get("e044000d").await.unwrap().expect("seeded kiro row");
        assert!(kiro.icon.is_none());
    }

    #[tokio::test]
    async fn builtin_managed_acp_rows_drop_runtime_bridge_command() {
        let (repo, _db) = setup().await;

        let claude = repo.get("2d23ff1c").await.unwrap().expect("seeded claude row");
        assert!(claude.command.is_none());
        assert_eq!(claude.args.as_deref(), Some(r#"[]"#));
        assert_eq!(claude.agent_source_info.as_deref(), Some(r#"{"binary_name":"claude"}"#));

        let codex = repo.get("8e1acf31").await.unwrap().expect("seeded codex row");
        assert!(codex.command.is_none());
        assert_eq!(codex.args.as_deref(), Some(r#"[]"#));
        assert_eq!(codex.agent_source_info.as_deref(), Some(r#"{"binary_name":"codex"}"#));

        let codebuddy = repo.get("8b20fd41").await.unwrap().expect("seeded codebuddy row");
        assert_eq!(codebuddy.command.as_deref(), Some("npx"));
        assert_eq!(
            codebuddy.args.as_deref(),
            Some(r#"["-y","--package","@tencent-ai/codebuddy-code","codebuddy","--acp"]"#)
        );
        assert_eq!(
            codebuddy.agent_source_info.as_deref(),
            Some(r#"{"binary_name":"codebuddy","bridge_binary":"npx"}"#)
        );
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates() {
        let (repo, _db) = setup().await;
        let mut p = custom_params("custom-0001", "my-claude");
        let first = repo.upsert(&p).await.unwrap();
        assert_eq!(first.name, "my-claude");
        assert!(first.enabled);

        p.description = Some("updated");
        p.enabled = false;
        let second = repo.upsert(&p).await.unwrap();
        assert_eq!(second.description.as_deref(), Some("updated"));
        assert!(!second.enabled);
        // No duplicate row introduced.
        let matches: Vec<_> = repo
            .list_all()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.id == "custom-0001")
            .collect();
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn apply_handshake_updates_only_specified_fields() {
        let (repo, _db) = setup().await;
        let updated = repo
            .apply_handshake(
                "2d23ff1c",
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(Some(r#"{"loadSession":true}"#)),
                    auth_methods: Some(Some(r#"[{"id":"oauth"}]"#)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .expect("claude row exists");

        assert_eq!(updated.agent_capabilities.as_deref(), Some(r#"{"loadSession":true}"#));
        assert_eq!(updated.auth_methods.as_deref(), Some(r#"[{"id":"oauth"}]"#));
        assert!(updated.config_options.is_none());
    }

    #[tokio::test]
    async fn apply_handshake_reads_existing_row_through_safe_mapper() {
        let (repo, db) = setup().await;
        corrupt_cache_field(&db, "2d23ff1c", "config_options", "FF").await;

        let updated = repo
            .apply_handshake(
                "2d23ff1c",
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(Some(r#"{"loadSession":true}"#)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .expect("seed row");

        assert_eq!(updated.agent_capabilities.as_deref(), Some(r#"{"loadSession":true}"#));
        assert!(updated.config_options.is_none());
        assert_eq!(cache_field_blob(&db, "2d23ff1c", "config_options").await, None);
    }

    #[tokio::test]
    async fn apply_handshake_can_clear_to_null() {
        let (repo, _db) = setup().await;
        repo.apply_handshake(
            "2d23ff1c",
            &UpdateAgentHandshakeParams {
                agent_capabilities: Some(Some(r#"{"x":1}"#)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let cleared = repo
            .apply_handshake(
                "2d23ff1c",
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.agent_capabilities.is_none());
    }

    #[tokio::test]
    async fn apply_handshake_missing_row_returns_none() {
        let (repo, _db) = setup().await;
        let res = repo
            .apply_handshake(
                "does-not-exist",
                &UpdateAgentHandshakeParams {
                    agent_capabilities: Some(Some("{}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn set_enabled_toggles_flag() {
        let (repo, _db) = setup().await;
        assert!(repo.set_enabled("2d23ff1c", false).await.unwrap());
        let row = repo.get("2d23ff1c").await.unwrap().unwrap();
        assert!(!row.enabled);
        assert!(!repo.set_enabled("missing", true).await.unwrap());
    }

    #[tokio::test]
    async fn update_availability_snapshot_persists_last_check_fields() {
        let (repo, _db) = setup().await;
        let row = repo
            .upsert(&UpsertAgentMetadataParams {
                id: "agent-claude",
                icon: None,
                name: "Claude Code",
                name_i18n: None,
                description: None,
                description_i18n: None,
                backend: Some("claude"),
                agent_type: "acp",
                agent_source: "builtin",
                agent_source_info: None,
                enabled: true,
                command: Some("claude"),
                args: None,
                env: None,
                native_skills_dirs: None,
                behavior_policy: None,
                yolo_id: None,
                agent_capabilities: None,
                auth_methods: None,
                config_options: None,
                available_modes: None,
                available_models: None,
                available_commands: None,
                sort_order: 10,
            })
            .await
            .unwrap();

        repo.update_availability_snapshot(
            &row.id,
            &crate::models::UpdateAgentAvailabilitySnapshotParams {
                last_check_status: Some("available"),
                last_check_kind: Some("manual"),
                last_check_error_code: None,
                last_check_error_message: None,
                last_check_guidance: None,
                last_check_latency_ms: Some(180),
                last_check_at: Some(1_750_000_000_000),
                last_success_at: Some(1_750_000_000_000),
                last_failure_at: None,
            },
        )
        .await
        .unwrap();

        let refreshed = repo.get(&row.id).await.unwrap().unwrap();
        assert_eq!(refreshed.last_check_status.as_deref(), Some("available"));
        assert_eq!(refreshed.last_check_kind.as_deref(), Some("manual"));
        assert_eq!(refreshed.last_check_latency_ms, Some(180));
        assert_eq!(refreshed.last_success_at, Some(1_750_000_000_000));
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (repo, _db) = setup().await;
        let p = custom_params("custom-0002", "throwaway");
        repo.upsert(&p).await.unwrap();
        assert!(repo.delete("custom-0002").await.unwrap());
        assert!(repo.get("custom-0002").await.unwrap().is_none());
        assert!(!repo.delete("custom-0002").await.unwrap());
    }

    #[tokio::test]
    async fn same_source_same_name_allowed_with_different_ids() {
        let (repo, _db) = setup().await;
        let p1 = custom_params("custom-a", "dup");
        let p2 = custom_params("custom-b", "dup");
        repo.upsert(&p1).await.unwrap();
        repo.upsert(&p2).await.unwrap();
        let all = repo.list_all().await.unwrap();
        let dup_count = all
            .iter()
            .filter(|r| r.name == "dup" && r.agent_source == "custom")
            .count();
        assert_eq!(
            dup_count, 2,
            "both rows should coexist after dropping UNIQUE(agent_source,name)"
        );
    }

    #[tokio::test]
    async fn update_agent_overrides_persists_and_leaves_other_columns() {
        let (repo, _db) = setup().await;
        // Seed one agent row
        let p = custom_params("agent-x", "agent-x");
        repo.upsert(&p).await.unwrap();

        repo.update_agent_overrides("agent-x", Some("/real/bin/x"), Some(r#"[{"name":"K","value":"V"}]"#))
            .await
            .unwrap();

        let row = repo.get("agent-x").await.unwrap().unwrap();
        assert_eq!(row.command_override.as_deref(), Some("/real/bin/x"));
        assert_eq!(row.env_override.as_deref(), Some(r#"[{"name":"K","value":"V"}]"#));
        // seed columns untouched
        assert_eq!(row.name, "agent-x");
    }

    #[tokio::test]
    async fn global_builtin_rows_are_visible_to_all_users() {
        let (repo, _db) = setup().await;

        let user_a = repo.get_for_user(USER_A, "2d23ff1c").await.unwrap().unwrap();
        let user_b = repo.get_for_user(USER_B, "2d23ff1c").await.unwrap().unwrap();

        assert_eq!(user_a.name, "Claude Code");
        assert_eq!(user_b.name, "Claude Code");
        assert_eq!(user_a.agent_source, "builtin");
        assert_eq!(user_b.agent_source, "builtin");
    }

    #[tokio::test]
    async fn custom_agent_id_cannot_be_hijacked_by_another_user() {
        let (repo, _db) = setup().await;

        repo.upsert_for_user(USER_A, &custom_params("custom-shared", "User A Custom"))
            .await
            .unwrap();

        // agent_id is globally unique: another user upserting the same id must
        // neither take over nor shadow it — the attempt fails outright.
        let err = repo
            .upsert_for_user(USER_B, &custom_params("custom-shared", "User B Custom"))
            .await;
        assert!(err.is_err(), "cross-user agent_id hijack must be rejected");

        // Owner's row is untouched; the other user cannot see it at all.
        let user_a = repo.get_for_user(USER_A, "custom-shared").await.unwrap().unwrap();
        assert_eq!(user_a.name, "User A Custom");
        assert!(repo.get_for_user(USER_B, "custom-shared").await.unwrap().is_none());

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_metadata WHERE agent_id = 'custom-shared'")
            .fetch_one(_db.pool())
            .await
            .unwrap();
        assert_eq!(count, 1, "the catalog holds exactly one row per agent_id");
    }

    #[tokio::test]
    async fn enable_toggle_is_machine_level_shared_by_all_users() {
        // enabled gates whether the registry starts the agent, so it is
        // machine-level: toggling it through any user updates the single
        // catalog row and every user sees the change. There is no per-user
        // agent enable — that happens one layer up on assistants.
        let (repo, db) = setup().await;

        assert!(repo.set_enabled_for_user(USER_B, "2d23ff1c", false).await.unwrap());

        // Both users read the disabled state off the catalog.
        assert!(!repo.get_for_user(USER_B, "2d23ff1c").await.unwrap().unwrap().enabled);
        assert!(!repo.get_for_user(USER_A, "2d23ff1c").await.unwrap().unwrap().enabled);

        // The catalog row itself flipped, and there is still exactly one row.
        let global_enabled: bool =
            sqlx::query_scalar("SELECT enabled FROM agent_metadata WHERE user_id IS NULL AND agent_id = '2d23ff1c'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(!global_enabled);
        let catalog_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_metadata WHERE agent_id = '2d23ff1c'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(catalog_rows, 1, "toggles must never copy catalog rows");
    }

    #[tokio::test]
    async fn overrides_are_machine_level_shared_by_all_users() {
        // command/env overrides pick which binary+environment THIS machine runs
        // the agent with — machine-level, so they land on the catalog row and
        // every user sees them, regardless of who set them.
        let (repo, db) = setup().await;

        repo.update_agent_overrides_for_user(USER_B, "2d23ff1c", Some("/tmp/claude"), Some("[]"))
            .await
            .unwrap();

        // Both users read the same override off the catalog.
        let user_b = repo.get_for_user(USER_B, "2d23ff1c").await.unwrap().unwrap();
        let user_a = repo.get_for_user(USER_A, "2d23ff1c").await.unwrap().unwrap();
        assert_eq!(user_b.command_override.as_deref(), Some("/tmp/claude"));
        assert_eq!(user_a.command_override.as_deref(), Some("/tmp/claude"));

        // The write mutated the single catalog row, not a per-user delta.
        let global_override: Option<String> = sqlx::query_scalar(
            "SELECT command_override FROM agent_metadata WHERE user_id IS NULL AND agent_id = '2d23ff1c'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(global_override.as_deref(), Some("/tmp/claude"));
    }
}
