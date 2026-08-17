//! T7/T8 (spec §10.1/§10.5/§10.6, plan §5): conversation-layer convergence of a
//! `UserCancelTimeout` force-kill on the direct-CLI (`SessionAgentTask`) path.
//!
//! Builds a REAL `AgentInstance::Session` over a never-ending fake `SessionBackend`
//! (the turn never finishes on its own — the ELECTRON-3RW window). Cancelling the
//! turn then force-killing it must (T7) drive the real `StreamRelay` to a CLEAN
//! `Finish` terminal and let the runtime state converge back to `Idle`
//! (`can_send_message=true`, cancelling cleared) WITHOUT waiting for the backend,
//! and (T8) broadcast a `turn.completed` `WebSocketMessage` scoped to that
//! conversation (no cross-conversation leak).

use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::{AgentInstance, agent_task::IAgentTask, session_agent::SessionAgentTask};
use aionui_api_types::ConversationRuntimeStateKind;
use aionui_common::{AgentKillReason, AgentType, now_ms};
use aionui_conversation::{
    runtime_state::ConversationRuntimeStateService,
    stream_relay::{RelayTerminal, StreamRelay},
};
use aionui_db::{IConversationRepository, SqliteConversationRepository, init_database_memory, models::ConversationRow};
use aionui_realtime::BroadcastEventBus;
use aionui_session::{Admission, BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope};
use futures_util::stream::BoxStream;

/// A `SessionBackend` whose turn NEVER terminates on its own (`events()` is a
/// pending stream): only a force-kill's injected clean `Finish` can converge it.
struct PendingBackend;

#[async_trait::async_trait]
impl SessionBackend for PendingBackend {
    async fn dispatch(&self, c: Command) -> Result<CommandReceipt, BackendError> {
        let admission = match c {
            Command::Send { .. } => Admission::Started,
            _ => Admission::NoTurn,
        };
        Ok(CommandReceipt {
            accepted: true,
            admission,
            turn_gen: 1,
        })
    }
    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        use futures_util::StreamExt as _;
        futures_util::stream::pending().boxed()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}

async fn setup_repo() -> (Arc<SqliteConversationRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let now = now_ms();
    repo.create(&ConversationRow {
        id: "conv-1".into(),
        user_id: "system_default_user".into(),
        name: "Force-kill convergence".into(),
        r#type: "aionrs".into(),
        extra: "{}".into(),
        model: None,
        status: Some("running".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now,
        updated_at: now,
        project_id: None,
        folder_id: None,
        name_source: None,
    })
    .await
    .unwrap();
    (repo, db)
}

fn session_task() -> Arc<SessionAgentTask> {
    let backend: Arc<dyn SessionBackend> = Arc::new(PendingBackend);
    SessionAgentTask::new(
        AgentType::Acp,
        "conv-1".into(),
        "user-1".into(),
        "/w".into(),
        backend,
        None,
    )
}

/// T7: a cancelling, in-flight Session turn is gated (`can_send_message=false`);
/// after `UserCancelTimeout` force-kill drives a clean relay `Finish` and the
/// turn claim is released (mirroring `turn_orchestrator` post-relay release), the
/// runtime summary converges to `Idle` — the gate recovers without the backend
/// ever finishing naturally.
#[tokio::test]
async fn user_cancel_kill_converges_session_turn_to_idle() {
    let (repo, _db) = setup_repo().await;
    let bus = Arc::new(BroadcastEventBus::new(64));

    let task = session_task();
    let inst = AgentInstance::Session(Arc::clone(&task));

    // Claim the turn + mark cancelling → the stuck, gated state (log E4–E8).
    let runtime = Arc::new(ConversationRuntimeStateService::default());
    let mut claim = runtime.try_claim_turn("conv-1", "turn-1").expect("turn claim");
    runtime.mark_cancelling("conv-1");
    let before = runtime.summary_from_parts("conv-1", IAgentTask::status(task.as_ref()), true, 0, false);
    assert_eq!(
        before.state,
        ConversationRuntimeStateKind::Cancelling,
        "an in-flight, cancelling turn is gated before the kill"
    );
    assert!(!before.can_send_message, "gate closed while cancelling");

    // Relay subscribes BEFORE the kill; the kill's clean Finish drives it terminal.
    let rx = IAgentTask::subscribe(task.as_ref());
    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "system_default_user".into(),
        repo.clone(),
        bus.clone(),
    );
    let relay_handle = tokio::spawn(relay.consume(rx));

    // Force-kill: converges WITHOUT waiting for the never-ending backend.
    inst.kill_and_wait(Some(AgentKillReason::UserCancelTimeout)).await;

    let outcome = tokio::time::timeout(Duration::from_secs(2), relay_handle)
        .await
        .expect("relay terminates promptly after kill")
        .expect("relay task did not panic");
    assert_eq!(
        outcome.terminal,
        RelayTerminal::Finish,
        "the kill must drive a CLEAN Finish terminal, not a crash Error"
    );

    // Mirror the orchestrator's post-relay release (turn_orchestrator.rs:517).
    claim.release_for_turn("turn-1");

    assert!(!runtime.is_claimed("conv-1"), "turn claim released");
    assert!(!runtime.is_cancelling("conv-1"), "cancelling cleared on release");
    let after = runtime.summary_from_parts("conv-1", IAgentTask::status(task.as_ref()), false, 0, false);
    assert_eq!(
        after.state,
        ConversationRuntimeStateKind::Idle,
        "runtime converges back to Idle after the forced release"
    );
    assert!(after.can_send_message, "gate recovered — user can send again");
}

