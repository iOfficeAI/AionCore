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

const AGENT_METADATA_SAFE_COLUMNS: &str = "\
    id, icon, name, name_i18n, description, description_i18n, \
    backend, agent_type, agent_source, agent_source_info, \
    enabled, command, args, env, native_skills_dirs, \
    behavior_policy, yolo_id, \
    CAST(agent_capabilities AS BLOB) AS agent_capabilities, \
    CAST(auth_methods AS BLOB) AS auth_methods, \
    CAST(config_options AS BLOB) AS config_options, \
    CAST(available_modes AS BLOB) AS available_modes, \
    CAST(available_models AS BLOB) AS available_models, \
    CAST(available_commands AS BLOB) AS available_commands, \
    sort_order, \
    last_check_status, last_check_kind, last_check_error_code, last_check_error_message, \
    last_check_guidance, last_check_latency_ms, last_check_at, last_success_at, last_failure_at, \
    command_override, env_override, created_at, updated_at";

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

    async fn fetch_all_safe(&self, sql: &str) -> Result<Vec<AgentMetadataRow>, DbError> {
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        let mut decoded = Vec::with_capacity(rows.len());
        for row in rows {
            decoded.push(self.decode_and_repair(row).await?);
        }
        Ok(decoded)
    }

    async fn fetch_optional_safe(&self, sql: &str, bind: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        let row = sqlx::query(sql).bind(bind).fetch_optional(&self.pool).await?;
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

    async fn decode_and_repair(&self, row: SqliteRow) -> Result<AgentMetadataRow, DbError> {
        let safe = AgentMetadataSafeRow::from_sqlite_row(row)?;
        let (model, invalid_fields) = safe.into_model();
        if !invalid_fields.is_empty() {
            self.clear_invalid_utf8_cache_fields(&model.id, &invalid_fields).await;
        }
        Ok(model)
    }

    async fn clear_invalid_utf8_cache_fields(&self, id: &str, fields: &[AgentMetadataCacheField]) {
        for field in fields {
            warn!(
                table = "agent_metadata",
                row_id = %id,
                field = field.column_name(),
                action = "clear_invalid_utf8",
                "Clearing invalid UTF-8 from rebuildable agent metadata cache field"
            );
        }

        for field in fields {
            let sql = format!(
                "UPDATE agent_metadata SET {} = NULL, updated_at = ? WHERE id = ?",
                field.column_name()
            );

            if let Err(err) = sqlx::query(&sql).bind(now_ms()).bind(id).execute(&self.pool).await {
                warn!(
                    table = "agent_metadata",
                    row_id = %id,
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
        self.fetch_all_safe(&format!(
            "SELECT {AGENT_METADATA_SAFE_COLUMNS} FROM agent_metadata ORDER BY sort_order ASC, name ASC"
        ))
        .await
    }

    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe(
            &format!("SELECT {AGENT_METADATA_SAFE_COLUMNS} FROM agent_metadata WHERE id = ?"),
            id,
        )
        .await
    }

    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe_two_binds(
            &format!("SELECT {AGENT_METADATA_SAFE_COLUMNS} FROM agent_metadata WHERE agent_source = ? AND name = ?"),
            agent_source,
            name,
        )
        .await
    }

    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.fetch_optional_safe(
            &format!(
                "SELECT {AGENT_METADATA_SAFE_COLUMNS} FROM agent_metadata \
                 WHERE agent_source = 'builtin' AND backend = ? \
                 ORDER BY sort_order ASC, name ASC LIMIT 1"
            ),
            backend,
        )
        .await
    }

    async fn upsert(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        let now = now_ms();

        sqlx::query(
            "INSERT INTO agent_metadata \
                (id, icon, name, name_i18n, description, description_i18n, \
                 backend, agent_type, agent_source, agent_source_info, \
                 enabled, command, args, env, native_skills_dirs, \
                 behavior_policy, yolo_id, \
                 agent_capabilities, auth_methods, config_options, \
                 available_modes, available_models, available_commands, \
                 sort_order, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
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
                updated_at = excluded.updated_at",
        )
        .bind(params.id)
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

        let row = self
            .get(params.id)
            .await?
            .ok_or_else(|| DbError::Init(format!("upsert did not produce row for id '{}'", params.id)))?;
        Ok(row)
    }

    async fn apply_handshake(
        &self,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        let Some(existing) = self.get(id).await? else {
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
             WHERE id = ?",
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

        self.get(id).await
    }

    async fn update_availability_snapshot(
        &self,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
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
                last_success_at = ?, \
                last_failure_at = ?, \
                updated_at = ? \
             WHERE id = ?",
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

        self.get(id).await
    }

    async fn update_agent_overrides(
        &self,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE agent_metadata SET command_override = ?, env_override = ?, \
             updated_at = ? WHERE id = ?",
        )
        .bind(command_override)
        .bind(env_override)
        .bind(aionui_common::now_ms())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(DbError::Query)?;
        Ok(())
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, DbError> {
        let now = now_ms();
        let result = sqlx::query("UPDATE agent_metadata SET enabled = ?, updated_at = ? WHERE id = ?")
            .bind(enabled)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete(&self, id: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM agent_metadata WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteAgentMetadataRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
        (repo, db)
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
        let sql = format!("UPDATE agent_metadata SET {field} = CAST(x'{bytes_hex}' AS TEXT) WHERE id = ?");
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
        let sql = format!("SELECT CAST({field} AS BLOB) FROM agent_metadata WHERE id = ?");
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
        // 18 ACP vendors + 2 non-ACP builtins + 1 internal = 21.
        assert_eq!(rows.len(), 21);
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
            Some(r#"["-y","--package","@tencent-ai/codebuddy-code@2.97.0","codebuddy","--acp"]"#)
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
}
