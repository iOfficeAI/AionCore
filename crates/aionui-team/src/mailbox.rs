use std::sync::Arc;

use aionui_api_types::TeamMailboxChange;
use aionui_common::{generate_id, now_ms};
use aionui_db::ITeamRepository;
use aionui_db::models::MailboxMessageRow;
use tracing::debug;

use crate::activity_mapping::mailbox_row_to_response;
use crate::error::TeamError;
use crate::events::TeamEventEmitter;
use crate::types::{MailboxMessage, MailboxMessageType};

pub struct Mailbox {
    repo: Arc<dyn ITeamRepository>,
    /// Optional real-time emitter. When present, mailbox writes and read-marks
    /// broadcast `team.mailboxChanged`. Absent in unit tests that use
    /// [`Mailbox::new`] directly.
    events: Option<Arc<TeamEventEmitter>>,
    user_id: String,
}

impl Mailbox {
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

    /// Attaches a real-time event emitter for `team.mailboxChanged` broadcasts.
    pub fn with_events(mut self, events: Arc<TeamEventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    pub async fn write(
        &self,
        team_id: &str,
        to_agent_id: &str,
        from_agent_id: &str,
        msg_type: MailboxMessageType,
        content: &str,
        summary: Option<&str>,
    ) -> Result<MailboxMessage, TeamError> {
        self.write_with_files(team_id, to_agent_id, from_agent_id, msg_type, content, summary, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write_with_files(
        &self,
        team_id: &str,
        to_agent_id: &str,
        from_agent_id: &str,
        msg_type: MailboxMessageType,
        content: &str,
        summary: Option<&str>,
        files: Option<&[String]>,
    ) -> Result<MailboxMessage, TeamError> {
        let files_json = files
            .filter(|f| !f.is_empty())
            .map(|f| serde_json::to_string(f).unwrap_or_default());
        let row = MailboxMessageRow {
            id: generate_id(),
            team_id: team_id.to_owned(),
            to_agent_id: to_agent_id.to_owned(),
            from_agent_id: from_agent_id.to_owned(),
            msg_type: msg_type.to_string(),
            content: content.to_owned(),
            summary: summary.map(str::to_owned),
            files: files_json,
            read: false,
            created_at: now_ms(),
        };

        self.repo.write_message(&self.user_id, &row).await?;

        debug!(
            team_id,
            to = to_agent_id,
            from = from_agent_id,
            msg_type = %msg_type,
            "mailbox message written"
        );

        if let Some(events) = &self.events {
            events.broadcast_mailbox_changed(mailbox_row_to_response(&row), TeamMailboxChange::Created);
        }

        MailboxMessage::from_row(&row)
            .ok_or_else(|| TeamError::InvalidRequest(format!("invalid message type: {msg_type}")))
    }

    pub async fn read_unread(&self, team_id: &str, agent_id: &str) -> Result<Vec<MailboxMessage>, TeamError> {
        let rows = self.repo.read_unread_and_mark(&self.user_id, team_id, agent_id).await?;

        debug!(team_id, agent_id, count = rows.len(), "mailbox unread messages read");

        // The returned rows reflect the pre-mark state (read=false); they are
        // now read, so broadcast each as a `read` change. `read_unread` and
        // `mark_read_batch` are the only two `Read` emission points.
        if let Some(events) = &self.events {
            for row in &rows {
                let mut resp = mailbox_row_to_response(row);
                resp.read = true;
                events.broadcast_mailbox_changed(resp, TeamMailboxChange::Read);
            }
        }

        let messages = rows.iter().filter_map(MailboxMessage::from_row).collect();
        Ok(messages)
    }

    /// Reads all unread messages without marking them as read.
    /// Used by the drain_mailbox pattern: peek → prompt → mark_read on success.
    pub async fn peek_unread(&self, team_id: &str, agent_id: &str) -> Result<Vec<MailboxMessage>, TeamError> {
        let rows = self.repo.peek_unread(&self.user_id, team_id, agent_id).await?;
        debug!(team_id, agent_id, count = rows.len(), "mailbox peek_unread");
        let messages = rows.iter().filter_map(MailboxMessage::from_row).collect();
        Ok(messages)
    }

    /// Re-reads a claimed set of unread mailbox rows without marking them read.
    pub async fn peek_unread_by_ids(
        &self,
        team_id: &str,
        agent_id: &str,
        ids: &[String],
    ) -> Result<Vec<MailboxMessage>, TeamError> {
        let rows = self
            .repo
            .peek_unread_by_ids(&self.user_id, team_id, agent_id, ids)
            .await?;
        debug!(
            team_id,
            agent_id,
            requested_count = ids.len(),
            found_count = rows.len(),
            "mailbox peek_unread_by_ids"
        );
        Ok(rows.iter().filter_map(MailboxMessage::from_row).collect())
    }

    /// Marks the given message IDs as read. Called after successful prompt delivery.
    pub async fn mark_read_batch(&self, team_id: &str, ids: &[String]) -> Result<(), TeamError> {
        self.repo.mark_read_batch(&self.user_id, team_id, ids).await?;

        // Fetch the (now-read) rows to build full payloads and broadcast one
        // `read` change per message; the frontend upserts by id idempotently.
        if let Some(events) = &self.events
            && !ids.is_empty()
        {
            let rows = self.repo.list_messages_by_ids(ids).await?;
            for row in &rows {
                let mut resp = mailbox_row_to_response(row);
                resp.read = true;
                events.broadcast_mailbox_changed(resp, TeamMailboxChange::Read);
            }
        }
        Ok(())
    }

    /// Runtime-only cancel helper: mark currently unread mailbox rows as read
    /// for the provided agents so a cancelled run does not consume them later.
    pub async fn mark_all_unread_for_agents_read(
        &self,
        team_id: &str,
        agent_ids: &[String],
    ) -> Result<usize, TeamError> {
        let mut ids = Vec::new();
        for agent_id in agent_ids {
            let unread = self.peek_unread(team_id, agent_id).await?;
            ids.extend(unread.into_iter().map(|message| message.id));
        }
        let count = ids.len();
        if !ids.is_empty() {
            self.mark_read_batch(team_id, &ids).await?;
        }
        Ok(count)
    }

    pub async fn get_history(
        &self,
        team_id: &str,
        agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<MailboxMessage>, TeamError> {
        let rows = self.repo.get_history(&self.user_id, team_id, agent_id, limit).await?;
        let messages = rows.iter().filter_map(MailboxMessage::from_row).collect();
        Ok(messages)
    }

    pub async fn has_unread(&self, team_id: &str, agent_id: &str) -> Result<bool, TeamError> {
        let rows = self.repo.get_history(&self.user_id, team_id, agent_id, None).await?;
        Ok(rows.iter().any(|r| !r.read))
    }

    pub async fn delete_by_team(&self, team_id: &str) -> Result<(), TeamError> {
        let row = self
            .repo
            .get_team_for_restore(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_owned()))?;
        self.repo.delete_mailbox_by_team(&row.user_id, team_id).await?;
        debug!(team_id, "mailbox messages deleted for team");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockTeamRepo;
    use aionui_api_types::{TeamMailboxChangedPayload, WebSocketMessage};
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

    fn mailbox_with_events(repo: Arc<MockTeamRepo>) -> (Mailbox, Arc<RecordingBroadcaster>) {
        let bc = Arc::new(RecordingBroadcaster::new());
        let emitter = Arc::new(TeamEventEmitter::new(
            "t1".into(),
            "system_default_user".into(),
            bc.clone(),
        ));
        (Mailbox::new(repo).with_events(emitter), bc)
    }

    fn mailbox_changes(bc: &RecordingBroadcaster) -> Vec<TeamMailboxChangedPayload> {
        bc.events()
            .into_iter()
            .filter(|e| e.name == "team.mailboxChanged")
            .map(|e| serde_json::from_value(e.data).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn write_broadcasts_created() {
        let repo = Arc::new(MockTeamRepo::new());
        let (mailbox, bc) = mailbox_with_events(repo);

        mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "hi", None)
            .await
            .unwrap();

        let changes = mailbox_changes(&bc);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, TeamMailboxChange::Created);
        assert_eq!(changes[0].message.content, "hi");
        assert!(!changes[0].message.read);
    }

    #[tokio::test]
    async fn read_unread_broadcasts_read() {
        let repo = Arc::new(MockTeamRepo::new());
        let (mailbox, bc) = mailbox_with_events(repo);

        mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "m1", None)
            .await
            .unwrap();
        mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "m2", None)
            .await
            .unwrap();
        mailbox.read_unread("t1", "a1").await.unwrap();

        let read_changes: Vec<_> = mailbox_changes(&bc)
            .into_iter()
            .filter(|c| c.change == TeamMailboxChange::Read)
            .collect();
        assert_eq!(read_changes.len(), 2);
        assert!(read_changes.iter().all(|c| c.message.read));
    }

    #[tokio::test]
    async fn mark_read_batch_broadcasts_read() {
        let repo = Arc::new(MockTeamRepo::new());
        let (mailbox, bc) = mailbox_with_events(repo);

        let m = mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "m1", None)
            .await
            .unwrap();
        mailbox
            .mark_read_batch("t1", std::slice::from_ref(&m.id))
            .await
            .unwrap();

        let read_changes: Vec<_> = mailbox_changes(&bc)
            .into_iter()
            .filter(|c| c.change == TeamMailboxChange::Read)
            .collect();
        assert_eq!(read_changes.len(), 1);
        assert_eq!(read_changes[0].message.id, m.id);
        assert!(read_changes[0].message.read);
    }

    #[tokio::test]
    async fn no_emitter_does_not_panic_and_emits_nothing() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);
        let m = mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "m1", None)
            .await
            .unwrap();
        mailbox.read_unread("t1", "a1").await.unwrap();
        mailbox.mark_read_batch("t1", &[m.id]).await.unwrap();
        // No broadcaster attached: nothing to assert beyond not panicking.
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn write_and_read_unread() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        mailbox
            .write("t1", "a1", "user", MailboxMessageType::Message, "hi", None)
            .await
            .unwrap();
        mailbox
            .write("t1", "a1", "a2", MailboxMessageType::Message, "hello", None)
            .await
            .unwrap();

        let unread = mailbox.read_unread("t1", "a1").await.unwrap();
        assert_eq!(unread.len(), 2);
        assert_eq!(unread[0].content, "hi");
        assert_eq!(unread[1].content, "hello");

        let unread_again = mailbox.read_unread("t1", "a1").await.unwrap();
        assert!(unread_again.is_empty());
    }

    #[tokio::test]
    async fn write_idle_notification_with_summary() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        let msg = mailbox
            .write(
                "t1",
                "lead",
                "a1",
                MailboxMessageType::IdleNotification,
                "done",
                Some("Task complete"),
            )
            .await
            .unwrap();

        assert_eq!(msg.msg_type, MailboxMessageType::IdleNotification);
        assert_eq!(msg.summary.as_deref(), Some("Task complete"));
    }

    #[tokio::test]
    async fn get_history_includes_read_messages() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        mailbox
            .write("t1", "a1", "user", MailboxMessageType::Message, "m1", None)
            .await
            .unwrap();
        mailbox
            .write("t1", "a1", "user", MailboxMessageType::Message, "m2", None)
            .await
            .unwrap();

        mailbox.read_unread("t1", "a1").await.unwrap();

        let history = mailbox.get_history("t1", "a1", None).await.unwrap();
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn get_history_with_limit() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        for i in 0..5 {
            mailbox
                .write(
                    "t1",
                    "a1",
                    "user",
                    MailboxMessageType::Message,
                    &format!("msg-{i}"),
                    None,
                )
                .await
                .unwrap();
        }

        let history = mailbox.get_history("t1", "a1", Some(3)).await.unwrap();
        assert_eq!(history.len(), 3);
    }

    #[tokio::test]
    async fn delete_by_team_removes_all() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        mailbox
            .write("t1", "a1", "user", MailboxMessageType::Message, "x", None)
            .await
            .unwrap();
        mailbox
            .write("t2", "a1", "user", MailboxMessageType::Message, "y", None)
            .await
            .unwrap();

        mailbox.delete_by_team("t1").await.unwrap();

        let h1 = mailbox.get_history("t1", "a1", None).await.unwrap();
        assert!(h1.is_empty());

        let h2 = mailbox.get_history("t2", "a1", None).await.unwrap();
        assert_eq!(h2.len(), 1);
    }

    #[tokio::test]
    async fn read_unread_empty_when_no_messages() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        let unread = mailbox.read_unread("t1", "a1").await.unwrap();
        assert!(unread.is_empty());
    }

    #[tokio::test]
    async fn read_unread_scoped_to_agent() {
        let repo = Arc::new(MockTeamRepo::new());
        let mailbox = Mailbox::new(repo);

        mailbox
            .write("t1", "a1", "user", MailboxMessageType::Message, "for-a1", None)
            .await
            .unwrap();
        mailbox
            .write("t1", "a2", "user", MailboxMessageType::Message, "for-a2", None)
            .await
            .unwrap();

        let unread_a1 = mailbox.read_unread("t1", "a1").await.unwrap();
        assert_eq!(unread_a1.len(), 1);
        assert_eq!(unread_a1[0].content, "for-a1");

        let unread_a2 = mailbox.read_unread("t1", "a2").await.unwrap();
        assert_eq!(unread_a2.len(), 1);
        assert_eq!(unread_a2[0].content, "for-a2");
    }
}
