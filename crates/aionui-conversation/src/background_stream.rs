//! Out-of-turn stream delivery for direct-CLI (Session) conversations.
//!
//! The per-turn [`StreamRelay`] breaks at the turn's Finish — but a claude
//! session keeps talking after that:
//!
//! - **CLI-initiated turns.** When a background task (bash / Task subagent /
//!   workflow) completes, the CLI starts an UNPROMPTED turn to report the result
//!   (live 2026-08-03: BackendBound → tool check → MessageDelta ~30s after the
//!   launch turn ended). With only per-turn relays, that entire report was
//!   dropped: no WebSocket frames, no DB rows — the conversation looked dead.
//! - **Progress-card refreshes.** A background task's card ticks and settles
//!   between turns. Un-persisted settles left the stored row `running` forever,
//!   so the View Steps spinner never stopped after a reload.
//!
//! One watcher per live Session instance closes both gaps. It subscribes to the
//! instance's broadcast channel permanently and acts ONLY while no user turn is
//! active (the per-turn relay owns everything else):
//!
//! - `WorkflowProgress` → forwarded + persisted inline (a card refresh is not a
//!   turn).
//! - Turn content (text / thinking / tool calls / permissions…) → an **orphan
//!   turn**: a standard [`StreamRelay`] with freshly minted msg/turn ids, fed
//!   until the unprompted turn's own Finish. Full parity — segments,
//!   persistence, `turn.completed` — because it IS the normal relay.

use std::sync::Arc;

use aionui_ai_agent::protocol::events::{AgentStreamEvent, WorkflowProgressData};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case};
use aionui_db::IConversationRepository;
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::runtime_persistence::RuntimePersistenceCoordinator;
use crate::runtime_state::ConversationRuntimeStateService;
use crate::service::ConversationService;
use crate::stream_persistence::StreamPersistenceAdapter;
use crate::stream_relay::StreamRelay;

/// An orphan turn with NO frame for this long is presumed abandoned (a stray
/// content frame with no terminal); the feeder closes and the relay finalizes
/// with what it has, so the watcher can never be wedged forever.
const ORPHAN_TURN_IDLE_MS: u64 = 180_000;

pub(crate) struct BackgroundStreamWatcher {
    pub conversation_id: String,
    pub user_id: String,
    pub repo: Arc<dyn IConversationRepository>,
    pub broadcaster: Arc<dyn EventBroadcaster>,
    pub persistence: RuntimePersistenceCoordinator,
    pub runtime_state: Arc<ConversationRuntimeStateService>,
    /// True for non-Session (ACP manager) instances: consume ONLY agent
    /// session titles and leave every other frame to the existing ACP
    /// delivery paths (orphan turns / card refreshes are Session semantics).
    pub title_only: bool,
}

impl BackgroundStreamWatcher {
    /// Is this frame the start of CLI-initiated turn CONTENT (as opposed to a
    /// card refresh or bookkeeping noise)?
    fn is_orphan_turn_content(ev: &AgentStreamEvent) -> bool {
        matches!(
            ev,
            AgentStreamEvent::Text(_)
                | AgentStreamEvent::Thinking(_)
                | AgentStreamEvent::ToolCall(_)
                | AgentStreamEvent::AcpToolCall(_)
                | AgentStreamEvent::ToolGroup(_)
                | AgentStreamEvent::Plan(_)
                | AgentStreamEvent::Permission(_)
                | AgentStreamEvent::AcpPermission(_)
                | AgentStreamEvent::Error(_)
        )
        // Deliberately absent: Tips. Tips are OUR pump-side diagnostics, never
        // how a CLI-initiated turn begins (those open with thinking/tool/text) —
        // a stray out-of-turn tip fabricating a whole orphan turn is exactly how
        // the duplicate-terminal ACP_EMPTY_TURN bubble reached the user. A tip
        // INSIDE a running orphan turn still flows: the feeder forwards
        // everything once a turn is open.
    }

