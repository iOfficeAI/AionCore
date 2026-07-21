use tracing::debug;

use super::TeammateManager;
use crate::error::TeamError;
use crate::types::{MailboxMessage, MailboxMessageType, TeammateRole};

#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerAction {
    SendMessage {
        to: String,
        message: String,
        files: Vec<String>,
    },
    TaskCreate {
        subject: String,
        description: Option<String>,
        owner: Option<String>,
        blocked_by: Vec<String>,
    },
    TaskUpdate {
        task_id: String,
        status: Option<String>,
        description: Option<String>,
        owner: Option<String>,
        blocked_by: Option<Vec<String>>,
    },
    SpawnAgent {
        name: String,
        role: String,
        backend: String,
    },
    IdleNotification {
        summary: Option<String>,
    },
    ShutdownAgent {
        slot_id: String,
        reason: Option<String>,
    },
    RenameAgent {
        slot_id: String,
        new_name: String,
    },
}

impl TeammateManager {
    pub async fn create_task(
        &self,
        subject: &str,
        description: Option<&str>,
        owner: Option<&str>,
        blocked_by: &[String],
    ) -> Result<crate::types::TeamTask, TeamError> {
        self.task_board
            .create_task(&self.team_id, subject, description, owner, blocked_by)
            .await
    }

    pub async fn update_task(
        &self,
        task_id: &str,
        status: Option<&str>,
        description: Option<String>,
        owner: Option<String>,
        blocked_by: Option<Vec<String>>,
    ) -> Result<crate::types::TeamTask, TeamError> {
        use crate::task_board::TaskUpdate;
        use crate::types::TaskStatus;

        let update = TaskUpdate {
            status: status.and_then(TaskStatus::parse),
            description,
            owner,
            blocked_by,
            ..Default::default()
        };
        self.task_board.update_task(&self.team_id, task_id, &update).await
    }

    pub async fn execute_action(
        &self,
        from_slot_id: &str,
        action: &SchedulerAction,
    ) -> Result<Option<String>, TeamError> {
        match action {
            SchedulerAction::SendMessage { to, message, files } => {
                self.handle_send_message(from_slot_id, to, message, files).await?;
                Ok(None)
            }
            SchedulerAction::TaskCreate {
                subject,
                description,
                owner,
                blocked_by,
            } => {
                self.create_task(subject, description.as_deref(), owner.as_deref(), blocked_by)
                    .await?;
                Ok(None)
            }
            SchedulerAction::TaskUpdate {
                task_id,
                status,
                description,
                owner,
                blocked_by,
            } => {
                self.update_task(
                    task_id,
                    status.as_deref(),
                    description.clone(),
                    owner.clone(),
                    blocked_by.clone(),
                )
                .await?;
                Ok(None)
            }
            SchedulerAction::IdleNotification { summary } => {
                self.handle_idle_notification(from_slot_id, summary.as_deref()).await
            }
            SchedulerAction::SpawnAgent { name, role, backend } => {
                debug!(
                    team_id = %self.team_id,
                    from = from_slot_id,
                    name, role, backend,
                    "spawn_agent action — requires TeamSession to complete"
                );
                Ok(None)
            }
            SchedulerAction::ShutdownAgent { slot_id, reason } => {
                self.request_shutdown_agent(from_slot_id, slot_id, reason.as_deref())
                    .await?;
                Ok(None)
            }
            SchedulerAction::RenameAgent { slot_id, new_name } => {
                self.handle_rename_agent(slot_id, new_name).await?;
                Ok(None)
            }
        }
    }

    pub async fn finalize_turn(&self, slot_id: &str, actions: &[SchedulerAction]) -> Result<Option<String>, TeamError> {
        let mut summary: Option<String> = None;
        for action in actions {
            if let SchedulerAction::IdleNotification { summary: s } = action {
                if summary.is_none() {
                    summary.clone_from(s);
                }
                continue;
            }
            self.execute_action(slot_id, action).await?;
        }

        self.mark_idle(slot_id, summary.as_deref()).await
    }

    async fn handle_send_message(
        &self,
        from_slot_id: &str,
        to: &str,
        message: &str,
        files: &[String],
    ) -> Result<(), TeamError> {
        if to == "*" {
            let slots = self.slots.lock().await;
            let targets: Vec<String> = slots.keys().filter(|id| id.as_str() != from_slot_id).cloned().collect();
            drop(slots);

            for target in &targets {
                self.mailbox
                    .write_with_files(
                        &self.team_id,
                        target,
                        from_slot_id,
                        MailboxMessageType::Message,
                        message,
                        None,
                        Some(files),
                    )
                    .await?;
            }
        } else {
            self.mailbox
                .write_with_files(
                    &self.team_id,
                    to,
                    from_slot_id,
                    MailboxMessageType::Message,
                    message,
                    None,
                    Some(files),
                )
                .await?;
        }
        Ok(())
    }

    async fn handle_idle_notification(
        &self,
        from_slot_id: &str,
        summary: Option<&str>,
    ) -> Result<Option<String>, TeamError> {
        self.mark_idle(from_slot_id, summary).await
    }

    pub async fn request_shutdown_agent(
        &self,
        from_slot_id: &str,
        target_slot_id: &str,
        reason: Option<&str>,
    ) -> Result<MailboxMessage, TeamError> {
        let from_role = {
            let slots = self.slots.lock().await;
            let slot = slots
                .get(from_slot_id)
                .ok_or_else(|| TeamError::AgentNotFound(from_slot_id.to_owned()))?;
            slot.agent.role
        };

        if from_role != TeammateRole::Lead {
            return Err(TeamError::InvalidRequest("only lead can shutdown agents".into()));
        }

        {
            let slots = self.slots.lock().await;
            let target = slots
                .get(target_slot_id)
                .ok_or_else(|| TeamError::AgentNotFound(target_slot_id.to_owned()))?;
            if target.agent.role == TeammateRole::Lead {
                return Err(TeamError::InvalidRequest("cannot shutdown the team lead".into()));
            }
        }

        self.mailbox
            .write(
                &self.team_id,
                target_slot_id,
                from_slot_id,
                MailboxMessageType::ShutdownRequest,
                reason.unwrap_or("shutdown requested"),
                None,
            )
            .await
    }

    async fn handle_rename_agent(&self, slot_id: &str, new_name: &str) -> Result<(), TeamError> {
        self.rename_agent(slot_id, new_name).await
    }
}
