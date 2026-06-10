//! Black-box integration tests for `ITeamRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.
//!
//! Covers test-plan items from Phase 11 test-plan:
//! - Section 1 (Team CRUD): TC-1..TC-7, TL-1..TL-3, TG-1..TG-2, TD-1..TD-6, TR-1..TR-4
//! - Section 4 (Mailbox): MW-1..MW-3, MR-1..MR-4, MH-1..MH-3, MD-1..MD-2
//! - Section 5 (Task Board): TK-1..TK-3, TU-1..TU-5, CU-1..CU-4, TT-1..TT-3, TKD-1..TKD-2
//! - Section 10 (Data Consistency): DC-1, DC-4

use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::{MailboxMessageRow, TeamRow, TeamTaskRow};
use aionui_db::{
    DbError, ITeamRepository, SqliteTeamRepository, UpdateTaskParams, UpdateTeamParams, init_database_memory,
};

async fn repo() -> (Arc<dyn ITeamRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteTeamRepository::new(db.pool().clone()));
    (r as Arc<dyn ITeamRepository>, db)
}

fn make_team(id: &str, name: &str) -> TeamRow {
    make_team_for_user(id, "system_default_user", name)
}

fn make_team_for_user(id: &str, user_id: &str, name: &str) -> TeamRow {
    let now = now_ms();
    TeamRow {
        id: id.into(),
        user_id: user_id.into(),
        name: name.into(),
        workspace: String::new(),
        workspace_mode: "shared".into(),
        agents:
            r#"[{"slot_id":"a1","name":"Lead","role":"lead","conversation_id":"conv-1","backend":"claude","model":""}]"#
                .into(),
        lead_agent_id: Some("a1".into()),
        session_mode: None,
        agents_version: "1.0.1".into(),
        created_at: now,
        updated_at: now,
    }
}

fn make_mailbox_msg(id: &str, team_id: &str, to: &str, from: &str, msg_type: &str) -> MailboxMessageRow {
    MailboxMessageRow {
        id: id.into(),
        team_id: team_id.into(),
        to_agent_id: to.into(),
        from_agent_id: from.into(),
        msg_type: msg_type.into(),
        content: format!("content-{id}"),
        summary: None,
        files: None,
        read: false,
        created_at: now_ms(),
    }
}

fn make_task(id: &str, team_id: &str, subject: &str) -> TeamTaskRow {
    let now = now_ms();
    TeamTaskRow {
        id: id.into(),
        team_id: team_id.into(),
        subject: subject.into(),
        description: None,
        status: "pending".into(),
        owner: None,
        blocked_by: "[]".into(),
        blocks: "[]".into(),
        metadata: None,
        created_at: now,
        updated_at: now,
    }
}

// ── Team CRUD Tests ──────────────────────────────────────────────────

#[tokio::test]
async fn create_and_get_team() {
    let (repo, _db) = repo().await;
    let team = make_team("t1", "Team Alpha");
    repo.create_team(&team).await.unwrap();

    let fetched = repo.get_team("t1").await.unwrap().expect("team exists");
    assert_eq!(fetched.id, "t1");
    assert_eq!(fetched.name, "Team Alpha");
    assert_eq!(fetched.lead_agent_id, Some("a1".into()));
}

#[tokio::test]
async fn get_nonexistent_team_returns_none() {
    let (repo, _db) = repo().await;
    let result = repo.get_team("nonexistent").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn list_teams_empty() {
    let (repo, _db) = repo().await;
    let teams = repo.list_teams().await.unwrap();
    assert!(teams.is_empty());
}

#[tokio::test]
async fn list_teams_multiple() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Alpha")).await.unwrap();
    repo.create_team(&make_team("t2", "Beta")).await.unwrap();

    let teams = repo.list_teams().await.unwrap();
    assert_eq!(teams.len(), 2);
    assert_eq!(teams[0].id, "t1");
    assert_eq!(teams[1].id, "t2");
}

