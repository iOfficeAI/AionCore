//! HTTP contract types for `/api/assistants/*`.
//!
//! Mirror of `src/common/types/assistantTypes.ts` on the frontend; any
//! shape change must land in the same PR on both sides.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Response + source enum
// ---------------------------------------------------------------------------

/// Origin of an assistant in the merged list.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AssistantSource {
    Builtin,
    User,
}

/// Wire shape returned by `GET /api/assistants` (single element) and
/// by the single-resource CRUD handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantResponse {
    pub id: String,
    pub source: AssistantSource,
    pub name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub name_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub description_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    pub enabled: bool,
    pub sort_order: i32,
    pub preset_agent_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_skill_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_builtin_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub prompts_i18n: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantProfileResponse {
    pub name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub name_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub description_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantStateResponse {
    pub enabled: bool,
    pub sort_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantEngineResponse {
    pub agent_backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantRulesResponse {
    pub content: String,
    pub storage_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantPromptsResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub recommended_i18n: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDefaultScalarResponse {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDefaultListResponse {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDefaultScalarRequest {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDefaultListRequest {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AssistantDefaultsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AssistantDefaultScalarRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<AssistantDefaultScalarRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<AssistantDefaultListRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcps: Option<AssistantDefaultListRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDefaultsResponse {
    pub model: AssistantDefaultScalarResponse,
    pub permission: AssistantDefaultScalarResponse,
    pub skills: AssistantDefaultListResponse,
    pub mcps: AssistantDefaultListResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantCapabilitiesResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_skill_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_skill_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_disabled_builtin_skill_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantPreferencesResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_permission_value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_skill_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_disabled_builtin_skill_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_mcp_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDetailResponse {
    pub id: String,
    pub source: AssistantSource,
    pub profile: AssistantProfileResponse,
    pub state: AssistantStateResponse,
    pub engine: AssistantEngineResponse,
    pub rules: AssistantRulesResponse,
    pub prompts: AssistantPromptsResponse,
    pub defaults: AssistantDefaultsResponse,
    pub capabilities: AssistantCapabilitiesResponse,
    pub preferences: AssistantPreferencesResponse,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// `POST /api/assistants`. Server generates `id` when absent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateAssistantRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub preset_agent_type: Option<String>,
    #[serde(default)]
    pub enabled_skills: Option<Vec<String>>,
    #[serde(default)]
    pub custom_skill_names: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_builtin_skills: Option<Vec<String>>,
    #[serde(default)]
    pub prompts: Option<Vec<String>>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub name_i18n: Option<HashMap<String, String>>,
    #[serde(default)]
    pub description_i18n: Option<HashMap<String, String>>,
    #[serde(default)]
    pub prompts_i18n: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub recommended_prompts: Option<Vec<String>>,
    #[serde(default)]
    pub recommended_prompts_i18n: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub defaults: Option<AssistantDefaultsRequest>,
}

/// `PUT /api/assistants/{id}`. All fields optional; partial update semantics.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UpdateAssistantRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub preset_agent_type: Option<String>,
    #[serde(default)]
    pub enabled_skills: Option<Vec<String>>,
    #[serde(default)]
    pub custom_skill_names: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_builtin_skills: Option<Vec<String>>,
    #[serde(default)]
    pub prompts: Option<Vec<String>>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub name_i18n: Option<HashMap<String, String>>,
    #[serde(default)]
    pub description_i18n: Option<HashMap<String, String>>,
    #[serde(default)]
    pub prompts_i18n: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub recommended_prompts: Option<Vec<String>>,
    #[serde(default)]
    pub recommended_prompts_i18n: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub defaults: Option<AssistantDefaultsRequest>,
}

/// `PATCH /api/assistants/{id}/state`. Upserts `assistant_overrides`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SetAssistantStateRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

/// `POST /api/assistants/import`. Bulk insert-only from legacy Electron
/// config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportAssistantsRequest {
    pub assistants: Vec<CreateAssistantRequest>,
}

/// Aggregate result of `POST /api/assistants/import`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportAssistantsResult {
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
    #[serde(default)]
    pub errors: Vec<ImportError>,
}

/// Per-row error within [`ImportAssistantsResult::errors`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportError {
    pub id: String,
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_source_serializes_lowercase() {
        let json = serde_json::to_string(&AssistantSource::Builtin).unwrap();
        assert_eq!(json, "\"builtin\"");
        let json = serde_json::to_string(&AssistantSource::User).unwrap();
        assert_eq!(json, "\"user\"");
    }

    #[test]
    fn assistant_response_round_trip_snake_case() {
        let resp = AssistantResponse {
            id: "a1".into(),
            source: AssistantSource::User,
            name: "Name".into(),
            name_i18n: HashMap::new(),
            description: None,
            description_i18n: HashMap::new(),
            avatar: None,
            enabled: true,
            sort_order: 5,
            preset_agent_type: "gemini".into(),
            enabled_skills: vec![],
            custom_skill_names: vec![],
            disabled_builtin_skills: vec![],
            context: None,
            context_i18n: HashMap::new(),
            prompts: vec![],
            prompts_i18n: HashMap::new(),
            models: vec![],
            last_used_at: Some(1_234),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["preset_agent_type"], "gemini");
        assert_eq!(json["sort_order"], 5);
        assert_eq!(json["last_used_at"], 1234);
    }

    #[test]
    fn create_assistant_request_accepts_minimal_body() {
        let json = serde_json::json!({ "name": "X" });
        let req: CreateAssistantRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name, "X");
        assert!(req.id.is_none());
        assert!(req.preset_agent_type.is_none());
        assert!(req.defaults.is_none());
    }

    #[test]
    fn update_assistant_request_supports_partial() {
        let json = serde_json::json!({ "name": "renamed" });
        let req: UpdateAssistantRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name.as_deref(), Some("renamed"));
        assert!(req.description.is_none());
        assert!(req.defaults.is_none());
    }

    #[test]
    fn create_request_accepts_defaults_and_recommended_prompts() {
        let json = serde_json::json!({
            "name": "planner",
            "recommended_prompts": ["Plan work"],
            "defaults": {
                "model": { "mode": "fixed", "value": "openai/gpt-5" },
                "skills": { "mode": "fixed", "value": ["skill-a"] }
            }
        });
        let req: CreateAssistantRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.recommended_prompts.unwrap(), vec!["Plan work"]);
        let defaults = req.defaults.unwrap();
        assert_eq!(defaults.model.unwrap().mode, "fixed");
        assert_eq!(defaults.skills.unwrap().value, vec!["skill-a"]);
    }

    #[test]
    fn set_state_request_all_optional() {
        let req: SetAssistantStateRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(req.enabled.is_none());
        assert!(req.sort_order.is_none());
        assert!(req.last_used_at.is_none());
    }

    #[test]
    fn import_result_default_is_zeroes() {
        let r = ImportAssistantsResult::default();
        assert_eq!(r.imported, 0);
        assert_eq!(r.skipped, 0);
        assert_eq!(r.failed, 0);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn assistant_response_rejects_camel_case() {
        // Body has BOTH snake_case (valid required values) AND camelCase aliases.
        // Prove: snake is consumed; camel is silently ignored (NOT aliased over snake).
        let json = serde_json::json!({
            "id": "a1",
            "source": "user",
            "name": "X",
            "enabled": true,
            "sort_order": 7,                   // snake required field
            "preset_agent_type": "gemini",     // snake required field
            "presetAgentType": "claude",       // legacy camel — must be ignored
            "sortOrder": 99,                   // legacy camel — must be ignored
            "lastUsedAt": 111_222,             // legacy camel for optional field — must be ignored
        });
        let resp: AssistantResponse = serde_json::from_value(json).unwrap();
        // If camel were aliased, these would be the camel values.
        assert_eq!(
            resp.preset_agent_type, "gemini",
            "snake_case preset_agent_type must win"
        );
        assert_eq!(resp.sort_order, 7, "snake_case sort_order must win");
        assert!(
            resp.last_used_at.is_none(),
            "camelCase lastUsedAt must NOT alias into last_used_at"
        );
    }
}
