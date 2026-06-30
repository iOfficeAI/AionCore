//! Unit tests for partial real ACP config option snapshots and set-path routing.

use agent_client_protocol::schema::{
    ModelInfo, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode,
    SessionModeState, SessionModelState,
};

use super::super::config_options::{ConfigSetPath, resolve_set_path};
use super::*;

fn snapshot_option<'a>(snapshot: &'a ConfigSnapshot, option_id: &str) -> &'a aionui_api_types::AcpConfigOptionDto {
    snapshot
        .options
        .iter()
        .find(|option| option.id == option_id)
        .unwrap_or_else(|| panic!("missing config option {option_id}"))
}

#[test]
fn config_snapshot_supplements_missing_mode_from_non_empty_runtime_catalog() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "full-access",
        vec![
            SessionMode::new("read-only", "Read Only"),
            SessionMode::new("full-access", "Full Access"),
        ],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(snapshot.options.len(), 2);
    assert_eq!(
        snapshot_option(&snapshot, "reasoning_effort").current_value.as_deref(),
        Some("high")
    );
    let mode = snapshot_option(&snapshot, "mode");
    assert_eq!(mode.category.as_deref(), Some("mode"));
    assert_eq!(mode.current_value.as_deref(), Some("full-access"));
    assert_eq!(mode.options.len(), 2);
    assert_eq!(mode.options[1].value, "full-access");

    let real_options = session.config_options().expect("real options remain cached");
    assert_eq!(real_options.len(), 1);
    assert_eq!(real_options[0].id.to_string(), "reasoning_effort");
}

#[test]
fn config_snapshot_supplements_missing_mode_from_preloaded_catalog_using_desired_current() {
    let mut session = AcpSession::new(Some(ModeId::new("full-access")), None, HashMap::new());
    session.preload_advertised_catalogs(
        Some(SessionModeState::new(
            "auto",
            vec![
                SessionMode::new("read-only", "Read Only"),
                SessionMode::new("auto", "Default"),
                SessionMode::new("full-access", "Full Access"),
            ],
        )),
        None,
    );

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    let mode = snapshot_option(&snapshot, "mode");
    assert_eq!(mode.current_value.as_deref(), Some("full-access"));
    assert_eq!(mode.options.len(), 3);
    assert_eq!(
        resolve_set_path(&snapshot, "mode", "read-only"),
        Ok(ConfigSetPath::LegacyMode)
    );
}

#[test]
fn preload_advertised_catalogs_reports_seeded_catalog_counts() {
    let mut session = AcpSession::new(None, None, HashMap::new());

    let summary = session.preload_advertised_catalogs(
        Some(SessionModeState::new(
            "auto",
            vec![
                SessionMode::new("read-only", "Read Only"),
                SessionMode::new("auto", "Default"),
                SessionMode::new("full-access", "Full Access"),
            ],
        )),
        Some(SessionModelState::new(
            "gpt-5.5",
            vec![
                ModelInfo::new("gpt-5.5", "GPT-5.5"),
                ModelInfo::new("gpt-5.4", "GPT-5.4"),
            ],
        )),
    );

    assert!(summary.any_preloaded());
    assert!(summary.mode_preloaded);
    assert!(summary.model_preloaded);
    assert_eq!(summary.mode_catalog_count, 3);
    assert_eq!(summary.model_catalog_count, 2);
}

#[test]
fn config_snapshot_supplements_missing_model_from_non_empty_runtime_catalog() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_models(SessionModelState::new(
        "gpt-5",
        vec![ModelInfo::new("gpt-5", "GPT-5"), ModelInfo::new("gpt-4.1", "GPT-4.1")],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "medium",
            vec![SessionConfigSelectOption::new("medium", "Medium")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(snapshot.options.len(), 2);
    let model = snapshot_option(&snapshot, "model");
    assert_eq!(model.category.as_deref(), Some("model"));
    assert_eq!(model.current_value.as_deref(), Some("gpt-5"));
    assert_eq!(model.options.len(), 2);
    assert_eq!(model.options[0].value, "gpt-5");

    let real_options = session.config_options().expect("real options remain cached");
    assert_eq!(real_options.len(), 1);
    assert_eq!(real_options[0].id.to_string(), "reasoning_effort");
}

#[test]
fn config_snapshot_keeps_real_mode_option_without_runtime_merge() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "runtime-build",
        vec![
            SessionMode::new("runtime-build", "Runtime Build"),
            SessionMode::new("runtime-plan", "Runtime Plan"),
        ],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "real-plan",
            vec![SessionConfigSelectOption::new("real-plan", "Real Plan")],
        )
        .category(SessionConfigOptionCategory::Mode),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(snapshot.options.len(), 1);
    let mode = snapshot_option(&snapshot, "mode");
    assert_eq!(mode.current_value.as_deref(), Some("real-plan"));
    assert_eq!(mode.options.len(), 1);
    assert_eq!(mode.options[0].value, "real-plan");
}

