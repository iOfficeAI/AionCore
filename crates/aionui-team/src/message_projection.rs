use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_db::models::MessageRow;
use aionui_realtime::EventBroadcaster;
use async_trait::async_trait;
use tracing::info;

use crate::error::TeamError;
use crate::events::TEAMMATE_MESSAGE_EVENT;
use crate::visibility::{TeamVisibilityPolicy, strip_system_notes};

const TEXT_MESSAGE_TYPE: &str = "text";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamProjectionSource {
    User,
    Teammate {
        from_slot_id: String,
        from_name: String,
        sender_backend: Option<String>,
        sender_conversation_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct TeamProjectionRequest {
    pub team_id: String,
    pub slot_id: String,
    pub conversation_id: String,
    pub source: TeamProjectionSource,
    pub content: String,
    pub files: Vec<String>,
    pub visibility: TeamVisibilityPolicy,
    pub dedupe_key: Option<String>,
}

impl TeamProjectionRequest {
    pub fn user_visible(
        team_id: impl Into<String>,
        slot_id: impl Into<String>,
        conversation_id: impl Into<String>,
        content: impl Into<String>,
        files: Vec<String>,
    ) -> Self {
        Self {
            team_id: team_id.into(),
            slot_id: slot_id.into(),
            conversation_id: conversation_id.into(),
            source: TeamProjectionSource::User,
            content: content.into(),
            files,
            visibility: TeamVisibilityPolicy::user_message(),
            dedupe_key: None,
        }
    }

    pub fn teammate_visible(
        team_id: impl Into<String>,
        slot_id: impl Into<String>,
        conversation_id: impl Into<String>,
        from_slot_id: impl Into<String>,
        from_name: impl Into<String>,
        content: impl Into<String>,
        mailbox_message_id: impl Into<String>,
    ) -> Self {
        let team_id = team_id.into();
        let conversation_id = conversation_id.into();
        let mailbox_message_id = mailbox_message_id.into();
        Self {
            dedupe_key: Some(teammate_dedupe_key(&team_id, &mailbox_message_id, &conversation_id)),
            team_id,
            slot_id: slot_id.into(),
            conversation_id,
            source: TeamProjectionSource::Teammate {
                from_slot_id: from_slot_id.into(),
                from_name: from_name.into(),
                sender_backend: None,
                sender_conversation_id: None,
            },
            content: content.into(),
            files: Vec::new(),
            visibility: TeamVisibilityPolicy::teammate_message(),
        }
    }

    fn should_insert_visible_bubble(&self) -> bool {
        match self.source {
            TeamProjectionSource::User => self.visibility.insert_user_visible_bubble,
            TeamProjectionSource::Teammate { .. } => self.visibility.insert_teammate_visible_bubble,
        }
    }
}

pub fn teammate_dedupe_key(team_id: &str, mailbox_message_id: &str, conversation_id: &str) -> String {
    format!("team:{team_id}:mailbox:{mailbox_message_id}:conversation:{conversation_id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedTeamMessage {
    Inserted { msg_id: String },
    AlreadyProjected { msg_id: String },
    Skipped,
}

#[async_trait]
pub trait TeamProjectionMessageStore: Send + Sync {
    fn mint_message_id(&self) -> String;

    async fn find_projected_message(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, TeamError>;

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), TeamError>;
}

pub struct TeamMessageProjection<S: ?Sized> {
    store: Arc<S>,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl<S> TeamMessageProjection<S>
where
    S: TeamProjectionMessageStore + ?Sized,
{
    pub fn new(store: Arc<S>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { store, broadcaster }
    }

    pub async fn project(&self, request: TeamProjectionRequest) -> Result<ProjectedTeamMessage, TeamError> {
        if !request.should_insert_visible_bubble() {
            info!(
                team_id = %request.team_id,
                slot_id = %request.slot_id,
                conversation_id = %request.conversation_id,
                event_name = "",
                outcome = "skipped",
                "Team message projection skipped by visibility policy"
            );
            return Ok(ProjectedTeamMessage::Skipped);
        }

        let msg_id = request
            .dedupe_key
            .clone()
            .unwrap_or_else(|| self.store.mint_message_id());

        if request.dedupe_key.is_some()
            && let Some(existing) = self
                .store
                .find_projected_message(&request.conversation_id, &msg_id, TEXT_MESSAGE_TYPE)
                .await?
        {
            let existing_msg_id = existing.msg_id.unwrap_or(existing.id);
            info!(
                team_id = %request.team_id,
                slot_id = %request.slot_id,
                conversation_id = %request.conversation_id,
                event_name = TEAMMATE_MESSAGE_EVENT,
                outcome = "already_projected",
                "Team message projection deduped"
            );
            return Ok(ProjectedTeamMessage::AlreadyProjected {
                msg_id: existing_msg_id,
            });
        }

        let row = Self::build_message_row(&request, &msg_id, aionui_common::now_ms())?;
        self.store.insert_projected_message(&row).await?;

        if let TeamProjectionSource::Teammate {
            from_slot_id,
            from_name,
            sender_backend,
            sender_conversation_id,
        } = &request.source
        {
            let payload = serde_json::json!({
                "team_id": request.team_id,
                "slot_id": request.slot_id,
                "conversation_id": request.conversation_id,
                "msg_id": msg_id,
                "content": request.content,
                "from_slot_id": from_slot_id,
                "from_name": from_name,
                "teammate_message": true,
                "sender_backend": sender_backend,
                "sender_conversation_id": sender_conversation_id,
            });
            self.broadcaster
                .broadcast(WebSocketMessage::new(TEAMMATE_MESSAGE_EVENT, payload));
        }

        info!(
            team_id = %request.team_id,
            slot_id = %request.slot_id,
            conversation_id = %request.conversation_id,
            event_name = match request.source {
                TeamProjectionSource::User => "message.stream",
                TeamProjectionSource::Teammate { .. } => TEAMMATE_MESSAGE_EVENT,
            },
            outcome = "inserted",
            "Team message projected"
        );

        Ok(ProjectedTeamMessage::Inserted { msg_id })
    }

    pub fn build_message_row(
        request: &TeamProjectionRequest,
        msg_id: &str,
        created_at: aionui_common::TimestampMs,
    ) -> Result<MessageRow, TeamError> {
        let (position, content) = match &request.source {
            TeamProjectionSource::User => {
                let content = if request.visibility.strip_system_notes {
                    strip_system_notes(&request.content)
                } else {
                    request.content.clone()
                };
                (
                    "right",
                    serde_json::json!({
                        "content": content,
                    }),
                )
            }
            TeamProjectionSource::Teammate {
                from_slot_id: _,
                from_name,
                sender_backend,
                sender_conversation_id,
            } => (
                "left",
                serde_json::json!({
                    "content": request.content,
                    "teammate_message": true,
                    "sender_name": from_name,
                    "sender_backend": sender_backend,
                    "sender_conversation_id": sender_conversation_id,
                }),
            ),
        };

        Ok(MessageRow {
            id: msg_id.to_owned(),
            conversation_id: request.conversation_id.clone(),
            msg_id: Some(msg_id.to_owned()),
            r#type: TEXT_MESSAGE_TYPE.into(),
            content: serde_json::to_string(&content)?,
            position: Some(position.into()),
            status: Some("finish".into()),
            hidden: request.visibility.allow_hidden_conversation_message,
            created_at,
        })
    }
}
