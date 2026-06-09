//! Unit tests for `AcpSession`. Pulled out of `session.rs` so that file
//! stays under the 1000-line per-file budget. Linked via
//! `#[path = "session_tests.rs"] mod tests;` from `session.rs`, so
//! `super::*` resolves to the `session` module's private scope.

use agent_client_protocol::schema::{ModelInfo, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode};

use super::*;

fn make_session() -> AcpSession {
    AcpSession::new(Some(ModeId::new("default")), None, HashMap::new())
}

#[test]
fn assign_session_id_emits_event() {
    let mut session = make_session();
    session.set_session_id(SessionId::new("sess-1"));
    assert_eq!(session.session_id(), Some("sess-1"));
    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        AcpSessionEvent::SessionAssigned {
            session_id: SessionId::new("sess-1"),
        }
    );
}

#[test]
fn assign_session_id_is_idempotent() {
    let mut session = make_session();
    session.set_session_id(SessionId::new("sess-1"));
    session.drain_events();
    session.set_session_id(SessionId::new("sess-1"));
    assert!(session.drain_events().is_empty());
}

#[test]
fn mark_opened_emits_once() {
    let mut session = make_session();
    session.mark_opened();
    session.mark_opened();
    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], AcpSessionEvent::SessionOpened);
    assert!(session.is_opened());
}

#[test]
fn set_desired_mode_emits_when_changed() {
    let mut session = make_session();
    assert!(session.set_desired_mode(ModeId::new("plan")));
    assert_eq!(session.desired_mode(), Some("plan"));
    let events = session.drain_events();
    assert_eq!(
        events[0],
        AcpSessionEvent::DesiredModeChanged {
            mode: ModeId::new("plan"),
        }
    );
}

#[test]
fn set_desired_mode_rejects_empty() {
    let mut session = make_session();
    assert!(!session.set_desired_mode(ModeId::new("")));
    assert!(session.drain_events().is_empty());
}

#[test]
fn set_desired_mode_no_op_when_unchanged() {
    let mut session = make_session();
    session.set_desired_mode(ModeId::new("plan"));
    session.drain_events();
    assert!(!session.set_desired_mode(ModeId::new("plan")));
    assert!(session.drain_events().is_empty());
}

#[test]
fn set_desired_mode_validates_against_advertised() {
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "code",
        vec![SessionMode::new("code", "Code"), SessionMode::new("plan", "Plan")],
    ));
    assert!(session.set_desired_mode(ModeId::new("plan")));
    assert!(!session.set_desired_mode(ModeId::new("nonexistent")));
}

#[test]
fn set_desired_mode_allows_any_when_advertised_empty() {
    let mut session = make_session();
    assert!(session.set_desired_mode(ModeId::new("anything")));
}

#[test]
fn can_select_mode_reports_unavailable_advertised_mode() {
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "code",
        vec![SessionMode::new("code", "Code"), SessionMode::new("plan", "Plan")],
    ));

    assert!(session.can_select_mode("plan"));
    assert!(!session.can_select_mode("nonexistent"));
    assert!(!session.can_select_mode(""));
}

#[test]
fn apply_observed_mode_does_not_change_desired() {
    let mut session = make_session();
    session.set_desired_mode(ModeId::new("plan"));
    session.drain_events();
    session.apply_observed_mode(ModeId::new("code"));
    assert_eq!(session.desired_mode(), Some("plan"));
    assert_eq!(session.observed_mode(), Some("code"));
}

#[test]
fn apply_observed_mode_syncs_advertised_current_without_losing_available() {
    use agent_client_protocol::schema::SessionMode;
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "default",
        vec![SessionMode::new("default", "Default"), SessionMode::new("plan", "Plan")],
    ));
    session.drain_events();

    session.apply_observed_mode(ModeId::new("plan"));

    assert_eq!(session.observed_mode(), Some("plan"));
    assert_eq!(session.current_mode_id().as_deref(), Some("plan"));
    let modes = session.modes().expect("modes present");
    assert_eq!(modes.available_modes.len(), 2, "available_modes must be preserved");
}