    pub async fn run(self, mut rx: broadcast::Receiver<AgentStreamEvent>) {
        info!(
            conversation_id = %self.conversation_id,
            "background stream watcher started"
        );
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        conversation_id = %self.conversation_id,
                        lagged = n,
                        "background stream watcher lagged; frames dropped"
                    );
                    continue;
                }
                // Instance torn down (conversation deleted / process replaced).
                Err(broadcast::error::RecvError::Closed) => break,
            };
            // Agent session titles are consumed HERE, unconditionally — the
            // watcher is the SINGLE consumer for both backend families, and the
            // active-turn gate below must NOT apply:
            // - claude: the generate_session_title reply lands seconds AFTER the
            //   first turn's Finish (live 2026-08-04: TurnResult 07:21:33 →
            //   title frame 07:21:36) — the per-turn relay is already gone.
            // - ACP agents (pi/omp, live 2026-08-04): session_info_update fires
            //   at session-open (no turn yet) and ~1ms BEFORE the turn's Finish
            //   — racing the relay's exit. Only a gate-free persistent consumer
            //   catches both. apply_agent_title is guarded (name_source) and
            //   idempotent (same-title no-op), so timing is never harmful.
            if let AgentStreamEvent::AcpSessionInfo(payload) = &ev {
                if let Some(title) = payload
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    if let Err(e) = crate::service::apply_agent_title(
                        &self.repo,
                        &self.broadcaster,
                        &self.user_id,
                        &self.conversation_id,
                        title,
                    )
                    .await
                    {
                        warn!(
                            conversation_id = %self.conversation_id,
                            error = %e,
                            "agent session title apply failed (background)"
                        );
                    }
                } else {
                    tracing::debug!(
                        conversation_id = %self.conversation_id,
                        "session info frame without title ignored"
                    );
                }
                continue;
            }
            // Title-only watchers (ACP manager instances) stop here: orphan
            // turns and card refreshes are Session-instance semantics.
            if self.title_only {
                continue;
            }
            // While a user turn is active its own relay owns the stream.
            if self.runtime_state.active_turn_id_for(&self.conversation_id).is_some() {
                continue;
            }
            match &ev {
                AgentStreamEvent::WorkflowProgress(data) => self.handle_card_refresh(data).await,
                ev_ref if Self::is_orphan_turn_content(ev_ref) => {
                    self.run_orphan_turn(ev.clone(), &mut rx).await;
                }
                // Start/Finish strays, usage (the pump broadcasts usage itself),
                // internal signals — nothing to deliver.
                _ => {}
            }
        }
        info!(
            conversation_id = %self.conversation_id,
            "background stream watcher stopped (stream closed)"
        );
    }

    /// A between-turns card refresh: forward the two frames the frontend already
    /// renders, and persist them so a reload shows the truth (the un-persisted
    /// settle was exactly the "View Steps spinner never ends" bug).
    async fn handle_card_refresh(&self, data: &WorkflowProgressData) {
        if data.settle_only {
            // Update-only settle for a card the pump has no memory of (see the
            // field's docs): apply to an EXISTING stored row and forward, or
            // drop silently. Never insert — the same unknown-terminal shape
            // fires for workflow-internal refs that never had a row, and
            // inserting for those would conjure junk cards.
            let adapter = StreamPersistenceAdapter::new(
                self.user_id.clone(),
                self.conversation_id.clone(),
                String::new(),
                self.repo.clone(),
                Some(self.persistence.clone()),
            );
            if adapter.settle_tool_call_if_present(&data.card).await {
                self.forward_card_frames(data);
            }
            return;
        }
        self.forward_card_frames(data);
        let adapter = StreamPersistenceAdapter::new(
            self.user_id.clone(),
            self.conversation_id.clone(),
            // Only text segments read the adapter's msg_id; tool rows key by call_id.
            String::new(),
            self.repo.clone(),
            Some(self.persistence.clone()),
        );
        adapter.persist_tool_call(&data.card).await;
        if !data.agents.is_empty() {
            adapter.persist_tool_group(&data.agents).await;
        }
    }

    fn forward_card_frames(&self, data: &WorkflowProgressData) {
        for (kind, msg_id, body) in [
            ("tool_call", data.card.call_id.clone(), serde_json::to_value(&data.card)),
            (
                "tool_group",
                data.agents.first().map(|a| a.call_id.clone()).unwrap_or_default(),
                serde_json::to_value(&data.agents),
            ),
        ] {
            // A background-task card has no roster; an empty tool_group would
            // persist a junk row under a minted id.
            if kind == "tool_group" && data.agents.is_empty() {
                continue;
            }
            let mut body = match body {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %ErrorChain(&e), kind, "background stream: serialize failed");
                    continue;
                }
            };
            normalize_keys_to_snake_case(&mut body);
            self.broadcaster.broadcast(aionui_api_types::WebSocketMessage::new(
                "message.stream",
                json!({
                    "conversation_id": self.conversation_id,
                    "user_id": self.user_id,
                    "msg_id": msg_id,
                    "turn_id": "",
                    "type": kind,
                    "data": body,
                    "hidden": false,
                }),
            ));
        }
    }

    /// Run a CLI-initiated turn through a REGULAR relay so it gets everything a
    /// user turn gets: WS forwarding, text segments, persistence, and the
    /// `turn.completed` bookkeeping (the relay's default `complete_turn`).
    async fn run_orphan_turn(&self, first: AgentStreamEvent, rx: &mut broadcast::Receiver<AgentStreamEvent>) {
        let msg_id = ConversationService::mint_msg_id();
        let turn_id = ConversationService::mint_turn_id();
        info!(
            conversation_id = %self.conversation_id,
            turn_id = %turn_id,
            first_frame = frame_kind(&first),
            "background stream: CLI-initiated turn started"
        );
        let relay = StreamRelay::new(
            self.conversation_id.clone(),
            msg_id,
            turn_id.clone(),
            self.user_id.clone(),
            self.repo.clone(),
            self.broadcaster.clone(),
        )
        .with_runtime_state(Arc::clone(&self.runtime_state))
        .with_persistence(self.persistence.clone());

        let (feed_tx, feed_rx) = broadcast::channel::<AgentStreamEvent>(256);
        let mut relay_task = tokio::spawn(relay.consume(feed_rx));
        let _ = feed_tx.send(first);

        let idle = std::time::Duration::from_millis(ORPHAN_TURN_IDLE_MS);
        let outcome = loop {
            tokio::select! {
                joined = &mut relay_task => break joined,
                next = tokio::time::timeout(idle, rx.recv()) => match next {
                    Ok(Ok(ev)) => {
                        // A user turn starting mid-orphan-turn takes the stream
                        // over; stop feeding so two relays never double-process.
                        if self
                            .runtime_state
                            .active_turn_id_for(&self.conversation_id)
                            .is_some()
                        {
                            warn!(
                                conversation_id = %self.conversation_id,
                                turn_id = %turn_id,
                                "background stream: user turn started mid CLI-initiated turn; closing orphan feed"
                            );
                            drop(feed_tx);
                            break relay_task.await;
                        }
                        let _ = feed_tx.send(ev);
                    }
                    // Source closed (instance torn down) or idle too long: close
                    // the feed so the relay finalizes with what it has.
                    Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                        drop(feed_tx);
                        break relay_task.await;
                    }
                    Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                        warn!(lagged = n, "background stream: orphan turn feed lagged");
                        continue;
                    }
                },
            }
        };
        match outcome {
            Ok(outcome) => info!(
                conversation_id = %self.conversation_id,
                turn_id = %turn_id,
                terminal = ?outcome.terminal,
                "background stream: CLI-initiated turn finished"
            ),
            Err(e) => warn!(
                conversation_id = %self.conversation_id,
                turn_id = %turn_id,
                error = %e,
                "background stream: orphan relay task failed"
            ),
        }
    }
}

