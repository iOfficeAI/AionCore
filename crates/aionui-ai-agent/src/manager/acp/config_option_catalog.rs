use agent_client_protocol::schema::{
    ModelInfo, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionMode, SessionModeState, SessionModelState,
};
use aionui_api_types::{AgentHandshake, ModelInfoEntry, ModelInfoPayload};
use aionui_common::normalize_keys_to_snake_case;
use serde_json::{Map, Value};

pub(crate) fn derive_modes_from_config_options(options: &[SessionConfigOption]) -> Option<SessionModeState> {
    let select = find_select(options, &["mode", "modes"], &SessionConfigOptionCategory::Mode)?;
    let available_modes: Vec<SessionMode> = flatten_select_options(&select.options)
        .into_iter()
        .map(|option| {
            SessionMode::new(option.value.to_string(), option.name.clone()).description(option.description.clone())
        })
        .collect();

    if available_modes.is_empty() {
        return None;
    }

    let current_mode_id = non_empty_or_first(select.current_value.to_string(), &available_modes[0].id.to_string());
    Some(SessionModeState::new(current_mode_id, available_modes))
}

pub(crate) fn derive_models_from_config_options(options: &[SessionConfigOption]) -> Option<SessionModelState> {
    let select = find_select(options, &["model", "models"], &SessionConfigOptionCategory::Model)?;
    let available_models: Vec<ModelInfo> = flatten_select_options(&select.options)
        .into_iter()
        .map(|option| {
            ModelInfo::new(option.value.to_string(), option.name.clone()).description(option.description.clone())
        })
        .collect();

    if available_models.is_empty() {
        return None;
    }

    let current_model_id = non_empty_or_first(
        select.current_value.to_string(),
        &available_models[0].model_id.to_string(),
    );
    Some(SessionModelState::new(current_model_id, available_models))
}

pub(crate) fn enrich_handshake_with_existing_config_option_catalog(
    handshake: &AgentHandshake,
    existing: Option<&AgentHandshake>,
) -> AgentHandshake {
    let mut enriched = handshake.clone();
    let Some(config_options) = handshake
        .config_options
        .as_ref()
        .and_then(extract_config_options_from_value)
    else {
        return enriched;
    };

    if should_fill_catalog(
        &enriched.available_modes,
        existing.and_then(|handshake| handshake.available_modes.as_ref()),
        "available_modes",
        "availableModes",
    ) && let Some(modes) = derive_modes_from_config_options(&config_options)
        && let Some(value) = mode_state_to_snake_value(&modes)
    {
        enriched.available_modes = Some(value);
    }

    if should_fill_catalog(
        &enriched.available_models,
        existing.and_then(|handshake| handshake.available_models.as_ref()),
        "available_models",
        "availableModels",
    ) && let Some(models) = derive_models_from_config_options(&config_options)
        && let Some(value) = model_state_to_payload_value(&models)
    {
        enriched.available_models = Some(value);
    }

    enriched
}

pub(crate) fn extract_config_options_from_value(value: &Value) -> Option<Vec<SessionConfigOption>> {
    decode_config_options(value).or_else(|| {
        value
            .as_object()
            .and_then(|map| map.get("config_options").or_else(|| map.get("configOptions")))
            .and_then(decode_config_options)
    })
}

fn find_select<'a>(
    options: &'a [SessionConfigOption],
    ids: &[&str],
    category: &SessionConfigOptionCategory,
) -> Option<&'a agent_client_protocol::schema::SessionConfigSelect> {
    options
        .iter()
        .find_map(|option| {
            if option.category.as_ref() == Some(category) {
                return select_from_kind(&option.kind);
            }
            None
        })
        .or_else(|| {
            options.iter().find_map(|option| {
                let id = option.id.to_string();
                if !ids.iter().any(|candidate| *candidate == id) {
                    return None;
                }
                select_from_kind(&option.kind)
            })
        })
}