#[test]
fn apply_observed_model_syncs_advertised_current_without_losing_available() {
    use agent_client_protocol::schema::ModelInfo;
    let mut session = make_session();
    session.apply_advertised_models(SessionModelState::new(
        "claude-sonnet-4",
        vec![
            ModelInfo::new("claude-sonnet-4", "Sonnet 4"),
            ModelInfo::new("claude-opus-4", "Opus 4"),
        ],
    ));
    session.drain_events();

    session.apply_observed_model(ModelId::new("claude-opus-4"));

    assert_eq!(session.observed_model(), Some("claude-opus-4"));
    assert_eq!(session.current_model_id().as_deref(), Some("claude-opus-4"));
    let models = session.model_info().expect("models present");
    assert_eq!(models.available_models.len(), 2, "available_models must be preserved");
}

#[test]
fn apply_observed_mode_creates_advertised_when_empty() {
    let mut session = make_session();
    session.apply_observed_mode(ModeId::new("plan"));
    assert_eq!(session.current_mode_id().as_deref(), Some("plan"));
}

#[test]
fn apply_observed_model_creates_advertised_when_empty() {
    let mut session = make_session();
    session.apply_observed_model(ModelId::new("claude-opus-4"));
    assert_eq!(session.current_model_id().as_deref(), Some("claude-opus-4"));
}

#[test]
fn confirm_mode_aligns_desired_and_current() {
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "default",
        vec![SessionMode::new("default", "Default"), SessionMode::new("plan", "Plan")],
    ));
    session.drain_events();

    session.confirm_mode(ModeId::new("plan"));

    assert_eq!(session.desired_mode(), Some("plan"));
    assert_eq!(session.observed_mode(), Some("plan"));
    assert_eq!(session.current_mode_id().as_deref(), Some("plan"));
    assert!(session.plan_reconcile().is_empty());
    assert_eq!(
        session.drain_events(),
        vec![AcpSessionEvent::ObservedModeSynced {
            mode: ModeId::new("plan"),
        }]
    );
}

#[test]
fn confirm_model_aligns_desired_and_current() {
    use agent_client_protocol::schema::ModelInfo;
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_models(SessionModelState::new(
        "claude-sonnet-4",
        vec![
            ModelInfo::new("claude-sonnet-4", "Sonnet 4"),
            ModelInfo::new("claude-opus-4", "Opus 4"),
        ],
    ));
    session.drain_events();

    session.confirm_model(ModelId::new("claude-opus-4"));

    assert_eq!(session.desired_model(), Some("claude-opus-4"));
    assert_eq!(session.observed_model(), Some("claude-opus-4"));
    assert_eq!(session.current_model_id().as_deref(), Some("claude-opus-4"));
    assert!(session.plan_reconcile().is_empty());
    assert_eq!(
        session.drain_events(),
        vec![AcpSessionEvent::ObservedModelSynced {
            model: ModelId::new("claude-opus-4"),
        }]
    );
}

#[test]
fn apply_observed_config_emits_on_change_and_is_idempotent() {
    let mut session = make_session();
    session.apply_observed_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));
    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpSessionEvent::ObservedConfigSynced { selections } => {
            assert_eq!(
                selections.get(&ConfigKey::new("reasoning")),
                Some(&ConfigValue::new("high"))
            );
        }
        other => panic!("expected ObservedConfigSynced, got {other:?}"),
    }

    // Idempotent repeat: no new event.
    session.apply_observed_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));
    assert!(session.drain_events().is_empty());
}

#[test]
fn apply_observed_config_closes_plan_reconcile_drift() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.set_desired_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));
    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("reasoning"),
            value: ConfigValue::new("high"),
        }]
    );

    session.apply_observed_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));
    assert!(
        session.plan_reconcile().is_empty(),
        "plan_reconcile must be a no-op once observed catches up to desired",
    );
}

#[test]
fn plan_reconcile_detects_mode_drift() {
    let mut session = make_session();
    session.set_desired_mode(ModeId::new("plan"));
    session.apply_observed_mode(ModeId::new("default"));
    let actions = session.plan_reconcile();
    assert_eq!(
        actions,
        vec![ReconcileAction::SetMode {
            mode: ModeId::new("plan"),
        }]
    );
}

