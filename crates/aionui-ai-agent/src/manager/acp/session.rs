use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AvailableCommand, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOptions, SessionModeState, SessionModelState, UsageUpdate,
};

use super::agent_event_tracker::AcpSessionEvent;
use super::agent_reconcile::ReconcileAction;
use super::config_option_catalog::{
    derive_models_from_config_options, derive_modes_from_config_options, merge_config_options,
};
use super::config_options::ConfigSnapshot;
use crate::protocol::error::CloseReason;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId, PersistedSessionState, SessionId};

/// What the user wants the session to be (intent).
#[derive(Debug, Clone, Default)]
struct Desired {
    mode_id: Option<ModeId>,
    model_id: Option<ModelId>,
    config_selections: HashMap<ConfigKey, ConfigValue>,
    pending_startup_config: Vec<PendingStartupConfigSeed>,
}

/// What the CLI last reported (ground truth from the backend).
#[derive(Debug, Clone, Default)]
struct Observed {
    mode_id: Option<ModeId>,
    model_id: Option<ModelId>,
    config_current: HashMap<ConfigKey, ConfigValue>,
}

/// What the CLI advertises as available options.
#[derive(Debug, Clone, Default)]
struct Advertised {
    modes: Option<SessionModeState>,
    models: Option<SessionModelState>,
    config_options: Option<Vec<SessionConfigOption>>,
    context_usage: Option<UsageUpdate>,
    agent_capabilities: Option<AgentCapabilities>,
    auth_methods: Option<Vec<AuthMethod>>,
    available_commands: Option<Vec<AvailableCommand>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CatalogPreloadSummary {
    pub(crate) mode_preloaded: bool,
    pub(crate) model_preloaded: bool,
    pub(crate) mode_catalog_count: usize,
    pub(crate) model_catalog_count: usize,
}

impl CatalogPreloadSummary {
    pub(crate) fn any_preloaded(self) -> bool {
        self.mode_preloaded || self.model_preloaded
    }
}

/// Aggregate root for a single ACP session's lifecycle and state.
///
/// Encapsulates the three-layer state model (desired / observed / advertised)
/// and protects invariants:
/// - `session_id` is assigned at most once per lifecycle
/// - `desired.mode_id` must be in `advertised.modes` (when modes are known)
/// - `plan_reconcile` is a pure function: no side effects, fully testable
///
/// All mutations happen through aggregate methods which may emit domain
/// events (collected in `pending_events` and drained by the driver).
#[derive(Debug, Clone)]
pub struct AcpSession {
    session_id: Option<SessionId>,
    opened: bool,
    desired: Desired,
    observed: Observed,
    advertised: Advertised,
    /// Single-flight lease over user-triggered config/mode/model updates.
    ///
    /// Held as an `Arc<AtomicBool>` (not a plain `bool`) so a
    /// `ConfigSetGuardToken` can carry an owning clone and reset the flag from
    /// its `Drop` on every exit path — success, error, RPC timeout,
    /// future-cancel, and panic unwind. This removes the ELECTRON-3MS deadlock
    /// where a hung `set_config_option` RPC left the lease stuck until runtime
    /// recovery rebuilt the session. Cloning `AcpSession` shares the flag
    /// (transient in-flight state); no invariant depends on it being distinct.
    config_set_in_flight: Arc<AtomicBool>,
    pending_events: Vec<AcpSessionEvent>,
    /// Whether `open_session_new` has just completed and the next prompt
    /// should receive preset_context / skill-index injection.
    ///
    /// Lifecycle:
    /// - writer: `AcpAgentManager::open_session_new` after a successful
    ///   `session/new` handshake.
    /// - reader: `SessionNewPreludeHook` via `take_pending_session_new_prelude`.
    /// - invalidation: any `take_*` call drains it to `false`.
    ///
    /// Starts `false` so resume paths, warmup-only flows, and aborted
    /// session/new attempts all correctly observe "no prelude pending".
    pending_session_new_prelude: bool,
    /// Why the session most recently terminated, if at all.
    ///
    /// Lifecycle (see also `CloseReason` doc comment):
    /// - writer: `record_close_reason`, called by the manager from each
    ///   close path (`send_message` Err, `cancel`, `kill`, post-init exit
    ///   detection).
    /// - reader: `last_close_reason` (non-destructive) for diagnostics and
    ///   `take_close_reason` (drain) by the toast-builder right before the
    ///   `Error` event broadcast.
    /// - invalidation: cleared on `clear_session_id` so a rebuilt session
    ///   does not inherit the previous turn's close reason.
    last_close_reason: Option<CloseReason>,
}

/// RAII lease over `config_set_in_flight`. Holds an `Arc<AtomicBool>` clone;
/// its `Drop` synchronously stores `false`, so the lease is released on
/// success, error, timeout, future-cancel, and panic unwind alike. Not
/// `Clone`/`Copy` — a single guard owns the lease for its whole lifetime.
#[derive(Debug)]
pub struct ConfigSetGuardToken {
    flag: Arc<AtomicBool>,
}

impl Drop for ConfigSetGuardToken {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingStartupConfigSeed {
    category: SessionConfigOptionCategory,
    value: ConfigValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingStartupConfigSeedResult {
    Applied {
        category: SessionConfigOptionCategory,
        option_id: ConfigKey,
    },
    OptionNotAdvertised {
        category: SessionConfigOptionCategory,
    },
    ValueNotSelectable {
        category: SessionConfigOptionCategory,
    },
}

impl AcpSession {
    pub fn new(
        initial_mode: Option<ModeId>,
        initial_model: Option<ModelId>,
        config_selections: HashMap<ConfigKey, ConfigValue>,
    ) -> Self {
        Self {
            session_id: None,
            opened: false,
            pending_session_new_prelude: false,
            desired: Desired {
                mode_id: initial_mode,
                model_id: initial_model,
                config_selections,
                pending_startup_config: Vec::new(),
            },
            observed: Observed::default(),
            advertised: Advertised::default(),
            config_set_in_flight: Arc::new(AtomicBool::new(false)),
            pending_events: Vec::new(),
            last_close_reason: None,
        }
    }
}

impl AcpSession {
    /// Atomically claim the single-flight config lease. Returns a RAII guard
    /// on success (releases the lease on drop), or `None` when a config update
    /// is already in flight — preserving the `rejected_in_progress` semantics.
    pub fn try_begin_config_set(&mut self) -> Option<ConfigSetGuardToken> {
        if self
            .config_set_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(ConfigSetGuardToken {
                flag: Arc::clone(&self.config_set_in_flight),
            })
        } else {
            None
        }
    }
}

// ─── Session Id and Session Opened ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_ref().map(SessionId::as_str)
    }