fn select_from_kind(kind: &SessionConfigKind) -> Option<&agent_client_protocol::schema::SessionConfigSelect> {
    match kind {
        SessionConfigKind::Select(select) => Some(select),
        _ => None,
    }
}

fn flatten_select_options(options: &SessionConfigSelectOptions) -> Vec<&SessionConfigSelectOption> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups.iter().flat_map(|group| group.options.iter()).collect(),
        _ => Vec::new(),
    }
}

fn decode_config_options(value: &Value) -> Option<Vec<SessionConfigOption>> {
    serde_json::from_value(value.clone())
        .ok()
        .or_else(|| serde_json::from_value(keys_to_camel_case(value.clone())).ok())
}

fn mode_state_to_snake_value(modes: &SessionModeState) -> Option<Value> {
    let mut value = serde_json::to_value(modes).ok()?;
    normalize_keys_to_snake_case(&mut value);
    Some(value)
}

fn model_state_to_payload_value(models: &SessionModelState) -> Option<Value> {
    let available_models: Vec<ModelInfoEntry> = models
        .available_models
        .iter()
        .map(|model| ModelInfoEntry {
            id: model.model_id.to_string(),
            label: model.name.clone(),
        })
        .collect();
    let current_model_id = models.current_model_id.to_string();
    let current_model_label = available_models
        .iter()
        .find(|model| model.id == current_model_id)
        .map(|model| model.label.clone())
        .unwrap_or_else(|| current_model_id.clone());

    serde_json::to_value(ModelInfoPayload {
        current_model_id: Some(current_model_id),
        current_model_label: Some(current_model_label),
        available_models,
    })
    .ok()
}

fn has_non_empty_catalog(value: &Option<Value>, snake_key: &str, camel_key: &str) -> bool {
    value
        .as_ref()
        .is_some_and(|value| has_non_empty_catalog_value(value, snake_key, camel_key))
}

fn has_non_empty_catalog_value(value: &Value, snake_key: &str, camel_key: &str) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => map
            .get(snake_key)
            .or_else(|| map.get(camel_key))
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty()),
        _ => false,
    }
}

fn should_fill_catalog(incoming: &Option<Value>, existing: Option<&Value>, snake_key: &str, camel_key: &str) -> bool {
    if has_non_empty_catalog(incoming, snake_key, camel_key) {
        return false;
    }
    if incoming.is_none() && existing.is_some_and(|value| has_non_empty_catalog_value(value, snake_key, camel_key)) {
        return false;
    }
    true
}

fn keys_to_camel_case(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let next_key = if key == "_meta" { key } else { snake_to_camel(&key) };
                    let next_value = if next_key == "_meta" {
                        value
                    } else {
                        keys_to_camel_case(value)
                    };
                    (next_key, next_value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(keys_to_camel_case).collect()),
        other => other,
    }
}