#[test]
fn plan_reconcile_empty_when_aligned() {
    let mut session = make_session();
    session.set_desired_mode(ModeId::new("plan"));
    session.apply_observed_mode(ModeId::new("plan"));
    assert!(session.plan_reconcile().is_empty());
}

#[test]
fn plan_reconcile_detects_config_drift() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.set_desired_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));
    let actions = session.plan_reconcile();
    assert_eq!(
        actions,
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("reasoning"),
            value: ConfigValue::new("high"),
        }]
    );
}

#[test]
fn plan_reconcile_config_aligned_when_observed_matches() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.set_desired_config(ConfigKey::new("reasoning"), ConfigValue::new("high"));

    session.apply_advertised_config_options(vec![SessionConfigOption::select(
        "reasoning",
        "Reasoning",
        "high",
        vec![
            SessionConfigSelectOption::new("low", "Low"),
            SessionConfigSelectOption::new("high", "High"),
        ],
    )]);
    assert!(session.plan_reconcile().is_empty());
}

#[test]
fn drain_events_clears_buffer() {
    let mut session = make_session();
    session.set_session_id(SessionId::new("s1"));
    session.mark_opened();
    assert_eq!(session.drain_events().len(), 2);
    assert!(session.drain_events().is_empty());
}

#[test]
fn apply_advertised_modes_sets_observed() {
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new("code", vec![SessionMode::new("code", "Code")]));
    assert_eq!(session.observed_mode(), Some("code"));
    assert_eq!(session.current_mode_id().as_deref(), Some("code"));
}

#[test]
fn apply_advertised_models_sets_observed() {
    let mut session = make_session();
    session.apply_advertised_models(SessionModelState::new("claude-4", Vec::new()));
    assert_eq!(session.observed_model(), Some("claude-4"));
}

#[test]
fn set_desired_model_emits_when_changed() {
    let mut session = make_session();
    assert!(session.set_desired_model(ModelId::new("claude-sonnet-4")));
    assert_eq!(session.desired_model(), Some("claude-sonnet-4"));
    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        AcpSessionEvent::DesiredModelChanged {
            model: ModelId::new("claude-sonnet-4"),
        }
    );
}

#[test]
fn set_desired_model_rejects_empty() {
    let mut session = make_session();
    assert!(!session.set_desired_model(ModelId::new("")));
    assert!(session.drain_events().is_empty());
}

#[test]
fn set_desired_model_no_op_when_unchanged() {
    let mut session = make_session();
    session.set_desired_model(ModelId::new("claude-sonnet-4"));
    session.drain_events();
    assert!(!session.set_desired_model(ModelId::new("claude-sonnet-4")));
    assert!(session.drain_events().is_empty());
}

#[test]
fn set_desired_model_validates_against_advertised() {
    use agent_client_protocol::schema::ModelInfo;
    let mut session = make_session();
    session.apply_advertised_models(SessionModelState::new(
        "claude-sonnet-4",
        vec![
            ModelInfo::new("claude-sonnet-4", "Sonnet 4"),
            ModelInfo::new("claude-opus-4", "Opus 4"),
        ],
    ));
    assert!(session.set_desired_model(ModelId::new("claude-opus-4")));
    assert!(!session.set_desired_model(ModelId::new("nonexistent")));
}

#[test]
fn can_select_model_reports_unavailable_advertised_model() {
    use agent_client_protocol::schema::ModelInfo;
    let mut session = make_session();
    session.apply_advertised_models(SessionModelState::new(
        "claude-sonnet-4",
        vec![
            ModelInfo::new("claude-sonnet-4", "Sonnet 4"),
            ModelInfo::new("claude-opus-4", "Opus 4"),
        ],
    ));

    assert!(session.can_select_model("claude-opus-4"));
    assert!(!session.can_select_model("nonexistent"));
    assert!(!session.can_select_model(""));
}

#[test]
fn set_desired_model_allows_any_when_advertised_empty() {
    let mut session = make_session();
    assert!(session.set_desired_model(ModelId::new("anything")));
}

