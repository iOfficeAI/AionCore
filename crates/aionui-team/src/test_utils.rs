use aionui_common::now_ms;
use aionui_db::models::{MailboxMessageRow, TeamRow, TeamTaskRow};
use aionui_db::{DbError, ITeamRepository, UpdateTaskParams, UpdateTeamParams};
use std::sync::Mutex;

#[derive(Default)]
pub struct MockState {
    pub messages: Vec<MailboxMessageRow>,
    pub tasks: Vec<TeamTaskRow>,
    pub fail_message_writes: bool,
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

    pub fn with_message_write_failure() -> Self {
        let repo = Self::new();
        repo.state.lock().unwrap().fail_message_writes = true;
        repo
    }
}

#[async_trait::async_trait]
impl ITeamRepository for MockTeamRepo {
    // ── Team CRUD (stubs) ───────────────────────────────────────────

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

    // ── Mailbox ─────────────────────────────────────────────────────

    async fn write_message(&self, row: &MailboxMessageRow) -> Result<(), DbError> {
        let mut state = self.state.lock().unwrap();
        if state.fail_message_writes {
            return Err(DbError::Init("forced mailbox write failure".into()));
        }
        state.messages.push(row.clone());
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

    // ── TaskBoard ───────────────────────────────────────────────────

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

#[cfg(test)]
pub(crate) mod workspace_harness {
    use std::sync::{Arc, Mutex};

    use aionui_ai_agent::{AgentError, IWorkerTaskManager};
    use aionui_api_types::{CreateTeamRequest, WebSocketMessage};
    use aionui_common::{AgentKillReason, PaginatedResult, now_ms};
    use aionui_db::models::{
        AgentMetadataRow, ConversationRow, MessageRow, TeamRow, TeamTaskRow, UpdateAgentHandshakeParams,
        UpsertAgentMetadataParams,
    };
    use aionui_db::{
        ConversationFilters, ConversationRowUpdate, DbError, IAgentMetadataRepository, IConversationRepository,
        IProviderRepository, ITeamRepository, MessageRowUpdate, MessageSearchRow, SortOrder, UpdateTeamParams,
    };
    use aionui_realtime::EventBroadcaster;
    use async_trait::async_trait;

    use crate::ports::{
        AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
        AgentTurnStarted, AgentTurnStatus, TeamConversationBindingLookup, TeamConversationLookupPort,
    };
    use crate::provisioning::{
        TeamConversationAdoptRequest, TeamConversationCreateRequest, TeamConversationCreateResult,
        TeamConversationProvisioningPort,
    };
    use crate::{TeamError, TeamProjectionMessageStore, TeamSessionService};

    pub(crate) struct MockConversationRepo {
        conversations: Mutex<Vec<ConversationRow>>,
    }

    impl MockConversationRepo {
        fn new() -> Self {
            Self {
                conversations: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn get_extra(&self, id: &str) -> Option<serde_json::Value> {
            self.conversations
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| serde_json::from_str(&c.extra).ok())
        }
    }

    #[async_trait]
    impl IConversationRepository for MockConversationRepo {
        async fn get(&self, id: &str) -> Result<Option<ConversationRow>, DbError> {
            Ok(self.conversations.lock().unwrap().iter().find(|c| c.id == id).cloned())
        }

        async fn create(&self, row: &ConversationRow) -> Result<(), DbError> {
            self.conversations.lock().unwrap().push(row.clone());
            Ok(())
        }

        async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), DbError> {
            let mut conversations = self.conversations.lock().unwrap();
            let conversation = conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| DbError::NotFound(id.to_owned()))?;
            if let Some(ref extra) = updates.extra {
                conversation.extra = extra.clone();
            }
            if let Some(ref name) = updates.name {
                conversation.name = name.clone();
            }
            if let Some(ref model) = updates.model {
                conversation.model = model.clone();
            }
            if let Some(pinned) = updates.pinned {
                conversation.pinned = pinned;
            }
            if let Some(updated_at) = updates.updated_at {
                conversation.updated_at = updated_at;
            }
            Ok(())
        }

        async fn delete(&self, id: &str) -> Result<(), DbError> {
            self.conversations.lock().unwrap().retain(|c| c.id != id);
            Ok(())
        }

        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &ConversationFilters,
        ) -> Result<PaginatedResult<ConversationRow>, DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }

        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<ConversationRow>, DbError> {
            Ok(None)
        }