#[tokio::test]
async fn list_teams_by_user_filters_to_owner() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team_for_user("t1", "user-a", "Alpha"))
        .await
        .unwrap();
    repo.create_team(&make_team_for_user("t2", "user-b", "Beta"))
        .await
        .unwrap();
    repo.create_team(&make_team_for_user("t3", "user-a", "Gamma"))
        .await
        .unwrap();

    let teams = repo.list_teams_by_user("user-a").await.unwrap();

    assert_eq!(teams.len(), 2);
    assert_eq!(teams[0].id, "t1");
    assert_eq!(teams[1].id, "t3");
    assert!(teams.iter().all(|team| team.user_id == "user-a"));
}

#[tokio::test]
async fn update_team_name() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Old Name")).await.unwrap();

    repo.update_team(
        "t1",
        &UpdateTeamParams {
            name: Some("New Name".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let team = repo.get_team("t1").await.unwrap().unwrap();
    assert_eq!(team.name, "New Name");
}

#[tokio::test]
async fn update_team_agents_json() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let new_agents = r#"[{"slotId":"a1"},{"slotId":"a2"}]"#;
    repo.update_team(
        "t1",
        &UpdateTeamParams {
            agents: Some(new_agents.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let team = repo.get_team("t1").await.unwrap().unwrap();
    assert_eq!(team.agents, new_agents);
}

#[tokio::test]
async fn update_nonexistent_team_returns_not_found() {
    let (repo, _db) = repo().await;
    let result = repo
        .update_team(
            "nonexistent",
            &UpdateTeamParams {
                name: Some("X".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

#[tokio::test]
async fn delete_team() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();
    repo.delete_team("t1").await.unwrap();

    let result = repo.get_team("t1").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn delete_nonexistent_team_returns_not_found() {
    let (repo, _db) = repo().await;
    let result = repo.delete_team("nonexistent").await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

// ── Mailbox Tests ────────────────────────────────────────────────────

#[tokio::test]
async fn write_and_read_unread_messages() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    // Write 3 messages to agent a1
    for i in 1..=3 {
        let msg = make_mailbox_msg(&format!("m{i}"), "t1", "a1", "a2", "message");
        repo.write_message(&msg).await.unwrap();
    }

    // Read unread: should return 3
    let unread = repo.read_unread_and_mark("t1", "a1").await.unwrap();
    assert_eq!(unread.len(), 3);
    assert!(!unread[0].read); // returned rows reflect pre-mark state
    assert_eq!(unread[0].msg_type, "message");

    // Read again: should return 0 (all marked read)
    let unread2 = repo.read_unread_and_mark("t1", "a1").await.unwrap();
    assert!(unread2.is_empty());
}

#[tokio::test]
async fn read_unread_no_messages() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let unread = repo.read_unread_and_mark("t1", "a1").await.unwrap();
    assert!(unread.is_empty());
}

#[tokio::test]
async fn write_idle_notification_with_summary() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let mut msg = make_mailbox_msg("m1", "t1", "a1", "a2", "idle_notification");
    msg.summary = Some("Task completed".into());
    repo.write_message(&msg).await.unwrap();

    let history = repo.get_history("t1", "a1", None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].msg_type, "idle_notification");
    assert_eq!(history[0].summary.as_deref(), Some("Task completed"));
}

#[tokio::test]
async fn write_shutdown_request() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let msg = make_mailbox_msg("m1", "t1", "a1", "a2", "shutdown_request");
    repo.write_message(&msg).await.unwrap();

    let history = repo.get_history("t1", "a1", None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].msg_type, "shutdown_request");
}

#[tokio::test]
async fn get_history_with_limit() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    for i in 1..=10 {
        let msg = make_mailbox_msg(&format!("m{i}"), "t1", "a1", "a2", "message");
        repo.write_message(&msg).await.unwrap();
    }

    let history = repo.get_history("t1", "a1", Some(5)).await.unwrap();
    assert_eq!(history.len(), 5);
}

#[tokio::test]
async fn get_history_no_limit() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    for i in 1..=3 {
        let msg = make_mailbox_msg(&format!("m{i}"), "t1", "a1", "a2", "message");
        repo.write_message(&msg).await.unwrap();
    }

    let history = repo.get_history("t1", "a1", None).await.unwrap();
    assert_eq!(history.len(), 3);
}

#[tokio::test]
async fn get_history_empty() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let history = repo.get_history("t1", "a1", None).await.unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn get_history_includes_read_messages() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let msg = make_mailbox_msg("m1", "t1", "a1", "a2", "message");
    repo.write_message(&msg).await.unwrap();

    // Read and mark
    repo.read_unread_and_mark("t1", "a1").await.unwrap();

    // History should still return the message
    let history = repo.get_history("t1", "a1", None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].read);
}

