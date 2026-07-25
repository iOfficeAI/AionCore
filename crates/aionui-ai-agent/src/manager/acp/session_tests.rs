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
fn config_set_guard_rejects_second_in_flight_update_and_releases() {
    let mut session = AcpSession::new(None, None, Default::default());

    let first = session.try_begin_config_set();
    assert!(first.is_some());
    assert!(session.try_begin_config_set().is_none());

    // Dropping the RAII guard releases the lease (there is no explicit
    // end_config_set anymore); a fresh claim then succeeds.
    drop(first);
    assert!(session.try_begin_config_set().is_some());
}

#[test]
fn config_set_guard_releases_on_scope_exit() {
    let mut session = AcpSession::new(None, None, Default::default());
    {
        let _guard = session.try_begin_config_set().expect("first claim succeeds");
        assert!(
            session.try_begin_config_set().is_none(),
            "second claim must be rejected while the first guard is alive"
        );
    }
    assert!(
        session.try_begin_config_set().is_some(),
        "lease must be released once the guard leaves scope"
    );
}

#[test]
fn config_set_guard_releases_on_panic_unwind() {
    // Mirrors acp.rs::replay_suppression_guard_clears_on_panic_unwind: a panic
    // while the lease is held must still run the guard's Drop and free it.
    // Relies on panic = "unwind" (the default); would not run under "abort".
    let mut session = AcpSession::new(None, None, Default::default());

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = session.try_begin_config_set().expect("first claim succeeds");
        assert!(session.try_begin_config_set().is_none());
        panic!("simulated failure while a config set is in flight");
    }));

    assert!(
        session.try_begin_config_set().is_some(),
        "lease must be released after a panic unwind through the guarded scope"
    );
}

#[tokio::test(start_paused = true)]
async fn config_set_guard_releases_on_future_cancel() {
    // The key RAII payoff over `timeout + explicit end`: cancelling the future
    // that holds the lease (client disconnect / turn cancel) still releases it
    // via Drop. See spec §10.2.
    let mut session = AcpSession::new(None, None, Default::default());

    let guard = session.try_begin_config_set().expect("first claim succeeds");
    let held = async move {
        // Guard is held across the await point, then the future is cancelled.
        let _g = guard;
        std::future::pending::<()>().await;
    };

    // Drive to the await point, then cancel by letting the timeout drop `held`.
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(1), held).await;
    assert!(
        cancelled.is_err(),
        "the holding future must be cancelled at its await point"
    );

    assert!(
        session.try_begin_config_set().is_some(),
        "lease must be released when the future holding it is cancelled"
    );
}

#[test]
fn config_set_failure_path_leaves_three_layer_state_and_reconcile_untouched() {
    // Simulates a failed/timed-out set_config_option: the confirmed path only
    // mutates desired/observed/advertised AFTER an RPC Ok, so on Err none of
    // the three layers change and no phantom reconcile is produced (§10.5).
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "default",
        vec![SessionMode::new("default", "Default"), SessionMode::new("plan", "Plan")],
    ));
    session.confirm_mode(ModeId::new("plan"));
    session.drain_events();

    let desired_before = session.desired_mode().map(str::to_owned);
    let observed_before = session.observed_mode().map(str::to_owned);
    let current_before = session.current_mode_id();
    assert!(session.plan_reconcile().is_empty(), "baseline must be aligned");

    // ── RPC "fails": the failure path performs no local state mutation. ──

    assert_eq!(session.desired_mode().map(str::to_owned), desired_before);
    assert_eq!(session.observed_mode().map(str::to_owned), observed_before);
    assert_eq!(session.current_mode_id(), current_before);
    assert!(
        session.plan_reconcile().is_empty(),
        "a failed/timed-out config RPC must not create a phantom reconcile action"
    );
    assert!(
        session.drain_events().is_empty(),
        "the failure path must not emit any domain events"
    );
}

