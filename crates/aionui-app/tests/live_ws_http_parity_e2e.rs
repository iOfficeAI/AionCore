//! Live WS↔HTTP parity e2e — spawns the REAL app and drives a REAL agent CLI
//! turn (claude / codex), then diffs what the WebSocket streamed against what
//! `GET /messages` returns after the turn.
//!
//! Origin: Slack C0BEMT26MBL p1785290726878679 — encrypted-thinking models made
//! the runtime (WS) view and the reload (HTTP) view diverge because empty
//! thinking segments were skipped at persist time.
//!
//! Requires `claude` / `codex` on PATH and provider credentials in the
//! environment, so the tests are `#[ignore]`d. Run explicitly:
//!
//! ```sh
//! cargo test -p aionui-app --test live_ws_http_parity_e2e -- --ignored --nocapture
//! ```

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_app::{AppConfig, AppServices, create_router};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tower::ServiceExt;

struct LiveApp {
    addr: SocketAddr,
    router: axum::Router,
    token: String,
    csrf: String,
}

async fn start_live_app() -> LiveApp {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let mut router = create_router(&services).await.expect("build router");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_router = router.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_router).await.unwrap();
    });

    let (token, csrf) = common::setup_and_login(&mut router, &services, "liveuser", "live-pass-123").await;
    LiveApp {
        addr,
        router,
        token,
        csrf,
    }
}

async fn http_json(app: &LiveApp, method: &str, uri: &str, body: Value) -> Value {
    let req = common::json_with_token(method, uri, body, &app.token, &app.csrf);
    let resp = app.router.clone().oneshot(req).await.unwrap();
    common::body_json(resp).await
}

async fn http_get(app: &LiveApp, uri: &str) -> Value {
    let req = common::get_with_token(uri, &app.token);
    let resp = app.router.clone().oneshot(req).await.unwrap();
    common::body_json(resp).await
}

/// Spawn a background reader that records every WS frame.
async fn connect_ws_recorder(addr: SocketAddr, token: &str) -> Arc<Mutex<Vec<Value>>> {
    let url = format!("ws://{addr}/ws");
    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (_sink, mut stream) = ws.split();

    let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_frames = frames.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let tungstenite::Message::Text(text) = msg
                && let Ok(v) = serde_json::from_str::<Value>(&text)
            {
                sink_frames.lock().unwrap().push(v);
            }
        }
    });
    frames
}

fn stream_frames_for<'a>(frames: &'a [Value], conv_id: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|f| f["name"] == "message.stream" && f["data"]["conversation_id"] == conv_id)
        .collect()
}

