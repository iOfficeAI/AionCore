//! Regression for #946 through the real SessionAgentTask event pump.
//! Synthetic backend events are not claims about any CLI's wire protocol.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_ai_agent::{
    AgentStreamEvent, IAgentTask,
    protocol::events::{ToolCallEventData, ToolCallStatus},
    session_agent::SessionAgentTask,
};
use aionui_common::AgentType;
use aionui_session::{
    Admission, BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope, SessionEvent,
    ToolResultContent, TurnOutcome,
};
use futures_util::{StreamExt, stream::BoxStream};
use tokio::sync::{broadcast, mpsc, oneshot};

type Input = (SessionEnvelope, oneshot::Sender<()>);

struct ControlledBackend(Mutex<Option<mpsc::UnboundedReceiver<Input>>>);

#[async_trait::async_trait]
impl SessionBackend for ControlledBackend {
    async fn dispatch(&self, _: Command) -> Result<CommandReceipt, BackendError> {
        Ok(CommandReceipt {
            accepted: true,
            admission: Admission::NoTurn,
            turn_gen: 1,
        })
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let rx = self.0.lock().unwrap().take().unwrap();
        // Acknowledge on the NEXT poll: the real pump has handled the preceding
        // event by then. Tests never need sleeps or assumptions about scheduling.
        futures_util::stream::unfold((rx, None::<oneshot::Sender<()>>), |(mut rx, ack)| async move {
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
            let (event, ack) = rx.recv().await?;
            Some((event, (rx, Some(ack))))
        })
        .boxed()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}

struct Session {
    task: Arc<SessionAgentTask>,
    tx: mpsc::UnboundedSender<Input>,
    rx: broadcast::Receiver<AgentStreamEvent>,
}

impl Session {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-preview".into(),
            "user-preview".into(),
            "/synthetic-workspace".into(),
            Arc::new(ControlledBackend(Mutex::new(Some(rx)))),
            None,
        );
        let rx = task.subscribe();
        Self { task, tx, rx }
    }

    async fn send(&self, event: SessionEvent) {
        self.send_gen(1, event).await;
    }

    async fn send_gen(&self, turn_gen: u64, event: SessionEvent) {
        let (ack, processed) = oneshot::channel();
        self.tx
            .send((
                SessionEnvelope {
                    session_id: "conv-preview".into(),
                    turn_gen,
                    event,
                },
                ack,
            ))
            .unwrap();
        processed.await.unwrap();
    }

    async fn delta(&self, text: &str) {
        self.delta_for("call-preview", text).await;
    }

    async fn delta_for(&self, id: &str, text: &str) {
        self.send(SessionEvent::ToolOutputDelta {
            item_id: id.into(),
            text: text.into(),
        })
        .await;
    }

    async fn call(&self, input: serde_json::Value) {
        self.send(SessionEvent::ToolCall {
            tool_use_id: "call-preview".into(),
            name: "synthetic-tool".into(),
            subagent: Default::default(),
            input,
            parent_tool_use_id: Some("parent-tool".into()),
        })
        .await;
    }

    async fn result(&self, output: &str, is_error: bool) {
        self.send(SessionEvent::ToolResult {
            tool_use_id: "call-preview".into(),
            is_error,
            content: vec![ToolResultContent::Text(output.into())],
            parent_tool_use_id: Some("parent-tool".into()),
        })
        .await;
    }
}

fn end(outcome: TurnOutcome, is_error: bool) -> SessionEvent {
    SessionEvent::TurnResult {
        is_error,
        api_error_status: None,
        result_text: if is_error {
            "synthetic failure".into()
        } else {
            String::new()
        },
        epoch: 0,
        outcome,
    }
}

fn drain(rx: &mut broadcast::Receiver<AgentStreamEvent>) -> Vec<AgentStreamEvent> {
    let mut frames = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(frame) => frames.push(frame),
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => return frames,
            Err(err) => panic!("events must not be lost: {err}"),
        }
    }
}

fn outputs(frames: &[AgentStreamEvent]) -> Vec<&ToolCallEventData> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            AgentStreamEvent::ToolCall(data) if data.output.is_some() => Some(data),
            _ => None,
        })
        .collect()
}

