//! Own DTOs for the legacy (pre-1.0) ACP session-model surface.
//!
//! The upstream SDK removed `ModelInfo` / `SessionModelState` /
//! `SetSessionModelRequest` in 0.13.5 (model selection moved to session
//! config options), but old-camp agents (Gemini CLI, Qwen Code, Hermes, ...)
//! still advertise model catalogs and accept `session/set_model` on the
//! wire. These types keep that surface parseable client-side, deliberately
//! accepting `modelId` (wire spec), `model_id` (defensive vs peer SDK
//! drift) and bare `id` — the last is our own normalized cache format, so
//! cache re-reads and live frames share one parse path.
//!
//! Field names mirror the removed SDK types so existing consumers stay
//! source-compatible.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One selectable model advertised by a legacy-surface agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyModelEntry {
    #[serde(rename = "modelId", alias = "model_id", alias = "id")]
    pub model_id: String,
    #[serde(default, alias = "label")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl LegacyModelEntry {
    pub fn new(model_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            name: name.into(),
            description: None,
        }
    }

    /// Builder-style description setter (parity with the removed SDK type).
    #[must_use]
    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }
}

/// Model catalog plus current selection for a legacy-surface session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacySessionModelState {
    #[serde(rename = "currentModelId", alias = "current_model_id", default)]
    pub current_model_id: String,
    #[serde(rename = "availableModels", alias = "available_models", default)]
    pub available_models: Vec<LegacyModelEntry>,
}

impl LegacySessionModelState {
    pub fn new(current_model_id: impl Into<String>, available_models: Vec<LegacyModelEntry>) -> Self {
        Self {
            current_model_id: current_model_id.into(),
            available_models,
        }
    }

    /// Parse a runtime model-state update: the `models` value of a
    /// `session/new` / `session/load` response, or a model-state
    /// `session/update` notification payload.
    ///
    /// Requires the current-model key to be present (updates without it were
    /// rejected by the old typed parse too); tolerates an empty catalog —
    /// the session aggregate preserves the existing catalog for empty
    /// runtime updates.
    pub fn from_state_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let current_model_id = string_field(object, "currentModelId", "current_model_id")?;
        let available_models = object
            .get("availableModels")
            .or_else(|| object.get("available_models"))
            .map(parse_entries_lenient)
            .unwrap_or_default();
        Some(Self {
            current_model_id,
            available_models,
        })
    }

    /// Parse a persisted/handshake catalog column: our own normalized
    /// `{available_models:[{id,label}]}` shape or a raw wire catalog.
    ///
    /// Requires a non-empty catalog; falls back to the first entry when the
    /// current id is absent (parity with the previous extraction chain).
    pub fn from_catalog_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let entries_value = object
            .get("available_models")
            .or_else(|| object.get("availableModels"))?;
        let available_models = parse_entries_lenient(entries_value);
        if available_models.is_empty() {
            return None;
        }
        let current_model_id = string_field(object, "currentModelId", "current_model_id")
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| available_models[0].model_id.clone());
        Some(Self {
            current_model_id,
            available_models,
        })
    }
}

