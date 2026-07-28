//! Pure catalog / thought-level helpers for the direct-CLI session path
//! (`session_agent`): the initial-effort resolution (persisted selection wins,
//! legacy alias keys, drop-invalid validation), the per-model effort catalog
//! resolution, the persisted-handshake catalog preload, and the capabilities →
//! `agent_metadata` partial projection. Split out of `session_agent.rs` purely
//! for file-size hygiene — every item is a pure function / plain data holder
//! with no runtime or backend dependency.

use crate::shared_kernel::PersistedSessionState;
use aionui_api_types::AcpBuildExtra;

/// The `config_selections` key under which a session's chosen reasoning-effort
/// level is persisted. Neither backend emits a `ConfigChanged` for effort (only
/// mode/model), so `set_config_option` persists it here directly and
/// `build_session_instance` re-applies it after open — there is no spawn-time
/// effort flag; it rides a post-open control_request (claude) /
/// `thread/settings/update{effort}` (codex). The three accepted incoming option
/// ids (`effort`/`reasoning_effort`/`thought_level`) all normalize to this one
/// storage key.
pub(crate) const EFFORT_CONFIG_KEY: &str = "effort";

/// Resolve the reasoning-effort catalog to surface for the effort picker, mirroring the
/// backend's `effort_is_supported` current-model precedence: the efforts of the resolved
/// current model if it can be pinned, else the union across all advertised models (so we
/// don't hide a level some selectable model supports when the current model is ambiguous /
/// not-yet-known). Empty result = no effort axis → the caller omits the option entirely.
pub(crate) fn resolve_current_model_efforts(
    models: &[aionui_session::ModelInfo],
    current_model: Option<&str>,
) -> Vec<String> {
    if let Some(model) = current_model.and_then(|id| models.iter().find(|m| m.id == id)) {
        return model.reasoning_efforts.clone();
    }
    let mut union: Vec<String> = Vec::new();
    for m in models {
        for e in &m.reasoning_efforts {
            if !union.contains(e) {
                union.push(e.clone());
            }
        }
    }
    union
}

/// Every `config_selections` key an effort selection may be stored under, in
/// deterministic lookup precedence: the direct path's canonical
/// [`EFFORT_CONFIG_KEY`] first, then the raw option ids the LEGACY ACP path
/// persisted verbatim (`AcpSessionSyncService` writes the wire option id with no
/// canonicalization, and legacy agents advertised any of these — the same alias
/// set `config_option_aliases_for_category(ThoughtLevel)` matches on the legacy
/// read side). A conversation created before the session-model port can carry any
/// of them; reading only the canonical key would silently drop the user's choice
/// on upgrade.
pub(crate) const EFFORT_ALIAS_KEYS: [&str; 5] = [
    EFFORT_CONFIG_KEY, // "effort"
    "reasoning_effort",
    "thought_level",
    "thinking_budget",
    "thinking",
];

/// The persisted effort selection, if ANY alias key is present (first alias in
/// [`EFFORT_ALIAS_KEYS`] precedence wins when several coexist — e.g. a legacy
/// `thought_level` row later joined by a canonical `effort` write).
pub(crate) fn persisted_effort_selection(snapshot: &PersistedSessionState) -> Option<String> {
    EFFORT_ALIAS_KEYS.iter().find_map(|key| {
        snapshot
            .config_selections
            .iter()
            .find(|(k, _)| k.as_str() == *key)
            .map(|(_, v)| v.as_str().to_owned())
    })
}