async fn tick() {
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_reader_retains_at_most_one_mib_of_previews() {
    let mut session = Session::new();
    for _ in 0..1024 {
        session.delta(&"x".repeat(256)).await;
        tick().await;
    }
    let mut bytes = 0;
    let mut frames = 0;
    let mut lagged = 0;
    loop {
        match session.rx.try_recv() {
            Ok(AgentStreamEvent::ToolCall(data)) => {
                bytes += data.output.as_deref().map_or(0, str::len);
                frames += 1;
            }
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Lagged(n)) => lagged += n,
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(err) => panic!("unexpected closed session: {err}"),
        }
    }
    eprintln!("256 KiB output: retained_preview_bytes={bytes}, frames={frames}, lagged={lagged}");
    assert!(bytes <= 1024 * 1024, "unbounded cumulative copies: {bytes} bytes");
    assert_eq!(lagged, 0, "output previews must not overrun a stalled reader");
    assert!(frames > 0, "must exercise actual preview delivery");
    session.delta("LATEST-AFTER-BACKPRESSURE").await;
    tick().await;
    let resumed = drain(&mut session.rx);
    let resumed = outputs(&resumed);
    assert_eq!(resumed.len(), 1);
    assert!(
        resumed[0]
            .output
            .as_ref()
            .unwrap()
            .ends_with("LATEST-AFTER-BACKPRESSURE")
    );
    assert!(resumed[0].output.as_ref().unwrap().len() <= 64 * 1024);
    drop(session.task);
}

#[tokio::test(start_paused = true)]
async fn small_outputs_are_cumulative_but_coalesced_and_do_not_catch_up() {
    let mut session = Session::new();
    session.delta("first\n").await;
    session.delta("second\n").await;
    assert!(drain(&mut session.rx).is_empty());
    tick().await;
    let frames = drain(&mut session.rx);
    assert_eq!(outputs(&frames)[0].output.as_deref(), Some("first\nsecond\n"));

    session.delta("third\n").await;
    tokio::time::advance(Duration::from_millis(99)).await;
    tokio::task::yield_now().await;
    assert!(drain(&mut session.rx).is_empty());
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    let frames = drain(&mut session.rx);
    assert_eq!(outputs(&frames).len(), 1);
    assert_eq!(outputs(&frames)[0].output.as_deref(), Some("first\nsecond\nthird\n"));
    session.delta("fourth\n").await;
    tokio::time::advance(Duration::from_millis(99)).await;
    tokio::task::yield_now().await;
    assert!(drain(&mut session.rx).is_empty(), "no catch-up burst after stall");
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let frames = drain(&mut session.rx);
    assert_eq!(
        outputs(&frames)[0].output.as_deref(),
        Some("first\nsecond\nthird\nfourth\n")
    );
}

