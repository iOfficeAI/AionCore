//! Domain service tests for `TeamPresetService`.

use std::sync::Arc;

use aionui_api_types::{CreateTeamPresetRequest, TeamPresetMember, UpdateTeamPresetRequest};
use aionui_db::{SqliteTeamRepository, init_database_memory};
use aionui_team::TeamPresetService;

fn leader() -> TeamPresetMember {
    TeamPresetMember {
        assistant_backend: "acp".into(),
        assistant_id: Some("lead-1".into()),
        model: Some("claude".into()),
        assistant_name: "Lead".into(),
        role: "lead".into(),
        order: 0,
    }
}

fn worker(order: i64) -> TeamPresetMember {
    TeamPresetMember {
        assistant_backend: "acp".into(),
        assistant_id: Some(format!("worker-{order}")),
        model: Some("claude".into()),
        assistant_name: format!("Worker {order}"),
        role: "teammate".into(),
        order,
    }
}

fn create_request(name: &str) -> CreateTeamPresetRequest {
    CreateTeamPresetRequest {
        name: name.into(),
        icon: Some("🤖".into()),
        category: Some("dev".into()),
        description: "preset description".into(),
        expertise_tags: vec!["rust".into(), "testing".into()],
        example_prompts: vec!["prompt-a".into()],
        leader: leader(),
        members: vec![leader(), worker(1)],
    }
}

async fn service() -> TeamPresetService {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn aionui_db::ITeamRepository> = Arc::new(SqliteTeamRepository::new(db.pool().clone()));
    TeamPresetService::new(repo)
}

#[tokio::test]
async fn create_and_list_presets() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    assert_eq!(created.name, "Alpha");
    assert_eq!(created.user_id, "user-a");
    assert_eq!(created.version, 1);
    assert_eq!(created.members.len(), 2);

    let presets = svc.list_presets("user-a").await.unwrap();
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0].id, created.id);
}

#[tokio::test]
async fn get_preset_owned_by_user() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let fetched = svc.get_preset("user-a", &created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
}

#[tokio::test]
async fn get_preset_forbidden_for_other_user() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let result = svc.get_preset("user-b", &created.id).await;
    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
}

#[tokio::test]
async fn update_preset_patches_fields_and_bumps_version() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let updated = svc
        .update_preset(
            "user-a",
            &created.id,
            UpdateTeamPresetRequest {
                name: Some("Beta".into()),
                description: Some("updated".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "Beta");
    assert_eq!(updated.description, "updated");
    assert_eq!(updated.version, 2);
    assert_eq!(updated.icon, created.icon);
}

#[tokio::test]
async fn update_preset_forbidden_for_other_user() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let result = svc
        .update_preset(
            "user-b",
            &created.id,
            UpdateTeamPresetRequest {
                name: Some("Beta".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
}

#[tokio::test]
async fn delete_preset_owned_by_user() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    svc.delete_preset("user-a", &created.id).await.unwrap();

    let result = svc.get_preset("user-a", &created.id).await;
    assert!(matches!(result, Err(aionui_team::TeamError::PresetNotFound(_))));
}

#[tokio::test]
async fn delete_preset_forbidden_for_other_user() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let result = svc.delete_preset("user-b", &created.id).await;
    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
}

#[tokio::test]
async fn create_preset_rejects_empty_name() {
    let svc = service().await;
    let mut req = create_request("Alpha");
    req.name = "   ".into();
    let result = svc.create_preset("user-a", req).await;
    assert!(matches!(result, Err(aionui_team::TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn create_preset_rejects_leader_missing_from_members() {
    let svc = service().await;
    let mut req = create_request("Alpha");
    req.members = vec![worker(1)];
    let result = svc.create_preset("user-a", req).await;
    assert!(matches!(result, Err(aionui_team::TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn create_preset_rejects_non_contiguous_orders() {
    let svc = service().await;
    let mut req = create_request("Alpha");
    req.members = vec![leader(), worker(2)];
    let result = svc.create_preset("user-a", req).await;
    assert!(matches!(result, Err(aionui_team::TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn update_preset_revalidates_members_when_changing_leader() {
    let svc = service().await;

    let created = svc.create_preset("user-a", create_request("Alpha")).await.unwrap();
    let new_leader = TeamPresetMember {
        assistant_id: Some("not-in-members".into()),
        ..leader()
    };
    let result = svc
        .update_preset(
            "user-a",
            &created.id,
            UpdateTeamPresetRequest {
                leader: Some(new_leader),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(aionui_team::TeamError::InvalidRequest(_))));
}