#[test]
fn apply_observed_model_does_not_change_desired_model() {
    let mut session = make_session();
    session.set_desired_model(ModelId::new("claude-opus-4"));
    session.drain_events();
    session.apply_observed_model(ModelId::new("claude-sonnet-4"));
    assert_eq!(session.desired_model(), Some("claude-opus-4"));
    assert_eq!(session.observed_model(), Some("claude-sonnet-4"));
}

#[test]
fn plan_reconcile_detects_model_drift() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.set_desired_model(ModelId::new("claude-opus-4"));
    session.apply_observed_model(ModelId::new("claude-sonnet-4"));
    let actions = session.plan_reconcile();
    assert_eq!(
        actions,
        vec![ReconcileAction::SetModel {
            model: ModelId::new("claude-opus-4"),
        }]
    );
}

#[test]
fn plan_reconcile_model_aligned_when_observed_matches() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.set_desired_model(ModelId::new("claude-opus-4"));
    session.apply_observed_model(ModelId::new("claude-opus-4"));
    assert!(session.plan_reconcile().is_empty());
}

#[test]
fn new_with_initial_model_sets_desired_model() {
    let session = AcpSession::new(None, Some(ModelId::new("claude-opus-4")), HashMap::new());
    assert_eq!(session.desired_model(), Some("claude-opus-4"));
}

#[test]
fn clear_invalid_desired_model_drops_stale_initial_model() {
    use agent_client_protocol::schema::ModelInfo;

    let mut session = AcpSession::new(None, Some(ModelId::new("deepseek-v4-pro")), HashMap::new());
    session.apply_advertised_models(SessionModelState::new(
        "opus",
        vec![
            ModelInfo::new("default", "Default"),
            ModelInfo::new("opus", "Opus"),
            ModelInfo::new("sonnet", "Sonnet"),
        ],
    ));

    assert_eq!(
        session.clear_invalid_desired_model(),
        Some(ModelId::new("deepseek-v4-pro"))
    );
    assert_eq!(session.desired_model(), None);
    assert!(
        session.plan_reconcile().is_empty(),
        "invalid desired model must not produce session/set_model"
    );
}

#[test]
fn clear_invalid_desired_mode_drops_stale_initial_mode_without_changing_current() {
    let mut session = AcpSession::new(Some(ModeId::new("legacy-plan")), None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "code",
        vec![SessionMode::new("default", "Default"), SessionMode::new("code", "Code")],
    ));
    session.drain_events();

    assert_eq!(session.clear_invalid_desired_mode(), Some(ModeId::new("legacy-plan")));
    assert_eq!(session.desired_mode(), None);
    assert_eq!(session.observed_mode(), Some("code"));
    assert_eq!(session.current_mode_id().as_deref(), Some("code"));
    assert!(
        session.plan_reconcile().is_empty(),
        "invalid desired mode must not produce session/set_mode"
    );
}

#[test]
fn apply_advertised_config_options_emits_observed_config_synced_on_change() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_config_options(vec![SessionConfigOption::select(
        "reasoning",
        "Reasoning",
        "high",
        vec![
            SessionConfigSelectOption::new("low", "Low"),
            SessionConfigSelectOption::new("high", "High"),
        ],
    )]);
    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpSessionEvent::ObservedConfigSynced { selections } => {
            assert_eq!(
                selections.get(&ConfigKey::new("reasoning")),
                Some(&ConfigValue::new("high"))
            );
        }
        other => panic!("expected ObservedConfigSynced, got {other:?}"),
    }
}

#[test]
fn apply_advertised_config_options_idempotent_when_unchanged() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    let options = vec![SessionConfigOption::select(
        "reasoning",
        "Reasoning",
        "high",
        vec![
            SessionConfigSelectOption::new("low", "Low"),
            SessionConfigSelectOption::new("high", "High"),
        ],
    )];
    session.apply_advertised_config_options(options.clone());
    session.drain_events();

    session.apply_advertised_config_options(options);
    let events = session.drain_events();
    assert!(
        events.is_empty(),
        "no ObservedConfigSynced when observed unchanged, got {events:?}"
    );
}