async fn run_backend_parity(backend: &str, prompt: &str) {
    let app = start_live_app().await;

    // Workspace with a known file for a read-only tool call.
    let ws_dir = std::env::temp_dir().join(format!("live-parity-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();
    std::fs::write(ws_dir.join("hello.txt"), "AION_PARITY_42\n").unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}
        }),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("conversation create failed: {created}"))
        .to_owned();
    println!("[{backend}] conversation {conv_id} workspace {}", ws_dir.display());

    let frames = connect_ws_recorder(app.addr, &app.token).await;

    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": prompt}),
    )
    .await;
    println!("[{backend}] send accepted: {}", sent["success"]);

    // Pump until the relay forwards the terminal finish/error frame.
    let started = Instant::now();
    let mut confirmed: BTreeSet<String> = BTreeSet::new();
    let mut terminal: Option<String> = None;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        for f in stream_frames_for(&snapshot, &conv_id) {
            let ftype = f["data"]["type"].as_str().unwrap_or("");
            if ftype == "finish" || ftype == "error" {
                terminal = Some(ftype.to_owned());
            }
            // Best-effort auto-approval so a permission request can't wedge the turn.
            // call_id contract = `tool_call.tool_call_id` (see auto_confirm_permissions).
            if ftype.contains("permission") {
                let call_id = f["data"]["data"]["tool_call"]["tool_call_id"]
                    .as_str()
                    .or_else(|| f["data"]["data"]["call_id"].as_str())
                    .or_else(|| f["data"]["data"]["request_id"].as_str())
                    .or_else(|| f["data"]["msg_id"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                if !call_id.is_empty() && confirmed.insert(call_id.clone()) {
                    let option = f["data"]["data"]["options"][0].clone();
                    println!("[{backend}] auto-confirming permission {call_id}: {option}");
                    let resp = http_json(
                        &app,
                        "POST",
                        &format!("/api/conversations/{conv_id}/confirmations/{call_id}/confirm"),
                        json!({
                            "msg_id": f["data"]["msg_id"],
                            "data": option.get("optionId").cloned().unwrap_or(option),
                        }),
                    )
                    .await;
                    println!("[{backend}] confirm response: {resp}");
                }
            }
        }
        if terminal.is_some() {
            break;
        }
    }
    let terminal = terminal.unwrap_or_else(|| panic!("[{backend}] turn did not reach finish/error within 300s"));
    println!("[{backend}] terminal frame: {terminal} after {:?}", started.elapsed());
    // Small grace period so trailing persists/frames land.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- WS-side aggregation ----
    let snapshot = frames.lock().unwrap().clone();
    let stream = stream_frames_for(&snapshot, &conv_id);
    println!("[{backend}] ---- frame trace ({} stream frames) ----", stream.len());
    for f in &stream {
        let d = &f["data"];
        let ftype = d["type"].as_str().unwrap_or("?");
        let brief = match ftype {
            "text" | "content" | "thinking" => format!("{}B", d["data"]["content"].as_str().unwrap_or("").len()),
            "error" | "tips" => d["data"].to_string(),
            _ => d["data"].to_string().chars().take(160).collect::<String>(),
        };
        println!("  [{ftype}] msg_id={} {brief}", d["msg_id"].as_str().unwrap_or("?"));
    }
    for f in &snapshot {
        if f["name"] != "message.stream" {
            println!(
                "  [event:{}] {}",
                f["name"].as_str().unwrap_or("?"),
                f["data"].to_string().chars().take(200).collect::<String>()
            );
        }
    }
    let mut ws_thinking: BTreeMap<String, String> = BTreeMap::new(); // msg_id → accumulated content
    let mut ws_thinking_done: BTreeSet<String> = BTreeSet::new();
    let mut ws_text: BTreeMap<String, String> = BTreeMap::new();
    let mut ws_tools: BTreeSet<String> = BTreeSet::new();
    for f in &stream {
        let d = &f["data"];
        let msg_id = d["msg_id"].as_str().unwrap_or("").to_owned();
        match d["type"].as_str().unwrap_or("") {
            "thinking" => {
                if d["data"]["status"] == "done" {
                    ws_thinking_done.insert(msg_id);
                } else {
                    ws_thinking
                        .entry(msg_id)
                        .or_default()
                        .push_str(d["data"]["content"].as_str().unwrap_or(""));
                }
            }
            "text" | "content" => {
                ws_text
                    .entry(msg_id)
                    .or_default()
                    .push_str(d["data"]["content"].as_str().unwrap_or(""));
            }
            "tool_call" => {
                if let Some(id) = d["data"]["call_id"].as_str() {
                    ws_tools.insert(id.to_owned());
                }
            }
            "acp_tool_call" => {
                if let Some(id) = d["data"]["update"]["tool_call_id"].as_str() {
                    ws_tools.insert(id.to_owned());
                }
            }
            _ => {}
        }
    }

    // ---- HTTP-side aggregation ----
    let listed = http_get(&app, &format!("/api/conversations/{conv_id}/messages?limit=200")).await;
    let items = listed["data"]["items"].as_array().cloned().unwrap_or_default();
    let mut http_thinking: BTreeMap<String, Value> = BTreeMap::new();
    let mut http_text: BTreeMap<String, String> = BTreeMap::new();
    let mut http_tools: BTreeSet<String> = BTreeSet::new();
    let mut http_other: Vec<String> = Vec::new();
    for m in &items {
        let msg_id = m["msg_id"].as_str().or(m["id"].as_str()).unwrap_or("").to_owned();
        match m["type"].as_str().unwrap_or("") {
            "thinking" => {
                http_thinking.insert(msg_id, m["content"].clone());
            }
            "text" => {
                if m["position"] != "right" && m["hidden"] != true {
                    http_text
                        .entry(msg_id)
                        .or_default()
                        .push_str(m["content"]["content"].as_str().unwrap_or(""));
                }
            }
            "tool_call" | "acp_tool_call" => {
                http_tools.insert(m["id"].as_str().unwrap_or("").to_owned());
            }
            other => http_other.push(other.to_owned()),
        }
    }

    // ---- Parity report ----
    println!("\n===== [{backend}] WS ↔ HTTP parity =====");
    println!(
        "WS   : thinking segments={} (done={}), text segments={}, tools={}",
        ws_thinking.len(),
        ws_thinking_done.len(),
        ws_text.len(),
        ws_tools.len()
    );
    println!(
        "HTTP : thinking rows={}, text rows={}, tool rows={}, other row types={:?}",
        http_thinking.len(),
        http_text.len(),
        http_tools.len(),
        http_other
    );

    let mut diffs: Vec<String> = Vec::new();
    for (msg_id, ws_content) in &ws_thinking {
        match http_thinking.get(msg_id) {
            None => diffs.push(format!("thinking segment {msg_id} streamed on WS but has NO HTTP row")),
            Some(row) => {
                let http_content = row["content"].as_str().unwrap_or("");
                if http_content != ws_content {
                    diffs.push(format!(
                        "thinking {msg_id} content mismatch: WS {}B vs HTTP {}B",
                        ws_content.len(),
                        http_content.len()
                    ));
                }
                if !row["duration_ms"].is_u64() {
                    diffs.push(format!("thinking {msg_id} HTTP row lacks duration_ms"));
                }
            }
        }
        println!(
            "  thinking {msg_id}: WS content {}B → HTTP row {}",
            ws_content.len(),
            if http_thinking.contains_key(msg_id) {
                "✅"
            } else {
                "❌"
            }
        );
    }
    for msg_id in http_thinking.keys() {
        if !ws_thinking.contains_key(msg_id) {
            diffs.push(format!("thinking row {msg_id} in HTTP but never streamed on WS"));
        }
    }

    let ws_text_all: String = ws_text.values().cloned().collect();
    let http_text_all: String = http_text.values().cloned().collect();
    println!(
        "  text: WS {} segments {}B vs HTTP {} rows {}B",
        ws_text.len(),
        ws_text_all.len(),
        http_text.len(),
        http_text_all.len()
    );
    if http_text_all.is_empty() && !ws_text_all.is_empty() {
        diffs.push("assistant text streamed on WS but absent from HTTP".into());
    }

    if ws_tools != http_tools {
        let ws_only: Vec<_> = ws_tools.difference(&http_tools).collect();
        let http_only: Vec<_> = http_tools.difference(&ws_tools).collect();
        diffs.push(format!("tool rows differ: WS-only={ws_only:?} HTTP-only={http_only:?}"));
    }
    println!("  tools: WS {:?} vs HTTP {:?}", ws_tools, http_tools);

    if diffs.is_empty() {
        println!("===== [{backend}] parity: ✅ no WS↔HTTP divergence =====\n");
    } else {
        println!("===== [{backend}] parity: ❌ {} divergences =====", diffs.len());
        for d in &diffs {
            println!("  - {d}");
        }
    }
    assert_eq!(terminal, "finish", "[{backend}] turn must complete cleanly");
    assert!(diffs.is_empty(), "[{backend}] WS↔HTTP divergences: {diffs:?}");
}

