//! HTTP integration tests for `/api/assistants/*` plus the source-dispatched
//! `/api/skills/assistant-rule/*` and user assistant-skill helpers.
//!
//! Each test exercises the router end-to-end via `tower::ServiceExt::oneshot`
//! against a real `aionui_app::create_router_with_states` instance backed by
//! an in-memory SQLite database. The assistant module state is re-built with
//! a temp-dir built-in manifest, a temp user-data dir, and a temp extension
//! registry for the unrelated extension router state without touching
//! `~/.aionui/`.

mod common;

use std::sync::Arc;

use aionui_api_types::{
    AgentManagementRow, AgentManagementStatus, AgentSnapshotCheckKind, AgentSnapshotCheckStatus, AgentSource,
    AgentSourceInfo, BehaviorPolicy,
};
use aionui_app::{AppConfig, AppServices, ModuleStates, build_module_states, create_router_with_states};
use aionui_assistant::{AssistantAgentCatalogPort, AssistantRouterState, AssistantService, BuiltinAssistantRegistry};
use aionui_common::AgentType;
use aionui_db::{
    IAssistantDefinitionRepository, IAssistantOverlayRepository, IAssistantOverrideRepository,
    IAssistantPreferenceRepository, IAssistantRepository, IProviderRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantOverlayRepository, SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository,
    SqliteAssistantRepository, SqliteProviderRepository, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, init_database_memory,
};
use aionui_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionSource, ExtensionStateStore,
    ExternalPathsManager, HubIndexManager, HubInstaller, HubRouterState, ScanPath, SkillPaths, SkillRouterState,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use common::{body_json, delete_with_token, get_with_token, json_with_token, setup_and_login};

// ---------------------------------------------------------------------------
// Fixture — router + temp dirs + services
// ---------------------------------------------------------------------------

/// Hold onto the temp dirs for the lifetime of the fixture so on-disk
/// fixtures survive until the test returns.
#[allow(dead_code)]
struct Fixture {
    app: axum::Router,
    services: AppServices,
    token: String,
    csrf: String,
    // user-data root containing assistant-rules / assistant-skills / assistant-avatars
    user_data_dir: std::path::PathBuf,
    // dir holding assistants.json manifest + per-file rule/skill/avatar assets
    builtin_assets_dir: std::path::PathBuf,
    _user_tmp: TempDir,
    _builtin_tmp: TempDir,
    _ext_tmp: TempDir,
}

#[derive(Clone)]
struct TestAgentCatalog {
    rows: Vec<AgentManagementRow>,
}

#[async_trait::async_trait]
impl AssistantAgentCatalogPort for TestAgentCatalog {
    async fn list_management_agents(&self) -> Result<Vec<AgentManagementRow>, aionui_assistant::AssistantError> {
        Ok(self.rows.clone())
    }
}

fn test_agent_row(id: &str, backend: Option<&str>, agent_type: AgentType, name: &str) -> AgentManagementRow {
    AgentManagementRow {
        id: id.to_owned(),
        icon: None,
        name: name.to_owned(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: backend.map(str::to_owned),
        agent_type,
        agent_source: match agent_type {
            AgentType::Aionrs => AgentSource::Internal,
            _ => AgentSource::Builtin,
        },
        agent_source_info: AgentSourceInfo::default(),
        enabled: true,
        installed: true,
        command: backend.map(str::to_owned),
        args: Vec::new(),
        env: Vec::new(),
        native_skills_dirs: None,
        behavior_policy: BehaviorPolicy {
            supports_team: true,
            ..Default::default()
        },
        yolo_id: None,
        sort_order: 0,
        team_capable: true,
        status: AgentManagementStatus::Online,
        last_check_status: Some(AgentSnapshotCheckStatus::Online),
        last_check_kind: Some(AgentSnapshotCheckKind::Manual),
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_error_details: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        has_command_override: false,
        env_override_key_count: 0,
    }
}

async fn insert_generated_bare_assistant(
    fx: &Fixture,
    assistant_id: &str,
    source_ref: &str,
    _backend: &str,
    name: &str,
) {
    let pool = fx.services.database.pool().clone();
    let definition_repo = SqliteAssistantDefinitionRepository::new(pool.clone());
    let overlay_repo = SqliteAssistantOverlayRepository::new(pool);

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: &format!("asstdef-{assistant_id}"),
            assistant_id,
            source: "generated",
            owner_type: "system",
            source_ref: Some(source_ref),
            source_version: None,
            source_hash: None,
            name,
            name_i18n: "{}",
            description: None,
            description_i18n: "{}",
            avatar_type: "none",
            avatar_value: None,
            agent_id: source_ref,
            rule_resource_type: "none",
            rule_resource_ref: None,
            rule_inline_content: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "auto",
            default_skill_ids: "[]",
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        })
        .await
        .unwrap();
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: &format!("asstdef-{assistant_id}"),
            enabled: true,
            sort_order: 5,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
}

