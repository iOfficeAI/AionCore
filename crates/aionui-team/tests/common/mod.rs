use aionui_common::now_ms;
use aionui_db::models::{MailboxMessageRow, TeamRow, TeamTaskRow};
use aionui_db::{DbError, ITeamRepository, UpdateTaskParams, UpdateTeamParams};
use std::sync::Mutex;

#[derive(Default)]
pub struct MockState {
    pub messages: Vec<MailboxMessageRow>,
    pub tasks: Vec<TeamTaskRow>,
}

pub struct MockTeamRepo {
    pub state: Mutex<MockState>,
}

impl MockTeamRepo {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState::default()),
        }
    }
}

#[async_trait::async_trait]
impl ITeamRepository for MockTeamRepo {
    async fn create_team(&self, _row: &TeamRow) -> Result<(), DbError> {
        Ok(())
    }
    async fn list_teams(&self) -> Result<Vec<TeamRow>, DbError> {
        Ok(vec![])
    }
    async fn list_teams_by_user(&self, _user_id: &str) -> Result<Vec<TeamRow>, DbError> {
        Ok(vec![])
    }
    async fn get_team(&self, _id: &str) -> Result<Option<TeamRow>, DbError> {
        Ok(None)
    }
    async fn update_team(&self, _id: &str, _p: &UpdateTeamParams) -> Result<(), DbError> {
        Ok(())
    }
    async fn delete_team(&self, _id: &str) -> Result<(), DbError> {
        Ok(())
    }

    async fn write_message(&self, row: &MailboxMessageRow) -> Result<(), DbError> {
        self.state.lock().unwrap().messages.push(row.clone());
        Ok(())
    }

    async fn read_unread_and_mark(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError> {
        let mut state = self.state.lock().unwrap();
        let mut result = vec![];
        for msg in &mut state.messages {
            if msg.team_id == team_id && msg.to_agent_id == to_agent_id && !msg.read {
                msg.read = true;
                result.push(msg.clone());
            }
        }
        Ok(result)
    }

    async fn peek_unread(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError> {
        let state = self.state.lock().unwrap();
        let result = state
            .messages
            .iter()
            .filter(|m| m.team_id == team_id && m.to_agent_id == to_agent_id && !m.read)
            .cloned()
            .collect();
        Ok(result)
    }

    async fn mark_read_batch(&self, ids: &[String]) -> Result<(), DbError> {
        let mut state = self.state.lock().unwrap();
        for msg in &mut state.messages {
            if ids.contains(&msg.id) {
                msg.read = true;
            }
        }
        Ok(())
    }

    async fn get_history(
        &self,
        team_id: &str,
        to_agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<MailboxMessageRow>, DbError> {
        let state = self.state.lock().unwrap();
        let iter = state
            .messages
            .iter()
            .filter(|m| m.team_id == team_id && m.to_agent_id == to_agent_id);
        let msgs: Vec<_> = match limit {
            Some(n) => iter.take(n as usize).cloned().collect(),
            None => iter.cloned().collect(),
        };
        Ok(msgs)
    }

    async fn delete_mailbox_by_team(&self, team_id: &str) -> Result<(), DbError> {
        self.state.lock().unwrap().messages.retain(|m| m.team_id != team_id);
        Ok(())
    }

    async fn create_task(&self, row: &TeamTaskRow) -> Result<(), DbError> {
        self.state.lock().unwrap().tasks.push(row.clone());
        Ok(())
    }

    async fn find_task_by_id(&self, team_id: &str, task_id: &str) -> Result<Option<TeamTaskRow>, DbError> {
        let state = self.state.lock().unwrap();
        let found = state
            .tasks
            .iter()
            .find(|t| t.team_id == team_id && t.id == task_id)
            .cloned();
        Ok(found)
    }

    async fn update_task(&self, task_id: &str, params: &UpdateTaskParams) -> Result<(), DbError> {
        let mut state = self.state.lock().unwrap();
        let task = state
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| DbError::NotFound(task_id.to_owned()))?;
        if let Some(ref s) = params.status {
            task.status = s.clone();
        }
        if let Some(ref d) = params.description {
            task.description = Some(d.clone());
        }
        if let Some(ref o) = params.owner {
            task.owner = Some(o.clone());
        }
        if let Some(ref b) = params.blocked_by {
            task.blocked_by = b.clone();
        }
        if let Some(ref m) = params.metadata {
            task.metadata = Some(m.clone());
        }
        task.updated_at = now_ms();
        Ok(())
    }

    async fn list_tasks(&self, team_id: &str) -> Result<Vec<TeamTaskRow>, DbError> {
        let state = self.state.lock().unwrap();
        let tasks = state.tasks.iter().filter(|t| t.team_id == team_id).cloned().collect();
        Ok(tasks)
    }

    async fn append_to_blocks(&self, task_id: &str, blocked_task_id: &str) -> Result<(), DbError> {
        let mut state = self.state.lock().unwrap();
        let task = state
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| DbError::NotFound(task_id.to_owned()))?;
        let mut blocks: Vec<String> = serde_json::from_str(&task.blocks).unwrap_or_default();
        blocks.push(blocked_task_id.to_owned());
        task.blocks = serde_json::to_string(&blocks).unwrap();
        Ok(())
    }

    async fn remove_from_blocked_by(&self, task_id: &str, unblocked_task_id: &str) -> Result<(), DbError> {
        let mut state = self.state.lock().unwrap();
        let task = state
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| DbError::NotFound(task_id.to_owned()))?;
        let mut blocked_by: Vec<String> = serde_json::from_str(&task.blocked_by).unwrap_or_default();
        blocked_by.retain(|id| id != unblocked_task_id);
        task.blocked_by = serde_json::to_string(&blocked_by).unwrap();
        Ok(())
    }

    async fn delete_tasks_by_team(&self, team_id: &str) -> Result<(), DbError> {
        self.state.lock().unwrap().tasks.retain(|t| t.team_id != team_id);
        Ok(())
    }
}