#[test]
fn config_options_snapshot_is_empty_without_real_or_legacy_catalog() {
    let session = AcpSession::new(None, None, Default::default());
    let snapshot = session.config_snapshot();
    assert!(snapshot.options.is_empty());
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
fn confirm_mode_preserves_available_mode_catalog() {
    let mut session = make_session();
    session.apply_advertised_modes(SessionModeState::new(
        "default",
        vec![SessionMode::new("default", "Default"), SessionMode::new("plan", "Plan")],
    ));
    session.drain_events();

    session.confirm_mode(ModeId::new("plan"));

    let snapshot = session.config_snapshot();
    let mode = snapshot
        .options
        .iter()
        .find(|option| option.id == "mode")
        .expect("mode option present");
    assert_eq!(mode.current_value.as_deref(), Some("plan"));
    // Confirming a value must not shrink the advertised catalog.
    assert_eq!(mode.options.len(), 2);
}

#[test]
fn confirm_model_preserves_available_model_catalog() {
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

    let snapshot = session.config_snapshot();
    let model = snapshot
        .options
        .iter()
        .find(|option| option.id == "model")
        .expect("model option present");
    assert_eq!(model.current_value.as_deref(), Some("claude-opus-4"));
    assert_eq!(model.options.len(), 2);
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
fn apply_advertised_config_options_merges_partial_updates_and_keeps_model_reasoning_independent() {
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
    assert_eq!(models.current_model_id.to_string(), "gpt-5.5");
    assert_eq!(models.available_models.len(), 2);
    assert_eq!(models.available_models[0].model_id.to_string(), "gpt-5.5");
    assert_eq!(models.available_models[1].model_id.to_string(), "gpt-5.4");
    assert_eq!(
        config_options
            .iter()
            .find(|option| option.id.to_string() == "reasoning_effort")
            .and_then(|option| match &option.kind {
                agent_client_protocol::schema::SessionConfigKind::Select(select) => {
                    Some(select.current_value.to_string())
                }
                _ => None,
            }),
        Some("medium".to_owned())
    );
}

#[test]
fn apply_advertised_config_options_preserves_confirmed_explicit_model_when_current_values_lag() {
    let mut session = make_session();
    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.5",
            vec![
                SessionConfigSelectOption::new("gpt-5.5", "GPT-5.5"),
                SessionConfigSelectOption::new("gpt-5.4", "GPT-5.4"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "low",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);
    session.drain_events();

    session.confirm_model(ModelId::new("gpt-5.4"));
    session.drain_events();

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.5",
            vec![
                SessionConfigSelectOption::new("gpt-5.5", "GPT-5.5"),
                SessionConfigSelectOption::new("gpt-5.4", "GPT-5.4"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "low",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    let models = session.model_info().expect("model catalog");
    assert_eq!(
        models.current_model_id.to_string(),
        "gpt-5.4",
        "lagging config option current values must not overwrite an explicitly confirmed model"
    );
    assert_eq!(models.available_models.len(), 2);
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

#[test]
fn pending_model_seed_resolves_category_to_raw_config_key_and_suppresses_legacy_set_model() {
    let mut session = AcpSession::new(None, Some(ModelId::new("openai/gpt-5")), HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new("openai/gpt-5"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "opencode/big-pickle",
            vec![
                SessionConfigSelectOption::new("opencode/big-pickle", "Big Pickle"),
                SessionConfigSelectOption::new("openai/gpt-5", "GPT-5"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    let results = session.resolve_pending_startup_config_seeds();
    assert_eq!(
        results,
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::Model,
            option_id: ConfigKey::new("model"),
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("model"),
            value: ConfigValue::new("openai/gpt-5"),
        }]
    );
}

#[test]
fn pending_mode_seed_resolves_category_to_raw_config_key_and_suppresses_legacy_set_mode() {
    let mut session = AcpSession::new(Some(ModeId::new("build")), None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new("build"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("build", "Build"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode),
    ]);

    let results = session.resolve_pending_startup_config_seeds();
    assert_eq!(
        results,
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::Mode,
            option_id: ConfigKey::new("mode"),
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("mode"),
            value: ConfigValue::new("build"),
        }]
    );
}

#[test]
fn pending_mode_seed_maps_full_access_to_agent_full_access_when_catalog_selects_it() {
    let mut session = AcpSession::new(Some(ModeId::new("full-access")), None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new("full-access"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "auto",
            vec![
                SessionConfigSelectOption::new("auto", "Auto"),
                SessionConfigSelectOption::new("agent-full-access", "Agent Full Access"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode),
    ]);

    let results = session.resolve_pending_startup_config_seeds_with_mode_normalizer(|requested, available_values| {
        assert_eq!(requested, "full-access");
        assert_eq!(available_values, vec!["auto", "agent-full-access"]);
        "agent-full-access".to_owned()
    });
    assert_eq!(
        results,
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::Mode,
            option_id: ConfigKey::new("mode"),
        }]
    );
    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("mode"),
            value: ConfigValue::new("agent-full-access"),
        }]
    );
}

#[test]
fn pending_mode_seed_maps_agent_full_access_to_full_access_when_legacy_catalog_selects_it() {
    let mut session = AcpSession::new(Some(ModeId::new("agent-full-access")), None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new("agent-full-access"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "mode",
            "Mode",
            "auto",
            vec![
                SessionConfigSelectOption::new("auto", "Auto"),
                SessionConfigSelectOption::new("full-access", "Full Access"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode),
    ]);

    let results = session.resolve_pending_startup_config_seeds_with_mode_normalizer(|requested, available_values| {
        assert_eq!(requested, "agent-full-access");
        assert_eq!(available_values, vec!["auto", "full-access"]);
        "full-access".to_owned()
    });
    assert_eq!(
        results,
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::Mode,
            option_id: ConfigKey::new("mode"),
        }]
    );
    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("mode"),
            value: ConfigValue::new("full-access"),
        }]
    );
}