    pub fn session_id_vo(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Assign (or restore) a session ID. Idempotent: re-assigning the same
    /// ID is a no-op. Assigning a *different* ID after one is already set
    /// is an invariant violation (the aggregate must be recreated).
    pub fn set_session_id(&mut self, sid: SessionId) {
        if let Some(existing) = &self.session_id {
            debug_assert_eq!(existing, &sid, "session_id reassignment attempted");
            return;
        }
        self.session_id = Some(sid.clone());
        self.pending_events
            .push(AcpSessionEvent::SessionAssigned { session_id: sid });
    }

    /// Drop a stale session id so the aggregate can be re-seeded with a
    /// freshly-issued one. Used when the CLI rejects the persisted sid
    /// with `SessionNotFound` (ELECTRON-1HQ): the resume helpers fall
    /// back to `open_session_new`, which calls `set_session_id` again.
    /// Also clears the `opened` flag so the next `ensure_session_opened`
    /// goes down the "no sid" branch instead of the "sid+opened" no-op.
    pub fn clear_session_id(&mut self) {
        self.session_id = None;
        self.opened = false;
        // A rebuilt session must not inherit the prior turn's close reason —
        // otherwise the next user-facing error would surface stale context.
        self.last_close_reason = None;
    }

    /// Record the reason the most recent turn closed. Overwrites any
    /// previous reason — only the latest one is meaningful for the next
    /// user-facing toast. Pass `None` to clear (rare; mostly used by tests
    /// and `clear_session_id`).
    pub fn record_close_reason(&mut self, reason: Option<CloseReason>) {
        self.last_close_reason = reason;
    }

    /// Read the last close reason without consuming it. Used for
    /// diagnostics and for tests.
    pub fn last_close_reason(&self) -> Option<&CloseReason> {
        self.last_close_reason.as_ref()
    }

    /// Drain the last close reason. Called by the close-path handler in
    /// `AcpAgentManager` right before broadcasting the `Error` event so
    /// the same reason is not re-rendered on a follow-up request.
    pub fn take_close_reason(&mut self) -> Option<CloseReason> {
        self.last_close_reason.take()
    }

    pub fn is_opened(&self) -> bool {
        self.opened
    }

    /// Mark the session as opened with the CLI (first turn handshake complete).
    pub fn mark_opened(&mut self) {
        if !self.opened {
            self.opened = true;
            self.pending_events.push(AcpSessionEvent::SessionOpened);
        }
    }

    /// Set the flag signalling that the next prompt carries the first
    /// post-`session/new` payload. Idempotent.
    pub fn mark_pending_session_new_prelude(&mut self) {
        self.pending_session_new_prelude = true;
    }

    pub fn has_pending_session_new_prelude(&self) -> bool {
        self.pending_session_new_prelude
    }

    /// Consume the prelude flag. Returns `true` exactly once after
    /// `mark_pending_session_new_prelude`; subsequent calls return `false`.
    pub fn take_pending_session_new_prelude(&mut self) -> bool {
        std::mem::replace(&mut self.pending_session_new_prelude, false)
    }
}

// ─── Getters Setters desired ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn desired_mode(&self) -> Option<&str> {
        self.desired.mode_id.as_ref().map(ModeId::as_str)
    }

    pub fn desired_mode_id(&self) -> Option<&ModeId> {
        self.desired.mode_id.as_ref()
    }

    pub fn desired_model(&self) -> Option<&str> {
        self.desired.model_id.as_ref().map(ModelId::as_str)
    }

    pub fn desired_model_id(&self) -> Option<&ModelId> {
        self.desired.model_id.as_ref()
    }

    pub fn desired_config_selections(&self) -> &HashMap<ConfigKey, ConfigValue> {
        &self.desired.config_selections
    }

    /// Whether the requested model can be selected in the current session.
    ///
    /// Before the ACP backend advertises models, keep the historical permissive
    /// behavior so initial seeds can still be reconciled once the session opens.
    pub fn can_select_model(&self, model_id: &str) -> bool {
        !model_id.is_empty() && self.is_model_valid(model_id)
    }

    /// Whether the requested mode can be selected in the current session.
    ///
    /// Before the ACP backend advertises modes, keep the historical permissive
    /// behavior so initial seeds can still be reconciled once the session opens.
    pub fn can_select_mode(&self, mode_id: &str) -> bool {
        !mode_id.is_empty() && self.is_mode_valid(mode_id)
    }

    /// Set the user's desired mode. Emits `DesiredModeChanged` if the
    /// value actually changed. When advertised modes are known, the mode
    /// must be in the list (otherwise the call is a no-op).
    pub fn set_desired_mode(&mut self, mode: ModeId) -> bool {
        if mode.as_str().is_empty() {
            return false;
        }
        if !self.is_mode_valid(mode.as_str()) {
            return false;
        }
        if self.desired.mode_id.as_ref() == Some(&mode) {
            return false;
        }
        self.desired.mode_id = Some(mode.clone());
        self.pending_events.push(AcpSessionEvent::DesiredModeChanged { mode });
        true
    }

    /// Set the user's desired model. Emits `DesiredModelChanged` if the
    /// value actually changed. When advertised models are known, the model
    /// must be in the list (otherwise the call is a no-op).
    pub fn set_desired_model(&mut self, model: ModelId) -> bool {
        if model.as_str().is_empty() {
            return false;
        }
        if !self.is_model_valid(model.as_str()) {
            return false;
        }
        if self.desired.model_id.as_ref() == Some(&model) {
            return false;
        }
        self.desired.model_id = Some(model.clone());
        self.pending_events.push(AcpSessionEvent::DesiredModelChanged { model });
        true
    }

    /// Drop a desired model that is not advertised by the active ACP session.
    ///
    /// Initial model seeds can be loaded before `session/new` reports the
    /// provider's available models. Once advertised models are known, reconcile
    /// must not issue `session/set_model` for a stale seed.
    pub fn clear_invalid_desired_model(&mut self) -> Option<ModelId> {
        let model = self.desired.model_id.clone()?;
        if self.is_model_valid(model.as_str()) {
            return None;
        }
        self.desired.model_id = None;
        Some(model)
    }

    /// Drop a desired mode that is not advertised by the active ACP session.
    ///
    /// Initial mode seeds can be loaded before `session/new` reports the
    /// provider's available modes. Once advertised modes are known, reconcile
    /// must not issue `session/set_mode` for a stale seed.
    pub fn clear_invalid_desired_mode(&mut self) -> Option<ModeId> {
        let mode = self.desired.mode_id.clone()?;
        if self.is_mode_valid(mode.as_str()) {
            return None;
        }
        self.desired.mode_id = None;
        Some(mode)
    }

    /// Set a user's desired config selection.
    pub fn set_desired_config(&mut self, key: ConfigKey, value: ConfigValue) {
        let changed = self.desired.config_selections.get(&key) != Some(&value);
        self.desired.config_selections.insert(key, value);
        if changed {
            let selections = self.desired.config_selections.clone();
            self.pending_events
                .push(AcpSessionEvent::DesiredConfigChanged { selections });
        }
    }

    pub(crate) fn seed_pending_startup_config(&mut self, category: SessionConfigOptionCategory, value: ConfigValue) {
        if value.as_str().is_empty() {
            return;
        }
        if self
            .desired
            .pending_startup_config
            .iter()
            .any(|seed| seed.category == category && seed.value == value)
        {
            return;
        }
        self.desired
            .pending_startup_config
            .push(PendingStartupConfigSeed { category, value });
    }

    #[cfg(test)]
    pub(crate) fn resolve_pending_startup_config_seeds(&mut self) -> Vec<PendingStartupConfigSeedResult> {
        self.resolve_pending_startup_config_seeds_with_mode_normalizer(|requested, _available_values| {
            requested.to_owned()
        })
    }

    pub(crate) fn resolve_pending_startup_config_seeds_with_mode_normalizer(
        &mut self,
        mode_normalizer: impl Fn(&str, Vec<&str>) -> String,
    ) -> Vec<PendingStartupConfigSeedResult> {
        let seeds = std::mem::take(&mut self.desired.pending_startup_config);
        if seeds.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::with_capacity(seeds.len());
        for seed in seeds {
            let Some(options) = self.advertised.config_options.as_ref() else {
                self.handle_unresolved_startup_config_seed(&seed, false);
                results.push(PendingStartupConfigSeedResult::OptionNotAdvertised {
                    category: seed.category,
                });
                continue;
            };

            let Some(option) = select_option_for_startup_seed(options, &seed.category) else {
                self.handle_unresolved_startup_config_seed(&seed, false);
                results.push(PendingStartupConfigSeedResult::OptionNotAdvertised {
                    category: seed.category,
                });
                continue;
            };

            let resolved_value = if seed.category == SessionConfigOptionCategory::Mode {
                ConfigValue::new(mode_normalizer(seed.value.as_str(), select_option_values(&option.kind)))
            } else {
                seed.value.clone()
            };

            if !select_option_contains_value(&option.kind, resolved_value.as_str()) {
                self.handle_unresolved_startup_config_seed(&seed, true);
                results.push(PendingStartupConfigSeedResult::ValueNotSelectable {
                    category: seed.category,
                });
                continue;
            }

            let option_id = ConfigKey::new(option.id.to_string());
            self.clear_legacy_desired_for_config_category(&seed.category);
            self.set_desired_config(option_id.clone(), resolved_value);
            results.push(PendingStartupConfigSeedResult::Applied {
                category: seed.category,
                option_id,
            });
        }
        results
    }

    fn clear_legacy_desired_for_config_category(&mut self, category: &SessionConfigOptionCategory) {
        match category {
            SessionConfigOptionCategory::Mode => {
                self.desired.mode_id = None;
            }
            SessionConfigOptionCategory::Model => {
                self.desired.model_id = None;
            }
            _ => {}
        }
    }

    fn handle_unresolved_startup_config_seed(&mut self, seed: &PendingStartupConfigSeed, option_was_advertised: bool) {
        match seed.category {
            SessionConfigOptionCategory::Mode | SessionConfigOptionCategory::Model if !option_was_advertised => {
                // Keep legacy desired mode/model so old ACP implementations still use set_mode/set_model.
            }
            SessionConfigOptionCategory::Mode | SessionConfigOptionCategory::Model => {
                self.clear_legacy_desired_for_config_category(&seed.category);
            }
            SessionConfigOptionCategory::ThoughtLevel if !option_was_advertised => {
                self.seed_pending_startup_config(seed.category.clone(), seed.value.clone());
            }
            _ => {}
        }
    }
}

