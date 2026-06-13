use std::sync::Arc;

use aionui_api_types::AgentHandshake;
use aionui_db::{SqliteAgentMetadataRepository, init_database_memory};

use crate::manager::acp::config_option_catalog::extract_config_options_from_value;

use super::AgentRegistry;

async fn registry() -> Arc<AgentRegistry> {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()));
    let reg = AgentRegistry::new(repo);
    reg.hydrate().await.unwrap();
    reg
}

#[tokio::test]
async fn apply_handshake_derives_catalogs_from_config_options_before_persisting() {
    let reg = registry().await;
    let opencode = reg.find_builtin_by_backend("opencode").await.unwrap();

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!({
                "config_options": [
                    {
                        "id": "modes",
                        "name": "Mode",
                        "type": "select",
                        "current_value": "build",
                        "options": [
                            {"value": "build", "name": "Build"},
                            {"value": "plan", "name": "Plan"}
                        ]
                    },
                    {
                        "id": "models",
                        "name": "Model",
                        "type": "select",
                        "current_value": "sonnet",
                        "options": [
                            {"value": "sonnet", "name": "Sonnet"},
                            {"value": "opus", "name": "Opus"}
                        ]
                    }
                ]
            })),
            available_modes: Some(serde_json::json!({"available_modes": []})),
            available_models: Some(serde_json::json!({"available_models": []})),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let refreshed = reg.get(&opencode.id).await.unwrap();
    assert_eq!(
        refreshed.handshake.available_modes,
        Some(serde_json::json!({
            "current_mode_id": "build",
            "available_modes": [
                {"id": "build", "name": "Build"},
                {"id": "plan", "name": "Plan"}
            ]
        }))
    );
    assert_eq!(
        refreshed.handshake.available_models,
        Some(serde_json::json!({
            "current_model_id": "sonnet",
            "current_model_label": "Sonnet",
            "available_models": [
                {"id": "sonnet", "label": "Sonnet"},
                {"id": "opus", "label": "Opus"}
            ]
        }))
    );
}

#[tokio::test]
async fn apply_handshake_falls_back_to_available_catalogs_when_config_options_have_no_catalogs() {
    let reg = registry().await;
    let opencode = reg.find_builtin_by_backend("opencode").await.unwrap();
    let explicit_models = serde_json::json!({
        "current_model_id": "explicit",
        "current_model_label": "Explicit",
        "available_models": [{"id": "explicit", "label": "Explicit"}]
    });

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!([
                {
                    "id": "reasoning",
                    "name": "Reasoning",
                    "type": "select",
                    "currentValue": "high",
                    "options": [{"value": "high", "name": "High"}]
                }
            ])),
            available_models: Some(explicit_models.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let refreshed = reg.get(&opencode.id).await.unwrap();
    assert_eq!(refreshed.handshake.available_models, Some(explicit_models));
}