const PROMPT: &str = "Read the file hello.txt in this workspace using your file-reading tool, \
    then reply with its exact content and nothing else.";

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_ws_http_parity() {
    run_backend_parity("claude", PROMPT).await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_ws_http_parity() {
    run_backend_parity("codex", PROMPT).await;
}

/// The prompt shape the 2.1.220 interrupt probe proved launches a non-blocking
/// background workflow whose launch turn ends while the workflow flies (RUN A,
/// samples/claude-cli/2.1.220/_probe_workflow_interrupt.py) — exactly the state
/// where the pump suppresses the launch Finish and holds the turn open.
const WORKFLOW_PROMPT: &str = "用 Workflow 工具启动一个 workflow：一个 phase，一个 agent，\
    单纯执行 sleep 90（睡 90 秒）。用后台方式启动（run_in_background），\
    启动拿到 task id 后立刻回复我「已启动」，不要等待它完成。";

/// Auto-approve every permission frame in `snapshot` not already confirmed —
/// same best-effort logic as `run_backend_parity`, so a default-mode Workflow
/// launch (or its Bash) can't wedge the flight phase.
async fn auto_confirm_permissions(app: &LiveApp, conv_id: &str, snapshot: &[Value], confirmed: &mut BTreeSet<String>) {
    for f in stream_frames_for(snapshot, conv_id) {
        let ftype = f["data"]["type"].as_str().unwrap_or("");
        if !ftype.contains("permission") {
            continue;
        }
        // The confirm contract (MessageAcpPermission.tsx): call_id = the acp_permission
        // frame's `tool_call.tool_call_id` — the claude control_request id the backend
        // keyed the pending permission by. The msg_id fallback exists only for exotic
        // frames; answering with it yields "no pending permission" and wedges the turn.
        let call_id = f["data"]["data"]["tool_call"]["tool_call_id"]
            .as_str()
            .or_else(|| f["data"]["data"]["call_id"].as_str())
            .or_else(|| f["data"]["data"]["request_id"].as_str())
            .or_else(|| f["data"]["msg_id"].as_str())
            .unwrap_or_default()
            .to_owned();
        if call_id.is_empty() || !confirmed.insert(call_id.clone()) {
            continue;
        }
        let option = f["data"]["data"]["options"][0].clone();
        println!("[wf-cancel] auto-confirming permission {call_id}: {option}");
        let resp = http_json(
            app,
            "POST",
            &format!("/api/conversations/{conv_id}/confirmations/{call_id}/confirm"),
            json!({
                "msg_id": f["data"]["msg_id"],
                "data": option.get("optionId").cloned().unwrap_or(option),
            }),
        )
        .await;
        println!("[wf-cancel] confirm response: {resp}");
    }
}

/// LIVE guard for the OTHER side of the cancel-drain settlement branch: a
/// workflow left to complete NATURALLY must still deliver its completion
/// message and terminal Finish (`Completed` drain waits for the CLI's real
/// terminal result — settling there would break the relay before the
/// completion message lands), and the conversation must answer a follow-up.
/// Companion to `live_claude_workflow_cancel_recovers_conversation`.
#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_workflow_natural_completion_still_finishes() {
    // Pump-level diagnostics (suppression / roster / settlement) — the WS stream
    // drops SubagentUpdate, so the roster story is only visible in tracing.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "aionui_ai_agent=debug,aionui_session=info",
        ))
        .try_init();
    let app = start_live_app().await;

    let ws_dir = std::env::temp_dir().join(format!("live-wf-natural-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {"workspace": ws_dir.to_string_lossy(), "backend": "claude"}
        }),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("conversation create failed: {created}"))
        .to_owned();
    println!("[wf-natural] conversation {conv_id} workspace {}", ws_dir.display());

    let frames = connect_ws_recorder(app.addr, &app.token).await;

    // Short sleep so natural completion lands well inside the pump window.
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "用 Workflow 工具启动一个 workflow：一个 phase，一个 agent，\
            单纯执行 sleep 20（睡 20 秒）。用后台方式启动（run_in_background），\
            启动拿到 task id 后立刻回复我「已启动」，不要等待它完成。"}),
    )
    .await;
    assert!(sent["data"]["turn_id"].is_string(), "send failed: {sent}");

    let mut confirmed: BTreeSet<String> = BTreeSet::new();

    // Flight must establish first (suppression engaged): tool + text, no finish.
    let started = Instant::now();
    let (mut saw_text, mut saw_tool) = (false, false);
    while started.elapsed() < Duration::from_secs(150) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        auto_confirm_permissions(&app, &conv_id, &snapshot, &mut confirmed).await;
        for f in stream_frames_for(&snapshot, &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "text" | "content" => saw_text = true,
                "tool_call" | "acp_tool_call" => saw_tool = true,
                "finish" => panic!("[wf-natural] finish before flight established — suppression did not engage"),
                _ => {}
            }
        }
        if saw_text && saw_tool {
            break;
        }
    }
    assert!(
        saw_text && saw_tool,
        "[wf-natural] no workflow flight within 150s (text={saw_text} tool={saw_tool})"
    );
    let text_bytes_at_flight: usize = {
        let snapshot = frames.lock().unwrap().clone();
        stream_frames_for(&snapshot, &conv_id)
            .iter()
            .filter(|f| matches!(f["data"]["type"].as_str(), Some("text") | Some("content")))
            .map(|f| f["data"]["data"]["content"].as_str().unwrap_or("").len())
            .sum()
    };
    println!(
        "[wf-natural] flight established after {:?}; waiting for NATURAL completion",
        started.elapsed()
    );

    // No cancel: the workflow sleeps 20s, completes, and the CLI's terminal
    // result must close the turn (2.1.176 invariant, unbroken by the fix).
    let mut finished: Option<Duration> = None;
    while started.elapsed() < Duration::from_secs(240) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        auto_confirm_permissions(&app, &conv_id, &snapshot, &mut confirmed).await;
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            finished = Some(started.elapsed());
            break;
        }
    }
    if finished.is_none() {
        // Dump the full stream trace before failing so the wedge is diagnosable.
        let snapshot = frames.lock().unwrap().clone();
        for f in stream_frames_for(&snapshot, &conv_id) {
            let d = &f["data"];
            println!(
                "  [{}] msg_id={} {}",
                d["type"].as_str().unwrap_or("?"),
                d["msg_id"].as_str().unwrap_or("?"),
                d["data"].to_string().chars().take(160).collect::<String>()
            );
        }
        panic!("[wf-natural] workflow turn never finished naturally within 240s");
    }
    let finished = finished.unwrap();
    let text_bytes_at_end: usize = {
        let snapshot = frames.lock().unwrap().clone();
        stream_frames_for(&snapshot, &conv_id)
            .iter()
            .filter(|f| matches!(f["data"]["type"].as_str(), Some("text") | Some("content")))
            .map(|f| f["data"]["data"]["content"].as_str().unwrap_or("").len())
            .sum()
    };
    assert!(
        text_bytes_at_end > text_bytes_at_flight,
        "[wf-natural] no completion message streamed after the launch reply \
         ({text_bytes_at_flight}B → {text_bytes_at_end}B) — the relay closed too early"
    );
    println!(
        "[wf-natural] ✅ natural completion finished in {finished:?} (launch {text_bytes_at_flight}B → total {text_bytes_at_end}B)"
    );

    // Next turn must open normally (TurnStarted state reset is per-turn only).
    let pre = frames.lock().unwrap().len();
    let follow_at = Instant::now();
    let sent2 = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "只回两个字：在的"}),
    )
    .await;
    assert!(sent2["data"]["turn_id"].is_string(), "follow-up rejected: {sent2}");
    let mut follow_done = false;
    while follow_at.elapsed() < Duration::from_secs(90) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot[pre..], &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            follow_done = true;
            break;
        }
    }
    assert!(follow_done, "[wf-natural] follow-up turn did not finish within 90s");
    println!("[wf-natural] ✅ follow-up turn finished in {:?}", follow_at.elapsed());
}