// ─── Getters observed ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn observed_mode(&self) -> Option<&str> {
        self.observed.mode_id.as_ref().map(ModeId::as_str)
    }

    pub fn observed_mode_id(&self) -> Option<&ModeId> {
        self.observed.mode_id.as_ref()
    }

    pub fn observed_model(&self) -> Option<&str> {
        self.observed.model_id.as_ref().map(ModelId::as_str)
    }

    pub fn observed_model_id(&self) -> Option<&ModelId> {
        self.observed.model_id.as_ref()
    }
}

// ─── Getters advertised ───────────────────────────────────────────────────────
impl AcpSession {
    pub fn modes(&self) -> Option<&SessionModeState> {
        self.advertised.modes.as_ref()
    }

    pub fn model_info(&self) -> Option<&SessionModelState> {
        self.advertised.models.as_ref()
    }

    pub fn config_options(&self) -> Option<&[SessionConfigOption]> {
        self.advertised.config_options.as_deref()
    }

    pub(crate) fn config_snapshot(&self) -> ConfigSnapshot {
        if let Some(options) = self.advertised.config_options.clone() {
            return ConfigSnapshot::from_real_options_with_runtime_supplements(
                options,
                self.advertised.modes.as_ref(),
                self.advertised.models.as_ref(),
            );
        }
        ConfigSnapshot::from_legacy_catalogs(self.advertised.modes.as_ref(), self.advertised.models.as_ref())
    }