/// Resolve the initial reasoning-effort level for a session about to open:
/// the interactive-switch-persisted selection (any [`EFFORT_ALIAS_KEYS`] key)
/// wins over the create-time resolved default `config.thought_level` (assistant
/// fixed default / auto preference, written into the conversation's build extra
/// by the conversation service) — the same snapshot-wins precedence
/// `spec_mode_model` applies to mode/model.
///
/// An EMPTY persisted value blocks the default entirely (returns `None`): the
/// legacy path's `has_persisted_config_for_category` keys on the PRESENCE of the
/// selection, not its content, so an explicitly-cleared level must not resurrect
/// the assistant default on the next open — presence parity with legacy.
///
/// A resolved value is then validated against `known_efforts` (the best catalog
/// knowledge at open: live capabilities, else the persisted-handshake preload):
/// a NON-empty catalog that omits the value drops the seed (the legacy path's
/// `pending_startup_config` ValueNotSelectable semantics — never highlight a
/// level the model can't run); an EMPTY/unknown catalog is permissive (matches
/// ACP `is_*_valid`: an absent catalog cannot invalidate; the backend still
/// validates on dispatch and the pump reconciles on catalog arrival/reject).
pub(crate) fn resolve_initial_effort(
    session_snapshot: Option<&PersistedSessionState>,
    config: &AcpBuildExtra,
    known_efforts: &[String],
) -> Option<String> {
    let effort = match session_snapshot.and_then(persisted_effort_selection) {
        // Presence blocks the default (legacy parity): an empty OR whitespace-only
        // persisted value means "cleared", not "fall back to the assistant
        // default" (legacy trim — a blank string is effectively absent).
        Some(persisted) if persisted.trim().is_empty() => return None,
        Some(persisted) => persisted.trim().to_string(),
        None => {
            let level = config.thought_level.clone()?;
            let trimmed = level.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.to_string()
        }
    };
    if !known_efforts.is_empty() && !known_efforts.iter().any(|e| e == &effort) {
        tracing::warn!(
            effort = %effort,
            ?known_efforts,
            "session-port: initial thought level is not in the advertised effort catalog; dropping the seed"
        );
        return None;
    }
    Some(effort)
}

/// Cold-start catalog snapshot extracted from a persisted `agent_metadata`
/// handshake, in the SAME `aionui_session` shape the getters read off live
/// `capabilities()` — so serving the preload is a drop-in fallback with no shape
/// translation at read time. Empty vectors + `None` currents = nothing persisted.
#[derive(Default, Clone)]
pub(crate) struct CatalogPreload {
    pub(crate) available_models: Vec<aionui_session::ModelInfo>,
    pub(crate) current_model: Option<String>,
    pub(crate) available_modes: Vec<aionui_session::ModeInfo>,
    pub(crate) current_mode: Option<String>,
}

impl CatalogPreload {
    /// Parse the persisted handshake's `available_models` / `available_modes`
    /// columns into the live-capabilities shape. Reuses the ACP path's
    /// `extract_models_from_value` / `extract_modes_from_value` (the same
    /// multi-shape parser that accepts both the `{available_models:[{id,label}]}`
    /// column shape `spawn_catalog_writeback` persists AND a live-claude handshake),
    /// so the two paths stay byte-compatible. Per-model `reasoning_efforts` are read
    /// from the raw column JSON directly (`spawn_catalog_writeback` persists them
    /// alongside id/label; the shared parser's state does not model efforts): without
    /// them a cold-start `get_config_options` served zero efforts → the thinking
    /// picker vanished (and the initial-effort seed could not be validated) until
    /// the live catalog landed seconds later.
    pub(crate) fn from_handshake(handshake: &aionui_api_types::AgentHandshake) -> Self {
        use crate::manager::acp::config_option_catalog::{extract_models_from_value, extract_modes_from_value};
        // id → reasoning_efforts from the raw persisted entries (empty when the
        // column predates efforts persistence or came from a live-claude handshake).
        let efforts_by_id: std::collections::HashMap<String, Vec<String>> = handshake
            .available_models
            .as_ref()
            .and_then(|v| v.get("available_models"))
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(serde_json::Value::as_str)?.to_string();
                        let efforts = m
                            .get("reasoning_efforts")
                            .and_then(serde_json::Value::as_array)?
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        Some((id, efforts))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (available_models, current_model) = handshake
            .available_models
            .as_ref()
            .and_then(extract_models_from_value)
            .map(|state| {
                let models = state
                    .available_models
                    .iter()
                    .map(|m| aionui_session::ModelInfo {
                        id: m.model_id.to_string(),
                        name: m.name.clone(),
                        description: m.description.clone(),
                        reasoning_efforts: efforts_by_id.get(&m.model_id.to_string()).cloned().unwrap_or_default(),
                    })
                    .collect::<Vec<_>>();
                let current = state.current_model_id.to_string();
                (models, (!current.is_empty()).then_some(current))
            })
            .unwrap_or_default();
        let (available_modes, current_mode) = handshake
            .available_modes
            .as_ref()
            .and_then(extract_modes_from_value)
            .map(|state| {
                let modes = state
                    .available_modes
                    .iter()
                    .map(|m| aionui_session::ModeInfo {
                        id: m.id.to_string(),
                        name: m.name.clone(),
                        description: m.description.clone(),
                    })
                    .collect::<Vec<_>>();
                let current = state.current_mode_id.to_string();
                (modes, (!current.is_empty()).then_some(current))
            })
            .unwrap_or_default();
        Self {
            available_models,
            current_model,
            available_modes,
            current_mode,
        }
    }
}