/// LIVE regression test for ELECTRON-3RP/3RW: cancelling a turn held open by an
/// in-flight workflow must settle the suppressed launch Finish from the
/// `Interrupted` drain (seconds), NOT via the 15s UserCancelTimeout watchdog —
/// and the conversation must accept and answer a follow-up message afterwards.
/// Drives the app exactly as the frontend does: REST send → WS stream → REST
/// cancel (turn_id from the send response) → REST send again.
#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_workflow_cancel_recovers_conversation() {
    // Pump + backend diagnostics (suppression / roster / permission answers).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "aionui_ai_agent=debug,aionui_session=debug",
        ))
        .try_init();
    let app = start_live_app().await;

    let ws_dir = std::env::temp_dir().join(format!("live-wf-cancel-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {"workspace": ws_dir.to_string_lossy(), "backend": "claude"}
        }),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("conversation create failed: {created}"))
        .to_owned();
    println!("[wf-cancel] conversation {conv_id} workspace {}", ws_dir.display());

    let frames = connect_ws_recorder(app.addr, &app.token).await;

    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": WORKFLOW_PROMPT}),
    )
    .await;
    let turn_id = sent["data"]["turn_id"]
        .as_str()
        .unwrap_or_else(|| panic!("send failed: {sent}"))
        .to_owned();
    println!("[wf-cancel] workflow prompt sent, turn {turn_id}");

    let mut confirmed: BTreeSet<String> = BTreeSet::new();

    // ---- Phase 1: wait until the workflow flight is established ----
    // Evidence: a tool_call streamed (the Workflow launch) AND launch reply text
    // streamed, with NO finish — the turn is being held open by the suppressed
    // launch Finish. A finish here means no workflow launched (model variance /
    // launch failure) and the scenario precondition is not met.
    let started = Instant::now();
    let (mut saw_text, mut saw_tool) = (false, false);
    while started.elapsed() < Duration::from_secs(150) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        auto_confirm_permissions(&app, &conv_id, &snapshot, &mut confirmed).await;
        for f in stream_frames_for(&snapshot, &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "text" | "content" => saw_text = true,
                "tool_call" | "acp_tool_call" => saw_tool = true,
                "finish" => panic!(
                    "[wf-cancel] turn finished BEFORE cancel — the workflow did not hold the turn open \
                     (launch failed or model declined); scenario precondition not met"
                ),
                _ => {}
            }
        }
        if saw_text && saw_tool {
            break;
        }
    }
    assert!(
        saw_text && saw_tool,
        "[wf-cancel] no workflow flight within 150s (text={saw_text} tool={saw_tool})"
    );
    println!(
        "[wf-cancel] flight established after {:?}; consolidating 8s",
        started.elapsed()
    );
    // Let the workflow's sub-agent actually start (2.1.176: ~6s after the task),
    // still asserting the turn stays open.
    let consolidate = Instant::now();
    while consolidate.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        auto_confirm_permissions(&app, &conv_id, &snapshot, &mut confirmed).await;
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            panic!("[wf-cancel] turn finished during flight consolidation — precondition not met");
        }
    }

    // ---- Phase 2: cancel, exactly as the frontend does ----
    let pre_cancel = frames.lock().unwrap().len();
    let cancel_at = Instant::now();
    let resp = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({"turn_id": turn_id}),
    )
    .await;
    println!("[wf-cancel] cancel response: {resp}");
    assert_eq!(resp["success"], json!(true), "cancel must be accepted: {resp}");

    // The Interrupted drain must settle the owed Finish on the MAIN path. Before
    // the fix this frame only arrived via the 15s UserCancelTimeout force-kill;
    // the 12s deadline is deliberately INSIDE the watchdog window so a watchdog
    // rescue cannot masquerade as a pass.
    let mut settle: Option<Duration> = None;
    while cancel_at.elapsed() < Duration::from_secs(12) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot[pre_cancel..], &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            settle = Some(cancel_at.elapsed());
            break;
        }
    }
    let settle = settle.unwrap_or_else(|| {
        panic!("[wf-cancel] no finish within 12s of cancel — turn is wedged (watchdog would fire at 15s)")
    });
    println!("[wf-cancel] ✅ cancel settled the turn in {settle:?} (main path, not the 15s watchdog)");

    // ---- Phase 3: the conversation must answer a follow-up ----
    let pre_recovery = frames.lock().unwrap().len();
    let recovery_at = Instant::now();
    let sent2 = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "只回两个字：在的"}),
    )
    .await;
    assert!(
        sent2["data"]["turn_id"].is_string(),
        "follow-up send must be admitted (gate recovered): {sent2}"
    );
    let mut reply_text = String::new();
    let mut recovered: Option<Duration> = None;
    while recovery_at.elapsed() < Duration::from_secs(90) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        let mut finished = false;
        for f in stream_frames_for(&snapshot[pre_recovery..], &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "text" | "content" => reply_text.push_str(f["data"]["data"]["content"].as_str().unwrap_or("")),
                "finish" => finished = true,
                _ => {}
            }
        }
        if finished {
            recovered = Some(recovery_at.elapsed());
            break;
        }
        reply_text.clear(); // re-aggregated from the snapshot each round
    }
    let recovered =
        recovered.unwrap_or_else(|| panic!("[wf-cancel] follow-up turn did not finish within 90s — not recovered"));
    assert!(
        !reply_text.is_empty(),
        "[wf-cancel] follow-up finished but streamed no reply text"
    );
    println!(
        "[wf-cancel] ✅ follow-up answered in {recovered:?}: {:?}",
        reply_text.chars().take(60).collect::<String>()
    );
}