/// T8: on the forced convergence, the relay broadcasts a `turn.completed`
/// `WebSocketMessage<T>` whose payload is scoped to this conversation
/// (`conversation_id == conv-1`, `canSendMessage == true`) — the routing key a WS
/// fan-out uses, so no other conversation's subscriber is addressed.
#[tokio::test]
async fn kill_broadcasts_turn_completed_scoped_to_conversation() {
    let (repo, _db) = setup_repo().await;
    let bus = Arc::new(BroadcastEventBus::new(64));
    let mut ws_rx = bus.subscribe();

    let task = session_task();
    let inst = AgentInstance::Session(Arc::clone(&task));

    let rx = IAgentTask::subscribe(task.as_ref());
    let relay = StreamRelay::new(
        "conv-1".into(),
        "asst-1".into(),
        "turn-1".into(),
        "system_default_user".into(),
        repo.clone(),
        bus.clone(),
    );
    let relay_handle = tokio::spawn(relay.consume(rx));

    inst.kill_and_wait(Some(AgentKillReason::UserCancelTimeout)).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), relay_handle)
        .await
        .expect("relay terminates")
        .expect("relay task ok");

    // Find the turn.completed broadcast (drain everything the relay emitted).
    let mut completed = None;
    while let Ok(msg) = ws_rx.try_recv() {
        if msg.name == "turn.completed" {
            completed = Some(msg);
        }
    }
    let msg = completed.expect("a turn.completed WebSocketMessage was broadcast on convergence");

    // WebSocketMessage<T> shape (name + data) + conversation scoping.
    assert_eq!(msg.name, "turn.completed");
    assert_eq!(
        msg.data.get("conversation_id").and_then(|v| v.as_str()),
        Some("conv-1"),
        "completion is scoped to conv-1 — a different conversation's subscriber is not addressed"
    );
    assert_ne!(
        msg.data.get("conversation_id").and_then(|v| v.as_str()),
        Some("conv-2"),
        "no cross-conversation leak"
    );
    assert_eq!(
        msg.data.get("canSendMessage").and_then(|v| v.as_bool()),
        Some(true),
        "completion tells the frontend the gate reopened"
    );
}