/// Build the whole app with:
/// - a manifest at `{builtin_tmp}/assets/assistants.json` registering two
///   built-ins (`builtin-office` with rule/skill/avatar files on disk, and
///   `builtin-bare` with nothing referenced)
/// - a temp user-data dir that `AssistantService` uses for user rule/skill/
///   avatar storage
///
/// Also logs in `admin` and hands back the session + CSRF tokens so tests
/// can issue authenticated mutating requests.
async fn fixture() -> Fixture {
    let user_tmp = TempDir::new().unwrap();
    let builtin_tmp = TempDir::new().unwrap();
    let ext_tmp = TempDir::new().unwrap();

    let user_data_dir = user_tmp.path().to_path_buf();
    let builtin_assets_dir = builtin_tmp.path().join("assets");
    std::fs::create_dir_all(&builtin_assets_dir).unwrap();

    // Builtin manifest: office has rule/avatar on disk, bare has nothing.
    std::fs::create_dir_all(builtin_assets_dir.join("rules")).unwrap();
    std::fs::write(builtin_assets_dir.join("rules/office.en-US.md"), "office rule body").unwrap();
    // Tiny PNG-ish placeholder — content_type logic only inspects extension.
    std::fs::write(builtin_assets_dir.join("office.png"), b"not-a-real-png").unwrap();

    let manifest = json!({
        "version": "1.0.0",
        "assistants": [
            {
                "id": "builtin-office",
                "name": "Office",
                "agent_ref": "codex",
                "rule_file": "rules/office.{locale}.md",
                "avatar": "office.png",
            },
            {
                "id": "builtin-bare",
                "name": "Bare",
                "agent_ref": "codex",
            }
        ]
    });
    std::fs::write(
        builtin_assets_dir.join("assistants.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Extension fixture: a single extension directory containing a manifest
    // with `contributes.assistants = [{ id: "ext-helper", ... }]`.
    let ext_root = ext_tmp.path().join("extensions");
    let ext_dir = ext_root.join("fixture-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    let ext_manifest = json!({
        "name": "fixture-ext",
        "version": "1.0.0",
        "display_name": "Fixture Extension",
        "contributes": {
            "assistants": [{
                "id": "ext-helper",
                "name": "Helper",
                "description": "Contributed by fixture-ext",
                "system_prompt": "You are helpful.",
                "context": "Extension context body",
            }]
        }
    });
    std::fs::write(
        ext_dir.join("aion-extension.json"),
        serde_json::to_vec_pretty(&ext_manifest).unwrap(),
    )
    .unwrap();

    // Bring up in-memory DB + services + default module states.
    let db = init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _): (ModuleStates, _) = build_module_states(&services).await.expect("build module states");
    for table in [
        "assistant_preferences",
        "assistant_overlays",
        "assistant_definitions",
        "assistant_overrides",
        "assistants",
    ] {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(services.database.pool())
            .await
            .unwrap();
    }

    // Replace the extension + hub + skill states with freshly-constructed
    // ones rooted at our temp dirs. The defaults built by
    // `build_module_states` point at `~/.aionui/` for the state store and
    // external-paths file, which can hold arbitrary contents on a dev box
    // and poison the test. Building from scratch gives us a pristine
    // registry we can initialize with our fixture extension.
    let ext_data_dir = ext_tmp.path().join("ext-data");
    std::fs::create_dir_all(&ext_data_dir).unwrap();
    let state_store = ExtensionStateStore::new(ext_data_dir.join("extension-states.json"));
    let registry = ExtensionRegistry::new(state_store, services.event_bus.clone(), "1.0.0".to_string());
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: ext_root.clone(),
            source: ExtensionSource::Env,
        }])
        .await
        .unwrap();
    states.extension = ExtensionRouterState {
        registry: registry.clone(),
    };
    let hub_dir = ext_data_dir.join("extensions");
    let index_manager = HubIndexManager::new(hub_dir, registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());
    states.hub = HubRouterState {
        index_manager,
        installer,
    };
    let ext_paths_mgr = Arc::new(ExternalPathsManager::with_file(ext_data_dir.join("paths.json")).await);
    let skill_paths = SkillPaths {
        data_dir: ext_data_dir.clone(),
        user_skills_dir: ext_data_dir.join("skills"),
        cron_skills_dir: ext_data_dir.join("cron").join("skills"),
        builtin_skills_dir: ext_data_dir.join("builtin-skills"),
        builtin_rules_dir: ext_data_dir.join("builtin-rules"),
        assistant_rules_dir: user_data_dir.join("assistant-rules"),
        assistant_skills_dir: user_data_dir.join("assistant-skills"),
    };
    states.skill = SkillRouterState {
        skill_paths,
        skill_repo: std::sync::Arc::new(aionui_db::SqliteSkillRepository::new(services.database.pool().clone())),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None, // wired below once service is constructed
    };

    // Rebuild AssistantService pointing at our temp built-in manifest + temp
    // user-data dir. `build_module_states` loads the default built-in
    // registry (pointing at $exe_dir/assets or dev fallback) and uses
    // `~/.aionui/` for user data — neither is appropriate for tests.
    let pool = services.database.pool().clone();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> =
        Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let state_repo: Arc<dyn IAssistantOverlayRepository> =
        Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()));
    let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(pool.clone()));
    let override_repo: Arc<dyn IAssistantOverrideRepository> =
        Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    // Seed an OpenAI-compatible provider so create / import calls without
    // an explicit `agent_id` resolve to `"aionrs"` instead of
    // erroring out — mirroring a configured production setup.
    provider_repo
        .create(aionui_db::CreateProviderParams {
            id: None,
            platform: "openai",
            name: "Test OpenAI",
            base_url: "https://example.invalid",
            api_key_encrypted: "stub",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("seed provider");
    let builtin = Arc::new(BuiltinAssistantRegistry::load_from_dir(builtin_assets_dir.clone()));
    let service = Arc::new(AssistantService::new(
        pool,
        aionui_assistant::service::AssistantServiceDeps {
            definition_repo,
            state_repo,
            preference_repo,
            repo,
            override_repo,
            provider_repo,
            builtin,
            agent_catalog: Some(Arc::new(TestAgentCatalog {
                rows: vec![
                    test_agent_row("8e1acf31", Some("codex"), AgentType::Acp, "Codex CLI"),
                    test_agent_row("cc126dd5", Some("gemini"), AgentType::Acp, "Gemini CLI"),
                    test_agent_row("632f31d2", None, AgentType::Aionrs, "Aion CLI"),
                ],
            })),
        },
        user_data_dir.clone(),
    ));
    service.bootstrap_assistant_storage().await.unwrap();
    states.assistant = AssistantRouterState {
        service: service.clone(),
    };
    // Rewire the skill-router dispatcher so assistant-rule / assistant-skill
    // endpoints route through the test-configured service.
    let dispatcher: Arc<dyn AssistantRuleDispatcher> = service;
    states.skill.assistant_dispatcher = Some(dispatcher);

    let mut app = create_router_with_states(&services, states);
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    Fixture {
        app,
        services,
        token,
        csrf,
        user_data_dir,
        builtin_assets_dir,
        _user_tmp: user_tmp,
        _builtin_tmp: builtin_tmp,
        _ext_tmp: ext_tmp,
    }
}

// ===========================================================================
// GET /api/assistants
// ===========================================================================

#[tokio::test]
async fn list_populated_excludes_extension_assistants() {
    let fx = fixture().await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let list = json["data"].as_array().unwrap();
    // Extension-contributed assistants are no longer part of the unified
    // assistant catalog.
    assert_eq!(list.len(), 5, "body = {json}");
    let ids: Vec<&str> = list.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bare:8e1acf31"));
    assert!(ids.contains(&"bare:cc126dd5"));
    assert!(ids.contains(&"bare:632f31d2"));
    assert!(ids.contains(&"builtin-office"));
    assert!(ids.contains(&"builtin-bare"));
    assert!(!ids.contains(&"ext-helper"));
    let sources: Vec<&str> = list.iter().map(|a| a["source"].as_str().unwrap()).collect();
    assert!(sources.contains(&"bare"));
    assert!(sources.contains(&"builtin"));
    assert!(!sources.contains(&"extension"));
    let office = find_id(&json["data"], "builtin-office").expect("builtin-office missing from assistant list");
    assert_eq!(office["agent_id"], "8e1acf31");
    assert_eq!(office["agent"]["type"], "acp");
    assert_eq!(office["agent"]["source"], "builtin");
    assert_eq!(office["agent"]["acp_backend"], "codex");
    assert!(office["agent"].get("backend").is_none());
    assert!(office["agent"].get("id").is_none());
}

#[tokio::test]
async fn list_builtin_file_avatar_is_served_via_assistant_avatar_route() {
    let fx = fixture().await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let list = json["data"].as_array().unwrap();
    let builtin_office = list
        .iter()
        .find(|assistant| assistant["id"] == "builtin-office")
        .expect("builtin-office missing from assistant list");

    assert_eq!(
        builtin_office["avatar"].as_str(),
        Some("/api/assistants/builtin-office/avatar")
    );
}

#[tokio::test]
async fn list_generated_assistant_exposes_bare_runtime_fields() {
    let fx = fixture().await;
    insert_generated_bare_assistant(&fx, "bare:agent-droid", "agent-droid", "droid", "Droid").await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let list = json["data"].as_array().unwrap();
    let bare = list
        .iter()
        .find(|assistant| assistant["id"] == "bare:agent-droid")
        .expect("generated bare assistant missing from assistant list");

    assert_eq!(bare["source"], "bare");
    assert_eq!(bare["deletable"], false);
    assert_eq!(bare["agent_status"], "missing");
    assert_eq!(bare["agent_status_message"], Value::Null);
    assert_eq!(bare["team_selectable"], false);
    assert_eq!(
        bare["team_block_reason"],
        "This assistant's agent could not be resolved."
    );
}

#[tokio::test]
async fn list_requires_auth() {
    let fx = fixture().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/assistants")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn get_detail_returns_definition_state_preferences_and_rules() {
    let fx = fixture().await;

    let create_req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u1",
            "name": "Mine",
            "description": "hello",
            "agent_id": "632f31d2",
            "enabled_skills": ["legacy-default"],
            "custom_skill_names": ["custom-note"],
            "disabled_builtin_skills": ["todo-tracker"],
            "prompts": ["draft a summary"]
        }),
        &fx.token,
        &fx.csrf,
    );
    let create_resp = fx.app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);

    let write_rule_req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "u1", "content": "user rule body", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let write_rule_resp = fx.app.clone().oneshot(write_rule_req).await.unwrap();
    assert_eq!(write_rule_resp.status(), StatusCode::OK);

    let pool = fx.services.database.pool().clone();
    let definition_repo = SqliteAssistantDefinitionRepository::new(pool.clone());
    let state_repo = SqliteAssistantOverlayRepository::new(pool.clone());
    let preference_repo = SqliteAssistantPreferenceRepository::new(pool);
    let definition = definition_repo.get_by_assistant_id("u1").await.unwrap().unwrap();

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: &definition.id,
            assistant_id: &definition.assistant_id,
            source: &definition.source,
            owner_type: &definition.owner_type,
            source_ref: definition.source_ref.as_deref(),
            source_version: definition.source_version.as_deref(),
            source_hash: definition.source_hash.as_deref(),
            name: &definition.name,
            name_i18n: &definition.name_i18n,
            description: definition.description.as_deref(),
            description_i18n: &definition.description_i18n,
            avatar_type: &definition.avatar_type,
            avatar_value: definition.avatar_value.as_deref(),
            agent_id: &definition.agent_id,
            rule_resource_type: &definition.rule_resource_type,
            rule_resource_ref: definition.rule_resource_ref.as_deref(),
            rule_inline_content: definition.rule_inline_content.as_deref(),
            recommended_prompts: r#"["draft a summary","share next steps"]"#,
            recommended_prompts_i18n: r#"{"zh-CN":["总结一下"]}"#,
            default_model_mode: "fixed",
            default_model_value: Some("gpt-4.1"),
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "fixed",
            default_skill_ids: r#"["preset-pdf"]"#,
            custom_skill_names: &definition.custom_skill_names,
            default_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
            default_mcps_mode: "auto",
            default_mcp_ids: r#"["mcp-legacy"]"#,
        })
        .await
        .unwrap();
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: &definition.id,
            enabled: false,
            sort_order: 7,
            agent_id_override: Some("8e1acf31"),
            last_used_at: Some(1_725_000_001_234),
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: &definition.id,
            last_model_id: Some("gpt-5-mini"),
            last_permission_value: Some("workspace-write"),
            last_skill_ids: r#"["pref-skill"]"#,
            last_disabled_builtin_skill_ids: r#"["planner"]"#,
            last_mcp_ids: r#"["mcp-pref"]"#,
        })
        .await
        .unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/u1?locale=en-US", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = &json["data"];
    assert_eq!(data["id"], "u1");
    assert_eq!(data["source"], "user");
    assert_eq!(data["profile"]["name"], "Mine");
    assert_eq!(data["state"]["enabled"], false);
    assert_eq!(data["state"]["sort_order"], 7);
    assert_eq!(data["engine"]["agent_id"], "8e1acf31");
    assert_eq!(data["engine"]["agent"]["acp_backend"], "codex");
    assert!(data["engine"]["agent"].get("backend").is_none());
    assert!(data["engine"]["agent"].get("id").is_none());
    assert_eq!(data["engine"]["agent"]["type"], "acp");
    assert_eq!(data["rules"]["content"], "user rule body");
    assert_eq!(data["rules"]["storage_mode"], "user_file");
    assert_eq!(data["defaults"]["model"]["mode"], "fixed");
    assert_eq!(data["defaults"]["model"]["value"], "gpt-4.1");
    assert_eq!(data["defaults"]["skills"]["value"], json!(["preset-pdf"]));
    assert_eq!(data["capabilities"]["custom_skill_names"], json!(["custom-note"]));
    assert_eq!(data["preferences"]["last_permission_value"], "workspace-write");
    assert_eq!(data["preferences"]["last_skill_ids"], json!(["pref-skill"]));
}