#[test]
fn apply_advertised_config_options_derives_missing_mode_and_model_catalogs() {
    let mut session = AcpSession::new(None, None, HashMap::new());

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "modes",
            "Mode",
            "plan",
            vec![
                SessionConfigSelectOption::new("build", "Build"),
                SessionConfigSelectOption::new("plan", "Plan"),
            ],
        ),
        SessionConfigOption::select(
            "models",
            "Model",
            "opus",
            vec![
                SessionConfigSelectOption::new("sonnet", "Sonnet"),
                SessionConfigSelectOption::new("opus", "Opus"),
            ],
        ),
    ]);

    assert_eq!(session.observed_mode(), Some("plan"));
    assert_eq!(session.current_mode_id().as_deref(), Some("plan"));
    let modes = session.modes().expect("derived modes");
    assert_eq!(modes.available_modes.len(), 2);
    assert_eq!(modes.available_modes[1].id.to_string(), "plan");

    assert_eq!(session.observed_model(), Some("opus"));
    assert_eq!(session.current_model_id().as_deref(), Some("opus"));
    let models = session.model_info().expect("derived models");
    assert_eq!(models.available_models.len(), 2);
    assert_eq!(models.available_models[1].model_id.to_string(), "opus");
}

#[test]
fn apply_advertised_config_options_falls_back_to_existing_catalogs_when_config_options_have_no_catalogs() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "build",
        vec![SessionMode::new("build", "Build"), SessionMode::new("plan", "Plan")],
    ));
    session.apply_advertised_models(SessionModelState::new(
        "sonnet",
        vec![ModelInfo::new("sonnet", "Sonnet"), ModelInfo::new("opus", "Opus")],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![SessionConfigOption::select(
        "reasoning",
        "Reasoning",
        "high",
        vec![SessionConfigSelectOption::new("high", "High")],
    )]);

    assert_eq!(session.observed_mode(), Some("build"));
    assert_eq!(session.current_mode_id().as_deref(), Some("build"));
    let modes = session.modes().expect("explicit modes");
    assert_eq!(modes.available_modes.len(), 2);
    assert_eq!(modes.available_modes[0].id.to_string(), "build");

    assert_eq!(session.observed_model(), Some("sonnet"));
    assert_eq!(session.current_model_id().as_deref(), Some("sonnet"));
    let models = session.model_info().expect("explicit models");
    assert_eq!(models.available_models.len(), 2);
    assert_eq!(models.available_models[0].model_id.to_string(), "sonnet");
}

#[test]
fn apply_advertised_config_options_prefers_config_option_catalogs_over_existing_catalogs() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.apply_advertised_modes(SessionModeState::new(
        "available-mode",
        vec![SessionMode::new("available-mode", "Available Mode")],
    ));
    session.apply_advertised_models(SessionModelState::new(
        "available-model",
        vec![ModelInfo::new("available-model", "Available Model")],
    ));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "modes",
            "Mode",
            "config-mode",
            vec![SessionConfigSelectOption::new("config-mode", "Config Mode")],
        ),
        SessionConfigOption::select(
            "models",
            "Model",
            "config-model",
            vec![SessionConfigSelectOption::new("config-model", "Config Model")],
        ),
    ]);

    assert_eq!(session.observed_mode(), Some("config-mode"));
    assert_eq!(session.current_mode_id().as_deref(), Some("config-mode"));
    let modes = session.modes().expect("config option modes");
    assert_eq!(modes.available_modes.len(), 1);
    assert_eq!(modes.available_modes[0].id.to_string(), "config-mode");

    assert_eq!(session.observed_model(), Some("config-model"));
    assert_eq!(session.current_model_id().as_deref(), Some("config-model"));
    let models = session.model_info().expect("config option models");
    assert_eq!(models.available_models.len(), 1);
    assert_eq!(models.available_models[0].model_id.to_string(), "config-model");
}

