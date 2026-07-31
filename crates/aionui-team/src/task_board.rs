use std::sync::Arc;

use aionui_api_types::TeamTaskChange;
use aionui_common::{generate_id, now_ms};
use aionui_db::ITeamRepository;
use aionui_db::UpdateTaskParams;
use aionui_db::models::TeamTaskRow;
use tracing::debug;

use crate::activity_mapping::task_to_response;
use crate::error::TeamError;
use crate::events::TeamEventEmitter;
use crate::types::{TaskStatus, TeamTask};

pub struct TaskBoard {
    repo: Arc<dyn ITeamRepository>,
    /// Optional real-time emitter. When present, task create/update broadcast
    /// `team.taskChanged`. Absent in unit tests that use [`TaskBoard::new`]
    /// directly.
    events: Option<Arc<TeamEventEmitter>>,
    user_id: String,
}

/// Optional fields for task update.
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub status: Option<TaskStatus>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub blocked_by: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
}

impl TaskBoard {
    pub fn new(repo: Arc<dyn ITeamRepository>) -> Self {
        Self::new_for_user(repo, "system_default_user")
    }

    pub fn new_for_user(repo: Arc<dyn ITeamRepository>, user_id: impl Into<String>) -> Self {
        Self {
            repo,
            events: None,
            user_id: user_id.into(),
        }
    }

