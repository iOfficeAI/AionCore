use super::TeammateManager;
use crate::error::TeamError;
use crate::types::{MailboxMessage, MailboxMessageType, TeammateRole};

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

    /// Finalizes an agent turn by marking the slot idle. The leader re-wake and
    /// idle-notification bookkeeping live in [`mark_idle`](Self::mark_idle).
    pub async fn finalize_turn(&self, slot_id: &str) -> Result<Option<String>, TeamError> {
        self.mark_idle(slot_id, None).await
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
}