#[tokio::test(start_paused = true)]
async fn hot_tool_does_not_starve_other_pending_tools() {
    let mut session = Session::new();
    for id in ["a", "b", "c"] {
        session.delta_for(id, id).await;
    }
    for expected in ["a", "b", "c"] {
        tick().await;
        let frames = drain(&mut session.rx);
        let previews = outputs(&frames);
        assert_eq!(previews.len(), 1, "only one preview per session per tick");
        assert_eq!(previews[0].call_id, expected);
        for _ in 0..100 {
            session.delta_for("a", "hot").await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn huge_single_line_and_unicode_are_bounded_in_real_pump() {
    let mut session = Session::new();
    let text = format!("{}END", "é🙂".repeat(1024 * 1024));
    session.delta(&text).await;
    tick().await;
    let frames = drain(&mut session.rx);
    let previews = outputs(&frames);
    assert_eq!(previews.len(), 1);
    let output = previews[0].output.as_ref().unwrap();
    assert!(output.len() <= 64 * 1024);
    assert!(output.starts_with("[Live preview truncated;"));
    assert!(output.ends_with("END"));
    assert!(text.ends_with(output.split_once('\n').unwrap().1));
}

#[tokio::test(start_paused = true)]
async fn final_result_bypasses_preview_pressure_and_is_never_overwritten() {
    for is_error in [false, true] {
        let mut session = Session::new();
        session.call(serde_json::json!({"synthetic": true})).await;
        for _ in 0..100 {
            session.delta(&"x".repeat(8192)).await;
            tick().await;
        }
        let full = format!("{}FINAL", "f".repeat(256 * 1024));
        session.result(&full, is_error).await;
        session.send(end(TurnOutcome::EndTurn, false)).await;
        let frames = drain(&mut session.rx);
        let result = outputs(&frames).last().copied().unwrap();
        assert_eq!(result.output.as_deref(), Some(full.as_str()));
        assert_eq!(
            result.status,
            if is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Completed
            }
        );
        assert_eq!(result.name, "synthetic-tool");
        assert_eq!(result.parent_call_id.as_deref(), Some("parent-tool"));
        assert!(matches!(frames.last(), Some(AgentStreamEvent::Finish(_))));
        tick().await;
        assert!(drain(&mut session.rx).is_empty(), "pending preview removed at result");
    }
}

#[tokio::test(start_paused = true)]
async fn immediate_result_has_no_obsolete_running_preview() {
    let mut session = Session::new();
    session.delta("intermediate").await;
    session.result("authoritative", false).await;
    tick().await;
    let frames = drain(&mut session.rx);
    let results = outputs(&frames);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, ToolCallStatus::Completed);
    assert_eq!(results[0].output.as_deref(), Some("authoritative"));
}

#[tokio::test(start_paused = true)]
async fn cancellation_and_error_preserve_last_preview_before_turn_terminal() {
    for (outcome, is_error) in [
        (
            TurnOutcome::Cancelled {
                reason: aionui_session::CancelReason::UserCancel,
            },
            false,
        ),
        (TurnOutcome::EndTurn, true),
    ] {
        let mut session = Session::new();
        session.call(serde_json::Value::Null).await;
        session.delta(&format!("{}LATEST", "x".repeat(1024 * 1024))).await;
        session.send(end(outcome, is_error)).await;
        let frames = drain(&mut session.rx);
        let tool = outputs(&frames)[0];
        assert_eq!(tool.status, ToolCallStatus::Canceled);
        assert!(tool.output.as_ref().unwrap().ends_with("LATEST"));
        assert!(tool.output.as_ref().unwrap().len() <= 64 * 1024);
        assert!(matches!(
            frames.last(),
            Some(AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_))
        ));
        tick().await;
        assert!(drain(&mut session.rx).is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn stream_end_preserves_unsent_preview_as_canceled() {
    let session = Session::new();
    session.call(serde_json::Value::Null).await;
    session.delta("last output before stream closed").await;
    let Session { task, tx, mut rx } = session;
    drop(tx);
    tokio::task::yield_now().await;
    let frames = drain(&mut rx);
    let tool = outputs(&frames)[0];
    assert_eq!(tool.status, ToolCallStatus::Canceled);
    assert_eq!(tool.output.as_deref(), Some("last output before stream closed"));
    tick().await;
    assert!(drain(&mut rx).is_empty());
    drop(task);
}

#[tokio::test(start_paused = true)]
async fn detached_output_survives_clean_end_and_next_turn_until_its_result() {
    let mut session = Session::new();
    session.call(serde_json::json!({"source": "unifiedExecStartup"})).await;
    session.delta("before\n").await;
    session.send(end(TurnOutcome::EndTurn, false)).await;
    let frames = drain(&mut session.rx);
    assert!(outputs(&frames).is_empty(), "clean end must not cancel detached tool");
    session.send_gen(2, SessionEvent::TurnStarted { epoch: 0 }).await;
    session
        .send_gen(
            2,
            SessionEvent::ToolOutputDelta {
                item_id: "call-preview".into(),
                text: "after\n".into(),
            },
        )
        .await;
    tick().await;
    let frames = drain(&mut session.rx);
    assert_eq!(outputs(&frames)[0].output.as_deref(), Some("before\nafter\n"));
    session.result("FULL-DETACHED-RESULT", false).await;
    tick().await;
    let frames = drain(&mut session.rx);
    assert_eq!(outputs(&frames).len(), 1);
    assert_eq!(outputs(&frames)[0].status, ToolCallStatus::Completed);
}

#[tokio::test(start_paused = true)]
async fn new_generation_drops_non_detached_pending_preview() {
    let mut session = Session::new();
    session.delta("old turn").await;
    session.send_gen(2, SessionEvent::TurnStarted { epoch: 0 }).await;
    tick().await;
    assert!(outputs(&drain(&mut session.rx)).is_empty());
    session
        .send_gen(
            2,
            SessionEvent::ToolOutputDelta {
                item_id: "call-preview".into(),
                text: "new turn".into(),
            },
        )
        .await;
    tick().await;
    let frames = drain(&mut session.rx);
    assert_eq!(outputs(&frames)[0].output.as_deref(), Some("new turn"));
}

#[tokio::test(start_paused = true)]
async fn eight_sessions_five_mib_each_with_slow_readers() {
    let mut sessions: Vec<_> = (0..8).map(|_| Session::new()).collect();
    let chunk = "s".repeat(5120);
    let mut delivered_bytes = 0;
    let mut peak_queued_bytes = 0;
    for i in 0..1024 {
        for session in &sessions {
            session.delta(&chunk).await;
        }
        tick().await;
        // Each reader runs only every two seconds (20 producer ticks).
        if i % 20 == 19 || i == 1023 {
            let mut queued_bytes = 0;
            for session in &mut sessions {
                let frames = drain(&mut session.rx);
                let previews = outputs(&frames);
                let bytes: usize = previews.iter().map(|d| d.output.as_ref().unwrap().len()).sum();
                assert!(bytes <= 1024 * 1024);
                assert!(previews.iter().all(|d| d.output.as_ref().unwrap().len() <= 64 * 1024));
                queued_bytes += bytes;
            }
            peak_queued_bytes = peak_queued_bytes.max(queued_bytes);
            delivered_bytes += queued_bytes;
        }
    }
    assert!(delivered_bytes > 0);
    assert!(peak_queued_bytes <= 8 * 1024 * 1024);
    eprintln!(
        "8 sessions x 5 MiB: peak_retained_preview_bytes={peak_queued_bytes}, delivered_preview_bytes={delivered_bytes}"
    );
}

#[tokio::test]
async fn full_final_output_reaches_real_relay_websocket_event_and_sqlite() {
    use aionui_common::now_ms;
    use aionui_conversation::stream_relay::StreamRelay;
    use aionui_db::{
        IConversationRepository, MessagePageDirection, MessagePageParams, SqliteConversationRepository,
        init_database_memory, models::ConversationRow,
    };
    use aionui_realtime::BroadcastEventBus;

    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let now = now_ms();
    repo.create(&ConversationRow {
        id: "conv-preview".into(),
        user_id: "system_default_user".into(),
        name: "Synthetic preview regression".into(),
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
    let bus = Arc::new(BroadcastEventBus::new(64));
    let mut ws_rx = bus.subscribe();
    let mut session = Session::new();
    // Subscribe before any event; hold this reader while output is produced.
    let relay_rx = session.task.subscribe();
    session.call(serde_json::json!({"synthetic": true})).await;
    let full = format!("{}FINAL", "é🙂".repeat(100_000));
    session.delta(&full).await;
    // Here use real time: SQLite workers need to run independently of Tokio's
    // paused clock. Wait for an actual preview before adding the final result.
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), session.rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let AgentStreamEvent::ToolCall(data) = frame
            && data.output.is_some()
        {
            assert!(data.output.as_ref().unwrap().len() <= 64 * 1024);
            break;
        }
    }
    session.result(&full, false).await;
    session.send(end(TurnOutcome::EndTurn, false)).await;
    let relay = StreamRelay::new(
        "conv-preview".into(),
        "assistant-preview".into(),
        "turn-preview".into(),
        "system_default_user".into(),
        repo.clone(),
        bus,
    );
    tokio::time::timeout(Duration::from_secs(10), relay.consume(relay_rx))
        .await
        .unwrap();

    let messages = repo
        .list_messages_page(
            "system_default_user",
            "conv-preview",
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let row = messages.items.iter().find(|row| row.id == "call-preview").unwrap();
    assert_eq!(row.status.as_deref(), Some("finish"));
    let content: serde_json::Value = serde_json::from_str(&row.content).unwrap();
    assert_eq!(content["output"].as_str(), Some(full.as_str()));
    assert_eq!(content["status"], "completed");
    assert_eq!(content["name"], "synthetic-tool");
    assert_eq!(content["args"]["synthetic"], true);
    assert_eq!(content["parent_call_id"], "parent-tool");

    let mut saw_preview = false;
    let mut saw_final = false;
    while let Ok(frame) = ws_rx.try_recv() {
        // Existing response envelope: body carries the serialized ToolCall.
        let data = &frame.data["data"];
        if frame.data["type"] != "tool_call" {
            continue;
        }
        if data["status"] == "running"
            && let Some(text) = data["output"].as_str()
        {
            assert!(text.len() <= 64 * 1024);
            assert!(!saw_final, "no preview after authoritative final");
            saw_preview = true;
        }
        if data["status"] == "completed" {
            assert_eq!(data["output"].as_str(), Some(full.as_str()));
            saw_final = true;
        }
    }
    assert!(
        saw_preview && saw_final,
        "real relay must forward preview AND complete final"
    );
}