fn frame_kind(ev: &AgentStreamEvent) -> &'static str {
    match ev {
        AgentStreamEvent::Text(_) => "text",
        AgentStreamEvent::Thinking(_) => "thinking",
        AgentStreamEvent::ToolCall(_) => "tool_call",
        AgentStreamEvent::AcpToolCall(_) => "acp_tool_call",
        AgentStreamEvent::ToolGroup(_) => "tool_group",
        AgentStreamEvent::Plan(_) => "plan",
        AgentStreamEvent::Permission(_) => "permission",
        AgentStreamEvent::AcpPermission(_) => "acp_permission",
        AgentStreamEvent::Tips(_) => "tips",
        AgentStreamEvent::Error(_) => "error",
        _ => "other",
    }
}

/// Handle for one spawned watcher, used to detect instance replacement.
pub(crate) struct BackgroundWatcherHandle {
    pub instance_ptr: usize,
    pub join: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::protocol::events::tool_call::{
        ToolCallEventData, ToolCallStatus, ToolGroupEntry, ToolGroupStatus,
    };
    use aionui_ai_agent::protocol::events::{FinishEventData, TextEventData};
    use aionui_common::now_ms;
    use aionui_db::models::ConversationRow;
    use aionui_db::{
        IConversationRepository, IUserRepository, SqliteConversationRepository, SqliteUserRepository,
        init_database_memory,
    };