    /// Attaches a real-time event emitter for `team.taskChanged` broadcasts.
    pub fn with_events(mut self, events: Arc<TeamEventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    pub async fn create_task(
        &self,
        team_id: &str,
        subject: &str,
        description: Option<&str>,
        owner: Option<&str>,
        blocked_by: &[String],
    ) -> Result<TeamTask, TeamError> {
        for dep_id in blocked_by {
            let dep = self.repo.find_task_by_id(&self.user_id, team_id, dep_id).await?;
            if dep.is_none() {
                return Err(TeamError::BlockedTaskNotFound(dep_id.clone()));
            }
        }

        let task_id = generate_id();
        let now = now_ms();
        let blocked_by_json = serde_json::to_string(blocked_by)?;

        let row = TeamTaskRow {
            id: task_id.clone(),
            team_id: team_id.to_owned(),
            subject: subject.to_owned(),
            description: description.map(str::to_owned),
            status: TaskStatus::Pending.to_string(),
            owner: owner.map(str::to_owned),
            blocked_by: blocked_by_json,
            blocks: "[]".to_owned(),
            metadata: None,
            created_at: now,
            updated_at: now,
        };

        self.repo.create_task(&self.user_id, &row).await?;

        for dep_id in blocked_by {
            self.repo
                .append_to_blocks(&self.user_id, team_id, dep_id, &task_id)
                .await?;
        }

        debug!(team_id, task_id = %task_id, subject, "task created");

        let task = TeamTask::from_row(&row).map_err(TeamError::Json)?;
        if let Some(events) = &self.events {
            events.broadcast_task_changed(task_to_response(&task), TeamTaskChange::Created);
        }
        Ok(task)
    }

    pub async fn update_task(&self, team_id: &str, task_id: &str, update: &TaskUpdate) -> Result<TeamTask, TeamError> {
        let existing = self
            .repo
            .find_task_by_id(&self.user_id, team_id, task_id)
            .await?
            .ok_or_else(|| TeamError::TaskNotFound(task_id.to_owned()))?;

        let params = UpdateTaskParams {
            status: update.status.map(|s| s.to_string()),
            description: update.description.clone(),
            owner: update.owner.clone(),
            blocked_by: update.blocked_by.as_ref().map(serde_json::to_string).transpose()?,
            metadata: update.metadata.as_ref().map(serde_json::to_string).transpose()?,
        };

        self.repo.update_task(&self.user_id, team_id, task_id, &params).await?;

        if update.status == Some(TaskStatus::Completed) {
            self.check_unblocks(team_id, task_id, &existing).await?;
        }

        let updated = self
            .repo
            .find_task_by_id(&self.user_id, team_id, task_id)
            .await?
            .ok_or_else(|| TeamError::TaskNotFound(task_id.to_owned()))?;

        debug!(team_id, task_id, "task updated");

        let task = TeamTask::from_row(&updated).map_err(TeamError::Json)?;
        // Deletion is modeled as an update to `status=deleted`, not a separate
        // removed event; the frontend filters/removes by status.
        if let Some(events) = &self.events {
            events.broadcast_task_changed(task_to_response(&task), TeamTaskChange::Updated);
        }
        Ok(task)
    }

    pub async fn list_tasks(&self, team_id: &str) -> Result<Vec<TeamTask>, TeamError> {
        let rows = self.repo.list_tasks(&self.user_id, team_id).await?;
        let tasks = rows.iter().filter_map(|r| TeamTask::from_row(r).ok()).collect();
        Ok(tasks)
    }

    async fn check_unblocks(
        &self,
        team_id: &str,
        completed_task_id: &str,
        completed_row: &TeamTaskRow,
    ) -> Result<(), TeamError> {
        let blocks: Vec<String> = serde_json::from_str(&completed_row.blocks)?;
        for downstream_id in &blocks {
            self.repo
                .remove_from_blocked_by(&self.user_id, team_id, downstream_id, completed_task_id)
                .await?;
            debug!(
                completed = completed_task_id,
                unblocked = %downstream_id,
                "dependency unblocked"
            );
            // Broadcast the downstream task's changed dependency set so the
            // activity board and the downstream-owner wake path both observe it
            // is now (potentially) actionable. Non-fatal: a missing row or parse
            // failure must not abort the completing task's own update.
            if let Some(events) = &self.events
                && let Ok(Some(row)) = self.repo.find_task_by_id(&self.user_id, team_id, downstream_id).await
                && let Ok(task) = TeamTask::from_row(&row)
            {
                events.broadcast_task_changed(task_to_response(&task), TeamTaskChange::Updated);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockTeamRepo;
    use aionui_api_types::{TeamTaskChangedPayload, WebSocketMessage};
    use aionui_realtime::EventBroadcaster;

    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(vec![]),
            }
        }

        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn board_with_events(repo: Arc<MockTeamRepo>) -> (TaskBoard, Arc<RecordingBroadcaster>) {
        let bc = Arc::new(RecordingBroadcaster::new());
        let emitter = Arc::new(TeamEventEmitter::new(
            "t1".into(),
            "system_default_user".into(),
            bc.clone(),
        ));
        (TaskBoard::new(repo).with_events(emitter), bc)
    }

    fn task_changes(bc: &RecordingBroadcaster) -> Vec<TeamTaskChangedPayload> {
        bc.events()
            .into_iter()
            .filter(|e| e.name == "team.taskChanged")
            .map(|e| serde_json::from_value(e.data).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn create_task_broadcasts_created() {
        let repo = Arc::new(MockTeamRepo::new());
        let (board, bc) = board_with_events(repo);

        let task = board.create_task("t1", "Build", None, None, &[]).await.unwrap();

        let changes = task_changes(&bc);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, TeamTaskChange::Created);
        assert_eq!(changes[0].task.id, task.id);
        assert_eq!(changes[0].task.status, "pending");
    }

    #[tokio::test]
    async fn update_task_broadcasts_updated_including_deleted() {
        let repo = Arc::new(MockTeamRepo::new());
        let (board, bc) = board_with_events(repo);

        let task = board.create_task("t1", "Build", None, None, &[]).await.unwrap();
        board
            .update_task(
                "t1",
                &task.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Deleted),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let updated: Vec<_> = task_changes(&bc)
            .into_iter()
            .filter(|c| c.change == TeamTaskChange::Updated)
            .collect();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].task.id, task.id);
        assert_eq!(updated[0].task.status, "deleted");
    }

    #[tokio::test]
    async fn complete_task_broadcasts_downstream_unblock() {
        let repo = Arc::new(MockTeamRepo::new());
        let (board, bc) = board_with_events(repo);

        let a = board.create_task("t1", "A", None, None, &[]).await.unwrap();
        let b = board
            .create_task("t1", "B", None, None, std::slice::from_ref(&a.id))
            .await
            .unwrap();

        board
            .update_task(
                "t1",
                &a.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Completing A must broadcast B's unblock so the board/feed and the
        // downstream-owner wake path both observe B is now actionable.
        let b_updates: Vec<_> = task_changes(&bc)
            .into_iter()
            .filter(|c| c.task.id == b.id && c.change == TeamTaskChange::Updated)
            .collect();
        assert_eq!(b_updates.len(), 1, "completing A must broadcast B's unblock");
        assert!(
            b_updates[0].task.blocked_by.is_empty(),
            "B.blocked_by should be empty after A completes"
        );
    }

    #[tokio::test]
    async fn no_emitter_does_not_panic_and_emits_nothing() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);
        let task = board.create_task("t1", "Build", None, None, &[]).await.unwrap();
        board
            .update_task(
                "t1",
                &task.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // No broadcaster attached: nothing to assert beyond not panicking.
    }

    // -- Helper ---------------------------------------------------------------

    async fn create_simple_task(board: &TaskBoard, team_id: &str, subject: &str) -> TeamTask {
        board.create_task(team_id, subject, None, None, &[]).await.unwrap()
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn create_task_no_dependencies() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task = create_simple_task(&board, "t1", "Implement feature").await;
        assert_eq!(task.subject, "Implement feature");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.blocked_by.is_empty());
        assert!(task.blocks.is_empty());
    }