    pub fn context_usage(&self) -> Option<&UsageUpdate> {
        self.advertised.context_usage.as_ref()
    }

    pub fn agent_capabilities(&self) -> Option<&AgentCapabilities> {
        self.advertised.agent_capabilities.as_ref()
    }

    pub fn auth_methods(&self) -> Option<&[AuthMethod]> {
        self.advertised.auth_methods.as_deref()
    }

    pub fn available_commands(&self) -> Option<&[AvailableCommand]> {
        self.advertised.available_commands.as_deref()
    }

    pub fn current_mode_id(&self) -> Option<String> {
        self.advertised.modes.as_ref().map(|m| m.current_mode_id.to_string())
    }

    pub fn current_model_id(&self) -> Option<String> {
        self.advertised.models.as_ref().map(|m| m.current_model_id.to_string())
    }
}

// ─── Observations (from CLI responses/notifications) ───────────────
impl AcpSession {
    /// Record the CLI's current mode. Updates both `observed.mode_id` and
    /// the `advertised.modes.current_mode_id` (available_modes preserved);
    /// emits `ObservedModeSynced` when the value actually changed.
    pub fn apply_observed_mode(&mut self, mode: ModeId) {
        let changed = self.observed.mode_id.as_ref() != Some(&mode);
        self.observed.mode_id = Some(mode.clone());
        let available = self
            .advertised
            .modes
            .as_ref()
            .map(|m| m.available_modes.clone())
            .unwrap_or_default();
        self.advertised.modes = Some(SessionModeState::new(mode.as_str().to_owned(), available));
        if changed {
            self.pending_events.push(AcpSessionEvent::ObservedModeSynced { mode });
        }
    }