    struct Rig {
        user_id: String,
        repo: Arc<SqliteConversationRepository>,
        bus: Arc<aionui_realtime::BroadcastEventBus>,
        runtime_state: Arc<ConversationRuntimeStateService>,
        tx: broadcast::Sender<AgentStreamEvent>,
        _watcher: tokio::task::JoinHandle<()>,
    }

    async fn rig() -> Rig {
        rig_with(false).await
    }

    async fn rig_with(title_only: bool) -> Rig {
        let db = init_database_memory().await.unwrap();
        let user_repo = SqliteUserRepository::new(db.pool().clone());
        let user = user_repo.create_user("user-1", "hash").await.unwrap();
        let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        repo.create(&ConversationRow {
            id: "conv-1".into(),
            user_id: user.id.clone(),
            name: "test".into(),
            r#type: "claude".into(),
            extra: "{}".into(),
            model: None,
            status: Some("running".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: now_ms(),
            updated_at: now_ms(),
            project_id: None,
            folder_id: None,
            name_source: None,
        })
        .await
        .unwrap();
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());
        let (tx, _) = broadcast::channel(64);
        let watcher = BackgroundStreamWatcher {
            conversation_id: "conv-1".into(),
            user_id: user.id.clone(),
            repo: repo.clone(),
            broadcaster: bus.clone(),
            persistence: RuntimePersistenceCoordinator::new(Arc::clone(&runtime_state)),
            runtime_state: Arc::clone(&runtime_state),
            title_only,
        };
        let handle = tokio::spawn(watcher.run(tx.subscribe()));
        Rig {
            user_id: user.id,
            repo,
            bus,
            runtime_state,
            tx,
            _watcher: handle,
        }
    }