#[tokio::test]
async fn delete_mailbox_by_team() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team1")).await.unwrap();
    repo.create_team(&make_team("t2", "Team2")).await.unwrap();

    // Write messages to both teams
    let msg1 = make_mailbox_msg("m1", "t1", "a1", "a2", "message");
    let msg2 = make_mailbox_msg("m2", "t2", "a1", "a2", "message");
    repo.write_message(&msg1).await.unwrap();
    repo.write_message(&msg2).await.unwrap();

    // Delete team1 mailbox
    repo.delete_mailbox_by_team("t1").await.unwrap();

    // Team1 mailbox empty
    let h1 = repo.get_history("t1", "a1", None).await.unwrap();
    assert!(h1.is_empty());

    // Team2 mailbox intact
    let h2 = repo.get_history("t2", "a1", None).await.unwrap();
    assert_eq!(h2.len(), 1);
}

// ── Task Board Tests ─────────────────────────────────────────────────

#[tokio::test]
async fn create_and_list_tasks() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tk1", "t1", "Implement feature");
    repo.create_task(&task).await.unwrap();

    let tasks = repo.list_tasks("t1").await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].subject, "Implement feature");
    assert_eq!(tasks[0].status, "pending");
}

#[tokio::test]
async fn list_tasks_empty() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let tasks = repo.list_tasks("t1").await.unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn find_task_by_id() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tk1", "t1", "Task");
    repo.create_task(&task).await.unwrap();

    let found = repo.find_task_by_id("t1", "tk1").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, "tk1");
}

