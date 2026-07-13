use agent_client_protocol::schema::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionModeState, SessionModelState,
};
use aionui_api_types::{AcpConfigOptionDto, AcpConfigSelectOptionDto};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigSnapshot {
    pub(crate) options: Vec<AcpConfigOptionDto>,
    option_origins: Vec<ConfigOptionOrigin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigOptionOrigin {
    Real,
    SyntheticLegacyMode,
    SyntheticLegacyModel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConfigSupplementSummary {
    pub(crate) mode: bool,
    pub(crate) model: bool,
}

impl ConfigSupplementSummary {
    pub(crate) fn categories_csv(self) -> Option<&'static str> {
        match (self.mode, self.model) {
            (true, true) => Some("mode,model"),
            (true, false) => Some("mode"),
            (false, true) => Some("model"),
            (false, false) => None,
        }
    }
}

impl ConfigSnapshot {
    fn new(options: Vec<AcpConfigOptionDto>, option_origins: Vec<ConfigOptionOrigin>) -> Self {
        debug_assert_eq!(
            options.len(),
            option_origins.len(),
            "ConfigSnapshot options and origins must stay aligned"
        );
        Self {
            options,
            option_origins,
        }
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::new(Vec::new(), Vec::new())
    }

    pub(crate) fn from_real_options(options: Vec<SessionConfigOption>) -> Self {
        let options: Vec<AcpConfigOptionDto> = options.into_iter().map(dto_from_sdk_option).collect();
        let option_origins = vec![ConfigOptionOrigin::Real; options.len()];
        Self::new(options, option_origins)
    }

    pub(crate) fn from_legacy_catalogs(modes: Option<&SessionModeState>, models: Option<&SessionModelState>) -> Self {
        let mut options = Vec::new();
        let mut option_origins = Vec::new();
        if let Some(modes) = modes {
            options.push(dto_from_modes(modes));
            option_origins.push(ConfigOptionOrigin::SyntheticLegacyMode);
        }
        if let Some(models) = models {
            options.push(dto_from_models(models));
            option_origins.push(ConfigOptionOrigin::SyntheticLegacyModel);
        }
        Self::new(options, option_origins)
    }

    pub(crate) fn supplement_summary_for_real_options(
        options: &[SessionConfigOption],
        modes: Option<&SessionModeState>,
        models: Option<&SessionModelState>,
    ) -> ConfigSupplementSummary {
        let has_real_mode = options
            .iter()
            .any(|option| real_option_matches_category_or_id(option, &SessionConfigOptionCategory::Mode, "mode"));
        let has_real_model = options
            .iter()
            .any(|option| real_option_matches_category_or_id(option, &SessionConfigOptionCategory::Model, "model"));

        ConfigSupplementSummary {
            mode: !has_real_mode && modes.is_some_and(|modes| !modes.available_modes.is_empty()),
            model: !has_real_model && models.is_some_and(|models| !models.available_models.is_empty()),
        }
    }

    pub(crate) fn from_real_options_with_runtime_supplements(
        options: Vec<SessionConfigOption>,
        modes: Option<&SessionModeState>,
        models: Option<&SessionModelState>,
    ) -> Self {
        let summary = Self::supplement_summary_for_real_options(&options, modes, models);
        let mut snapshot = Self::from_real_options(options);

        if summary.mode
            && let Some(modes) = modes
        {
            snapshot.options.push(dto_from_modes(modes));
            snapshot.option_origins.push(ConfigOptionOrigin::SyntheticLegacyMode);
        }
        if summary.model
            && let Some(models) = models
        {
            snapshot.options.push(dto_from_models(models));
            snapshot.option_origins.push(ConfigOptionOrigin::SyntheticLegacyModel);
        }

        snapshot
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
    let Some((index, option)) = snapshot
        .options
        .iter()
        .enumerate()
        .find(|(_, option)| option.id == option_id)
    else {
        return Err(ConfigSetPathError::OptionNotFound);
    };
    if !option.options.is_empty() && !option.options.iter().any(|option| option.value == requested) {
        return Err(ConfigSetPathError::ValueNotSelectable);
    }
    match snapshot
        .option_origins
        .get(index)
        .copied()
        .unwrap_or(ConfigOptionOrigin::Real)
    {
        ConfigOptionOrigin::Real => Ok(ConfigSetPath::ConfigOption {
            option_id: option.id.clone(),
        }),
        ConfigOptionOrigin::SyntheticLegacyMode => Ok(ConfigSetPath::LegacyMode),
        ConfigOptionOrigin::SyntheticLegacyModel => Ok(ConfigSetPath::LegacyModel),
    }
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

fn real_option_matches_category_or_id(
    option: &SessionConfigOption,
    category: &SessionConfigOptionCategory,
    option_id: &str,
) -> bool {
    option.category.as_ref() == Some(category) || option.id.to_string() == option_id
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
        let snapshot = ConfigSnapshot::from_real_options(vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "auto",
                vec![SessionConfigSelectOption::new("full-access", "Full Access")],
            )
            .category(SessionConfigOptionCategory::Mode),
        ]);

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
