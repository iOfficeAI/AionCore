//! Forwards ACP session events to the catalog sync channel.
//!
//! Subscribes to the manager's stream event broadcast and projects
//! capability-advertising events (modes, models, commands, capabilities,
//! auth methods) into `AgentHandshake` partials that the registry's
//! catalog consumer writes to `agent_metadata`.

use agent_client_protocol::schema::v1::{SessionConfigOption, SessionModeState};
use aionui_api_types::AgentHandshake;
use aionui_common::normalize_keys_to_snake_case;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::protocol::events::AgentStreamEvent;
use crate::registry::CatalogSender;

/// Subscriber that projects session-driven ACP events into the
/// `agent_metadata` catalog so the stored handshake blob stays in sync
/// with what the CLI is actually advertising.
///
/// One task per `AcpAgentManager`; the task exits automatically when
/// the broadcast channel closes (i.e. the manager is dropped).
pub struct CatalogForwarder;

impl CatalogForwarder {
    /// Spawn the forwarder task. The returned handle is not normally
    /// awaited — callers drop it and rely on the broadcast channel
    /// closing to terminate the task.
    pub fn spawn(
        user_id: String,
        agent_id: String,
        mut event_rx: broadcast::Receiver<AgentStreamEvent>,
        catalog_tx: CatalogSender,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        if let Some(partial) = catalog_partial_from_event(&event) {
                            catalog_tx.send_partial(user_id.clone(), agent_id.clone(), partial);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            debug!(user_id, agent_id, "CatalogForwarder exiting");
        })
    }
}

/// Project an `AgentStreamEvent` onto the subset of `AgentHandshake`
/// fields the catalog cares about. Returns `None` for unrelated
/// events — the forwarder filters on that.
///
/// Event payloads may arrive here either already snake_case (from
/// `emit_snapshot_events`) or camelCase (from `SessionUpdate::*`
/// translation in `stream_event.rs`). We re-normalise unconditionally
/// so the persisted handshake blob is uniform; `camel_to_snake` is
/// idempotent on snake_case input.
fn catalog_partial_from_event(event: &AgentStreamEvent) -> Option<AgentHandshake> {
    fn snake(mut v: Value) -> Value {
        normalize_keys_to_snake_case(&mut v);
        v
    }
    match event {
        AgentStreamEvent::AcpModeInfo(v) => Some(AgentHandshake {
            available_modes: Some(snake(v.clone())),
            ..Default::default()
        }),
        AgentStreamEvent::AcpModelInfo(v) => Some(AgentHandshake {
            available_models: Some(snake(v.clone())),
            ..Default::default()
        }),
        AgentStreamEvent::AcpConfigOption(v) => Some(AgentHandshake {
            config_options: Some(snake(v.clone())),
            ..Default::default()
        }),
        AgentStreamEvent::AvailableCommands(data) => {
            // `AvailableCommand` is an ACP SDK struct — normalise on
            // the way into the catalog so the stored blob is snake_case.
            let mut cmds = serde_json::to_value(&data.commands).ok()?;
            normalize_keys_to_snake_case(&mut cmds);
            Some(AgentHandshake {
                available_commands: Some(cmds),
                ..Default::default()
            })
        }
        _ => None,
    }
}

/// Project a `session/new` response's catalog straight into an `AgentHandshake`,
/// bypassing the event stream.
///
/// The availability probe opens a real session (that is how it tells "reachable
/// but unauthorized" apart from other failures) and therefore already holds the
/// modes/models/config the agent advertises — but it has no `AcpAgentManager`, so
/// nothing emits the events `catalog_partial_from_event` feeds on. Without this
/// the catalog is discarded and the picker stays empty until the user opens a
/// conversation with the agent.
///
/// Shapes MUST stay identical to the event path (`catalog_partial_covers_session_fields`
/// pins mode/model/config; `probe_projection_matches_event_projection` pins that
/// both routes agree), because both write the same `agent_metadata` columns.
/// `available_commands` has no counterpart here: it arrives as a session/update
/// notification, never in the `session/new` response.
pub fn catalog_partial_from_session_new(
    modes: Option<&SessionModeState>,
    models: Option<&super::legacy_session_model::LegacySessionModelState>,
    config_options: Option<&[SessionConfigOption]>,
) -> Option<AgentHandshake> {
    use aionui_api_types::{ModelInfoEntry, ModelInfoPayload};

    let available_modes = modes.and_then(super::agent::sdk_to_snake_value).map(snake_value);
    let available_models = models
        .and_then(|models| {
            let current_id = models.current_model_id.clone();
            let available: Vec<ModelInfoEntry> = models
                .available_models
                .iter()
                .map(|entry| ModelInfoEntry {
                    id: entry.model_id.to_string(),
                    label: entry.name.clone(),
                })
                .collect();
            let current_label = available
                .iter()
                .find(|entry| entry.id == current_id)
                .map(|entry| entry.label.clone())
                .unwrap_or_else(|| current_id.clone());
            super::agent::sdk_to_snake_value(&ModelInfoPayload {
                current_model_id: Some(current_id),
                current_model_label: Some(current_label),
                available_models: available,
            })
        })
        .map(snake_value);
    let config_options = config_options
        .filter(|options| !options.is_empty())
        .and_then(|options| super::agent::sdk_to_snake_value(&serde_json::json!({ "config_options": options })))
        .map(snake_value);

    if available_modes.is_none() && available_models.is_none() && config_options.is_none() {
        return None;
    }
    Some(AgentHandshake {
        available_modes,
        available_models,
        config_options,
        ..Default::default()
    })
}