fn string_field(object: &serde_json::Map<String, Value>, camel: &str, snake: &str) -> Option<String> {
    object
        .get(camel)
        .or_else(|| object.get(snake))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Parse catalog entries one by one, skipping malformed items with a warning
/// (parity with the old SDK's skip-invalid-items behaviour). Entries missing
/// a display name fall back to the model id.
fn parse_entries_lenient(value: &Value) -> Vec<LegacyModelEntry> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(
            |entry| match serde_json::from_value::<LegacyModelEntry>(entry.clone()) {
                Ok(mut entry) if !entry.model_id.is_empty() => {
                    if entry.name.is_empty() {
                        entry.name = entry.model_id.clone();
                    }
                    Some(entry)
                }
                Ok(_) => {
                    tracing::warn!("skipped legacy model list entry with empty model id");
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "skipped malformed legacy model list entry");
                    None
                }
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_all_three_model_id_spellings() {
        // camelCase (wire spec), snake_case (defensive vs peer drift),
        // bare id (our own normalized cache format).
        for (payload, expect) in [
            (r#"{"modelId":"m1","name":"A"}"#, "m1"),
            (r#"{"model_id":"m2","name":"B"}"#, "m2"),
            (r#"{"id":"m3","name":"C"}"#, "m3"),
        ] {
            let entry: LegacyModelEntry = serde_json::from_str(payload).unwrap();
            assert_eq!(entry.model_id, expect);
        }
    }

    #[test]
    fn accepts_label_alias_for_name() {
        let entry: LegacyModelEntry = serde_json::from_str(r#"{"id":"m","label":"Label"}"#).unwrap();
        assert_eq!(entry.name, "Label");
    }

    #[test]
    fn tolerates_unknown_fields_and_missing_description() {
        let entry: LegacyModelEntry =
            serde_json::from_str(r#"{"modelId":"m","name":"N","_meta":{"x":1},"extra":true}"#).unwrap();
        assert_eq!(entry.model_id, "m");
        assert_eq!(entry.description, None);
    }

    #[test]
    fn state_parses_own_cache_format_without_warnings_shape() {
        // The exact shape our catalog writeback persists (Appendix D):
        // one lenient pass must succeed on the first attempt.
        let value = json!({
            "current_model_id": "deepseek-chat",
            "available_models": [
                {"id": "deepseek-chat", "label": "DeepSeek Chat"},
                {"id": "deepseek-reasoner", "label": "DeepSeek Reasoner"}
            ]
        });

        let state = LegacySessionModelState::from_catalog_value(&value).expect("cache shape");
        assert_eq!(state.current_model_id, "deepseek-chat");
        assert_eq!(state.available_models.len(), 2);
        assert_eq!(state.available_models[1].name, "DeepSeek Reasoner");
    }

    #[test]
    fn state_parses_wire_camel_case_catalog() {
        let value = json!({
            "currentModelId": "gemini-2.5-pro",
            "availableModels": [
                {"modelId": "gemini-2.5-pro", "name": "Gemini 2.5 Pro", "description": "flagship"},
                {"modelId": "gemini-2.5-flash", "name": "Gemini 2.5 Flash"}
            ]
        });

        let state = LegacySessionModelState::from_catalog_value(&value).expect("wire shape");
        assert_eq!(state.current_model_id, "gemini-2.5-pro");
        assert_eq!(state.available_models[0].description.as_deref(), Some("flagship"));
    }

    #[test]
    fn catalog_falls_back_to_first_entry_when_current_missing() {
        let value = json!({
            "availableModels": [
                {"modelId": "first", "name": "First"},
                {"modelId": "second", "name": "Second"}
            ]
        });

        let state = LegacySessionModelState::from_catalog_value(&value).expect("catalog");
        assert_eq!(state.current_model_id, "first");
    }

    #[test]
    fn catalog_rejects_empty_or_missing_lists() {
        assert!(LegacySessionModelState::from_catalog_value(&json!({"availableModels": []})).is_none());
        assert!(LegacySessionModelState::from_catalog_value(&json!({"currentModelId": "x"})).is_none());
        assert!(LegacySessionModelState::from_catalog_value(&json!("not an object")).is_none());
    }

    #[test]
    fn catalog_skips_malformed_entries_and_keeps_good_ones() {
        let value = json!({
            "availableModels": [
                {"name": "no id at all"},
                {"modelId": "good", "name": "Good"},
                42
            ]
        });

        let state = LegacySessionModelState::from_catalog_value(&value).expect("catalog");
        assert_eq!(state.available_models.len(), 1);
        assert_eq!(state.available_models[0].model_id, "good");
    }

    #[test]
    fn state_requires_current_model_key_but_tolerates_empty_catalog() {
        // Empty runtime update: the aggregate preserves the existing catalog.
        let state = LegacySessionModelState::from_state_value(&json!({"currentModelId": "m"})).expect("state");
        assert_eq!(state.current_model_id, "m");
        assert!(state.available_models.is_empty());

        // No current-model key: not a state update — reject like the old typed parse.
        assert!(LegacySessionModelState::from_state_value(&json!({"availableModels": []})).is_none());
    }

    #[test]
    fn entry_name_falls_back_to_model_id() {
        let value = json!({
            "currentModelId": "bare",
            "availableModels": [{"modelId": "bare"}]
        });

        let state = LegacySessionModelState::from_state_value(&value).expect("state");
        assert_eq!(state.available_models[0].name, "bare");
    }

    #[test]
    fn serializes_back_to_wire_camel_case() {
        let state = LegacySessionModelState::new("m1", vec![LegacyModelEntry::new("m1", "One")]);
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(
            value,
            json!({
                "currentModelId": "m1",
                "availableModels": [{"modelId": "m1", "name": "One"}]
            })
        );
    }
}
