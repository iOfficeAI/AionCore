//! `ConversationService::create_for_conversation_helper` — the inherit branch
//! and the validation order. The assistant-override branch lives in the same
//! file.

use super::*;
use crate::ConversationCreateError;
use aionui_api_types::{ConversationCreateRequest, ConversationToolErrorCode};
use aionui_db::UpsertConversationAssistantSnapshotParams;
use aionui_db::models::ConversationAssistantSnapshotRow;

const USER: &str = "user_1";

fn create_req(name: &str, workspace: Option<&str>, assistant_id: Option<&str>) -> ConversationCreateRequest {
    ConversationCreateRequest {
        name: name.to_owned(),
        workspace: workspace.map(str::to_owned),
        assistant_id: assistant_id.map(str::to_owned),
    }
}

/// A caller row in the shape the frontend leaves behind: `type` + legacy
/// `extra.{backend, agent_id, agent_source}` and NO assistant snapshot.
async fn insert_caller(
    repo: &Arc<MockRepo>,
    id: &str,
    agent_type: &str,
    extra: serde_json::Value,
    model: Option<&str>,
) {
    repo.create(&ConversationRow {
        id: id.to_owned(),
        user_id: USER.to_owned(),
        name: "caller".to_owned(),
        r#type: agent_type.to_owned(),
        extra: extra.to_string(),
        model: model.map(str::to_owned),
        status: Some("finished".to_owned()),
        source: Some("aionui".to_owned()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
    })
    .await
    .unwrap();
}

async fn snapshot_of(repo: &Arc<MockRepo>, conversation_id: &str) -> Option<ConversationAssistantSnapshotRow> {
    repo.get_assistant_snapshot(USER, conversation_id).await.unwrap()
}

// ── Inherit branch ──────────────────────────────────────────────────

#[tokio::test]
async fn inherits_workspace_type_and_legacy_triple_when_the_caller_has_no_snapshot() {
    let (svc, broadcaster, repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    let workspace = ensure_test_workspace_path();
    insert_caller(
        &repo,
        "caller-legacy",
        "acp",
        json!({ "workspace": workspace, "backend": "claude", "agent_id": "2d23ff1c", "agent_source": "builtin" }),
        None,
    )
    .await;
    broadcaster.take_events();

    let created = svc
        .create_for_conversation_helper(USER, "caller-legacy", &create_req("  重构鉴权模块  ", None, None))
        .await
        .unwrap();

    assert_eq!(created.name, "重构鉴权模块", "name is trimmed");
    assert_eq!(created.workspace, workspace);
    assert!(
        created.assistant.is_none(),
        "legacy triple carries no assistant identity"
    );
    let row = repo.get(USER, &created.id).await.unwrap().unwrap();
    assert_eq!(row.r#type, "acp");
    assert_eq!(row.source.as_deref(), Some("aionui"));
    assert!(row.name_source.is_none(), "agent-given names stay overwritable");
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert_eq!(extra["workspace"], json!(workspace));
    assert_eq!(extra["backend"], json!("claude"));
    assert_eq!(extra["agent_id"], json!("2d23ff1c"));
    assert_eq!(extra["agent_source"], json!("builtin"));
    assert!(
        extra.get("custom_workspace").is_none(),
        "request-only toggle must not persist"
    );
    assert!(extra.get("teamId").is_none());

    let events = broadcaster.take_events();
    let list_changed: Vec<_> = events.iter().filter(|e| e.name == "conversation.listChanged").collect();
    assert_eq!(list_changed.len(), 1);
    assert_eq!(list_changed[0].data["action"], json!("created"));
    assert_eq!(list_changed[0].data["user_id"], json!(USER));
    assert_eq!(list_changed[0].data["conversation_id"], json!(created.id));
}

#[tokio::test]
async fn inherits_the_assistant_snapshot_and_the_aionrs_model_verbatim() {
    let (svc, _broadcaster, repo, definition_repo, _overlay_repo, _preference_repo) =
        make_service_with_mock_task_manager_and_assistant_support(Arc::new(MockTaskManager::new())).await;
    upsert_test_assistant_definition(
        &definition_repo,
        "def-aionrs",
        "asst-aionrs",
        "632f31d2",
        "auto",
        "auto",
    )
    .await;
    let workspace = ensure_test_workspace_path();
    let caller_model = json!({ "provider_id": "prov-1", "model": "model-a", "use_model": "model-a" }).to_string();
    insert_caller(
        &repo,
        "caller-snap",
        "aionrs",
        json!({ "workspace": workspace }),
        Some(&caller_model),
    )
    .await;
    repo.upsert_assistant_snapshot(
        USER,
        &UpsertConversationAssistantSnapshotParams {
            conversation_id: "caller-snap",
            assistant_definition_id: "def-aionrs",
            assistant_id: "asst-aionrs",
            assistant_source: "builtin",
            agent_id: "632f31d2",
            rules_content: "",
            default_model_mode: "auto",
            resolved_model_id: Some("model-a"),
            default_permission_mode: "auto",
            resolved_permission_value: None,
            default_thought_level_mode: "auto",
            resolved_thought_level_value: None,
            default_skills_mode: "auto",
            resolved_skill_ids: "[]",
            resolved_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            resolved_mcp_ids: "[]",
        },
    )
    .await
    .unwrap();

    let created = svc
        .create_for_conversation_helper(USER, "caller-snap", &create_req("子任务", None, None))
        .await
        .unwrap();

    let assistant = created.assistant.expect("snapshot branch reports the assistant");
    assert_eq!(assistant.id, "asst-aionrs");
    let row = repo.get(USER, &created.id).await.unwrap().unwrap();
    assert_eq!(row.r#type, "aionrs");
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, "prov-1");
    assert_eq!(model.model, "model-a");
    let snapshot = snapshot_of(&repo, &created.id)
        .await
        .expect("new conversation gets its own snapshot");
    assert_eq!(snapshot.assistant_id, "asst-aionrs");
}

#[tokio::test]
async fn an_explicit_workspace_is_used_instead_of_the_callers() {
    let (svc, _broadcaster, repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    insert_caller(
        &repo,
        "caller-ws",
        "acp",
        json!({ "workspace": ensure_test_workspace_path(), "backend": "claude" }),
        None,
    )
    .await;
    let other = unique_test_workspace_path("runtime-create-explicit");
    let other_str = other.to_string_lossy().to_string();

    let created = svc
        .create_for_conversation_helper(USER, "caller-ws", &create_req("x", Some(&other_str), None))
        .await
        .unwrap();

    assert_eq!(created.workspace, other_str);
    let row = repo.get(USER, &created.id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert_eq!(
        extra["workspace"],
        json!(other_str),
        "no temp workspace was auto-provisioned"
    );
}

// ── Validation order / bad paths ─────────────────────────────────────

fn assert_code(
    result: Result<aionui_api_types::ConversationCreateResponse, ConversationCreateError>,
    code: ConversationToolErrorCode,
    status: u16,
) {
    let error = result.expect_err("expected a rejection");
    assert_eq!(error.code(), code, "{error}");
    assert_eq!(error.http_status(), status, "{error}");
}

#[tokio::test]
async fn a_missing_caller_row_is_transport_unavailable() {
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    let result = svc
        .create_for_conversation_helper(USER, "gone", &create_req("x", None, None))
        .await;
    assert_code(result, ConversationToolErrorCode::TransportUnavailable, 503);
}

#[tokio::test]
async fn a_team_caller_is_refused_before_any_other_check() {
    let (svc, _broadcaster, repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    insert_caller(&repo, "caller-team", "acp", json!({ "teamId": "team-1" }), None).await;
    // Blank name AND relative workspace would each fail later; team wins.
    let result = svc
        .create_for_conversation_helper(USER, "caller-team", &create_req("   ", Some("relative"), None))
        .await;
    assert_code(result, ConversationToolErrorCode::CallerIsTeam, 403);
    assert_eq!(repo.rows.lock().unwrap().len(), 1, "nothing was persisted");
}

#[tokio::test]
async fn a_blank_name_is_schema_validation_failed() {
    let (svc, _broadcaster, repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    insert_caller(
        &repo,
        "caller-a",
        "acp",
        json!({ "workspace": ensure_test_workspace_path() }),
        None,
    )
    .await;
    let result = svc
        .create_for_conversation_helper(USER, "caller-a", &create_req(" \t ", None, None))
        .await;
    assert_code(result, ConversationToolErrorCode::SchemaValidationFailed, 400);
}

#[tokio::test]
async fn a_relative_workspace_is_rejected_locally_and_a_missing_absolute_one_by_create() {
    let (svc, _broadcaster, repo) = make_service_with_mock_task_manager(Arc::new(MockTaskManager::new()));
    insert_caller(
        &repo,
        "caller-b",
        "acp",
        json!({ "workspace": ensure_test_workspace_path() }),
        None,
    )
    .await;

    let relative = svc
        .create_for_conversation_helper(USER, "caller-b", &create_req("x", Some("src/lib"), None))
        .await;
    assert_code(relative, ConversationToolErrorCode::WorkspaceNotAbsolute, 422);

    let missing = std::env::temp_dir().join("aionui-runtime-create-does-not-exist-9f2c");
    let missing_str = missing.to_string_lossy().to_string();
    let unavailable = svc
        .create_for_conversation_helper(USER, "caller-b", &create_req("x", Some(&missing_str), None))
        .await;
    assert_code(unavailable, ConversationToolErrorCode::WorkspaceUnavailable, 422);
    assert_eq!(repo.rows.lock().unwrap().len(), 1, "nothing was persisted");
}
