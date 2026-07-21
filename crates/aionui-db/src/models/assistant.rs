//! Row models and repository parameter structs for the assistants domain.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `assistants` table (user-authored assistants only).
///
/// JSON-encoded columns (`enabled_skills`, `custom_skill_names`,
/// `disabled_builtin_skills`, `prompts`, `models`, `*_i18n`) stay as opaque
/// strings at this layer; the service deserializes them.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub enabled_skills: Option<String>,
    pub custom_skill_names: Option<String>,
    pub disabled_builtin_skills: Option<String>,
    pub prompts: Option<String>,
    pub models: Option<String>,
    pub name_i18n: Option<String>,
    pub description_i18n: Option<String>,
    pub prompts_i18n: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `assistant_overrides` table (per-assistant user state).
///
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantOverrideRow {
    pub assistant_id: String,
    pub enabled: bool,
    pub sort_order: i32,
    pub last_used_at: Option<TimestampMs>,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `assistant_definitions` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantDefinitionRow {
    pub id: String,
    pub assistant_id: String,
    pub source: String,
    pub owner_type: String,
    pub source_ref: Option<String>,
    pub name: String,
    pub name_i18n: String,
    pub description: Option<String>,
    pub description_i18n: String,
    pub avatar_type: String,
    pub avatar_value: Option<String>,
    pub agent_id: String,
    pub rule_resource_type: String,
    pub rule_resource_ref: Option<String>,
    pub recommended_prompts: String,
    pub recommended_prompts_i18n: String,
    pub default_model_mode: String,
    pub default_model_value: Option<String>,
    pub default_permission_mode: String,
    pub default_permission_value: Option<String>,
    pub default_thought_level_mode: String,
    pub default_thought_level_value: Option<String>,
    pub default_skills_mode: String,
    pub default_skill_ids: String,
    pub custom_skill_names: String,
    pub default_disabled_builtin_skill_ids: String,
    pub default_mcps_mode: String,
    pub default_mcp_ids: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub deleted_at: Option<TimestampMs>,
}

/// Row mapping for the `assistant_overlays` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantOverlayRow {
    pub assistant_definition_id: String,
    pub enabled: bool,
    pub sort_order: i32,
    pub agent_id_override: Option<String>,
    pub last_used_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `assistant_preferences` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantPreferenceRow {
    pub assistant_definition_id: String,
    pub last_model_id: Option<String>,
    pub last_permission_value: Option<String>,
    pub last_thought_level_value: Option<String>,
    pub last_skill_ids: String,
    pub last_disabled_builtin_skill_ids: String,
    pub last_mcp_ids: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Insert parameters for `IAssistantRepository::create` / `::upsert`.
///
/// JSON fields are pre-serialized strings so the repository layer stays
/// agnostic to how the service encodes them.
#[derive(Debug, Clone)]
pub struct CreateAssistantParams<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub avatar: Option<&'a str>,
    pub enabled_skills: Option<&'a str>,
    pub custom_skill_names: Option<&'a str>,
    pub disabled_builtin_skills: Option<&'a str>,
    pub prompts: Option<&'a str>,
    pub models: Option<&'a str>,
    pub name_i18n: Option<&'a str>,
    pub description_i18n: Option<&'a str>,
    pub prompts_i18n: Option<&'a str>,
}

/// Partial update parameters for `IAssistantRepository::update`.
///
/// Every field is `Option` — `None` keeps the current value.
#[derive(Debug, Clone, Default)]
pub struct UpdateAssistantParams<'a> {
    pub name: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub avatar: Option<Option<&'a str>>,
    pub enabled_skills: Option<Option<&'a str>>,
    pub custom_skill_names: Option<Option<&'a str>>,
    pub disabled_builtin_skills: Option<Option<&'a str>>,
    pub prompts: Option<Option<&'a str>>,
    pub models: Option<Option<&'a str>>,
    pub name_i18n: Option<Option<&'a str>>,
    pub description_i18n: Option<Option<&'a str>>,
    pub prompts_i18n: Option<Option<&'a str>>,
}

/// Upsert parameters for `IAssistantOverrideRepository::upsert`.
///
#[derive(Debug, Clone, Default)]
pub struct UpsertOverrideParams<'a> {
    pub assistant_id: &'a str,
    pub enabled: bool,
    pub sort_order: i32,
    pub last_used_at: Option<TimestampMs>,
}

/// Insert-or-update parameters for `assistant_definitions`.
#[derive(Debug, Clone)]
pub struct UpsertAssistantDefinitionParams<'a> {
    pub id: &'a str,
    pub assistant_id: &'a str,
    pub source: &'a str,
    pub owner_type: &'a str,
    pub source_ref: Option<&'a str>,
    pub name: &'a str,
    pub name_i18n: &'a str,
    pub description: Option<&'a str>,
    pub description_i18n: &'a str,
    pub avatar_type: &'a str,
    pub avatar_value: Option<&'a str>,
    pub agent_id: &'a str,
    pub rule_resource_type: &'a str,
    pub rule_resource_ref: Option<&'a str>,
    pub recommended_prompts: &'a str,
    pub recommended_prompts_i18n: &'a str,
    pub default_model_mode: &'a str,
    pub default_model_value: Option<&'a str>,
    pub default_permission_mode: &'a str,
    pub default_permission_value: Option<&'a str>,
    pub default_thought_level_mode: &'a str,
    pub default_thought_level_value: Option<&'a str>,
    pub default_skills_mode: &'a str,
    pub default_skill_ids: &'a str,
    pub custom_skill_names: &'a str,
    pub default_disabled_builtin_skill_ids: &'a str,
    pub default_mcps_mode: &'a str,
    pub default_mcp_ids: &'a str,
}

/// Insert-or-update parameters for `assistant_overlays`.
#[derive(Debug, Clone)]
pub struct UpsertAssistantOverlayParams<'a> {
    pub assistant_definition_id: &'a str,
    pub enabled: bool,
    pub sort_order: i32,
    pub agent_id_override: Option<&'a str>,
    pub last_used_at: Option<TimestampMs>,
}

/// Insert-or-update parameters for `assistant_preferences`.
#[derive(Debug, Clone)]
pub struct UpsertAssistantPreferenceParams<'a> {
    pub assistant_definition_id: &'a str,
    pub last_model_id: Option<&'a str>,
    pub last_permission_value: Option<&'a str>,
    pub last_thought_level_value: Option<&'a str>,
    pub last_skill_ids: &'a str,
    pub last_disabled_builtin_skill_ids: &'a str,
    pub last_mcp_ids: &'a str,
}