    async fn rows_of_type(rig: &Rig, ty: &str) -> Vec<aionui_db::models::MessageRow> {
        rig.repo
            .list_messages_page(
                &rig.user_id,
                "conv-1",
                &aionui_db::MessagePageParams {
                    limit: 100,
                    direction: aionui_db::MessagePageDirection::InitialLatest,
                },
            )
            .await
            .unwrap()
            .items
            .into_iter()
            .filter(|m| m.r#type == ty)
            .collect()
    }

    /// Poll until `check` yields Some or ~2s passes.
    async fn eventually<T>(mut check: impl AsyncFnMut() -> Option<T>) -> Option<T> {
        for _ in 0..40 {
            if let Some(v) = check().await {
                return Some(v);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        None
    }

    fn settled_card_with(call_id: &str, status: ToolCallStatus) -> AgentStreamEvent {
        AgentStreamEvent::WorkflowProgress(WorkflowProgressData {
            card: ToolCallEventData {
                call_id: call_id.into(),
                name: "Bash".into(),
                args: serde_json::json!({"command": "sleep 30"}),
                status,
                input: None,
                output: None,
                description: Some("sleep 30 · bg task b1 · 00:01".into()),
            },
            agents: vec![],
            settle_only: false,
        })
    }

    fn settled_card() -> AgentStreamEvent {
        AgentStreamEvent::WorkflowProgress(WorkflowProgressData {
            card: ToolCallEventData {
                call_id: "toolu_bg".into(),
                name: "Bash".into(),
                args: serde_json::json!({"command": "sleep 30"}),
                status: ToolCallStatus::Completed,
                input: None,
                output: None,
                description: Some("sleep 30 · bg task b1 · 00:30".into()),
            },
            agents: vec![ToolGroupEntry {
                call_id: "toolu_bg:1".into(),
                name: "run:A".into(),
                status: ToolGroupStatus::Success,
                description: None,
            }],
            settle_only: false,
        })
    }

    /// The spinner bug: an out-of-turn card settle used to reach only the live
    /// WebSocket. After a reload the stored row still said `running`, so the
    /// View Steps spinner never ended. The watcher must PERSIST the settle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn out_of_turn_card_settle_is_persisted_and_forwarded() {
        let rig = rig().await;
        let mut ws = rig.bus.subscribe();
        rig.tx.send(settled_card()).unwrap();

        let row = eventually(async || {
            rows_of_type(&rig, "tool_call")
                .await
                .into_iter()
                .find(|m| m.id == "toolu_bg")
        })
        .await
        .expect("the settled card must be persisted");
        assert_eq!(row.status.as_deref(), Some("finish"), "settled → finish, spinner ends");
        assert!(row.content.contains("00:30"), "latest headline stored: {}", row.content);

        let group = rows_of_type(&rig, "tool_group").await;
        assert_eq!(group.len(), 1, "agent rows persisted too");
        assert_eq!(group[0].id, "toolu_bg:1");
        assert_eq!(group[0].status.as_deref(), Some("finish"));

        // And the live view got both frames.
        let mut kinds = Vec::new();
        while let Ok(evt) = ws.try_recv() {
            if evt.name == "message.stream" {
                kinds.push(evt.data["type"].as_str().unwrap_or("").to_owned());
            }
        }
        assert!(kinds.contains(&"tool_call".to_owned()), "got {kinds:?}");
        assert!(kinds.contains(&"tool_group".to_owned()), "got {kinds:?}");
    }

    /// The dead-conversation bug: after a background task completes, the CLI
    /// starts an unprompted turn to report the result — previously dropped
    /// wholesale. The watcher must run it through a REAL relay: text persisted,
    /// forwarded, and the turn completed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_initiated_turn_is_delivered_persisted_and_completed() {
        let rig = rig().await;
        let mut ws = rig.bus.subscribe();
        rig.tx
            .send(AgentStreamEvent::Text(TextEventData {
                content: "BG_DONE — the sleep finished.".into(),
            }))
            .unwrap();
        rig.tx
            .send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();

        let row = eventually(async || rows_of_type(&rig, "text").await.into_iter().next())
            .await
            .expect("the report text must be persisted");
        assert!(row.content.contains("BG_DONE"), "stored: {}", row.content);

        // Live view saw the content, and the turn was properly closed out.
        let mut saw_content = false;
        let mut saw_completed = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !(saw_content && saw_completed) {
            match tokio::time::timeout(std::time::Duration::from_millis(200), ws.recv()).await {
                Ok(Ok(evt)) => {
                    saw_content |= evt.name == "message.stream" && evt.data["type"] == "content";
                    saw_completed |= evt.name == "turn.completed";
                }
                _ => break,
            }
        }
        assert!(saw_content, "the report streams to the live view");
        assert!(saw_completed, "the orphan turn is book-kept like a real turn");
    }

    /// A `settle_only` frame updates an EXISTING row and forwards; for an
    /// unknown row it is dropped silently — never inserted, never forwarded
    /// (the same unknown-terminal shape fires for workflow-internal refs that
    /// never had a row; inserting would conjure junk cards).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settle_only_updates_existing_rows_and_drops_unknown_ones() {
        let rig = rig().await;
        // Seed a live card row the normal way.
        rig.tx
            .send(settled_card_with("toolu_seed", ToolCallStatus::Running))
            .unwrap();
        eventually(async || {
            rows_of_type(&rig, "tool_call")
                .await
                .into_iter()
                .find(|m| m.id == "toolu_seed" && m.status.as_deref() == Some("work"))
        })
        .await
        .expect("seed row persisted as running");

        let mut ws = rig.bus.subscribe();
        // Status-only settle for the seeded row…
        rig.tx
            .send(AgentStreamEvent::WorkflowProgress(WorkflowProgressData {
                card: ToolCallEventData {
                    call_id: "toolu_seed".into(),
                    name: String::new(),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Canceled,
                    input: None,
                    output: None,
                    description: None,
                },
                agents: vec![],
                settle_only: true,
            }))
            .unwrap();
        // …and one for a row that never existed.
        rig.tx
            .send(AgentStreamEvent::WorkflowProgress(WorkflowProgressData {
                card: ToolCallEventData {
                    call_id: "toolu_ghost".into(),
                    name: String::new(),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Canceled,
                    input: None,
                    output: None,
                    description: None,
                },
                agents: vec![],
                settle_only: true,
            }))
            .unwrap();

        let row = eventually(async || {
            rows_of_type(&rig, "tool_call")
                .await
                .into_iter()
                .find(|m| m.id == "toolu_seed" && m.status.as_deref() == Some("finish"))
        })
        .await
        .expect("the existing row must settle to finish");
        // Merge-patch kept the row's own identity: name survives a status-only frame.
        assert!(row.content.contains("\"name\":\"Bash\""), "name kept: {}", row.content);

        // The ghost never materialized — no row, no WS frame.
        assert!(
            !rows_of_type(&rig, "tool_call")
                .await
                .iter()
                .any(|m| m.id == "toolu_ghost"),
            "a settle-only frame must never insert"
        );
        let mut ghost_forwarded = false;
        while let Ok(evt) = ws.try_recv() {
            if evt.name == "message.stream" && evt.data["data"]["call_id"] == "toolu_ghost" {
                ghost_forwarded = true;
            }
        }
        assert!(
            !ghost_forwarded,
            "a settle-only frame for an unknown row must not reach the UI"
        );
    }

