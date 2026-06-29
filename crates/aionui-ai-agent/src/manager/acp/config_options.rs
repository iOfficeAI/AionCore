use agent_client_protocol::schema::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionModeState, SessionModelState,
};
use aionui_api_types::{AcpConfigOptionDto, AcpConfigSelectOptionDto};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigSnapshot {
    pub(crate) options: Vec<AcpConfigOptionDto>,
}

impl ConfigSnapshot {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self { options: Vec::new() }
    }

    pub(crate) fn from_real_options(options: Vec<SessionConfigOption>) -> Self {
        Self {
            options: options.into_iter().map(dto_from_sdk_option).collect(),
        }
    }

    pub(crate) fn from_legacy_catalogs(modes: Option<&SessionModeState>, models: Option<&SessionModelState>) -> Self {
        let mut options = Vec::new();
        if let Some(modes) = modes {
            options.push(dto_from_modes(modes));
        }
        if let Some(models) = models {
            options.push(dto_from_models(models));
        }
        Self { options }
    }

    pub(crate) fn option_current(&self, option_id: &str) -> Option<String> {
        self.options
            .iter()
            .find(|option| option.id == option_id)
            .and_then(|option| option.current_value.clone())
    }

    pub(crate) fn observed_matches(&self, option_id: &str, requested: &str) -> bool {
        self.option_current(option_id).as_deref() == Some(requested)
    }

    pub(crate) fn is_mode_option(&self, option_id: &str) -> bool {
        self.options
            .iter()
            .find(|option| option.id == option_id)
            .is_some_and(|option| option.category.as_deref() == Some("mode") || option.id == "mode")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigSetPath {
    ConfigOption { option_id: String },
    LegacyMode,
    LegacyModel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigSetPathError {
    OptionNotFound,
    ValueNotSelectable,
}

pub(crate) fn resolve_set_path(
    snapshot: &ConfigSnapshot,
    option_id: &str,
    requested: &str,
) -> Result<ConfigSetPath, ConfigSetPathError> {
    let Some(option) = snapshot.options.iter().find(|option| option.id == option_id) else {
        return Err(ConfigSetPathError::OptionNotFound);
    };
    if !option.options.is_empty() && !option.options.iter().any(|option| option.value == requested) {
        return Err(ConfigSetPathError::ValueNotSelectable);
    }
    Ok(ConfigSetPath::ConfigOption {
        option_id: option.id.clone(),
    })
}

fn dto_from_sdk_option(option: SessionConfigOption) -> AcpConfigOptionDto {
    let (option_type, current_value, options) = match option.kind {
        SessionConfigKind::Select(select) => {
            let values = flatten_select_options(&select.options)
                .into_iter()
                .map(dto_from_select_option)
                .collect();
            ("select".to_owned(), Some(select.current_value.to_string()), values)
        }
        _ => ("string".to_owned(), None, Vec::new()),
    };

    AcpConfigOptionDto {
        id: option.id.to_string(),
        name: Some(option.name),
        label: None,
        description: option.description,
        category: option.category.as_ref().map(category_to_api),
        option_type,
        current_value,
        options,
    }
}

fn dto_from_modes(modes: &SessionModeState) -> AcpConfigOptionDto {
    AcpConfigOptionDto {
        id: "mode".to_owned(),
        name: Some("Mode".to_owned()),
        label: None,
        description: None,
        category: Some("mode".to_owned()),
        option_type: "select".to_owned(),
        current_value: Some(modes.current_mode_id.to_string()),
        options: modes
            .available_modes
            .iter()
            .map(|mode| AcpConfigSelectOptionDto {
                value: mode.id.to_string(),
                name: Some(mode.name.clone()),
                label: None,
                description: mode.description.clone(),
            })
            .collect(),
    }
}

fn dto_from_models(models: &SessionModelState) -> AcpConfigOptionDto {
    AcpConfigOptionDto {
        id: "model".to_owned(),
        name: Some("Model".to_owned()),
        label: None,
        description: None,
        category: Some("model".to_owned()),
        option_type: "select".to_owned(),
        current_value: Some(models.current_model_id.to_string()),
        options: models
            .available_models
            .iter()
            .map(|model| AcpConfigSelectOptionDto {
                value: model.model_id.to_string(),
                name: Some(model.name.clone()),
                label: None,
                description: model.description.clone(),
            })
            .collect(),
    }
}

fn dto_from_select_option(option: &SessionConfigSelectOption) -> AcpConfigSelectOptionDto {
    AcpConfigSelectOptionDto {
        value: option.value.to_string(),
        name: Some(option.name.clone()),
        label: None,
        description: option.description.clone(),
    }
}

fn category_to_api(category: &SessionConfigOptionCategory) -> String {
    match category {
        SessionConfigOptionCategory::Mode => "mode".to_owned(),
        SessionConfigOptionCategory::Model => "model".to_owned(),
        SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn flatten_select_options(options: &SessionConfigSelectOptions) -> Vec<&SessionConfigSelectOption> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups.iter().flat_map(|group| group.options.iter()).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        ModelInfo, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode,
        SessionModeState, SessionModelState,
    };

    #[test]
    fn dto_uses_snake_case_current_value() {
        let options = vec![
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning Effort",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];

        let snapshot = ConfigSnapshot::from_real_options(options);

        assert_eq!(snapshot.options[0].id, "reasoning_effort");
        assert_eq!(snapshot.options[0].category.as_deref(), Some("thought_level"));
        assert_eq!(snapshot.options[0].current_value.as_deref(), Some("high"));
        assert_eq!(snapshot.options[0].option_type, "select");
    }

    #[test]
    fn synthetic_snapshot_adds_mode_and_model_only_when_config_options_missing() {
        let modes = SessionModeState::new(
            "plan",
            vec![SessionMode::new("plan", "Plan"), SessionMode::new("build", "Build")],
        );
        let models = SessionModelState::new(
            "opus",
            vec![ModelInfo::new("opus", "Opus"), ModelInfo::new("sonnet", "Sonnet")],
        );

        let snapshot = ConfigSnapshot::from_legacy_catalogs(Some(&modes), Some(&models));

        assert_eq!(snapshot.options.len(), 2);
        assert_eq!(snapshot.option_current("mode").as_deref(), Some("plan"));
        assert_eq!(snapshot.option_current("model").as_deref(), Some("opus"));
        assert!(snapshot.option_current("reasoning_effort").is_none());
    }

    #[test]
    fn resolve_set_path_prefers_real_config_option_over_legacy_mode() {
        let snapshot = ConfigSnapshot {
            options: vec![AcpConfigOptionDto {
                id: "mode".to_owned(),
                name: Some("Mode".to_owned()),
                label: None,
                description: None,
                category: Some("mode".to_owned()),
                option_type: "select".to_owned(),
                current_value: Some("auto".to_owned()),
                options: vec![AcpConfigSelectOptionDto {
                    value: "full-access".to_owned(),
                    name: Some("Full Access".to_owned()),
                    label: None,
                    description: None,
                }],
            }],
        };

        assert_eq!(
            resolve_set_path(&snapshot, "mode", "full-access"),
            Ok(ConfigSetPath::ConfigOption {
                option_id: "mode".to_owned(),
            })
        );
    }

    #[test]
    fn resolve_set_path_rejects_missing_thought_level() {
        let snapshot = ConfigSnapshot::empty();

        assert_eq!(
            resolve_set_path(&snapshot, "reasoning_effort", "high"),
            Err(ConfigSetPathError::OptionNotFound)
        );
    }
}