#[tokio::test]
async fn get_detail_generated_assistant_exposes_bare_runtime_fields() {
    let fx = fixture().await;
    insert_generated_bare_assistant(&fx, "bare:agent-droid", "agent-droid", "droid", "Droid").await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token(
            "/api/assistants/bare:agent-droid?locale=en-US",
            &fx.token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = &json["data"];
    assert_eq!(data["id"], "bare:agent-droid");
    assert_eq!(data["source"], "bare");
    assert_eq!(data["deletable"], false);
    assert_eq!(data["agent_status"], "missing");
    assert_eq!(data["agent_status_message"], Value::Null);
    assert_eq!(data["team_selectable"], false);
    assert_eq!(
        data["team_block_reason"],
        "This assistant's agent could not be resolved."
    );
    assert_eq!(data["engine"]["agent_id"], "agent-droid");
    assert_eq!(data["engine"]["agent"], Value::Null);
}

// ===========================================================================
// POST /api/assistants
// ===========================================================================

#[tokio::test]
async fn create_happy_path_returns_201() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "Mine", "description": "hello" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], "u1");
    assert_eq!(json["data"]["source"], "user");
    assert_eq!(json["data"]["name"], "Mine");
    assert_eq!(json["data"]["description"], "hello");
}

#[tokio::test]
async fn create_rejects_empty_name_with_400() {
    let fx = fixture().await;
    let req = json_with_token("POST", "/api/assistants", json!({ "name": "   " }), &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_rejects_builtin_id_collision_with_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "builtin-office", "name": "spoof" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_allows_id_that_matches_extension_registry_assistant() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "ext-helper", "name": "spoof" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_user_avatar_from_local_file_is_served_via_assistant_avatar_route() {
    let fx = fixture().await;
    let source_avatar = fx.user_data_dir.join("picked-avatar.png");
    std::fs::write(&source_avatar, b"picked-avatar-bytes").unwrap();

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u-avatar",
            "name": "Avatar User",
            "avatar": source_avatar.to_string_lossy(),
            "agent_id": "632f31d2",
        }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["avatar"], "/api/assistants/u-avatar/avatar");

    let persisted_avatar = fx.user_data_dir.join("assistant-avatars/u-avatar.png");
    assert!(
        persisted_avatar.exists(),
        "persisted avatar missing: {}",
        persisted_avatar.display()
    );
    assert_eq!(std::fs::read(&persisted_avatar).unwrap(), b"picked-avatar-bytes");

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/u-avatar/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&bytes[..], b"picked-avatar-bytes");
}