#[test]
fn pending_model_seed_falls_back_to_legacy_set_model_when_model_config_option_is_absent() {
    let mut session = AcpSession::new(None, Some(ModelId::new("openai/gpt-5")), HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new("openai/gpt-5"));

    session.apply_advertised_modes(SessionModeState::new("build".to_owned(), vec![]));

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::OptionNotAdvertised {
            category: SessionConfigOptionCategory::Model,
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetModel {
            model: ModelId::new("openai/gpt-5"),
        }]
    );
}

#[test]
fn pending_mode_seed_falls_back_to_legacy_set_mode_when_mode_config_option_is_absent() {
    let mut session = AcpSession::new(Some(ModeId::new("build")), None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new("build"));

    session.apply_advertised_models(SessionModelState::new("gpt-5".to_owned(), vec![]));

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::OptionNotAdvertised {
            category: SessionConfigOptionCategory::Mode,
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetMode {
            mode: ModeId::new("build"),
        }]
    );
}

#[test]
fn pending_mode_seed_uses_legacy_set_mode_when_preloaded_catalog_is_supplemental_only() {
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
    session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new("full-access"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::OptionNotAdvertised {
            category: SessionConfigOptionCategory::Mode,
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetMode {
            mode: ModeId::new("full-access"),
        }]
    );
}

#[test]
fn pending_model_seed_is_dropped_without_legacy_fallback_when_model_config_option_rejects_value() {
    let mut session = AcpSession::new(None, Some(ModelId::new("openai/gpt-5")), HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new("openai/gpt-5"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "opencode/big-pickle",
            vec![SessionConfigSelectOption::new("opencode/big-pickle", "Big Pickle")],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::ValueNotSelectable {
            category: SessionConfigOptionCategory::Model,
        }]
    );
    assert!(session.plan_reconcile().is_empty());
}

#[test]
fn startup_model_seed_prevents_opencode_default_model_config_from_remaining_selected() {
    let mut session = AcpSession::new(None, Some(ModelId::new("openai/gpt-5")), HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new("openai/gpt-5"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "opencode/big-pickle",
            vec![
                SessionConfigSelectOption::new("opencode/big-pickle", "OpenCode Big Pickle"),
                SessionConfigSelectOption::new("openai/gpt-5", "OpenAI GPT-5"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "mode",
            "Mode",
            "build",
            vec![SessionConfigSelectOption::new("build", "Build")],
        )
        .category(SessionConfigOptionCategory::Mode),
    ]);

    session.resolve_pending_startup_config_seeds();

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("model"),
            value: ConfigValue::new("openai/gpt-5"),
        }]
    );

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "openai/gpt-5",
            vec![
                SessionConfigSelectOption::new("opencode/big-pickle", "OpenCode Big Pickle"),
                SessionConfigSelectOption::new("openai/gpt-5", "OpenAI GPT-5"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    assert!(session.plan_reconcile().is_empty());
    assert_eq!(
        session
            .config_options()
            .and_then(|options| options.iter().find(|option| option.id.to_string() == "model"))
            .and_then(|option| match &option.kind {
                agent_client_protocol::schema::SessionConfigKind::Select(select) => {
                    Some(select.current_value.to_string())
                }
                _ => None,
            }),
        Some("openai/gpt-5".to_owned())
    );
}

#[test]
fn pending_thought_level_seed_resolves_category_to_raw_config_key() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("high"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::ThoughtLevel,
            option_id: ConfigKey::new("reasoning_effort"),
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("reasoning_effort"),
            value: ConfigValue::new("high"),
        }]
    );
}