    /// Record the CLI's current model. Updates both `observed.model_id` and
    /// the `advertised.models.current_model_id` (available_models preserved);
    /// emits `ObservedModelSynced` when the value actually changed.
    pub fn apply_observed_model(&mut self, model: ModelId) {
        let changed = self.observed.model_id.as_ref() != Some(&model);
        self.observed.model_id = Some(model.clone());
        let available = self
            .advertised
            .models
            .as_ref()
            .map(|m| m.available_models.clone())
            .unwrap_or_default();
        self.advertised.models = Some(SessionModelState::new(model.as_str().to_owned(), available));
        if changed {
            self.pending_events.push(AcpSessionEvent::ObservedModelSynced { model });
        }
    }

    /// Confirm a user command after the ACP backend accepted it.
    ///
    /// Unlike `apply_observed_mode`, this also aligns the pending intent so
    /// a later startup/recovery reconcile does not pull the session back to
    /// the previous desired mode.
    pub fn confirm_mode(&mut self, mode: ModeId) {
        self.desired.mode_id = Some(mode.clone());
        self.apply_observed_mode(mode);
    }

    /// Confirm a user command after the ACP backend accepted it.
    ///
    /// Unlike `apply_observed_model`, this also aligns the pending intent so
    /// a later startup/recovery reconcile does not pull the session back to
    /// the previous desired model.
    pub fn confirm_model(&mut self, model: ModelId) {
        self.desired.model_id = Some(model.clone());
        self.apply_observed_model(model);
    }

    /// Record the CLI's current value for a single config option. Mirrors
    /// `apply_observed_mode/model`: diff-driven, emits `ObservedConfigSynced`
    /// with the full selection map when the value actually changed. Used by
    /// the reconcile loop after a successful `set_config_option` so
    /// `plan_reconcile` treats the drift as resolved.
    pub fn apply_observed_config(&mut self, key: ConfigKey, value: ConfigValue) {
        let changed = self.observed.config_current.get(&key) != Some(&value);
        self.observed.config_current.insert(key, value);
        if changed {
            let selections = self.observed.config_current.clone();
            self.pending_events
                .push(AcpSessionEvent::ObservedConfigSynced { selections });
        }
    }