/// LIVE proof that a running workflow is actually VISIBLE.
///
/// Everything a workflow does after launch arrives only as `system/task_*`
/// frames, which the pump used to consume silently — the conversation showed
/// nothing for the whole flight. The unit tests drive synthetic events; only this
/// one proves the real CLI's frames reach the WebSocket as renderable rows.
///
/// The assertion deliberately requires the roster to CHANGE across frames: a
/// single static frame would also satisfy "something was sent" while still
/// leaving the user staring at a frozen card.
#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_workflow_progress_streams() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "aionui_ai_agent=debug,aionui_session=info",
        ))
        .try_init();
    let app = start_live_app().await;

    let ws_dir = std::env::temp_dir().join(format!("live-wf-progress-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {"workspace": ws_dir.to_string_lossy(), "backend": "claude"}
        }),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("conversation create failed: {created}"))
        .to_owned();
    println!("[wf-progress] conversation {conv_id} workspace {}", ws_dir.display());

    let frames = connect_ws_recorder(app.addr, &app.token).await;

    // Two agents so the roster has something to show, and long enough that the
    // progress stream spans many frames.
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "用 Workflow 工具启动一个 workflow：一个 phase「Run」，两个 agent 并行，\
            每个 agent 各自执行 sleep 20（睡 20 秒）。用后台方式启动（run_in_background），\
            启动后立刻回复我「已启动」，不要等待它完成。"}),
    )
    .await;
    assert!(sent["data"]["turn_id"].is_string(), "send failed: {sent}");

    let mut confirmed: BTreeSet<String> = BTreeSet::new();
    let started = Instant::now();
    // Distinct roster snapshots seen, in arrival order.
    let mut rosters: Vec<String> = Vec::new();
    let mut cards: Vec<Value> = Vec::new();

    while started.elapsed() < Duration::from_secs(180) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        auto_confirm_permissions(&app, &conv_id, &snapshot, &mut confirmed).await;
        rosters.clear();
        cards.clear();
        for f in stream_frames_for(&snapshot, &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "tool_group" => {
                    let serialized = f["data"]["data"].to_string();
                    if rosters.last() != Some(&serialized) {
                        rosters.push(serialized);
                    }
                }
                "tool_call" if f["data"]["data"]["name"] == "Workflow" => cards.push(f["data"]["data"].clone()),
                _ => {}
            }
        }
        // Enough evidence: the roster moved at least once and the workflow ended.
        let settled = rosters.last().is_some_and(|r| !r.contains("Executing"));
        if rosters.len() >= 2 && settled {
            break;
        }
    }

    assert!(
        rosters.len() >= 2,
        "[wf-progress] the roster must stream and CHANGE while the workflow runs; saw {} distinct snapshot(s)",
        rosters.len()
    );
    // claude's OWN `tool_use` frame for the Workflow call is also a tool_call
    // named "Workflow" — it carries the script but no headline. The progress
    // projections are the ones with a description, so filter to those before
    // asserting anything about headlines.
    let progress_cards: Vec<&Value> = cards
        .iter()
        .filter(|c| c["description"].as_str().is_some_and(|d| !d.is_empty()))
        .collect();
    assert!(
        !progress_cards.is_empty(),
        "[wf-progress] the container row must be re-emitted with a live headline; saw only {} raw tool_call frame(s)",
        cards.len()
    );
    // Identity fields must survive every projection — losing `name`/`args` would
    // blank the persisted row (the DB merges with JSON merge-patch).
    for card in &progress_cards {
        assert_eq!(card["name"], "Workflow", "[wf-progress] name must survive: {card}");
        assert!(
            card["args"]["script"].is_string(),
            "[wf-progress] args must be re-sent or merge-patch deletes them: {card}"
        );
    }

    // Show what the user would actually see, so the rendering can be judged from
    // a real run rather than a unit fixture.
    if let Some(head) = progress_cards.last() {
        println!(
            "\n[wf-progress] ── container row ──\n  ▸ {}   {}",
            head["name"].as_str().unwrap_or(""),
            head["description"].as_str().unwrap_or("")
        );
        if let Some(out) = head["output"].as_str() {
            println!("[wf-progress] ── expanded ──");
            for line in out.lines() {
                println!("  {line}");
            }
        }
    }
    if let Some(final_roster) = rosters.last()
        && let Ok(rows) = serde_json::from_str::<Vec<Value>>(final_roster)
    {
        println!("[wf-progress] ── agent rows ──");
        for r in rows {
            println!(
                "  {:<10} {:<14} {}",
                r["status"].as_str().unwrap_or("?"),
                r["name"].as_str().unwrap_or("?"),
                r["description"].as_str().unwrap_or("")
            );
        }
    }

    let last = rosters.last().unwrap();
    assert!(
        !last.contains("Executing"),
        "[wf-progress] no agent row may still be Executing after the workflow ends: {last}"
    );
    // The status vocabulary must be tool_group's PascalCase — snake_case would
    // miss every arm of normalizeToolGroupStatus and pin the rows to a spinner.
    assert!(
        last.contains("Success") || last.contains("Canceled") || last.contains("Error"),
        "[wf-progress] terminal rows must use the tool_group status vocabulary: {last}"
    );
    println!(
        "[wf-progress] ✅ {} roster updates, {} container updates",
        rosters.len(),
        cards.len()
    );
}