#[tokio::test]
async fn create_user_avatar_from_builtin_avatar_route_copies_builtin_asset() {
    let fx = fixture().await;

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u-avatar-from-builtin",
            "name": "Builtin Avatar Copy",
            "avatar": "/api/assistants/builtin-office/avatar",
            "agent_id": "632f31d2",
        }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["avatar"], "/api/assistants/u-avatar-from-builtin/avatar");

    let persisted_avatar = fx.user_data_dir.join("assistant-avatars/u-avatar-from-builtin.png");
    assert!(
        persisted_avatar.exists(),
        "persisted avatar missing: {}",
        persisted_avatar.display()
    );
    assert_eq!(std::fs::read(&persisted_avatar).unwrap(), b"not-a-real-png");

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token(
            "/api/assistants/u-avatar-from-builtin/avatar",
            &fx.token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&bytes[..], b"not-a-real-png");
}

#[tokio::test]
async fn create_user_avatar_from_absolute_builtin_avatar_route_copies_builtin_asset() {
    let fx = fixture().await;

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u-avatar-from-builtin-absolute",
            "name": "Builtin Avatar Absolute Copy",
            "avatar": "http://127.0.0.1:56663/api/assistants/builtin-office/avatar",
            "agent_id": "632f31d2",
        }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["avatar"],
        "/api/assistants/u-avatar-from-builtin-absolute/avatar"
    );

    let persisted_avatar = fx
        .user_data_dir
        .join("assistant-avatars/u-avatar-from-builtin-absolute.png");
    assert!(
        persisted_avatar.exists(),
        "persisted avatar missing: {}",
        persisted_avatar.display()
    );
    assert_eq!(std::fs::read(&persisted_avatar).unwrap(), b"not-a-real-png");
}