    pub fn apply_advertised_modes(&mut self, modes: SessionModeState) {
        let incoming_mode_catalog_count = modes.available_modes.len();
        let existing_mode_catalog_count = self
            .advertised
            .modes
            .as_ref()
            .map_or(0, |modes| modes.available_modes.len());
        let modes = self.preserve_existing_mode_catalog_if_empty(modes);
        let final_mode_catalog_count = modes.available_modes.len();
        let new_id = ModeId::new(modes.current_mode_id.to_string());
        let changed = self.observed.mode_id.as_ref() != Some(&new_id);
        if incoming_mode_catalog_count == 0
            && existing_mode_catalog_count > 0
            && final_mode_catalog_count == existing_mode_catalog_count
        {
            tracing::debug!(
                current_mode = %new_id,
                preserved_mode_catalog_count = final_mode_catalog_count,
                "ACP advertised modes kept existing catalog for empty runtime update"
            );
        }
        self.observed.mode_id = Some(new_id.clone());
        self.advertised.modes = Some(modes);
        if changed {
            self.pending_events
                .push(AcpSessionEvent::ObservedModeSynced { mode: new_id });
        }
    }

    pub fn apply_advertised_models(&mut self, models: SessionModelState) {
        let incoming_model_catalog_count = models.available_models.len();
        let existing_model_catalog_count = self
            .advertised
            .models
            .as_ref()
            .map_or(0, |models| models.available_models.len());
        let models = self.preserve_existing_model_catalog_if_empty(models);
        let final_model_catalog_count = models.available_models.len();
        let new_id = ModelId::new(models.current_model_id.to_string());
        let changed = self.observed.model_id.as_ref() != Some(&new_id);
        if incoming_model_catalog_count == 0
            && existing_model_catalog_count > 0
            && final_model_catalog_count == existing_model_catalog_count
        {
            tracing::debug!(
                current_model = %new_id,
                preserved_model_catalog_count = final_model_catalog_count,
                "ACP advertised models kept existing catalog for empty runtime update"
            );
        }
        self.observed.model_id = Some(new_id.clone());
        self.advertised.models = Some(models);
        if changed {
            self.pending_events
                .push(AcpSessionEvent::ObservedModelSynced { model: new_id });
        }
    }

    fn preserve_existing_mode_catalog_if_empty(&self, mut modes: SessionModeState) -> SessionModeState {
        if modes.available_modes.is_empty()
            && let Some(existing) = self.advertised.modes.as_ref()
            && !existing.available_modes.is_empty()
        {
            modes.available_modes.clone_from(&existing.available_modes);
        }
        modes
    }

    fn preserve_existing_model_catalog_if_empty(&self, mut models: SessionModelState) -> SessionModelState {
        if models.available_models.is_empty()
            && let Some(existing) = self.advertised.models.as_ref()
            && !existing.available_models.is_empty()
        {
            models.available_models.clone_from(&existing.available_models);
        }
        models
    }

    fn preserve_desired_model_in_catalog(&self, models: SessionModelState) -> SessionModelState {
        let Some(desired_model) = self.desired.model_id.as_ref() else {
            return models;
        };
        let desired_model_id = desired_model.as_str();
        if models.current_model_id.to_string() == desired_model_id {
            return models;
        }
        if models
            .available_models
            .iter()
            .any(|model| model.model_id.to_string() == desired_model_id)
        {
            return SessionModelState::new(desired_model_id.to_owned(), models.available_models.clone());
        }
        models
    }