#[test]
fn pending_thought_level_seed_resolves_alias_when_category_is_missing() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("high"));

    session.apply_advertised_config_options(vec![SessionConfigOption::select(
        "effort",
        "Effort",
        "none",
        vec![
            SessionConfigSelectOption::new("none", "None"),
            SessionConfigSelectOption::new("low", "Low"),
            SessionConfigSelectOption::new("medium", "Medium"),
            SessionConfigSelectOption::new("high", "High"),
        ],
    )]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::ThoughtLevel,
            option_id: ConfigKey::new("effort"),
        }]
    );

    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("effort"),
            value: ConfigValue::new("high"),
        }]
    );
}

#[test]
fn pending_thought_level_seed_waits_for_late_config_option_after_model_change() {
    let mut session = AcpSession::new(None, Some(ModelId::new("openai/gpt-5.5")), HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new("openai/gpt-5.5"));
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("medium"));

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "opencode/big-pickle",
            vec![
                SessionConfigSelectOption::new("opencode/big-pickle", "OpenCode Big Pickle"),
                SessionConfigSelectOption::new("openai/gpt-5.5", "OpenAI GPT-5.5"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![
            PendingStartupConfigSeedResult::Applied {
                category: SessionConfigOptionCategory::Model,
                option_id: ConfigKey::new("model"),
            },
            PendingStartupConfigSeedResult::OptionNotAdvertised {
                category: SessionConfigOptionCategory::ThoughtLevel,
            },
        ]
    );
    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("model"),
            value: ConfigValue::new("openai/gpt-5.5"),
        }]
    );

    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "openai/gpt-5.5",
            vec![
                SessionConfigSelectOption::new("opencode/big-pickle", "OpenCode Big Pickle"),
                SessionConfigSelectOption::new("openai/gpt-5.5", "OpenAI GPT-5.5"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "effort",
            "Effort",
            "none",
            vec![
                SessionConfigSelectOption::new("none", "None"),
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        ),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::ThoughtLevel,
            option_id: ConfigKey::new("effort"),
        }]
    );
    assert_eq!(
        session.plan_reconcile(),
        vec![ReconcileAction::SetConfigOption {
            key: ConfigKey::new("effort"),
            value: ConfigValue::new("medium"),
        }]
    );
}

#[test]
fn pending_thought_level_seed_waits_when_option_is_unavailable() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("high"));

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::OptionNotAdvertised {
            category: SessionConfigOptionCategory::ThoughtLevel,
        }]
    );
    assert!(session.plan_reconcile().is_empty());
    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::OptionNotAdvertised {
            category: SessionConfigOptionCategory::ThoughtLevel,
        }]
    );
}

#[test]
fn pending_thought_level_seed_is_dropped_when_value_is_not_selectable() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("xhigh"));
    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "effort",
            "Effort",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::ValueNotSelectable {
            category: SessionConfigOptionCategory::ThoughtLevel,
        }]
    );
    assert!(session.plan_reconcile().is_empty());
}

#[test]
fn pending_thought_level_seed_does_not_reconcile_when_observed_already_matches() {
    let mut session = AcpSession::new(None, None, HashMap::new());
    session.seed_pending_startup_config(SessionConfigOptionCategory::ThoughtLevel, ConfigValue::new("high"));
    session.apply_advertised_config_options(vec![
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]);

    assert_eq!(
        session.resolve_pending_startup_config_seeds(),
        vec![PendingStartupConfigSeedResult::Applied {
            category: SessionConfigOptionCategory::ThoughtLevel,
            option_id: ConfigKey::new("reasoning_effort"),
        }]
    );
    assert!(session.plan_reconcile().is_empty());
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
    assert!(s.has_pending_session_new_prelude());
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