fn snake_to_camel(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut uppercase_next = false;
    for ch in input.chars() {
        if ch == '_' {
            uppercase_next = true;
        } else if uppercase_next {
            out.extend(ch.to_uppercase());
            uppercase_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn non_empty_or_first(current: String, first: &str) -> String {
    if current.is_empty() { first.to_owned() } else { current }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        SessionConfigOptionCategory, SessionConfigSelectGroup, SessionConfigSelectOption, SessionMode,
    };
    use serde_json::json;

    #[test]
    fn derives_modes_and_models_from_ungrouped_select_options() {
        let options = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "plan",
                vec![
                    SessionConfigSelectOption::new("build", "Build"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ],
            ),
            SessionConfigOption::select(
                "model",
                "Model",
                "opus",
                vec![
                    SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    SessionConfigSelectOption::new("opus", "Opus"),
                ],
            ),
        ];

        let modes = derive_modes_from_config_options(&options).expect("mode catalog");
        assert_eq!(modes.current_mode_id.to_string(), "plan");
        assert_eq!(
            modes.available_modes,
            vec![SessionMode::new("build", "Build"), SessionMode::new("plan", "Plan")]
        );

        let models = derive_models_from_config_options(&options).expect("model catalog");
        assert_eq!(models.current_model_id.to_string(), "opus");
        assert_eq!(models.available_models.len(), 2);
        assert_eq!(models.available_models[0].model_id.to_string(), "sonnet");
        assert_eq!(models.available_models[0].name, "Sonnet");
    }

    #[test]
    fn derives_models_from_grouped_select_options() {
        let options = vec![SessionConfigOption::select(
            "models",
            "Model",
            "gpt-5",
            vec![
                SessionConfigSelectGroup::new(
                    "openai",
                    "OpenAI",
                    vec![
                        SessionConfigSelectOption::new("gpt-5", "GPT-5"),
                        SessionConfigSelectOption::new("gpt-5-mini", "GPT-5 mini"),
                    ],
                ),
                SessionConfigSelectGroup::new(
                    "anthropic",
                    "Anthropic",
                    vec![SessionConfigSelectOption::new("claude-sonnet-4", "Claude Sonnet 4")],
                ),
            ],
        )];

        let models = derive_models_from_config_options(&options).expect("model catalog");
        assert_eq!(models.current_model_id.to_string(), "gpt-5");
        assert_eq!(models.available_models.len(), 3);
        assert_eq!(models.available_models[2].model_id.to_string(), "claude-sonnet-4");
    }

    #[test]
    fn derives_from_semantic_categories_before_id_aliases() {
        let options = vec![
            SessionConfigOption::select(
                "reasoning",
                "Reasoning",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            ),
            SessionConfigOption::select(
                "session_mode",
                "Mode",
                "plan",
                vec![SessionConfigSelectOption::new("plan", "Plan")],
            )
            .category(SessionConfigOptionCategory::Mode),
            SessionConfigOption::select(
                "default_model",
                "Model",
                "opus",
                vec![SessionConfigSelectOption::new("opus", "Opus")],
            )
            .category(SessionConfigOptionCategory::Model),
        ];

        let modes = derive_modes_from_config_options(&options).expect("mode category");
        assert_eq!(modes.current_mode_id.to_string(), "plan");

        let models = derive_models_from_config_options(&options).expect("model category");
        assert_eq!(models.current_model_id.to_string(), "opus");
    }

    #[test]
    fn ignores_unknown_non_select_and_empty_options() {
        let options = vec![
            SessionConfigOption::select(
                "reasoning",
                "Reasoning",
                "high",
                vec![
                    SessionConfigSelectOption::new("low", "Low"),
                    SessionConfigSelectOption::new("high", "High"),
                ],
            ),
            SessionConfigOption::select("mode", "Mode", "plan", Vec::<SessionConfigSelectOption>::new()),
        ];

        assert!(derive_modes_from_config_options(&options).is_none());
        assert!(derive_models_from_config_options(&options).is_none());
    }

    #[test]
    fn falls_back_to_first_option_when_current_value_is_empty() {
        let options = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "",
                vec![
                    SessionConfigSelectOption::new("build", "Build"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ],
            ),
            SessionConfigOption::select(
                "model",
                "Model",
                "",
                vec![
                    SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    SessionConfigSelectOption::new("opus", "Opus"),
                ],
            ),
        ];

        let modes = derive_modes_from_config_options(&options).expect("mode catalog");
        assert_eq!(modes.current_mode_id.to_string(), "build");

        let models = derive_models_from_config_options(&options).expect("model catalog");
        assert_eq!(models.current_model_id.to_string(), "sonnet");
    }

    #[test]
    fn enriches_missing_handshake_catalogs_from_config_options() {
        let options = vec![
            SessionConfigOption::select(
                "modes",
                "Mode",
                "build",
                vec![
                    SessionConfigSelectOption::new("build", "Build"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ],
            ),
            SessionConfigOption::select(
                "models",
                "Model",
                "sonnet",
                vec![
                    SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    SessionConfigSelectOption::new("opus", "Opus"),
                ],
            ),
        ];
        let handshake = AgentHandshake {
            config_options: Some(serde_json::to_value(&options).unwrap()),
            ..Default::default()
        };

        let enriched = enrich_handshake_with_existing_config_option_catalog(&handshake, None);

        assert_eq!(
            enriched.available_modes,
            Some(json!({
                "current_mode_id": "build",
                "available_modes": [
                    {"id": "build", "name": "Build"},
                    {"id": "plan", "name": "Plan"}
                ]
            }))
        );
        assert_eq!(
            enriched.available_models,
            Some(json!({
                "current_model_id": "sonnet",
                "current_model_label": "Sonnet",
                "available_models": [
                    {"id": "sonnet", "label": "Sonnet"},
                    {"id": "opus", "label": "Opus"}
                ]
            }))
        );
    }

    #[test]
    fn extracts_wrapped_config_option_update_shapes() {
        let snake_wrapped = json!({
            "config_options": [
                {
                    "id": "models",
                    "name": "Model",
                    "type": "select",
                    "current_value": "opus",
                    "options": [
                        {"value": "sonnet", "name": "Sonnet"},
                        {"value": "opus", "name": "Opus"}
                    ]
                }
            ]
        });
        let camel_wrapped = json!({
            "configOptions": [
                {
                    "id": "modes",
                    "name": "Mode",
                    "type": "select",
                    "currentValue": "plan",
                    "options": [
                        {"value": "build", "name": "Build"},
                        {"value": "plan", "name": "Plan"}
                    ]
                }
            ]
        });

        let models = extract_config_options_from_value(&snake_wrapped).expect("snake wrapper");
        assert_eq!(
            derive_models_from_config_options(&models)
                .expect("model catalog")
                .current_model_id
                .to_string(),
            "opus"
        );

        let modes = extract_config_options_from_value(&camel_wrapped).expect("camel wrapper");
        assert_eq!(
            derive_modes_from_config_options(&modes)
                .expect("mode catalog")
                .current_mode_id
                .to_string(),
            "plan"
        );
    }

    #[test]
    fn enrich_keeps_explicit_non_empty_catalogs() {
        let explicit_modes = json!({
            "current_mode_id": "explicit",
            "available_modes": [{"id": "explicit", "name": "Explicit"}]
        });
        let explicit_models = json!({
            "current_model_id": "explicit-model",
            "current_model_label": "Explicit Model",
            "available_models": [{"id": "explicit-model", "label": "Explicit Model"}]
        });
        let handshake = AgentHandshake {
            config_options: Some(json!([
                {
                    "id": "models",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "derived",
                    "options": [{"value": "derived", "name": "Derived"}]
                }
            ])),
            available_modes: Some(explicit_modes.clone()),
            available_models: Some(explicit_models.clone()),
            ..Default::default()
        };

        let enriched = enrich_handshake_with_existing_config_option_catalog(&handshake, None);

        assert_eq!(enriched.available_modes, Some(explicit_modes));
        assert_eq!(enriched.available_models, Some(explicit_models));
    }

    #[test]
    fn enrich_skips_config_only_fallback_when_existing_catalog_is_non_empty() {
        let existing = AgentHandshake {
            available_models: Some(json!({
                "current_model_id": "explicit-model",
                "current_model_label": "Explicit Model",
                "available_models": [{"id": "explicit-model", "label": "Explicit Model"}]
            })),
            ..Default::default()
        };
        let incoming = AgentHandshake {
            config_options: Some(json!({
                "configOptions": [
                    {
                        "id": "models",
                        "name": "Model",
                        "type": "select",
                        "currentValue": "derived",
                        "options": [{"value": "derived", "name": "Derived"}]
                    }
                ]
            })),
            ..Default::default()
        };

        let enriched = enrich_handshake_with_existing_config_option_catalog(&incoming, Some(&existing));

        assert!(
            enriched.available_models.is_none(),
            "config-only partial must not overwrite an existing explicit catalog"
        );
    }
}