    pub fn apply_advertised_config_options(&mut self, options: Vec<SessionConfigOption>) {
        let options = merge_config_options(self.advertised.config_options.as_deref(), options);
        let supplement_summary = ConfigSnapshot::supplement_summary_for_real_options(
            &options,
            self.advertised.modes.as_ref(),
            self.advertised.models.as_ref(),
        );
        if let Some(supplemented_categories) = supplement_summary.categories_csv() {
            tracing::info!(
                supplemented_categories,
                real_option_count = options.len(),
                mode_catalog_count = self
                    .advertised
                    .modes
                    .as_ref()
                    .map_or(0, |modes| modes.available_modes.len()),
                model_catalog_count = self
                    .advertised
                    .models
                    .as_ref()
                    .map_or(0, |models| models.available_models.len()),
                "ACP config options supplemented from advertised catalog"
            );
        }

        if let Some(modes) = derive_modes_from_config_options(&options) {
            self.apply_advertised_modes(modes);
        }

        if let Some(models) = derive_models_from_config_options(&options) {
            self.apply_advertised_models(self.preserve_desired_model_in_catalog(models));
        }

        let mut changed = false;
        for opt in &options {
            if let Some(current) = extract_config_current_value(&opt.kind) {
                let key = ConfigKey::new(opt.id.to_string());
                let value = ConfigValue::new(current);
                if self.observed.config_current.insert(key, value.clone()).as_ref() != Some(&value) {
                    changed = true;
                }
            }
        }
        self.advertised.config_options = Some(options);
        if changed {
            let selections = self.observed.config_current.clone();
            self.pending_events
                .push(AcpSessionEvent::ObservedConfigSynced { selections });
        }
    }

    pub fn apply_advertised_capabilities(&mut self, caps: AgentCapabilities) {
        self.advertised.agent_capabilities = Some(caps);
    }

    pub fn apply_advertised_auth_methods(&mut self, methods: Vec<AuthMethod>) {
        self.advertised.auth_methods = Some(methods);
    }

    pub fn apply_advertised_commands(&mut self, commands: Vec<AvailableCommand>) {
        self.advertised.available_commands = Some(commands);
    }

    /// Record the CLI's latest context usage. Diff-driven: emits
    /// `ObservedContextUsageChanged` only when the usage payload differs
    /// from what we last cached, so the persistence consumer can debounce
    /// a stream of token updates into one DB write per turn.
    pub fn apply_context_usage(&mut self, usage: UsageUpdate) {
        let changed = self.advertised.context_usage.as_ref() != Some(&usage);
        self.advertised.context_usage = Some(usage.clone());
        if changed {
            let usage_json = serde_json::to_string(&usage).unwrap_or_default();
            self.pending_events
                .push(AcpSessionEvent::ObservedContextUsageChanged { usage_json });
        }
    }
}

impl AcpSession {
    /// Seed the aggregate with persisted user choices from DB.
    /// Called on resume paths before the CLI session/load response arrives.
    pub fn preload_persisted(&mut self, state: &PersistedSessionState) {
        if let Some(mode) = &state.current_mode_id {
            let available_modes = self
                .advertised
                .modes
                .as_ref()
                .map(|modes| modes.available_modes.clone())
                .unwrap_or_default();
            self.advertised.modes = Some(SessionModeState::new(mode.as_str().to_owned(), available_modes));
            self.observed.mode_id = Some(mode.clone());
        }
        if let Some(model) = &state.current_model_id {
            let available_models = self
                .advertised
                .models
                .as_ref()
                .map(|models| models.available_models.clone())
                .unwrap_or_default();
            self.advertised.models = Some(SessionModelState::new(model.as_str().to_owned(), available_models));
            self.observed.model_id = Some(model.clone());
        }
        if !state.config_selections.is_empty() {
            self.observed.config_current = state.config_selections.clone();
        }
        if let Some(usage) = &state.context_usage {
            self.advertised.context_usage = Some(usage.clone());
        }
    }

    pub(crate) fn preload_advertised_catalogs(
        &mut self,
        modes: Option<SessionModeState>,
        models: Option<SessionModelState>,
    ) -> CatalogPreloadSummary {
        let mut summary = CatalogPreloadSummary::default();

        if let Some(modes) = modes.filter(|modes| !modes.available_modes.is_empty())
            && self
                .advertised
                .modes
                .as_ref()
                .is_none_or(|existing| existing.available_modes.is_empty())
        {
            summary.mode_preloaded = true;
            summary.mode_catalog_count = modes.available_modes.len();
            self.advertised.modes = Some(self.mode_catalog_with_session_current(modes));
        }

        if let Some(models) = models.filter(|models| !models.available_models.is_empty())
            && self
                .advertised
                .models
                .as_ref()
                .is_none_or(|existing| existing.available_models.is_empty())
        {
            summary.model_preloaded = true;
            summary.model_catalog_count = models.available_models.len();
            self.advertised.models = Some(self.model_catalog_with_session_current(models));
        }

        summary
    }

    fn mode_catalog_with_session_current(&self, mut modes: SessionModeState) -> SessionModeState {
        let current = self
            .observed
            .mode_id
            .as_ref()
            .or(self.desired.mode_id.as_ref())
            .filter(|mode| {
                modes
                    .available_modes
                    .iter()
                    .any(|available| available.id.0.as_ref() == mode.as_str())
            });
        if let Some(current) = current {
            modes.current_mode_id = current.as_str().to_owned().into();
        }
        modes
    }