#[tokio::test]
async fn update_user_avatar_with_existing_route_preserves_served_file() {
    let fx = fixture().await;
    let source_avatar = fx.user_data_dir.join("picked-avatar.png");
    std::fs::write(&source_avatar, b"picked-avatar-bytes").unwrap();

    let create_req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": "u-avatar-stable",
            "name": "Avatar User",
            "avatar": source_avatar.to_string_lossy(),
            "agent_id": "632f31d2",
        }),
        &fx.token,
        &fx.csrf,
    );
    let create_resp = fx.app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);

    let update_req = json_with_token(
        "PUT",
        "/api/assistants/u-avatar-stable",
        json!({
            "avatar": "/api/assistants/u-avatar-stable/avatar"
        }),
        &fx.token,
        &fx.csrf,
    );
    let update_resp = fx.app.clone().oneshot(update_req).await.unwrap();
    assert_eq!(update_resp.status(), StatusCode::OK);

    let persisted_avatar = fx.user_data_dir.join("assistant-avatars/u-avatar-stable.png");
    assert!(
        persisted_avatar.exists(),
        "persisted avatar missing: {}",
        persisted_avatar.display()
    );
    assert_eq!(std::fs::read(&persisted_avatar).unwrap(), b"picked-avatar-bytes");

    let avatar_resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/u-avatar-stable/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(avatar_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn create_rejects_duplicate_user_id_with_409() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "A" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "B" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ===========================================================================
// PUT /api/assistants/{id}
// ===========================================================================

#[tokio::test]
async fn update_happy_path_returns_200() {
    let fx = fixture().await;
    create_user(&fx, "u1", "original").await;

    let req = json_with_token(
        "PUT",
        "/api/assistants/u1",
        json!({ "name": "renamed" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "renamed");
}

#[tokio::test]
async fn update_missing_user_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/ghost",
        json!({ "name": "renamed" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_builtin_is_forbidden() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/builtin-office",
        json!({ "name": "hijack" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn update_extension_registry_id_without_user_row_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/ext-helper",
        json!({ "name": "hijack" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// DELETE /api/assistants/{id}
// ===========================================================================

#[tokio::test]
async fn delete_happy_path_removes_row_and_user_assets() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    // Drop a rule, skill, and avatar on disk so the fs-cleanup branch has
    // something to remove.
    let rules_dir = fx.user_data_dir.join("assistant-rules");
    let skills_dir = fx.user_data_dir.join("assistant-skills");
    let avatars_dir = fx.user_data_dir.join("assistant-avatars");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::create_dir_all(&avatars_dir).unwrap();
    std::fs::write(rules_dir.join("u1.md"), "rule").unwrap();
    std::fs::write(skills_dir.join("u1.md"), "skill").unwrap();
    std::fs::write(avatars_dir.join("u1.png"), b"avatar").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Row is gone (list no longer contains u1).
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"u1"));

    // Fs cleanup ran.
    assert!(!rules_dir.join("u1.md").exists());
    assert!(!skills_dir.join("u1.md").exists());
    assert!(!avatars_dir.join("u1.png").exists());
}

#[tokio::test]
async fn delete_builtin_is_forbidden() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/builtin-office", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_extension_registry_id_without_user_row_returns_404() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/ext-helper", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// PATCH /api/assistants/{id}/state
// ===========================================================================

#[tokio::test]
async fn set_state_inserts_override_for_builtin() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/builtin-office/state",
        json!({ "enabled": false, "sort_order": 9 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["sort_order"], 9);
    assert_eq!(json["data"]["source"], "builtin");
}

#[tokio::test]
async fn set_state_updates_existing_override_for_user() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    // First call inserts.
    let req = json_with_token(
        "PATCH",
        "/api/assistants/u1/state",
        json!({ "enabled": false, "sort_order": 3 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Second call updates sort_order and preserves enabled when omitted.
    let req = json_with_token(
        "PATCH",
        "/api/assistants/u1/state",
        json!({ "sort_order": 7 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["sort_order"], 7);
}

#[tokio::test]
async fn set_state_extension_registry_id_without_user_row_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/ext-helper/state",
        json!({ "enabled": false }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_state_unknown_user_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/ghost/state",
        json!({ "enabled": true }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /api/assistants/import
// ===========================================================================

#[tokio::test]
async fn import_happy_path_inserts_new_rows() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A" },
            { "id": "u2", "name": "B" },
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 2);
    assert_eq!(json["data"]["skipped"], 0);
    assert_eq!(json["data"]["failed"], 0);
}

#[tokio::test]
async fn import_skips_builtin_collision() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "builtin-office", "name": "spoof" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 0);
    assert_eq!(json["data"]["skipped"], 1);
}

#[tokio::test]
async fn import_allows_id_that_matches_extension_registry_assistant() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "ext-helper", "name": "spoof" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 1);
    assert_eq!(json["data"]["skipped"], 0);
}

#[tokio::test]
async fn import_skips_already_imported_user_row() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A-updated" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 0);
    assert_eq!(json["data"]["skipped"], 1);

    // Verify we did NOT overwrite the original name.
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let entry = find_id(&json["data"], "u1").unwrap();
    assert_eq!(entry["name"], "A");
}

#[tokio::test]
async fn import_retry_is_idempotent() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A" }
        ]
    });
    // First attempt — imported.
    let req = json_with_token("POST", "/api/assistants/import", body.clone(), &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let first = body_json(resp).await;
    assert_eq!(first["data"]["imported"], 1);

    // Second attempt — same payload, now skipped.
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let second = body_json(resp).await;
    assert_eq!(second["data"]["imported"], 0);
    assert_eq!(second["data"]["skipped"], 1);
}

// ===========================================================================
// GET /api/assistants/{id}/avatar
// ===========================================================================

#[tokio::test]
async fn avatar_builtin_returns_bytes_with_content_type() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/builtin-office/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&bytes[..], b"not-a-real-png");
}

