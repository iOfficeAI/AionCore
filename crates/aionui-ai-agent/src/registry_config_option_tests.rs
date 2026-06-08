use std::sync::Arc;

use aionui_api_types::AgentHandshake;
use aionui_db::{SqliteAgentMetadataRepository, init_database_memory};

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
async fn apply_handshake_keeps_explicit_non_empty_available_models() {
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
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "derived",
                    "options": [{"value": "derived", "name": "Derived"}]
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
async fn apply_handshake_config_only_partial_does_not_overwrite_existing_catalogs() {
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
    assert_eq!(refreshed.handshake.available_modes, Some(explicit_modes));
    assert_eq!(refreshed.handshake.available_models, Some(explicit_models));
}