    fn model_catalog_with_session_current(&self, mut models: SessionModelState) -> SessionModelState {
        let current = self
            .observed
            .model_id
            .as_ref()
            .or(self.desired.model_id.as_ref())
            .filter(|model| {
                models
                    .available_models
                    .iter()
                    .any(|available| available.model_id.0.as_ref() == model.as_str())
            });
        if let Some(current) = current {
            models.current_model_id = current.as_str().to_owned().into();
        }
        models
    }
}

// ─── Reconcile ─────────────────────────────────────────────────────
impl AcpSession {
    /// Produce a list of actions needed to align CLI state with user intent.
    /// Pure function — no side effects. The driver executes the actions.
    pub fn plan_reconcile(&self) -> Vec<ReconcileAction> {
        let mut actions = Vec::new();

        if let Some(desired_mode) = &self.desired.mode_id
            && self.observed.mode_id.as_ref() != Some(desired_mode)
        {
            actions.push(ReconcileAction::SetMode {
                mode: desired_mode.clone(),
            });
        }

        if let Some(desired_model) = &self.desired.model_id
            && self.observed.model_id.as_ref() != Some(desired_model)
        {
            actions.push(ReconcileAction::SetModel {
                model: desired_model.clone(),
            });
        }

        for (key, desired_value) in &self.desired.config_selections {
            if self.observed.config_current.get(key) != Some(desired_value) {
                actions.push(ReconcileAction::SetConfigOption {
                    key: key.clone(),
                    value: desired_value.clone(),
                });
            }
        }

        actions
    }

    // ─── Event drain ───────────────────────────────────────────────────

    /// Consume and return all pending domain events.
    pub fn drain_events(&mut self) -> Vec<AcpSessionEvent> {
        std::mem::take(&mut self.pending_events)
    }

    // ─── Private helpers ───────────────────────────────────────────────

    fn is_mode_valid(&self, mode_id: &str) -> bool {
        match &self.advertised.modes {
            None => true,
            Some(modes) if modes.available_modes.is_empty() => true,
            Some(modes) => modes.available_modes.iter().any(|m| m.id.0.as_ref() == mode_id),
        }
    }

    fn is_model_valid(&self, model_id: &str) -> bool {
        match &self.advertised.models {
            None => true,
            Some(models) if models.available_models.is_empty() => true,
            Some(models) => models
                .available_models
                .iter()
                .any(|m| m.model_id.0.as_ref() == model_id),
        }
    }
}

fn extract_config_current_value(kind: &SessionConfigKind) -> Option<String> {
    match kind {
        SessionConfigKind::Select(sel) => Some(sel.current_value.to_string()),
        _ => None,
    }
}

fn select_option_for_startup_seed<'a>(
    options: &'a [SessionConfigOption],
    category: &SessionConfigOptionCategory,
) -> Option<&'a SessionConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_ref() == Some(category))
        .or_else(|| {
            let aliases = config_option_aliases_for_category(category);
            options.iter().find(|option| {
                let option_id = option.id.to_string();
                aliases.iter().any(|alias| *alias == option_id)
            })
        })
}

fn config_option_aliases_for_category(category: &SessionConfigOptionCategory) -> &'static [&'static str] {
    match category {
        SessionConfigOptionCategory::Mode => &["mode", "modes"],
        SessionConfigOptionCategory::Model => &["model", "models"],
        SessionConfigOptionCategory::ThoughtLevel => &[
            "thought_level",
            "reasoning_effort",
            "effort",
            "thinking_budget",
            "thinking",
        ],
        _ => &[],
    }
}

fn select_option_contains_value(kind: &SessionConfigKind, value: &str) -> bool {
    match kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => {
                options.iter().any(|option| option.value.to_string() == value)
            }
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .any(|option| option.value.to_string() == value),
            _ => false,
        },
        _ => false,
    }
}

fn select_option_values(kind: &SessionConfigKind) -> Vec<&str> {
    match kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => {
                options.iter().map(|option| option.value.0.as_ref()).collect()
            }
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|option| option.value.0.as_ref())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

// Tests live in sibling files linked via `#[path]` so this file stays under
// the 1000-line per-file budget. Inside those files `super::*` resolves to
// this module's private items.
#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_config_snapshot_tests.rs"]
mod config_snapshot_tests;