    #[tokio::test]
    async fn create_task_with_owner_and_description() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task = board
            .create_task("t1", "Design API", Some("REST endpoints"), Some("a1"), &[])
            .await
            .unwrap();
        assert_eq!(task.description.as_deref(), Some("REST endpoints"));
        assert_eq!(task.owner.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn create_task_with_dependencies() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo.clone());

        let task_a = create_simple_task(&board, "t1", "Task A").await;
        let task_b = board
            .create_task("t1", "Task B", None, None, std::slice::from_ref(&task_a.id))
            .await
            .unwrap();

        assert_eq!(task_b.blocked_by, vec![task_a.id.clone()]);

        let updated_a = repo
            .find_task_by_id("system_default_user", "t1", &task_a.id)
            .await
            .unwrap()
            .unwrap();
        let blocks_a: Vec<String> = serde_json::from_str(&updated_a.blocks).unwrap();
        assert_eq!(blocks_a, vec![task_b.id]);
    }

    #[tokio::test]
    async fn create_task_nonexistent_dependency_fails() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let result = board.create_task("t1", "X", None, None, &["nonexistent".into()]).await;
        assert!(matches!(result, Err(TeamError::BlockedTaskNotFound(_))));
    }

    #[tokio::test]
    async fn update_task_status() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task = create_simple_task(&board, "t1", "Work").await;
        let updated = board
            .update_task(
                "t1",
                &task.id,
                &TaskUpdate {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn update_task_description_and_owner() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task = create_simple_task(&board, "t1", "Work").await;
        let updated = board
            .update_task(
                "t1",
                &task.id,
                &TaskUpdate {
                    description: Some("New desc".into()),
                    owner: Some("a2".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.description.as_deref(), Some("New desc"));
        assert_eq!(updated.owner.as_deref(), Some("a2"));
    }

    #[tokio::test]
    async fn update_nonexistent_task_fails() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let result = board.update_task("t1", "nonexistent", &TaskUpdate::default()).await;
        assert!(matches!(result, Err(TeamError::TaskNotFound(_))));
    }

    #[tokio::test]
    async fn complete_task_unblocks_downstream() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task_a = create_simple_task(&board, "t1", "A").await;
        let task_b = board
            .create_task("t1", "B", None, None, std::slice::from_ref(&task_a.id))
            .await
            .unwrap();

        assert_eq!(task_b.blocked_by, vec![task_a.id.clone()]);

        board
            .update_task(
                "t1",
                &task_a.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let tasks = board.list_tasks("t1").await.unwrap();
        let b = tasks.iter().find(|t| t.id == task_b.id).unwrap();
        assert!(b.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn complete_task_unblocks_multiple_downstream() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task_a = create_simple_task(&board, "t1", "A").await;
        let task_b = board
            .create_task("t1", "B", None, None, std::slice::from_ref(&task_a.id))
            .await
            .unwrap();
        let task_c = board
            .create_task("t1", "C", None, None, std::slice::from_ref(&task_a.id))
            .await
            .unwrap();

        board
            .update_task(
                "t1",
                &task_a.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let tasks = board.list_tasks("t1").await.unwrap();
        let b = tasks.iter().find(|t| t.id == task_b.id).unwrap();
        let c = tasks.iter().find(|t| t.id == task_c.id).unwrap();
        assert!(b.blocked_by.is_empty());
        assert!(c.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn partial_unblock_preserves_other_dependencies() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task_a = create_simple_task(&board, "t1", "A").await;
        let task_x = create_simple_task(&board, "t1", "X").await;
        let task_b = board
            .create_task("t1", "B", None, None, &[task_a.id.clone(), task_x.id.clone()])
            .await
            .unwrap();

        assert_eq!(task_b.blocked_by.len(), 2);

        board
            .update_task(
                "t1",
                &task_a.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let tasks = board.list_tasks("t1").await.unwrap();
        let b = tasks.iter().find(|t| t.id == task_b.id).unwrap();
        assert_eq!(b.blocked_by, vec![task_x.id]);
    }

    #[tokio::test]
    async fn complete_task_no_downstream_is_noop() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let task = create_simple_task(&board, "t1", "Standalone").await;
        let updated = board
            .update_task(
                "t1",
                &task.id,
                &TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn list_tasks_empty() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        let tasks = board.list_tasks("t1").await.unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn list_tasks_returns_all() {
        let repo = Arc::new(MockTeamRepo::new());
        let board = TaskBoard::new(repo);

        create_simple_task(&board, "t1", "A").await;
        create_simple_task(&board, "t1", "B").await;
        create_simple_task(&board, "t2", "C").await;

        let tasks = board.list_tasks("t1").await.unwrap();
        assert_eq!(tasks.len(), 2);
    }
}