/// PROBE, not a contract test: drive real codex (app-server) through our full
/// stack and RECORD what background/collab work looks like on the wire — which
/// SessionEvents arrive, whether anything arrives after the turn's finish, and
/// what the out-of-turn watcher does with it. codex's background/collab frames
/// are unsampled territory; this prints facts instead of asserting shapes.
#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_background_work_probe() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "aionui_ai_agent=debug,aionui_session=debug,aionui_conversation=info",
        ))
        .try_init();
    let app = start_live_app().await;

    let ws_dir = std::env::temp_dir().join(format!("live-codex-bg-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {"workspace": ws_dir.to_string_lossy(), "backend": "codex"}
        }),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("conversation create failed: {created}"))
        .to_owned();
    println!("[codex-bg] conversation {conv_id} workspace {}", ws_dir.display());

    let frames = connect_ws_recorder(app.addr, &app.token).await;

    // Ask for BOTH shapes in one turn: a collab sub-agent and a long shell
    // command, launched without waiting. Whatever codex actually does — spawn,
    // refuse, run inline — is the data we came for.
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "请派一个协作 sub-agent(collab agent),让它执行 `sleep 40 && echo CODEX_AGENT_DONE` 然后汇报。\
            派出去之后立刻回复我「已派出」,不要等它完成。如果你还能把 shell 命令放到后台执行,也用后台方式跑一个 `sleep 30 && echo CODEX_BG_DONE`。"}),
    )
    .await;
    assert!(sent["data"]["turn_id"].is_string(), "send failed: {sent}");

    let started = Instant::now();
    let mut finish_at: Option<Duration> = None;
    let mut printed = 0usize;
    while started.elapsed() < Duration::from_secs(150) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        let stream: Vec<&Value> = stream_frames_for(&snapshot, &conv_id);
        // Print frames incrementally with elapsed + post-finish marker.
        for f in stream.iter().skip(printed) {
            let ftype = f["data"]["type"].as_str().unwrap_or("?");
            let brief = match ftype {
                "text" | "content" => f["data"]["data"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect::<String>(),
                "tool_call" => format!(
                    "{} {} {}",
                    f["data"]["data"]["name"].as_str().unwrap_or("?"),
                    f["data"]["data"]["status"].as_str().unwrap_or("?"),
                    f["data"]["data"]["description"].as_str().unwrap_or("")
                ),
                "tool_group" => f["data"]["data"].to_string().chars().take(90).collect(),
                _ => String::new(),
            };
            let tag = if finish_at.is_some() { "POST-FINISH" } else { "" };
            println!(
                "[codex-bg] +{:5.1}s {tag:11} {ftype:14} {brief}",
                started.elapsed().as_secs_f32()
            );
            if ftype == "finish" && finish_at.is_none() {
                finish_at = Some(started.elapsed());
            }
        }
        printed = stream.len();
        // Keep listening well past the finish: the whole point is what (if
        // anything) arrives once the turn is over.
        if let Some(f) = finish_at
            && started.elapsed() > f + Duration::from_secs(60)
        {
            break;
        }
    }
    println!(
        "[codex-bg] done: {} frames, finish at {:?}",
        printed,
        finish_at.map(|d| d.as_secs_f32())
    );
    assert!(printed > 0, "[codex-bg] no frames at all — probe produced nothing");
}