    /// While a USER turn is active, its own relay owns every frame — the watcher
    /// must not double-deliver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frames_during_an_active_turn_are_left_to_the_turn_relay() {
        let rig = rig().await;
        let claim = rig
            .runtime_state
            .try_claim_turn("conv-1", "turn-user")
            .expect("claimed");
        rig.tx
            .send(AgentStreamEvent::Text(TextEventData {
                content: "in-turn text the relay owns".into(),
            }))
            .unwrap();
        rig.tx
            .send(AgentStreamEvent::Finish(FinishEventData::default()))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            rows_of_type(&rig, "text").await.is_empty(),
            "the watcher must stay out of an active turn"
        );
        drop(claim);
    }

    /// A title-only (ACP manager) watcher applies titles even while a user
    /// turn is ACTIVE — pi/omp fire session_info_update ~1ms before the turn's
    /// Finish, racing the per-turn relay's exit; the gate-free consumer is the
    /// only one that reliably sees it. Non-title frames must stay untouched
    /// (no orphan turns for ACP instances).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn title_only_watcher_applies_title_even_during_active_turn() {
        let rig = rig_with(true).await;
        let claim = rig
            .runtime_state
            .try_claim_turn("conv-1", "turn-user")
            .expect("claimed");
        rig.tx
            .send(AgentStreamEvent::AcpSessionInfo(serde_json::json!({
                "title": "创建带时间戳的JSON文件"
            })))
            .unwrap();
        rig.tx
            .send(AgentStreamEvent::Text(TextEventData {
                content: "acp content the manager path owns".into(),
            }))
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let row = rig.repo.get(&rig.user_id, "conv-1").await.unwrap().unwrap();
            if row.name == "创建带时间戳的JSON文件" {
                assert_eq!(row.name_source.as_deref(), Some("agent"));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "title-only watcher must apply mid-turn titles, name still {:?}",
                row.name
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // The stray text frame must NOT spawn an orphan turn on the ACP path.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            rows_of_type(&rig, "text").await.is_empty(),
            "title-only watcher must never deliver content frames"
        );
        drop(claim);
    }

    /// The claude generate_session_title reply lands seconds AFTER the first
    /// turn's Finish (live 2026-08-04), between turns — the watcher must apply
    /// it (guarded rename + nameUpdated broadcast), since the per-turn relay is
    /// already gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn between_turns_agent_title_renames_and_broadcasts() {
        let rig = rig().await;
        let mut ws = rig.bus.subscribe();
        rig.tx
            .send(AgentStreamEvent::AcpSessionInfo(serde_json::json!({
                "title": "Fix login bug"
            })))
            .unwrap();

        // Poll until the watcher applies the rename (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let row = rig.repo.get(&rig.user_id, "conv-1").await.unwrap().unwrap();
            if row.name == "Fix login bug" {
                assert_eq!(row.name_source.as_deref(), Some("agent"));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not apply the between-turns title, name still {:?}",
                row.name
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let mut saw_name_updated = false;
        while let Ok(msg) = ws.try_recv() {
            if msg.name == "conversation.nameUpdated" {
                saw_name_updated = true;
                assert_eq!(msg.data["conversation_id"], "conv-1");
                assert_eq!(msg.data["name"], "Fix login bug");
            }
        }
        assert!(saw_name_updated, "nameUpdated must be broadcast");
    }
}