#[tokio::test]
async fn avatar_user_returns_bytes_after_file_planted() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let avatars_dir = fx.user_data_dir.join("assistant-avatars");
    std::fs::create_dir_all(&avatars_dir).unwrap();
    std::fs::write(avatars_dir.join("u1.svg"), b"<svg></svg>").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/u1/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/svg+xml")
    );
}

#[tokio::test]
async fn avatar_missing_returns_404() {
    let fx = fixture().await;
    // builtin-bare declared no avatar in the manifest; lookup should 404.
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/builtin-bare/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /api/skills/assistant-rule/read
// ===========================================================================

#[tokio::test]
async fn read_rule_builtin_returns_manifest_file_contents() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "builtin-office", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "office rule body");
}

#[tokio::test]
async fn read_rule_extension_returns_empty_string() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "ext-helper", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn read_rule_user_round_trip_through_write() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "u1", "content": "my rule", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "u1", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], "my rule");
}

// ===========================================================================
// POST /api/skills/assistant-rule/write
// ===========================================================================

#[tokio::test]
async fn write_rule_user_happy_path() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "u1", "content": "rule body" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // File was actually written.
    let file = fx.user_data_dir.join("assistant-rules/u1.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "rule body");
}

#[tokio::test]
async fn write_rule_builtin_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "builtin-office", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn write_rule_extension_registry_id_behaves_like_user_id() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "ext-helper", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// DELETE /api/skills/assistant-rule/{id}
// ===========================================================================