#[tokio::test]
async fn apply_handshake_prefers_config_options_over_available_catalogs() {
    let reg = registry().await;
    let opencode = reg.find_builtin_by_backend("opencode").await.unwrap();
    let existing_modes = serde_json::json!({
        "current_mode_id": "existing-mode",
        "available_modes": [{"id": "existing-mode", "name": "Existing Mode"}]
    });
    let existing_models = serde_json::json!({
        "current_model_id": "existing-model",
        "current_model_label": "Existing Model",
        "available_models": [{"id": "existing-model", "label": "Existing Model"}]
    });

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            available_modes: Some(existing_modes),
            available_models: Some(existing_models),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!({
                "configOptions": [
                    {
                        "id": "modes",
                        "name": "Mode",
                        "type": "select",
                        "currentValue": "config-mode",
                        "options": [{"value": "config-mode", "name": "Config Mode"}]
                    },
                    {
                        "id": "models",
                        "name": "Model",
                        "type": "select",
                        "currentValue": "config-model",
                        "options": [{"value": "config-model", "name": "Config Model"}]
                    }
                ]
            })),
            available_modes: Some(serde_json::json!({
                "current_mode_id": "incoming-mode",
                "available_modes": [{"id": "incoming-mode", "name": "Incoming Mode"}]
            })),
            available_models: Some(serde_json::json!({
                "current_model_id": "incoming-model",
                "current_model_label": "Incoming Model",
                "available_models": [{"id": "incoming-model", "label": "Incoming Model"}]
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let refreshed = reg.get(&opencode.id).await.unwrap();
    assert_eq!(
        refreshed.handshake.available_modes,
        Some(serde_json::json!({
            "current_mode_id": "config-mode",
            "available_modes": [{"id": "config-mode", "name": "Config Mode"}]
        }))
    );
    assert_eq!(
        refreshed.handshake.available_models,
        Some(serde_json::json!({
            "current_model_id": "config-model",
            "current_model_label": "Config Model",
            "available_models": [{"id": "config-model", "label": "Config Model"}]
        }))
    );
}

#[tokio::test]
async fn apply_handshake_config_only_partial_prefers_config_options_over_existing_catalogs() {
    let reg = registry().await;
    let opencode = reg.find_builtin_by_backend("opencode").await.unwrap();
    let explicit_modes = serde_json::json!({
        "current_mode_id": "explicit-mode",
        "available_modes": [{"id": "explicit-mode", "name": "Explicit Mode"}]
    });
    let explicit_models = serde_json::json!({
        "current_model_id": "explicit-model",
        "current_model_label": "Explicit Model",
        "available_models": [{"id": "explicit-model", "label": "Explicit Model"}]
    });

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            available_modes: Some(explicit_modes.clone()),
            available_models: Some(explicit_models.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!({
                "configOptions": [
                    {
                        "id": "modes",
                        "name": "Mode",
                        "type": "select",
                        "currentValue": "derived-mode",
                        "options": [{"value": "derived-mode", "name": "Derived Mode"}]
                    },
                    {
                        "id": "models",
                        "name": "Model",
                        "type": "select",
                        "currentValue": "derived-model",
                        "options": [{"value": "derived-model", "name": "Derived Model"}]
                    }
                ]
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let refreshed = reg.get(&opencode.id).await.unwrap();
    assert_eq!(
        refreshed.handshake.available_modes,
        Some(serde_json::json!({
            "current_mode_id": "derived-mode",
            "available_modes": [{"id": "derived-mode", "name": "Derived Mode"}]
        }))
    );
    assert_eq!(
        refreshed.handshake.available_models,
        Some(serde_json::json!({
            "current_model_id": "derived-model",
            "current_model_label": "Derived Model",
            "available_models": [{"id": "derived-model", "label": "Derived Model"}]
        }))
    );
}

#[tokio::test]
async fn apply_handshake_merges_partial_config_option_updates_before_persisting() {
    let reg = registry().await;
    let opencode = reg.find_builtin_by_backend("opencode").await.unwrap();

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!({
                "config_options": [
                    {
                        "id": "mode",
                        "name": "Mode",
                        "type": "select",
                        "category": "mode",
                        "current_value": "full-access",
                        "options": [
                            {"value": "auto", "name": "Default"},
                            {"value": "full-access", "name": "Full Access"}
                        ]
                    },
                    {
                        "id": "model",
                        "name": "Model",
                        "type": "select",
                        "category": "model",
                        "current_value": "gpt-5.4",
                        "options": [{"value": "gpt-5.4", "name": "gpt-5.4"}]
                    },
                    {
                        "id": "reasoning_effort",
                        "name": "Reasoning Effort",
                        "type": "select",
                        "category": "thought_level",
                        "current_value": "low",
                        "options": [{"value": "low", "name": "Low"}]
                    }
                ]
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    reg.apply_handshake_inner(
        &opencode.id,
        &AgentHandshake {
            config_options: Some(serde_json::json!({
                "config_options": [
                    {
                        "id": "model",
                        "name": "Model",
                        "type": "select",
                        "category": "model",
                        "current_value": "gpt-5.5",
                        "options": [
                            {"value": "gpt-5.5", "name": "GPT-5.5"},
                            {"value": "gpt-5.4", "name": "gpt-5.4"}
                        ]
                    },
                    {
                        "id": "reasoning_effort",
                        "name": "Reasoning Effort",
                        "type": "select",
                        "category": "thought_level",
                        "current_value": "medium",
                        "options": [
                            {"value": "low", "name": "Low"},
                            {"value": "medium", "name": "Medium"}
                        ]
                    }
                ]
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let refreshed = reg.get(&opencode.id).await.unwrap();
    let config_options =
        extract_config_options_from_value(refreshed.handshake.config_options.as_ref().expect("config options"))
            .expect("decoded config options");

    assert_eq!(config_options.len(), 3);
    assert!(config_options.iter().any(|option| option.id.to_string() == "mode"));
    assert_eq!(
        refreshed.handshake.available_modes,
        Some(serde_json::json!({
            "current_mode_id": "full-access",
            "available_modes": [
                {"id": "auto", "name": "Default"},
                {"id": "full-access", "name": "Full Access"}
            ]
        }))
    );
    assert_eq!(
        refreshed.handshake.available_models,
        Some(serde_json::json!({
            "current_model_id": "gpt-5.5",
            "current_model_label": "GPT-5.5",
            "available_models": [
                {"id": "gpt-5.5", "label": "GPT-5.5"},
                {"id": "gpt-5.4", "label": "gpt-5.4"}
            ]
        }))
    );
}