#[tokio::test]
async fn find_task_by_id_not_found() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let found = repo.find_task_by_id("t1", "nonexistent").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn update_task_status() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tk1", "t1", "Task");
    repo.create_task(&task).await.unwrap();

    repo.update_task(
        "tk1",
        &UpdateTaskParams {
            status: Some("in_progress".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.find_task_by_id("t1", "tk1").await.unwrap().unwrap();
    assert_eq!(updated.status, "in_progress");
}

#[tokio::test]
async fn update_task_description_and_owner() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tk1", "t1", "Task");
    repo.create_task(&task).await.unwrap();

    repo.update_task(
        "tk1",
        &UpdateTaskParams {
            description: Some("New description".into()),
            owner: Some("agent-2".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.find_task_by_id("t1", "tk1").await.unwrap().unwrap();
    assert_eq!(updated.description.as_deref(), Some("New description"));
    assert_eq!(updated.owner.as_deref(), Some("agent-2"));
}

#[tokio::test]
async fn update_nonexistent_task_returns_not_found() {
    let (repo, _db) = repo().await;
    let result = repo
        .update_task(
            "nonexistent",
            &UpdateTaskParams {
                status: Some("completed".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

#[tokio::test]
async fn append_to_blocks_and_remove_from_blocked_by() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    // Create taskA and taskB
    let task_a = make_task("tkA", "t1", "Task A");
    let mut task_b = make_task("tkB", "t1", "Task B");
    task_b.blocked_by = r#"["tkA"]"#.into();
    repo.create_task(&task_a).await.unwrap();
    repo.create_task(&task_b).await.unwrap();

    // Append tkB to taskA's blocks
    repo.append_to_blocks("tkA", "tkB").await.unwrap();

    let a = repo.find_task_by_id("t1", "tkA").await.unwrap().unwrap();
    let blocks: Vec<String> = serde_json::from_str(&a.blocks).unwrap();
    assert!(blocks.contains(&"tkB".to_string()));

    // Now complete taskA: remove tkA from taskB's blocked_by
    repo.remove_from_blocked_by("tkB", "tkA").await.unwrap();

    let b = repo.find_task_by_id("t1", "tkB").await.unwrap().unwrap();
    let blocked_by: Vec<String> = serde_json::from_str(&b.blocked_by).unwrap();
    assert!(!blocked_by.contains(&"tkA".to_string()));
}

#[tokio::test]
async fn append_to_blocks_idempotent() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tkA", "t1", "Task A");
    repo.create_task(&task).await.unwrap();

    repo.append_to_blocks("tkA", "tkB").await.unwrap();
    repo.append_to_blocks("tkA", "tkB").await.unwrap();

    let a = repo.find_task_by_id("t1", "tkA").await.unwrap().unwrap();
    let blocks: Vec<String> = serde_json::from_str(&a.blocks).unwrap();
    assert_eq!(blocks.len(), 1); // no duplicates
}

#[tokio::test]
async fn multi_dependency_unblock() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    // taskA blocks taskB and taskC
    let task_a = make_task("tkA", "t1", "A");
    let mut task_b = make_task("tkB", "t1", "B");
    task_b.blocked_by = r#"["tkA"]"#.into();
    let mut task_c = make_task("tkC", "t1", "C");
    task_c.blocked_by = r#"["tkA"]"#.into();

    repo.create_task(&task_a).await.unwrap();
    repo.create_task(&task_b).await.unwrap();
    repo.create_task(&task_c).await.unwrap();

    repo.append_to_blocks("tkA", "tkB").await.unwrap();
    repo.append_to_blocks("tkA", "tkC").await.unwrap();

    // Complete A: unblock both B and C
    repo.remove_from_blocked_by("tkB", "tkA").await.unwrap();
    repo.remove_from_blocked_by("tkC", "tkA").await.unwrap();

    let b = repo.find_task_by_id("t1", "tkB").await.unwrap().unwrap();
    let c = repo.find_task_by_id("t1", "tkC").await.unwrap().unwrap();
    let b_blocked: Vec<String> = serde_json::from_str(&b.blocked_by).unwrap();
    let c_blocked: Vec<String> = serde_json::from_str(&c.blocked_by).unwrap();
    assert!(b_blocked.is_empty());
    assert!(c_blocked.is_empty());
}

#[tokio::test]
async fn partial_unblock_preserves_other_blockers() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    // taskB is blocked by both tkA and tkX
    let task_a = make_task("tkA", "t1", "A");
    let mut task_b = make_task("tkB", "t1", "B");
    task_b.blocked_by = r#"["tkA","tkX"]"#.into();

    repo.create_task(&task_a).await.unwrap();
    repo.create_task(&task_b).await.unwrap();

    // Complete A only
    repo.remove_from_blocked_by("tkB", "tkA").await.unwrap();

    let b = repo.find_task_by_id("t1", "tkB").await.unwrap().unwrap();
    let blocked_by: Vec<String> = serde_json::from_str(&b.blocked_by).unwrap();
    assert_eq!(blocked_by, vec!["tkX"]);
}

#[tokio::test]
async fn no_blocks_task_completes_cleanly() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task = make_task("tkA", "t1", "A");
    repo.create_task(&task).await.unwrap();

    // Complete without any blocks to unblock
    repo.update_task(
        "tkA",
        &UpdateTaskParams {
            status: Some("completed".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let a = repo.find_task_by_id("t1", "tkA").await.unwrap().unwrap();
    assert_eq!(a.status, "completed");
    let blocks: Vec<String> = serde_json::from_str(&a.blocks).unwrap();
    assert!(blocks.is_empty());
}

#[tokio::test]
async fn delete_tasks_by_team() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team1")).await.unwrap();
    repo.create_team(&make_team("t2", "Team2")).await.unwrap();

    repo.create_task(&make_task("tk1", "t1", "T1 Task")).await.unwrap();
    repo.create_task(&make_task("tk2", "t2", "T2 Task")).await.unwrap();

    repo.delete_tasks_by_team("t1").await.unwrap();

    let t1_tasks = repo.list_tasks("t1").await.unwrap();
    assert!(t1_tasks.is_empty());

    let t2_tasks = repo.list_tasks("t2").await.unwrap();
    assert_eq!(t2_tasks.len(), 1);
}

#[tokio::test]
async fn tasks_contain_dependency_info() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task_a = make_task("tkA", "t1", "A");
    let mut task_b = make_task("tkB", "t1", "B");
    task_b.blocked_by = r#"["tkA"]"#.into();

    repo.create_task(&task_a).await.unwrap();
    repo.create_task(&task_b).await.unwrap();
    repo.append_to_blocks("tkA", "tkB").await.unwrap();

    let tasks = repo.list_tasks("t1").await.unwrap();
    assert_eq!(tasks.len(), 2);

    let a = tasks.iter().find(|t| t.id == "tkA").unwrap();
    let b = tasks.iter().find(|t| t.id == "tkB").unwrap();

    let a_blocks: Vec<String> = serde_json::from_str(&a.blocks).unwrap();
    assert!(a_blocks.contains(&"tkB".to_string()));

    let b_blocked_by: Vec<String> = serde_json::from_str(&b.blocked_by).unwrap();
    assert!(b_blocked_by.contains(&"tkA".to_string()));
}

// ── Data Consistency Tests ───────────────────────────────────────────

#[tokio::test]
async fn delete_team_cascades_mailbox_and_tasks() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    // Add mailbox messages and tasks
    let msg = make_mailbox_msg("m1", "t1", "a1", "a2", "message");
    repo.write_message(&msg).await.unwrap();
    let task = make_task("tk1", "t1", "Task");
    repo.create_task(&task).await.unwrap();

    // Delete team, then manually clean up related data (as service layer would)
    repo.delete_mailbox_by_team("t1").await.unwrap();
    repo.delete_tasks_by_team("t1").await.unwrap();
    repo.delete_team("t1").await.unwrap();

    // Verify all cleaned up
    let team = repo.get_team("t1").await.unwrap();
    assert!(team.is_none());
    let mail = repo.get_history("t1", "a1", None).await.unwrap();
    assert!(mail.is_empty());
    let tasks = repo.list_tasks("t1").await.unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn task_blocked_by_blocks_bidirectional_consistency() {
    let (repo, _db) = repo().await;
    repo.create_team(&make_team("t1", "Team")).await.unwrap();

    let task_a = make_task("tkA", "t1", "A");
    let mut task_b = make_task("tkB", "t1", "B");
    task_b.blocked_by = r#"["tkA"]"#.into();

    repo.create_task(&task_a).await.unwrap();
    repo.create_task(&task_b).await.unwrap();
    repo.append_to_blocks("tkA", "tkB").await.unwrap();

    // Verify bidirectional link
    let a = repo.find_task_by_id("t1", "tkA").await.unwrap().unwrap();
    let b = repo.find_task_by_id("t1", "tkB").await.unwrap().unwrap();

    let a_blocks: Vec<String> = serde_json::from_str(&a.blocks).unwrap();
    let b_blocked_by: Vec<String> = serde_json::from_str(&b.blocked_by).unwrap();

    assert!(a_blocks.contains(&"tkB".to_string()), "A.blocks should contain B");
    assert!(
        b_blocked_by.contains(&"tkA".to_string()),
        "B.blockedBy should contain A"
    );
}