        async fn list_by_cron_job(&self, _user_id: &str, _cron_job_id: &str) -> Result<Vec<ConversationRow>, DbError> {
            Ok(vec![])
        }

        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<Vec<ConversationRow>, DbError> {
            Ok(vec![])
        }

        async fn get_messages(
            &self,
            _conv_id: &str,
            _page: u32,
            _page_size: u32,
            _order: SortOrder,
        ) -> Result<PaginatedResult<MessageRow>, DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }

        async fn insert_message(&self, _message: &MessageRow) -> Result<(), DbError> {
            Ok(())
        }

        async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), DbError> {
            Ok(())
        }

        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<MessageRow>, DbError> {
            Ok(None)
        }

        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<PaginatedResult<MessageSearchRow>, DbError> {
            Ok(PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
    }

    pub(crate) struct FullMockTeamRepo {
        teams: Mutex<Vec<TeamRow>>,
    }

    impl FullMockTeamRepo {
        fn new() -> Self {
            Self {
                teams: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ITeamRepository for FullMockTeamRepo {
        async fn create_team(&self, row: &TeamRow) -> Result<(), DbError> {
            self.teams.lock().unwrap().push(row.clone());
            Ok(())
        }

        async fn list_teams(&self) -> Result<Vec<TeamRow>, DbError> {
            Ok(self.teams.lock().unwrap().clone())
        }

        async fn list_teams_by_user(&self, user_id: &str) -> Result<Vec<TeamRow>, DbError> {
            Ok(self
                .teams
                .lock()
                .unwrap()
                .iter()
                .filter(|team| team.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn get_team(&self, id: &str) -> Result<Option<TeamRow>, DbError> {
            Ok(self.teams.lock().unwrap().iter().find(|t| t.id == id).cloned())
        }

        async fn update_team(&self, id: &str, params: &UpdateTeamParams) -> Result<(), DbError> {
            let mut teams = self.teams.lock().unwrap();
            let team = teams
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or_else(|| DbError::NotFound(id.to_owned()))?;
            if let Some(ref name) = params.name {
                team.name = name.clone();
            }
            if let Some(ref workspace) = params.workspace {
                team.workspace = workspace.clone();
            }
            if let Some(ref agents) = params.agents {
                team.agents = agents.clone();
            }
            if let Some(ref lead_id) = params.lead_agent_id {
                team.lead_agent_id = Some(lead_id.clone());
            }
            team.updated_at = now_ms();
            Ok(())
        }

        async fn delete_team(&self, id: &str) -> Result<(), DbError> {
            self.teams.lock().unwrap().retain(|t| t.id != id);
            Ok(())
        }

        async fn write_message(&self, _row: &aionui_db::models::MailboxMessageRow) -> Result<(), DbError> {
            Ok(())
        }

        async fn read_unread_and_mark(
            &self,
            _team_id: &str,
            _to_agent_id: &str,
        ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
            Ok(vec![])
        }

        async fn peek_unread(
            &self,
            _team_id: &str,
            _to_agent_id: &str,
        ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
            Ok(vec![])
        }

        async fn mark_read_batch(&self, _ids: &[String]) -> Result<(), DbError> {
            Ok(())
        }

        async fn get_history(
            &self,
            _team_id: &str,
            _to_agent_id: &str,
            _limit: Option<i64>,
        ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
            Ok(vec![])
        }

        async fn delete_mailbox_by_team(&self, _team_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn create_task(&self, _row: &TeamTaskRow) -> Result<(), DbError> {
            Ok(())
        }

        async fn find_task_by_id(&self, _team_id: &str, _task_id: &str) -> Result<Option<TeamTaskRow>, DbError> {
            Ok(None)
        }

        async fn update_task(&self, _task_id: &str, _params: &aionui_db::UpdateTaskParams) -> Result<(), DbError> {
            Ok(())
        }

        async fn list_tasks(&self, _team_id: &str) -> Result<Vec<TeamTaskRow>, DbError> {
            Ok(vec![])
        }

        async fn append_to_blocks(&self, _task_id: &str, _blocked_task_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn remove_from_blocked_by(&self, _task_id: &str, _unblocked_task_id: &str) -> Result<(), DbError> {
            Ok(())
        }

        async fn delete_tasks_by_team(&self, _team_id: &str) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct FakeConversationPorts {
        repo: Arc<MockConversationRepo>,
        workspace_root: std::path::PathBuf,
    }

    impl FakeConversationPorts {
        fn new(repo: Arc<MockConversationRepo>) -> Self {
            Self {
                repo,
                workspace_root: std::env::temp_dir().join(format!(
                    "aionui-team-workspace-harness-{}",
                    aionui_common::generate_id()
                )),
            }
        }
    }

    #[async_trait]
    impl TeamConversationProvisioningPort for FakeConversationPorts {
        async fn create_team_conversation(
            &self,
            request: TeamConversationCreateRequest,
        ) -> Result<TeamConversationCreateResult, TeamError> {
            let id = aionui_common::generate_id();
            let workspace = request
                .extra
                .get("workspace")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let path = self.workspace_root.join("conversations").join(format!("acp-temp-{id}"));
                    std::fs::create_dir_all(&path).unwrap();
                    path.to_string_lossy().into_owned()
                });
            let mut extra = request.extra;
            extra["workspace"] = serde_json::Value::String(workspace.clone());
            self.repo
                .create(&ConversationRow {
                    id: id.clone(),
                    user_id: request.user_id,
                    name: request.name,
                    r#type: request.agent_type.serde_name().to_owned(),
                    pinned: false,
                    pinned_at: None,
                    source: None,
                    channel_chat_id: None,
                    extra: serde_json::to_string(&extra).unwrap(),
                    model: request
                        .top_level_model
                        .map(|model| serde_json::to_string(&model).expect("serialize provider model")),
                    status: Some("pending".into()),
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })
                .await?;
            Ok(TeamConversationCreateResult {
                conversation_id: id,
                workspace,
            })
        }

        async fn adopt_team_conversation(&self, _request: TeamConversationAdoptRequest) -> Result<(), TeamError> {
            Ok(())
        }

        async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, TeamError> {
            Ok(self.repo.get_extra(conversation_id).and_then(|extra| {
                extra
                    .get("workspace")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }))
        }

        async fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, TeamError> {
            let path = self
                .workspace_root
                .join("conversations")
                .join(format!("team-temp-{team_id}"));
            std::fs::create_dir_all(&path).unwrap();
            Ok(path.to_string_lossy().into_owned())
        }

        async fn patch_runtime_config(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), TeamError> {
            let mut extra = self
                .repo
                .get_extra(conversation_id)
                .unwrap_or_else(|| serde_json::json!({}));
            if let (Some(target), Some(source)) = (extra.as_object_mut(), patch.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            self.repo
                .update(
                    conversation_id,
                    &ConversationRowUpdate {
                        name: None,
                        model: None,
                        pinned: None,
                        pinned_at: None,
                        extra: Some(serde_json::to_string(&extra).unwrap()),
                        status: None,
                        updated_at: Some(now_ms()),
                    },
                )
                .await?;
            Ok(())
        }

        async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), TeamError> {
            self.patch_runtime_config(conversation_id, serde_json::json!({ "session_mode": mode }))
                .await
        }

        async fn warmup_agent_process(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _task_manager: &Arc<dyn IWorkerTaskManager>,
        ) -> Result<(), TeamError> {
            Ok(())
        }

        async fn delete_team_conversation(&self, _user_id: &str, _conversation_id: &str) -> Result<(), TeamError> {
            Ok(())
        }
    }

    #[async_trait]
    impl TeamProjectionMessageStore for FakeConversationPorts {
        fn mint_message_id(&self) -> String {
            aionui_common::generate_id()
        }

        async fn find_projected_message(
            &self,
            _conversation_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<MessageRow>, TeamError> {
            Ok(None)
        }

        async fn insert_projected_message(&self, _row: &MessageRow) -> Result<(), TeamError> {
            Ok(())
        }
    }

    #[async_trait]
    impl TeamConversationLookupPort for FakeConversationPorts {
        async fn lookup_team_binding_by_conversation(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<TeamConversationBindingLookup>, TeamError> {
            Ok(None)
        }
    }

    struct NullBroadcaster;

    impl EventBroadcaster for NullBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
    }

    struct NoopTaskManager;

    #[async_trait]
    impl IWorkerTaskManager for NoopTaskManager {
        fn get_task(&self, _conversation_id: &str) -> Option<aionui_ai_agent::AgentInstance> {
            None
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: aionui_ai_agent::types::BuildTaskOptions,
        ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
            Err(AgentError::Internal("workspace harness does not spawn agents".into()))
        }

        fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            Ok(())
        }

        fn kill_and_wait(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        async fn clear(&self) {}

        fn active_count(&self) -> usize {
            0
        }

        fn collect_idle(&self, _idle_threshold_ms: aionui_common::TimestampMs) -> Vec<String> {
            vec![]
        }
    }

    struct EmptyAgentMetadataRepo;

    #[async_trait]
    impl IAgentMetadataRepository for EmptyAgentMetadataRepo {
        async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
            Ok(vec![])
        }

        async fn get(&self, _id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn find_by_source_and_name(
            &self,
            _agent_source: &str,
            _name: &str,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn find_builtin_by_backend(&self, _backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }

        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<AgentMetadataRow>, DbError> {
            Ok(None)
        }

        async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
            Ok(false)
        }

        async fn delete(&self, _id: &str) -> Result<bool, DbError> {
            Ok(false)
        }
    }

    struct EmptyProviderRepo;

    #[async_trait]
    impl IProviderRepository for EmptyProviderRepo {
        async fn list(&self) -> Result<Vec<aionui_db::models::Provider>, DbError> {
            Ok(vec![])
        }

        async fn find_by_id(&self, _id: &str) -> Result<Option<aionui_db::models::Provider>, DbError> {
            Ok(None)
        }

        async fn create(
            &self,
            _params: aionui_db::CreateProviderParams<'_>,
        ) -> Result<aionui_db::models::Provider, DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }

        async fn update(
            &self,
            _id: &str,
            _params: aionui_db::UpdateProviderParams<'_>,
        ) -> Result<aionui_db::models::Provider, DbError> {
            Err(DbError::NotFound("not implemented".into()))
        }

        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }
    }

    struct NoopTurnPort;

    #[async_trait]
    impl AgentTurnExecutionPort for NoopTurnPort {
        async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
            if let Some(on_started) = request.on_started.as_ref() {
                on_started(AgentTurnStarted {
                    team_run_id: request.team_run_id.clone().expect("team run id"),
                    slot_id: request.slot_id.clone(),
                    role: request.role.clone(),
                    conversation_id: request.conversation_id.clone(),
                    turn_id: "turn-test".into(),
                })
                .await;
            }
            Ok(AgentTurnOutcome {
                conversation_id: request.conversation_id,
                turn_id: "turn-test".into(),
                status: AgentTurnStatus::Completed,
                runtime: None,
            })
        }
    }

    struct NoopCancellationPort;

    #[async_trait]
    impl AgentTurnCancellationPort for NoopCancellationPort {
        async fn cancel_agent_turn(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _turn_id: &str,
        ) -> Result<(), AgentTurnExecutionError> {
            Ok(())
        }
    }

    pub(crate) fn setup_with_factory_metadata_team_repo_and_conversation_repo() -> (
        Arc<TeamSessionService>,
        Arc<FullMockTeamRepo>,
        Arc<dyn IWorkerTaskManager>,
        Arc<MockConversationRepo>,
    ) {
        let team_repo = Arc::new(FullMockTeamRepo::new());
        let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
        let conv_repo = Arc::new(MockConversationRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
        let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
        let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
        let lookup_port: Arc<dyn TeamConversationLookupPort> = conversation_ports;
        let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
        let svc = TeamSessionService::new(
            team_repo_dyn,
            Arc::new(EmptyAgentMetadataRepo),
            Arc::new(EmptyProviderRepo),
            conversation_port,
            projection_store,
            lookup_port,
            broadcaster,
            task_manager.clone(),
            Arc::new(NoopTurnPort),
            Arc::new(NoopCancellationPort),
            Arc::new(std::path::PathBuf::from("/tmp/aioncore-test")),
            None,
        );
        (svc, team_repo, task_manager, conv_repo)
    }

    pub(crate) async fn force_team_workspace(repo: &Arc<FullMockTeamRepo>, team_id: &str, workspace: &str) {
        repo.update_team(
            team_id,
            &UpdateTeamParams {
                workspace: Some(workspace.to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("force workspace");
    }

    pub(crate) fn single_agent_team_request(name: &str) -> CreateTeamRequest {
        CreateTeamRequest {
            name: name.into(),
            agents: vec![aionui_api_types::TeamAgentInput {
                name: "Lead".into(),
                role: "lead".into(),
                backend: "acp".into(),
                model: "claude".into(),
                custom_agent_id: None,
                conversation_id: None,
            }],
            workspace: None,
        }
    }
}