#[test]
fn apply_advertised_config_options_merges_partial_updates_and_derives_model_reasoning_variants() {
    let mut session = make_session();
    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "full-access",
            vec![
                SessionConfigSelectOption::new("auto", "Default"),
                SessionConfigSelectOption::new("full-access", "Full Access"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode),
        SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.4",
            vec![SessionConfigSelectOption::new("gpt-5.4", "gpt-5.4")],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "low",
            vec![SessionConfigSelectOption::new("low", "Low")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.5",
            vec![
                SessionConfigSelectOption::new("gpt-5.5", "GPT-5.5"),
                SessionConfigSelectOption::new("gpt-5.4", "gpt-5.4"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let modes = session.modes().expect("mode catalog is preserved");
    assert_eq!(modes.current_mode_id.to_string(), "full-access");
    assert_eq!(modes.available_modes.len(), 2);

    let config_options = session.config_options().expect("config options are preserved");
    assert_eq!(config_options.len(), 3);
    assert!(config_options.iter().any(|option| option.id.to_string() == "mode"));

    let models = session.model_info().expect("model catalog");
    assert_eq!(models.current_model_id.to_string(), "gpt-5.5/medium");
    assert_eq!(models.available_models.len(), 4);
    assert_eq!(models.available_models[0].model_id.to_string(), "gpt-5.5/low");
    assert_eq!(models.available_models[1].model_id.to_string(), "gpt-5.5/medium");
}

#[test]
fn pending_model_notice_roundtrip_and_take_once() {
    use crate::shared_kernel::ModelId;

    let mut s = AcpSession::new(None, None, HashMap::new());
    assert!(s.take_pending_model_notice().is_none(), "default is None");

    s.set_pending_model_notice(ModelId::new("opus"));
    let taken = s.take_pending_model_notice();
    assert_eq!(taken.as_ref().map(|m| m.as_str()), Some("opus"));

    // Take is destructive — the second take must see None.
    assert!(s.take_pending_model_notice().is_none());
}

#[test]
fn set_desired_mode_plus_plan_reconcile_produces_set_mode_action() {
    // Startup/recovery reconcile still turns pending intent into a
    // ReconcileAction::SetMode when desired and observed diverge.
    let mut session = AcpSession::new(None, None, Default::default());
    session.apply_advertised_modes(SessionModeState::new(
        "default".to_owned(),
        vec![SessionMode::new("default", "Default"), SessionMode::new("plan", "Plan")],
    ));
    session.apply_observed_mode(ModeId::new("default"));
    assert_eq!(session.plan_reconcile(), vec![]);

    // Startup seed asks for "plan".
    assert!(session.set_desired_mode(ModeId::new("plan")));

    // Now reconcile should want to set CLI mode to "plan".
    let actions = session.plan_reconcile();
    assert_eq!(
        actions,
        vec![ReconcileAction::SetMode {
            mode: ModeId::new("plan")
        }]
    );
}

// Close-reason lifecycle tests live in `session_close_tests.rs` so
// session.rs stays under the 1000-line per-file budget. The `#[path]`
// attribute pulls them into this `tests` module's scope, so they
// inherit `make_session`, `CloseReason` (via `super::*`), etc.
#[path = "session_close_tests.rs"]
mod close_reason_tests;

#[test]
fn pending_session_new_prelude_defaults_to_false() {
    let mut s = make_session();
    assert!(!s.take_pending_session_new_prelude());
}

#[test]
fn mark_pending_session_new_prelude_sets_true() {
    let mut s = make_session();
    s.mark_pending_session_new_prelude();
    assert!(s.take_pending_session_new_prelude());
}

#[test]
fn take_pending_session_new_prelude_is_destructive() {
    let mut s = make_session();
    s.mark_pending_session_new_prelude();
    assert!(s.take_pending_session_new_prelude());
    assert!(!s.take_pending_session_new_prelude());
}

#[test]
fn mark_pending_session_new_prelude_is_idempotent() {
    let mut s = make_session();
    s.mark_pending_session_new_prelude();
    s.mark_pending_session_new_prelude();
    assert!(s.take_pending_session_new_prelude());
    assert!(!s.take_pending_session_new_prelude());
}