#[tokio::test]
async fn delete_rule_user_removes_file() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let rules_dir = fx.user_data_dir.join("assistant-rules");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("u1.md"), "body").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/skills/assistant-rule/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!rules_dir.join("u1.md").exists());
}

#[tokio::test]
async fn delete_rule_builtin_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-rule/builtin-office",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_rule_extension_registry_id_behaves_like_user_id() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-rule/ext-helper",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// POST /api/skills/assistant-skill/read
// ===========================================================================

#[tokio::test]
async fn read_skill_builtin_returns_empty_string() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "builtin-office", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn read_skill_extension_returns_empty_string() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "ext-helper", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn read_skill_user_round_trip_through_write() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "u1", "content": "my skill", "locale": "zh-CN" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "u1", "locale": "zh-CN" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], "my skill");
}

// ===========================================================================
// POST /api/skills/assistant-skill/write
// ===========================================================================

#[tokio::test]
async fn write_skill_user_happy_path() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "u1", "content": "skill body" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let file = fx.user_data_dir.join("assistant-skills/u1.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "skill body");
}

#[tokio::test]
async fn write_skill_builtin_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "builtin-office", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn write_skill_extension_registry_id_behaves_like_user_id() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "ext-helper", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// DELETE /api/skills/assistant-skill/{id}
// ===========================================================================

#[tokio::test]
async fn delete_skill_user_removes_file() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let skills_dir = fx.user_data_dir.join("assistant-skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("u1.md"), "body").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/skills/assistant-skill/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!skills_dir.join("u1.md").exists());
}

#[tokio::test]
async fn delete_skill_builtin_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-skill/builtin-office",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_skill_extension_registry_id_behaves_like_user_id() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-skill/ext-helper",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Helpers local to this test module
// ===========================================================================

async fn create_user(fx: &Fixture, id: &str, name: &str) {
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": id, "name": name }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create {id} failed");
}

fn find_id<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"].as_str() == Some(id))
}