fn snake_value(mut v: Value) -> Value {
    normalize_keys_to_snake_case(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::events::StartEventData;
    use serde_json::json;

    /// Each session-driven event projects onto exactly one handshake
    /// field. Unrelated events produce `None` so the forwarder sends
    /// nothing for them.
    #[test]
    fn catalog_partial_covers_session_fields() {
        let modes = catalog_partial_from_event(&AgentStreamEvent::AcpModeInfo(json!({"x": 1})))
            .expect("mode event must project");
        assert_eq!(modes.available_modes, Some(json!({"x": 1})));
        assert!(modes.available_models.is_none());

        let models =
            catalog_partial_from_event(&AgentStreamEvent::AcpModelInfo(json!([1]))).expect("model event must project");
        assert_eq!(models.available_models, Some(json!([1])));

        let cfg = catalog_partial_from_event(&AgentStreamEvent::AcpConfigOption(json!([
            {"id":"mode"}
        ])))
        .expect("config event must project");
        assert_eq!(cfg.config_options, Some(json!([{"id":"mode"}])));

        // An unrelated event emits no update.
        assert!(catalog_partial_from_event(&AgentStreamEvent::Start(StartEventData { session_id: None })).is_none());
    }

    /// The probe route and the event route write the SAME `agent_metadata`
    /// columns, so they must produce byte-identical blobs for the same session
    /// state — otherwise which route ran last would change what the picker reads.
    /// Feeds one state through both and compares.
    #[test]
    fn probe_projection_matches_event_projection() {
        use crate::manager::acp::legacy_session_model::{LegacyModelEntry, LegacySessionModelState};
        use agent_client_protocol::schema::v1::{SessionMode, SessionModeState};

        let modes = SessionModeState::new("code", vec![SessionMode::new("code", "Code")]);
        let models = LegacySessionModelState::new(
            "gpt-5".to_owned(),
            vec![LegacyModelEntry {
                model_id: "gpt-5".to_owned(),
                name: "GPT-5".to_owned(),
                description: None,
            }],
        );

        let probe = catalog_partial_from_session_new(Some(&modes), Some(&models), None)
            .expect("a session carrying modes and models projects");

        // Rebuild the events `emit_snapshot_events` would broadcast for the same
        // state, then run them through the forwarder's own projection.
        let mode_event = catalog_partial_from_event(&AgentStreamEvent::AcpModeInfo(
            crate::manager::acp::agent::sdk_to_snake_value(&modes).expect("modes serialize"),
        ))
        .expect("mode event projects");
        assert_eq!(probe.available_modes, mode_event.available_modes, "modes shape");

        let model_payload = aionui_api_types::ModelInfoPayload {
            current_model_id: Some("gpt-5".to_owned()),
            current_model_label: Some("GPT-5".to_owned()),
            available_models: vec![aionui_api_types::ModelInfoEntry {
                id: "gpt-5".to_owned(),
                label: "GPT-5".to_owned(),
            }],
        };
        let model_event = catalog_partial_from_event(&AgentStreamEvent::AcpModelInfo(
            crate::manager::acp::agent::sdk_to_snake_value(&model_payload).expect("models serialize"),
        ))
        .expect("model event projects");
        assert_eq!(probe.available_models, model_event.available_models, "models shape");

        // A session that advertises nothing yields no write at all, so the probe
        // never blanks a catalog a real conversation had already filled in.
        assert!(catalog_partial_from_session_new(None, None, None).is_none());
        assert!(catalog_partial_from_session_new(None, None, Some(&[])).is_none());
    }
}