/// Project a backend's discovered `Capabilities` (modes / models / slash commands)
/// into an `AgentHandshake` partial for the `agent_metadata` catalog. Verbatim port
/// of clean-slate `session_runtime::catalog_partial_from_caps`: emits both the ACP
/// `config_options[]` wire shape AND the top-level `available_modes`/`available_models`
/// columns directly (the shape-stable path that keeps the codex model picker from
/// going empty).
pub(crate) fn catalog_partial_from_caps(
    caps: &aionui_session::Capabilities,
) -> Option<aionui_api_types::AgentHandshake> {
    let mut config_options = Vec::new();
    if !caps.available_modes.is_empty() {
        config_options.push(serde_json::json!({
            "id": "mode",
            "category": "mode",
            "type": "select",
            "currentValue": caps.current_mode,
            "options": caps.available_modes.iter().map(|m| serde_json::json!({
                "value": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
        }));
    }
    if !caps.available_models.is_empty() {
        config_options.push(serde_json::json!({
            "id": "model",
            "category": "model",
            "type": "select",
            "currentValue": caps.current_model,
            "options": caps.available_models.iter().map(|m| serde_json::json!({
                "value": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
        }));
    }
    // Thought axis: project the discovered per-model efforts as the same
    // `thought_level` option `get_config_options` serves live, so the persisted
    // catalog keeps the thinking picker (and its current) across a cold start
    // instead of silently dropping the axis (the pre-fix behavior).
    let efforts = resolve_current_model_efforts(&caps.available_models, caps.current_model.as_deref());
    if !efforts.is_empty() {
        config_options.push(serde_json::json!({
            "id": "reasoning_effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": caps.current_effort,
            "options": efforts.iter().map(|e| serde_json::json!({
                "value": e, "name": e,
            })).collect::<Vec<_>>(),
        }));
    }
    let available_commands = if caps.slash_commands.is_empty() {
        None
    } else {
        Some(serde_json::json!(
            caps.slash_commands
                .iter()
                .map(|c| serde_json::json!({
                    "name": c.name, "description": c.description,
                }))
                .collect::<Vec<_>>()
        ))
    };
    if config_options.is_empty() && available_commands.is_none() {
        return None;
    }
    let config_options = if config_options.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(config_options))
    };
    // Also project the top-level `available_modes`/`available_models` fields directly
    // (shape: `{available_models:[{id,label}]}`), which `apply_handshake` persists to
    // the catalog columns VERBATIM — the authoritative, shape-stable path (matches what
    // a live claude handshake stores), so the codex model picker never goes empty.
    let available_modes = (!caps.available_modes.is_empty()).then(|| {
        serde_json::json!({
            "available_modes": caps.available_modes.iter().map(|m| serde_json::json!({
                "id": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
            "current_mode_id": caps.current_mode,
        })
    });
    let available_models = (!caps.available_models.is_empty()).then(|| {
        serde_json::json!({
            // `reasoning_efforts` rides each entry so `CatalogPreload::from_handshake`
            // can restore the effort axis on a cold start (additive to the
            // `{id,label}` shape `extract_models_from_value` parses).
            "available_models": caps.available_models.iter().map(|m| serde_json::json!({
                "id": m.id, "label": m.name, "reasoning_efforts": m.reasoning_efforts,
            })).collect::<Vec<_>>(),
            "current_model_id": caps.current_model,
        })
    });
    Some(aionui_api_types::AgentHandshake {
        config_options,
        available_modes,
        available_models,
        available_commands,
        ..Default::default()
    })
}