#[test]
fn config_snapshot_keeps_real_model_option_without_runtime_merge() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_models(SessionModelState::new(
        "runtime-model",
        vec![
            ModelInfo::new("runtime-model", "Runtime Model"),
            ModelInfo::new("runtime-extra", "Runtime Extra"),
        ],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "real-model",
            vec![SessionConfigSelectOption::new("real-model", "Real Model")],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(snapshot.options.len(), 1);
    let model = snapshot_option(&snapshot, "model");
    assert_eq!(model.current_value.as_deref(), Some("real-model"));
    assert_eq!(model.options.len(), 1);
    assert_eq!(model.options[0].value, "real-model");
}

#[test]
fn config_snapshot_does_not_supplement_mode_from_empty_runtime_catalog() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new("full-access", Vec::new()));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert!(snapshot.option_current("mode").is_none());
    assert_eq!(snapshot.options.len(), 1);
}

#[test]
fn config_snapshot_does_not_supplement_model_from_empty_runtime_catalog() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_models(SessionModelState::new("gpt-5", Vec::new()));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "medium",
            vec![SessionConfigSelectOption::new("medium", "Medium")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert!(snapshot.option_current("model").is_none());
    assert_eq!(snapshot.options.len(), 1);
}

#[test]
fn config_snapshot_does_not_supplement_thought_level() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new("build", vec![SessionMode::new("build", "Build")]));
    session.apply_advertised_models(SessionModelState::new("gpt-5", vec![ModelInfo::new("gpt-5", "GPT-5")]));
    session.drain_events();

    session.apply_advertised_config_options(vec![SessionConfigOption::select(
        "temperature",
        "Temperature",
        "low",
        vec![SessionConfigSelectOption::new("low", "Low")],
    )]);

    let snapshot = session.config_snapshot();
    assert!(snapshot.option_current("thought_level").is_none());
    assert!(snapshot.option_current("reasoning_effort").is_none());
    assert!(snapshot.option_current("mode").is_some());
    assert!(snapshot.option_current("model").is_some());
}

#[test]
fn config_snapshot_synthetic_mode_resolves_to_legacy_set_mode() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "read-only",
        vec![
            SessionMode::new("read-only", "Read Only"),
            SessionMode::new("full-access", "Full Access"),
        ],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(
        resolve_set_path(&snapshot, "mode", "full-access"),
        Ok(ConfigSetPath::LegacyMode)
    );
}

#[test]
fn config_snapshot_synthetic_model_resolves_to_legacy_set_model() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_models(SessionModelState::new(
        "gpt-4.1",
        vec![ModelInfo::new("gpt-4.1", "GPT-4.1"), ModelInfo::new("gpt-5", "GPT-5")],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(
        resolve_set_path(&snapshot, "model", "gpt-5"),
        Ok(ConfigSetPath::LegacyModel)
    );
}

#[test]
fn config_snapshot_real_mode_and_model_resolve_to_set_config_option() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "runtime-build",
        vec![SessionMode::new("runtime-build", "Runtime Build")],
    ));
    session.apply_advertised_models(SessionModelState::new(
        "runtime-model",
        vec![ModelInfo::new("runtime-model", "Runtime Model")],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "real-read-only",
            vec![SessionConfigSelectOption::new("real-read-only", "Real Read Only")],
        )
        .category(SessionConfigOptionCategory::Mode),
        SessionConfigOption::select(
            "model",
            "Model",
            "real-model",
            vec![SessionConfigSelectOption::new("real-model", "Real Model")],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    let snapshot = session.config_snapshot();
    assert_eq!(
        resolve_set_path(&snapshot, "mode", "real-read-only"),
        Ok(ConfigSetPath::ConfigOption {
            option_id: "mode".to_owned(),
        })
    );
    assert_eq!(
        resolve_set_path(&snapshot, "model", "real-model"),
        Ok(ConfigSetPath::ConfigOption {
            option_id: "model".to_owned(),
        })
    );
}
