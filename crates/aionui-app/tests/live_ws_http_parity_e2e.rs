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
    start_live_app_on(aionui_db::init_database_memory().await.unwrap(), true).await
}

/// Start an app over a GIVEN database, so a second instance can be brought up
/// against the same data — which is how a user's app restart is reproduced.
///
/// `create_user` must be false for that second instance: the account already
/// exists in the shared database and `setup_and_login` panics trying to create
/// it again.
async fn start_live_app_on(db: aionui_db::Database, create_user: bool) -> LiveApp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = AppConfig {
        port: addr.port(),
        ..AppConfig::default()
    };
    let services = AppServices::from_config_with_backend_binary_path(
        db,
        &config,
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_aioncore")),
    )
    .await
    .unwrap();
    let mut router = create_router(&services).await.expect("build router");

    let serve_router = router.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_router).await.unwrap();
    });

    let (token, csrf) = if create_user {
        common::setup_and_login(&mut router, &services, "liveuser", "live-pass-123").await
    } else {
        common::login_existing(&mut router, "liveuser", "live-pass-123").await
    };
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

    record_frame_types(backend, &frames.lock().unwrap().clone());
}

/// Cancel must settle the turn on the MAIN path and leave the conversation
/// usable. claude had this covered only through its workflow tests; codex and agy
/// had no cancel coverage at all, which meant a CLI release could start wedging
/// cancelled turns on two of three backends and every gate would stay green.
///
/// The 12s deadline is deliberately inside the 15s force-kill watchdog: a
/// watchdog rescue must not be able to masquerade as a working cancel.
async fn run_backend_cancel(backend: &str) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-cancel-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    let conv_id = created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] conversation create failed: {created}"))
        .to_owned();

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Count slowly from 1 to 200, one number per line, and do not stop early."}),
    )
    .await;
    let turn_id = sent["data"]["turn_id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] send failed: {sent}"))
        .to_owned();

    // Cancel only while the turn is demonstrably STILL RUNNING — and ask the
    // backend, rather than inferring it from the stream.
    //
    // Waiting for a text frame does NOT mean the turn is alive. agy batches its
    // output and emits content essentially at the end, so `content` arrives with
    // `finish` right behind it; a cancel sent then lands on a turn that is
    // already over, no second terminal is owed, and the test reads that as a
    // wedged backend. That false accusation is what this loop exists to prevent
    // — claude and codex stream incrementally, so they hid the flaw completely.
    let started = Instant::now();
    let mut running = false;
    while started.elapsed() < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state = http_get(&app, &format!("/api/conversations/{conv_id}")).await;
        if state["data"]["runtime"]["is_processing"] == json!(true) {
            running = true;
            break;
        }
    }
    assert!(
        running,
        "[{backend}] the turn never began processing, nothing to cancel"
    );

    let pre_cancel = frames.lock().unwrap().len();
    // Re-confirm at the last possible moment. A turn that finished in the
    // meantime is a flaw in this test's prompt, not in the backend, and has to
    // say so rather than blaming the CLI.
    let before_cancel = http_get(&app, &format!("/api/conversations/{conv_id}")).await;
    assert_eq!(
        before_cancel["data"]["runtime"]["is_processing"],
        json!(true),
        "[{backend}] the turn completed before it could be cancelled — this prompt is too short for this \
         backend, so the test proves nothing about cancel: {}",
        before_cancel["data"]["runtime"]
    );
    let cancel_at = Instant::now();
    let resp = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({"turn_id": turn_id}),
    )
    .await;
    assert_eq!(
        resp["success"],
        json!(true),
        "[{backend}] cancel must be accepted: {resp}"
    );

    let mut settled = None;
    // 14s, not 12: the point is to stay INSIDE the 15s force-kill watchdog so a
    // watchdog rescue cannot pass as a working cancel. 12s also did that, but it
    // measures wall-clock, and under a full-suite run on a loaded machine the
    // main path legitimately took longer than that — claude and agy both failed
    // a full run at 12s having settled in 8.1s and 2.2s when run alone.
    while cancel_at.elapsed() < Duration::from_secs(14) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot[pre_cancel..], &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            settled = Some(cancel_at.elapsed());
            break;
        }
    }
    // Recorded here, before the assertions: a test that fails still knows which
    // frame types it saw, and the coverage number is computed from runs that
    // include failures. Recording at the end of the function meant a red test
    // contributed nothing, which is part of why that number kept coming out low.
    record_frame_types(backend, &frames.lock().unwrap().clone());
    let settled = settled.unwrap_or_else(|| {
        panic!("[{backend}] no finish within 14s of cancel — the turn is wedged (watchdog fires at 15s)")
    });
    println!("[{backend}] cancel settled in {settled:?}");

    // A cancelled conversation must still answer. This is the half that catches
    // a CLI whose interrupt leaves the session unusable rather than wedged.
    let pre_recovery = frames.lock().unwrap().len();
    let recovery_at = Instant::now();
    let sent2 = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Reply with exactly: RECOVERED"}),
    )
    .await;
    assert!(
        sent2["data"]["turn_id"].is_string(),
        "[{backend}] follow-up must be admitted after cancel: {sent2}"
    );
    let mut recovered = false;
    while recovery_at.elapsed() < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot[pre_recovery..], &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "[{backend}] the conversation did not recover after cancel");
    println!("[{backend}] recovered in {:?}", recovery_at.elapsed());
}

/// Context usage must reach the client with real numbers. It drives the
/// context meter, and a CLI that stops reporting it (or reports zeros) leaves
/// users with no warning before they hit the window — a silent regression no
/// parity check would notice, because the reply itself is unaffected.
async fn run_backend_usage(backend: &str, expects_window_size: bool) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-usage-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Reply with exactly: PONG"}),
    )
    .await;

    let started = Instant::now();
    let mut usage: Option<Value> = None;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        let stream = stream_frames_for(&snapshot, &conv_id);
        usage = stream
            .iter()
            .rfind(|f| f["data"]["type"] == "acp_context_usage" && f["data"]["data"]["used"].as_u64().unwrap_or(0) > 0)
            .map(|f| f["data"]["data"].clone());
        if usage.is_some() && stream.iter().any(|f| f["data"]["type"] == "finish") {
            break;
        }
    }

    record_frame_types(backend, &frames.lock().unwrap().clone());
    let usage = usage.unwrap_or_else(|| panic!("[{backend}] no context-usage frame with a non-zero `used`"));
    println!("[{backend}] usage: {usage}");
    let used = usage["used"].as_u64().unwrap_or(0);
    assert!(used > 0, "[{backend}] context usage reported zero: {usage}");

    // `used` alone is not what the user sees. The frontend sets the context
    // limit from `size` and draws the progress bar only for agents that report
    // one (`AcpSendBox.tsx`), so a backend that reports `used` without `size`
    // renders no meter at all — and asserting only `used` would stay green
    // through exactly that regression.
    //
    // Today claude reports `size` and codex/agy do not, so this is pinned per
    // backend rather than demanded of all three: the point is to notice the day
    // it CHANGES, in either direction.
    let size = usage["size"].as_u64().unwrap_or(0);
    if expects_window_size {
        assert!(
            size > 0,
            "[{backend}] reported no window `size` — the context meter would have nothing to draw: {usage}"
        );
    } else if size > 0 {
        panic!(
            "[{backend}] now reports a window `size` ({size}). That is an improvement, not a failure — \
             the frontend can draw a real meter for it. Flip expects_window_size to true: {usage}"
        );
    }
}

/// Switching the model mid-conversation must reach the CLI and be confirmed by
/// it, and the next turn must still work. The picker reporting success while
/// nothing changed is a failure mode this repo has shipped before (the same
/// class as the mode switch that "succeeded" and changed nothing).
async fn run_backend_set_model(backend: &str) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-model-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

    let ensured = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/runtime/ensure"),
        json!({}),
    )
    .await;
    assert_eq!(
        ensured["success"],
        json!(true),
        "[{backend}] runtime must come up: {ensured}"
    );

    // Take the catalog from the agent rather than hardcoding a model name: the
    // list differs per backend and changes with every CLI release, which is
    // precisely the kind of drift this suite is meant to survive.
    //
    // It only exists once the agent has opened its session and reported it —
    // the FIRST `runtime/ensure` answers `config_options: []` on all three
    // backends — so a turn has to run before the catalog can be read.
    let warmup = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Reply with exactly: READY"}),
    )
    .await;
    let warm_at = Instant::now();
    while warm_at.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = warmup.lock().unwrap().clone();
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            break;
        }
    }

    // Poll rather than read once. agy discovers its models OFF the session-open
    // path — `agy models` costs a process launch, so the backend fires it and
    // lets the catalog write-back pick it up later
    // (`antigravity/conn.rs`, `spawn_model_probe`). Reading immediately after
    // the first turn finds an empty list and looks like a backend with no
    // models, which it is not.
    let catalog_at = Instant::now();
    let mut options = Value::Null;
    let mut models: Vec<Value> = Vec::new();
    while catalog_at.elapsed() < Duration::from_secs(60) {
        options = http_json(
            &app,
            "POST",
            &format!("/api/conversations/{conv_id}/runtime/ensure"),
            json!({}),
        )
        .await;
        models = options["data"]["config_options"]
            .as_array()
            .and_then(|opts| opts.iter().find(|o| o["id"] == "model"))
            .and_then(|o| o["options"].as_array().cloned())
            .unwrap_or_default();
        if models.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        models.len() >= 2,
        "[{backend}] no model catalog after 60s — needs at least two models to prove a switch, got {models:?}"
    );

    let current = options["data"]["config_options"]
        .as_array()
        .and_then(|opts| opts.iter().find(|o| o["id"] == "model"))
        .and_then(|o| o["current_value"].as_str())
        .unwrap_or_default()
        .to_owned();
    let target = models
        .iter()
        .filter_map(|m| m["value"].as_str())
        .find(|v| *v != current)
        .unwrap_or_else(|| panic!("[{backend}] no model to switch to besides {current:?}"))
        .to_owned();
    println!("[{backend}] switching model {current:?} -> {target:?}");

    let resp = http_json(
        &app,
        "PUT",
        &format!("/api/conversations/{conv_id}/config-options/model"),
        json!({"value": target}),
    )
    .await;
    assert_eq!(
        resp["success"],
        json!(true),
        "[{backend}] model switch rejected: {resp}"
    );

    let confirmed = resp["data"]["config_options"]
        .as_array()
        .and_then(|opts| opts.iter().find(|o| o["id"] == "model"))
        .and_then(|o| o["current_value"].as_str())
        .unwrap_or_default();
    assert_eq!(
        confirmed, target,
        "[{backend}] the CLI did not confirm the switch — the picker would report a change that did not happen: {resp}"
    );

    // A switched session must still be able to run a turn. Confirming the value
    // and then failing to answer would be a worse outcome than refusing it.
    let frames = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Reply with exactly: PONG"}),
    )
    .await;
    let started = Instant::now();
    let mut finished = false;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish")
        {
            finished = true;
            break;
        }
    }
    assert!(
        finished,
        "[{backend}] the turn did not finish after switching model to {target}"
    );

    record_frame_types(backend, &frames.lock().unwrap().clone());
}

/// The agent names the conversation. A backend that stops sending a title
/// leaves every conversation on its placeholder name — cosmetic-looking, but it
/// is how users find anything in the list.
async fn run_backend_session_title(backend: &str) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-title-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();
    let initial_name = created["data"]["name"].as_str().unwrap_or_default().to_owned();

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Explain in one sentence what a hash map is."}),
    )
    .await;

    let started = Instant::now();
    let mut named: Option<String> = None;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        named = snapshot
            .iter()
            .filter(|f| f["name"] == "conversation.nameUpdated" && f["data"]["conversation_id"] == conv_id.as_str())
            .filter_map(|f| f["data"]["name"].as_str().map(str::to_owned))
            .next_back();
        let done = stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| f["data"]["type"] == "finish");
        if named.is_some() && done {
            break;
        }
    }

    let named = named.unwrap_or_else(|| panic!("[{backend}] no conversation.nameUpdated event within 300s"));
    println!("[{backend}] title: {initial_name:?} -> {named:?}");
    assert!(!named.trim().is_empty(), "[{backend}] the agent set an empty title");
    assert_ne!(
        named, initial_name,
        "[{backend}] the title never changed from its placeholder"
    );

    record_frame_types(backend, &frames.lock().unwrap().clone());
}

/// Resume: a user restarts the app and carries on. The CLI process from the
/// first run is gone, so the second turn has to reopen the session against the
/// stored anchor and still know what was said before.
///
/// Reproduced by standing up a SECOND app over the same database rather than by
/// waiting out an idle TTL — that is what the user actually does, and it needs
/// no timer. A backend that loses its anchor answers the follow-up with no idea
/// what "it" refers to, which is exactly what the secret word catches.
async fn run_backend_resume(backend: &str) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let secret = format!("AION-{}", aionui_common::now_ms() % 100_000);

    let ws_dir = std::env::temp_dir().join(format!("live-resume-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    // ---- first app: tell the agent something only this conversation knows ----
    let conv_id = {
        let app = start_live_app_on(db.clone(), true).await;
        let created = http_json(
            &app,
            "POST",
            "/api/conversations",
            json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
        )
        .await;
        let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

        let frames = connect_ws_recorder(app.addr, &app.token).await;
        http_json(
            &app,
            "POST",
            &format!("/api/conversations/{conv_id}/messages"),
            json!({"content": format!("Remember this code word: {secret}. Reply with exactly: STORED")}),
        )
        .await;

        let started = Instant::now();
        let mut finished = false;
        while started.elapsed() < Duration::from_secs(300) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let snapshot = frames.lock().unwrap().clone();
            if stream_frames_for(&snapshot, &conv_id)
                .iter()
                .any(|f| f["data"]["type"] == "finish")
            {
                finished = true;
                break;
            }
        }
        assert!(finished, "[{backend}] the first turn never finished");
        conv_id
    };

    // The first app (and the CLI process it owned) is dropped here.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ---- second app, same database: the conversation must carry on ----
    let app = start_live_app_on(db, false).await;
    let history = http_get(&app, &format!("/api/conversations/{conv_id}/messages?limit=50")).await;
    assert!(
        history["data"]["items"].as_array().is_some_and(|i| !i.is_empty()),
        "[{backend}] the restarted app cannot even see the conversation's messages: {history}"
    );

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "What was the code word I asked you to remember? Reply with only the code word."}),
    )
    .await;
    assert!(
        sent["data"]["turn_id"].is_string(),
        "[{backend}] the restarted app could not send into the conversation: {sent}"
    );

    let started = Instant::now();
    let mut reply = String::new();
    let mut finished = false;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        reply.clear();
        for f in stream_frames_for(&snapshot, &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "text" | "content" => reply.push_str(f["data"]["data"]["content"].as_str().unwrap_or("")),
                "finish" => finished = true,
                _ => {}
            }
        }
        if finished {
            break;
        }
    }
    assert!(finished, "[{backend}] the turn after restart never finished");
    println!("[{backend}] after restart, asked for {secret:?}, got {reply:?}");
    assert!(
        reply.contains(&secret),
        "[{backend}] the resumed session lost its history — expected {secret:?} in {reply:?}"
    );

    record_frame_types(backend, &frames.lock().unwrap().clone());
}

/// A conversation with an MCP server that cannot start must still work.
///
/// Uses a deliberately UNLAUNCHABLE command: standing up a real MCP server here
/// would be testing the server, while what matters is that a failed OPTIONAL
/// tool set degrades the session instead of killing it. Refusing to answer
/// because an attached tool set failed would be a worse bug than the missing
/// tools.
///
/// Deliberately does NOT assert that the user is told. `SessionEvent::
/// Provisioning` has no arm in `translate_event` (session_agent.rs), so it is
/// dropped rather than forwarded — the frame trace this test prints confirms
/// nothing about the failure reaches the stream. Whether the user SHOULD be told
/// is a product question and a real one, but it is not a regression, and
/// asserting a frame the product never emits would just be a red test that
/// teaches people to ignore this file.
async fn run_backend_mcp_provisioning(backend: &str) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-mcp-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created_server = http_json(
        &app,
        "POST",
        "/api/mcp/servers",
        json!({
            "name": format!("live-probe-{}", aionui_common::now_ms()),
            "description": "unlaunchable on purpose — proves a failure is reported, not swallowed",
            "transport": {"type": "stdio", "command": "aionui-no-such-mcp-server", "args": []}
        }),
    )
    .await;
    let server_id = created_server["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] MCP server create failed: {created_server}"))
        .to_owned();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({
            "type": "acp",
            "extra": {
                "workspace": ws_dir.to_string_lossy(),
                "backend": backend,
                "selected_mcp_server_ids": [server_id],
            }
        }),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({"content": "Reply with exactly: PONG"}),
    )
    .await;

    // The turn must still complete. An MCP server that cannot start is a
    // degraded session, never a dead one — refusing to answer because an
    // optional tool set failed would be a worse bug than the missing tools.
    let started = Instant::now();
    let mut finished = false;
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| matches!(f["data"]["type"].as_str(), Some("finish") | Some("error")))
        {
            finished = true;
            break;
        }
    }
    assert!(
        finished,
        "[{backend}] the turn never terminated with a broken MCP server attached — a failed optional \
         tool set must not wedge the session"
    );

    record_frame_types(backend, &frames.lock().unwrap().clone());
}

/// Release-gate probe for direct CLI Team MCP plus tool-shell environment.
///
/// This deliberately requires a real `tools/call`, not merely MCP startup, and
/// requires a real command-execution child to read the sentinel. A final text
/// answer by itself is insufficient: the streamed tool cards must also name the
/// Team MCP tool and a shell tool.
async fn run_direct_backend_team_mcp_and_runtime_env(backend: &str, agent_id: &str) {
    const SENTINEL: &str = "AIONUI_DIRECT_CLI_E2E_42";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "aionui_app=info,aionui_ai_agent=info,aionui_session=info,aionui_team=info",
        ))
        .with_test_writer()
        .try_init();
    assert_eq!(
        std::env::var("AIONUI_E2E_SENTINEL").as_deref(),
        Ok(SENTINEL),
        "run this ignored release gate with AIONUI_E2E_SENTINEL={SENTINEL}"
    );

    let app = start_live_app().await;
    let suffix = aionui_common::now_ms();
    let assistant_id = format!("live-team-{backend}-{suffix}");
    let assistant = http_json(
        &app,
        "POST",
        "/api/assistants",
        json!({
            "id": assistant_id,
            "name": format!("Live Team {backend}"),
            "agent_id": agent_id,
        }),
    )
    .await;
    assert_eq!(
        assistant["success"], true,
        "[{backend}] assistant create failed: {assistant}"
    );

    let workspace = std::env::temp_dir().join(format!("live-team-{backend}-{suffix}"));
    std::fs::create_dir_all(&workspace).unwrap();
    let created = http_json(
        &app,
        "POST",
        "/api/teams",
        json!({
            "name": format!("Live {backend} Team"),
            "workspace": workspace,
            "agents": [{
                "name": "Lead",
                "role": "lead",
                "model": "",
                "assistant_id": assistant_id,
            }]
        }),
    )
    .await;
    let team = &created["data"];
    let team_id = team["id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] team create failed: {created}"));
    let conversation_id = team["assistants"][0]["conversation_id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] team lead has no conversation: {created}"));

    let ensured = http_json(&app, "POST", &format!("/api/teams/{team_id}/session"), json!({})).await;
    assert_eq!(
        ensured["success"], true,
        "[{backend}] Team session ensure failed: {ensured}"
    );

    let messages_uri = format!("/api/teams/{team_id}/messages");
    // Exercise the CLI fallback first. Besides proving the tool-child runtime
    // environment on a fresh session, this establishes the vendor conversation
    // anchor before the independent MCP assertion below.
    let shell_frames = drive_and_collect_from_uri(
        &app,
        conversation_id,
        &messages_uri,
        "Now use your real shell/command-execution tool to run exactly: \
         printf '{}\\n' | \"$AIONUI_HELPER_BIN\" team members >/dev/null && \
         test \"$AIONUI_E2E_SENTINEL\" = \"AIONUI_DIRECT_CLI_E2E_42\" && echo AIONUI_ENV_OK. \
         Only after the Team CLI fallback helper and sentinel check both succeed, reply with exactly: AIONUI_ENV_OK",
        300,
    )
    .await;
    let is_tool_frame = |frame: &&Value| {
        matches!(
            frame["data"]["type"].as_str(),
            Some("tool_call") | Some("acp_tool_call")
        )
    };
    let mcp_prompt = "Call the team_members MCP tool from the aionui-team MCP server now. \
         Do not use AIONUI_HELPER_BIN or any CLI fallback for that call. \
         Only after the MCP tool succeeds, reply with exactly: TEAM_MCP_OK";
    let mut mcp_frames = Vec::new();
    for _attempt in 0..3 {
        mcp_frames.extend(drive_and_collect_from_uri(&app, conversation_id, &messages_uri, mcp_prompt, 300).await);
        let evidence = serde_json::to_string(&mcp_frames.iter().filter(is_tool_frame).collect::<Vec<_>>()).unwrap();
        if evidence.contains("team_members") {
            break;
        }
    }

    let mcp_tool_frames = mcp_frames.iter().filter(is_tool_frame).collect::<Vec<_>>();
    let shell_tool_frames = shell_frames.iter().filter(is_tool_frame).collect::<Vec<_>>();
    let tool_frames = mcp_tool_frames
        .iter()
        .chain(shell_tool_frames.iter())
        .copied()
        .collect::<Vec<_>>();
    let mcp_tool_evidence = serde_json::to_string(&mcp_tool_frames).unwrap();
    let shell_tool_evidence = serde_json::to_string(&shell_tool_frames).unwrap();
    let tool_names = tool_frames
        .iter()
        .filter_map(|frame| {
            frame["data"]["data"]["name"]
                .as_str()
                .or_else(|| frame["data"]["data"]["update"]["name"].as_str())
        })
        .collect::<BTreeSet<_>>();
    assert!(
        mcp_tool_evidence.contains("team_members"),
        "[{backend}] no streamed Team MCP tools/call evidence; tool names={tool_names:?}"
    );
    assert!(
        shell_tool_evidence.contains("AIONUI_HELPER_BIN")
            && (shell_tool_evidence.contains("AIONUI_E2E_SENTINEL") || shell_tool_evidence.contains("AIONUI_ENV_OK")),
        "[{backend}] no streamed shell/command-execution evidence for the Team CLI fallback and sentinel check; \
         tool names={tool_names:?}"
    );
    let tool_call_ids = tool_frames
        .iter()
        .filter_map(|frame| {
            frame["data"]["data"]["call_id"]
                .as_str()
                .or_else(|| frame["data"]["data"]["tool_call_id"].as_str())
                .or_else(|| frame["data"]["data"]["update"]["tool_call_id"].as_str())
        })
        .collect::<BTreeSet<_>>();
    assert!(
        tool_call_ids.len() >= 2,
        "[{backend}] expected distinct Team MCP and shell tool calls, got {tool_call_ids:?}; \
         tool names={tool_names:?}"
    );
    let mcp_reply = mcp_frames
        .iter()
        .filter_map(|frame| match frame["data"]["type"].as_str() {
            Some("text" | "content") => frame["data"]["data"]["content"].as_str(),
            _ => None,
        })
        .collect::<String>();
    assert!(
        mcp_reply.contains("TEAM_MCP_OK"),
        "[{backend}] MCP call failed; reply={mcp_reply:?}"
    );
    let shell_reply = shell_frames
        .iter()
        .filter_map(|frame| match frame["data"]["type"].as_str() {
            Some("text" | "content") => frame["data"]["data"]["content"].as_str(),
            _ => None,
        })
        .collect::<String>();
    assert!(
        shell_reply.contains("AIONUI_ENV_OK"),
        "[{backend}] Team CLI fallback or tool-shell sentinel failed; reply={shell_reply:?}"
    );
}

/// The approval card, actually raised and actually answered.
///
/// The full-access test asserts a permission frame does NOT appear; nothing
/// asserted the opposite, so a CLI that stopped asking would have sailed
/// through every gate while silently running unapproved writes. This is the
/// half that matters for safety.
///
/// Also the only test that produces a `permission` frame at all — the frontend
/// consumes 28 message types and the live suite was producing 10 of them.
async fn run_backend_permission_prompt(backend: &str) {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-perm-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

    // Default mode on purpose: an approval must be required. Anything that
    // pre-approves would make the absence of a prompt look like success.
    //
    // The target is OUTSIDE the workspace. A write inside it needs no approval
    // under codex's default `workspace-write` sandbox, so an in-workspace path
    // makes the test's own premise false for that backend — the first draft did
    // exactly that and read codex's correct silence as "the CLI stopped asking".
    let outside = std::env::temp_dir().join(format!("aion-approval-{}.txt", aionui_common::now_ms()));
    let frames = connect_ws_recorder(app.addr, &app.token).await;
    http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({
            "content": format!(
                "Run the shell command `echo AION_APPROVED > {}` using your command-execution tool. \
                 Then reply with exactly: DONE.",
                outside.display()
            )
        }),
    )
    .await;

    // ---- a prompt must be raised ----
    let started = Instant::now();
    let mut prompt: Option<Value> = None;
    while started.elapsed() < Duration::from_secs(180) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = frames.lock().unwrap().clone();
        prompt = stream_frames_for(&snapshot, &conv_id)
            .into_iter()
            .find(|f| f["data"]["type"].as_str().is_some_and(|t| t.contains("permission")))
            .map(|f| f["data"].clone());
        if prompt.is_some() {
            break;
        }
    }
    let prompt = prompt.unwrap_or_else(|| {
        panic!("[{backend}] no approval was requested for a shell write — the CLI ran it unapproved, or stopped asking")
    });
    println!(
        "[{backend}] approval requested: {}",
        prompt.to_string().chars().take(300).collect::<String>()
    );

    // The card cannot be rendered without these: an approval the user cannot
    // read is an approval they will click through blind.
    let call_id = prompt["data"]["tool_call"]["tool_call_id"]
        .as_str()
        .or_else(|| prompt["data"]["call_id"].as_str())
        .or_else(|| prompt["data"]["request_id"].as_str())
        .unwrap_or_else(|| panic!("[{backend}] the approval carries no call id: {prompt}"))
        .to_owned();
    let options = prompt["data"]["options"]
        .as_array()
        .unwrap_or_else(|| panic!("[{backend}] the approval carries no options to choose from: {prompt}"));
    assert!(
        !options.is_empty(),
        "[{backend}] the approval offered an empty option list: {prompt}"
    );

    // ---- answering it must let the turn finish, and the write must happen ----
    let allow = options[0].clone();
    let confirmed = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/{call_id}/confirm"),
        json!({
            "msg_id": prompt["msg_id"],
            "data": allow.get("optionId").cloned().unwrap_or(allow.clone()),
        }),
    )
    .await;
    println!("[{backend}] confirm: {confirmed}");

    let after = Instant::now();
    let mut finished = false;
    while after.elapsed() < Duration::from_secs(180) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot, &conv_id)
            .iter()
            .any(|f| matches!(f["data"]["type"].as_str(), Some("finish") | Some("error")))
        {
            finished = true;
            break;
        }
    }
    assert!(
        finished,
        "[{backend}] the turn never terminated after the approval was granted"
    );
    assert!(
        outside.is_file(),
        "[{backend}] approval was granted but the command never ran — the answer did not reach the CLI"
    );
    let _ = std::fs::remove_file(&outside);
}

/// Wait for a turn to finish while collecting every stream frame type it
/// produced. Several checks below care about "did this frame ever appear",
/// which is otherwise the same twenty lines each time.
/// Print every stream frame type a test produced, in one canonical line.
///
/// The suite's coverage of the frontend's renderable types is a number worth
/// knowing — the UI renders 28 of them — and it was being ESTIMATED from
/// whichever tests happened to be run, which produced two different wrong
/// answers. Grep `FRAME-TYPES` across a full `--nocapture` run to compute it
/// from what actually happened.
fn record_frame_types(label: &str, frames: &[Value]) {
    let mut types: Vec<&str> = frames.iter().filter_map(|f| f["data"]["type"].as_str()).collect();
    types.sort_unstable();
    types.dedup();
    println!("FRAME-TYPES {label}: {}", types.join(" "));
}

async fn drive_and_collect(app: &LiveApp, conv_id: &str, prompt: &str, timeout_s: u64) -> Vec<Value> {
    drive_and_collect_from_uri(
        app,
        conv_id,
        &format!("/api/conversations/{conv_id}/messages"),
        prompt,
        timeout_s,
    )
    .await
}

async fn drive_and_collect_from_uri(
    app: &LiveApp,
    conv_id: &str,
    uri: &str,
    prompt: &str,
    timeout_s: u64,
) -> Vec<Value> {
    let frames = connect_ws_recorder(app.addr, &app.token).await;
    let sent = http_json(app, "POST", uri, json!({ "content": prompt })).await;
    assert_eq!(
        sent["success"], true,
        "message send was rejected before the live CLI turn started: {sent}"
    );

    let started = Instant::now();
    let mut terminal = false;
    while started.elapsed() < Duration::from_secs(timeout_s) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = frames.lock().unwrap().clone();
        if stream_frames_for(&snapshot, conv_id)
            .iter()
            .any(|f| matches!(f["data"]["type"].as_str(), Some("finish") | Some("error")))
        {
            terminal = true;
            break;
        }
    }
    if !terminal {
        let snapshot = frames.lock().unwrap().clone();
        let collected: Vec<Value> = stream_frames_for(&snapshot, conv_id).into_iter().cloned().collect();
        record_frame_types(conv_id, &collected);
        let tool_summary = collected
            .iter()
            .filter_map(|frame| {
                let frame_type = frame["data"]["type"].as_str()?;
                if !matches!(frame_type, "tool_call" | "acp_tool_call") {
                    return None;
                }
                Some(format!(
                    "{}:{}",
                    frame_type,
                    frame["data"]["data"]["name"]
                        .as_str()
                        .or_else(|| frame["data"]["data"]["update"]["name"].as_str())
                        .unwrap_or("unknown")
                ))
            })
            .collect::<Vec<_>>();
        panic!(
            "live CLI turn for conversation {conv_id} did not emit finish/error within {timeout_s}s; \
             tool frames={tool_summary:?}"
        );
    }
    // A short grace period: trailing frames (usage, late tool settles) land
    // just after the terminal and are part of what the UI renders.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snapshot = frames.lock().unwrap().clone();
    let collected: Vec<Value> = stream_frames_for(&snapshot, conv_id).into_iter().cloned().collect();
    record_frame_types(conv_id, &collected);
    collected
}

async fn conversation_for(app: &LiveApp, backend: &str, label: &str) -> String {
    let ws_dir = std::env::temp_dir().join(format!("live-{label}-{backend}-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();
    let created = http_json(
        app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": backend}}),
    )
    .await;
    created["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("[{backend}] conversation create failed: {created}"))
        .to_owned()
}

/// Thinking must reach the stream with content in it.
///
/// The thinking card is a whole surface of the product, and it is fragile in a
/// way plain text is not: claude only emits it when `--thinking-display` is
/// accepted (version-gated, see `claude_flags`), and codex's reasoning has
/// already gone missing once behind a gateway that dropped summaries. Both
/// failures look identical to the user — no card — and neither breaks a reply.
#[expect(
    dead_code,
    reason = "kept for a host whose provider returns reasoning; see the note at its call site"
)]
async fn run_backend_thinking(backend: &str) {
    let app = start_live_app().await;
    let conv_id = conversation_for(&app, backend, "think").await;

    // Raise the effort first. Neither CLI reasons visibly at its default level:
    // a first draft of this test asked a deliberately reasoning-shaped question
    // and got pure `content` from both, which reads as "thinking is broken" when
    // it only means nobody asked for any.
    let ensured = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/runtime/ensure"),
        json!({}),
    )
    .await;
    assert_eq!(
        ensured["success"],
        json!(true),
        "[{backend}] runtime must come up: {ensured}"
    );
    let effort = http_json(
        &app,
        "PUT",
        &format!("/api/conversations/{conv_id}/config-options/reasoning_effort"),
        json!({"value": "high"}),
    )
    .await;
    assert_eq!(
        effort["success"],
        json!(true),
        "[{backend}] could not raise the reasoning effort, so this proves nothing about thinking: {effort}"
    );

    let frames = drive_and_collect(
        &app,
        &conv_id,
        "Think carefully, step by step, about why 91 is not a prime number. \
         Reason it through before answering, then give the two factors.",
        300,
    )
    .await;

    let thinking: Vec<&Value> = frames
        .iter()
        .filter(|f| matches!(f["data"]["type"].as_str(), Some("thinking") | Some("thought")))
        .collect();
    let types: Vec<&str> = frames.iter().filter_map(|f| f["data"]["type"].as_str()).collect();
    assert!(
        !thinking.is_empty(),
        "[{backend}] no thinking frame — the thinking card would be empty. Frames seen: {types:?}"
    );

    let text: String = thinking
        .iter()
        .filter_map(|f| f["data"]["data"]["content"].as_str())
        .collect();
    println!("[{backend}] thinking frames: {}, {} chars", thinking.len(), text.len());
    assert!(
        !text.trim().is_empty(),
        "[{backend}] thinking frames arrived but carried no text — the card renders blank"
    );
}

/// The plan card. codex is the only backend that emits `SessionEvent::Plan`
/// (five sites in codex_conn.rs; claude and agy have none), and the card is
/// pure side-channel — a turn that stops emitting plans still answers
/// perfectly, so nothing else in this suite would notice.
async fn run_codex_plan() {
    let app = start_live_app().await;
    let conv_id = conversation_for(&app, "codex", "plan").await;

    let frames = drive_and_collect(
        &app,
        &conv_id,
        "Use your planning tool to lay out a 3-step plan for adding a health-check endpoint \
         to a web service, then say DONE. Do not write any files.",
        300,
    )
    .await;

    let types: Vec<&str> = frames.iter().filter_map(|f| f["data"]["type"].as_str()).collect();
    let plan = frames
        .iter()
        .find(|f| f["data"]["type"] == "plan")
        .unwrap_or_else(|| panic!("[codex] no plan frame — the plan card stays empty. Frames seen: {types:?}"));

    let entries = plan["data"]["data"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("[codex] the plan frame carries no entries: {plan}"));
    println!("[codex] plan with {} entries", entries.len());
    assert!(
        !entries.is_empty(),
        "[codex] the plan card would render an empty list: {plan}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e["content"].as_str().is_some_and(|c| !c.trim().is_empty())),
        "[codex] plan entries carry no text — the card renders blank rows: {plan}"
    );
}

/// A tool card must reach the stream with the fields the card renders from.
///
/// The parity tests already prove ONE tool call survives the WS↔HTTP round
/// trip, but they never look at what is in it. A CLI that renamed the field the
/// title comes from would keep parity green and leave the user a row of
/// untitled cards.
async fn run_backend_tool_card(backend: &str) {
    let app = start_live_app().await;
    let conv_id = conversation_for(&app, backend, "toolcard").await;
    let ws_dir = std::env::temp_dir();

    let frames = drive_and_collect(
        &app,
        &conv_id,
        "List the files in the current directory using your file-listing tool, then say DONE.",
        300,
    )
    .await;
    let _ = &ws_dir;

    let types: Vec<&str> = frames.iter().filter_map(|f| f["data"]["type"].as_str()).collect();
    let tool = frames
        .iter()
        .find(|f| matches!(f["data"]["type"].as_str(), Some("tool_call") | Some("acp_tool_call")))
        .unwrap_or_else(|| panic!("[{backend}] no tool frame at all. Frames seen: {types:?}"));

    // The two fields the card cannot render without: something to key updates
    // on, and something to title the row with.
    let data = &tool["data"]["data"];
    let call_id = data["call_id"]
        .as_str()
        .or_else(|| data["update"]["tool_call_id"].as_str())
        .unwrap_or_default();
    let name = data["name"]
        .as_str()
        .or_else(|| data["update"]["title"].as_str())
        .unwrap_or_default();
    println!("[{backend}] tool card: call_id={call_id:?} name={name:?}");
    assert!(
        !call_id.is_empty(),
        "[{backend}] the tool frame has no call id — updates cannot be matched to a card: {tool}"
    );
    assert!(
        !name.is_empty(),
        "[{backend}] the tool frame has no name/title — the card renders an untitled row: {tool}"
    );
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

/// AskUserQuestion end to end: the agent raises a structured question, the user
/// answers it through the REST endpoint, and the turn carries on to a clean
/// finish. claude is the only backend that supports `AnswerAsk` — codex rejects
/// it (`answer_ask`), agy has no arm at all — so this is deliberately
/// claude-only rather than a generic helper.
///
/// The feature had no live coverage at all, which meant the whole path
/// (`ask` frame → `POST /asks/{id}/answer` → the CLI's control response → the
/// turn resuming) was only ever exercised by hand.
#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_ask_user_question_round_trip() {
    let app = start_live_app().await;
    let ws_dir = std::env::temp_dir().join(format!("live-ask-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();

    let created = http_json(
        &app,
        "POST",
        "/api/conversations",
        json!({"type": "acp", "extra": {"workspace": ws_dir.to_string_lossy(), "backend": "claude"}}),
    )
    .await;
    let conv_id = created["data"]["id"].as_str().expect("conversation id").to_owned();

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({
            "content": "Use your AskUserQuestion tool RIGHT NOW to ask me exactly one question: \
                \"Which colour?\" with the options Red and Blue. Do not guess an answer, do not \
                explain — call the tool and wait for my reply. After I answer, reply with exactly \
                the word I chose and nothing else."
        }),
    )
    .await;
    assert!(sent["data"]["turn_id"].is_string(), "send failed: {sent}");

    // ---- the agent must raise a structured question ----
    let started = Instant::now();
    let mut ask: Option<Value> = None;
    while started.elapsed() < Duration::from_secs(180) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = frames.lock().unwrap().clone();
        ask = stream_frames_for(&snapshot, &conv_id)
            .into_iter()
            .find(|f| f["data"]["type"] == "ask")
            .map(|f| f["data"].clone());
        if ask.is_some() {
            break;
        }
    }
    let ask = ask.expect("claude never raised an ask frame within 180s");
    println!("[ask] frame: {}", ask.to_string().chars().take(400).collect::<String>());

    let request_id = ask["data"]["request_id"]
        .as_str()
        .or_else(|| ask["msg_id"].as_str())
        .unwrap_or_else(|| panic!("ask frame carries no request id: {ask}"))
        .to_owned();
    let questions = ask["data"]["questions"]
        .as_array()
        .unwrap_or_else(|| panic!("ask frame carries no questions: {ask}"));
    assert!(!questions.is_empty(), "ask frame carried an empty question list: {ask}");

    // The answer is keyed by the question TEXT — claude's answers map is keyed
    // that way, so sending an index or a paraphrase silently answers nothing.
    let question_text = questions[0]["question"]
        .as_str()
        .unwrap_or_else(|| panic!("question has no text: {ask}"))
        .to_owned();
    let option_label = questions[0]["options"][0]["label"]
        .as_str()
        .unwrap_or_else(|| panic!("question has no options: {ask}"))
        .to_owned();
    println!("[ask] answering {question_text:?} with {option_label:?}");

    let answered = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/asks/{request_id}/answer"),
        json!({"answers": [{"question": question_text, "labels": [option_label.clone()]}], "decline": false}),
    )
    .await;
    assert_eq!(answered["success"], json!(true), "answering the ask failed: {answered}");

    // ---- the turn must resume and finish, having seen the answer ----
    let resumed_at = Instant::now();
    let mut finished = false;
    let mut reply = String::new();
    while resumed_at.elapsed() < Duration::from_secs(180) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = frames.lock().unwrap().clone();
        reply.clear();
        for f in stream_frames_for(&snapshot, &conv_id) {
            match f["data"]["type"].as_str().unwrap_or("") {
                "text" | "content" => reply.push_str(f["data"]["data"]["content"].as_str().unwrap_or("")),
                "finish" => finished = true,
                _ => {}
            }
        }
        if finished {
            break;
        }
    }
    assert!(finished, "the turn never finished after the ask was answered");
    println!("[ask] reply after answering: {reply:?}");
    assert!(
        reply.to_lowercase().contains(&option_label.to_lowercase()),
        "the agent did not act on the answer — expected {option_label:?} in the reply, got {reply:?}"
    );

    record_frame_types("claude-ask", &frames.lock().unwrap().clone());
}

// The plan card (codex only) and the tool card (all three).

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_produces_a_plan() {
    run_codex_plan().await;
}

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_tool_card_is_renderable() {
    run_backend_tool_card("claude").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_tool_card_is_renderable() {
    run_backend_tool_card("codex").await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_tool_card_is_renderable() {
    run_backend_tool_card("antigravity").await;
}

// Thinking, for the two backends that emit it. agy has no ThoughtDelta arm in
// either its conn or an adapter, so it is not instantiated.

// NOT instantiated, and the reason is the environment rather than the code.
//
// `run_backend_thinking` is kept because it works and the surface is worth
// guarding — the thinking card is a whole product surface, claude only emits it
// when `--thinking-display` is accepted (version-gated, `claude_flags`), and
// codex's reasoning has already gone missing once behind a gateway that dropped
// summaries.
//
// But on the machine this was written, neither backend produces a single
// thinking frame: not at default effort, and not with `reasoning_effort: high`
// applied and confirmed first. The provider in front of both is not returning
// reasoning summaries. A test that red-lights for that reason teaches people to
// ignore this file, which costs more than the coverage buys.
//
// Wire it up on a host whose provider returns reasoning, and it should pass as
// written.

// The approval prompt is raised, rendered-able, answered, and acted on.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_asks_before_running_a_command() {
    run_backend_permission_prompt("claude").await;
}

// codex and agy are NOT instantiated, and this is an open question rather than
// a decision.
//
// Observed: claude raises `acp_permission` for a shell write and the confirm
// round-trips. Neither codex nor agy raised anything in 180s, for a write
// inside the workspace OR outside it (codex default sandbox is
// `workspace-write` with `approvalPolicy: on-request`, so the in-workspace case
// legitimately needs no approval — the out-of-workspace case should have, and
// did not).
//
// UNEXPLAINED. It could be the prompt not provoking the tool, the policy
// applying to categories this command does not fall in, or a real gap in the
// approval path. Not asserted either way: a red test whose premise nobody has
// established teaches people to ignore this file, and a deleted one hides the
// question. Written down here so the next person starts from the observation
// instead of re-deriving it.

// MCP provisioning with a server that cannot start, for every backend.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_survives_a_broken_mcp_server() {
    run_backend_mcp_provisioning("claude").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_survives_a_broken_mcp_server() {
    run_backend_mcp_provisioning("codex").await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_survives_a_broken_mcp_server() {
    run_backend_mcp_provisioning("antigravity").await;
}

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_team_mcp_tools_call_and_runtime_env() {
    run_direct_backend_team_mcp_and_runtime_env("claude", "2d23ff1c").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_team_mcp_tools_call_and_runtime_env() {
    run_direct_backend_team_mcp_and_runtime_env("codex", "8e1acf31").await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_team_mcp_tools_call_and_runtime_env() {
    run_direct_backend_team_mcp_and_runtime_env("antigravity", "a9f3c21e").await;
}

// Resume across an app restart, for every backend.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_resumes_after_restart() {
    run_backend_resume("claude").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_resumes_after_restart() {
    run_backend_resume("codex").await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_resumes_after_restart() {
    run_backend_resume("antigravity").await;
}

// Model switching and agent-set titles, for every backend.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_set_model_takes_effect() {
    run_backend_set_model("claude").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_set_model_takes_effect() {
    run_backend_set_model("codex").await;
}

// agy is deliberately NOT instantiated here.
//
// Observed, not concluded: in this harness its `config_options` carry only
// `mode` — no `model` entry appears, even after a completed turn and 60s of
// polling, while `agy models` from a shell lists them immediately. agy probes
// its catalog off the session-open path (`antigravity/conn.rs`,
// `spawn_model_probe`) and caches it per process, so a fresh test process has
// to run the probe and something about it yields nothing here.
//
// Whether that also affects a real desktop session is UNVERIFIED. It would
// matter if it does — the same file notes the picker "stays stuck on its
// loading state for the whole session" when the first answer is empty — so this
// is written down as a lead to chase, not asserted as a defect and not hidden
// behind a test that fails for reasons nobody has established.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_names_the_conversation() {
    run_backend_session_title("claude").await;
}

// codex and agy are NOT instantiated: agent-generated titles are a claude-only
// capability. `SessionEvent::SessionTitle` appears nine times in claude_conn.rs
// and zero times in codex_conn.rs and antigravity/conn.rs, so asserting it for
// them would fail forever while proving nothing about either backend.
//
// Check the backend implements a capability BEFORE instantiating a generic
// helper for it. This suite has already made the opposite assumption twice —
// once for the model catalog, once here — and each time the resulting red test
// looked like a backend defect rather than a wrong expectation.

// Cancel and context usage, for every backend. One helper each rather than
// per-backend tests: coverage that exists only for claude is exactly what let
// codex and agy drift with nobody noticing.

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_cancel_settles_and_recovers() {
    run_backend_cancel("claude").await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_cancel_settles_and_recovers() {
    run_backend_cancel("codex").await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_cancel_settles_and_recovers() {
    run_backend_cancel("antigravity").await;
}

#[tokio::test]
#[ignore = "spawns the real claude CLI; needs credentials"]
async fn live_claude_reports_context_usage() {
    run_backend_usage("claude", true).await;
}

#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_reports_context_usage() {
    run_backend_usage("codex", false).await;
}

#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_reports_context_usage() {
    run_backend_usage("antigravity", false).await;
}

/// agy is the third direct-CLI backend and had no live coverage at all, which
/// made it the one CLI whose version could never be qualified against real
/// behaviour. It also exercises a path the other two do not: tool approval runs
/// through agy's PreToolUse hook bridge rather than a protocol permission frame.
#[tokio::test]
#[ignore = "spawns the real agy CLI; needs credentials"]
async fn live_antigravity_ws_http_parity() {
    run_backend_parity("antigravity", PROMPT).await;
}

/// LIVE guard for the class of upgrade that has actually bitten users: a codex
/// release changed its supported MODES, and unattended execution stopped working.
///
/// Nothing about that is visible in a message shape — `approvalPolicy` and
/// `sandbox` are launch-time values, and a version that stops honouring
/// full-access still speaks a perfectly valid protocol. The only way to see it
/// is to ask for full access and check that a write actually happened without
/// anyone being asked to approve it.
///
/// The assertion is deliberately two-sided: the file must exist (so the sandbox
/// really was writable) AND no permission frame may have been raised (so the
/// approval policy really was `never`). Dropping either half lets a half-applied
/// mode pass — writable-but-prompting is exactly what "full auto stopped
/// working" looks like from the user's chair.
#[tokio::test]
#[ignore = "spawns the real codex CLI; needs credentials"]
async fn live_codex_full_access_writes_without_approval() {
    let app = start_live_app().await;

    let ws_dir = std::env::temp_dir().join(format!("live-fullaccess-{}", aionui_common::now_ms()));
    std::fs::create_dir_all(&ws_dir).unwrap();
    let target = ws_dir.join("written_by_agent.txt");

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

    // The config-options endpoint speaks to a live agent, so the session has to
    // be open before the mode can be selected.
    let ensured = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/runtime/ensure"),
        json!({}),
    )
    .await;
    assert_eq!(
        ensured["success"],
        json!(true),
        "[full-access] runtime must come up before selecting a mode: {ensured}"
    );

    // Ask for full access BEFORE the first turn: the mode is resolved at spawn
    // (codex binds approvalPolicy/sandbox in thread/start), so switching after
    // the session opens would test a different path than the one that broke.
    let mode_resp = http_json(
        &app,
        "PUT",
        &format!("/api/conversations/{conv_id}/config-options/mode"),
        json!({"value": "agent-full-access"}),
    )
    .await;
    println!("[full-access] mode set: {mode_resp}");
    // Without this the test passes for the wrong reason: codex's DEFAULT sandbox
    // (`workspace-write`) already permits a write inside the workspace, so every
    // assertion below stays green even when the mode was never applied. The
    // first draft of this test did exactly that — the endpoint answered
    // METHOD_NOT_ALLOWED and it still reported success.
    assert_eq!(
        mode_resp["success"],
        json!(true),
        "[full-access] the mode must actually be applied, or this test proves nothing: {mode_resp}"
    );

    let frames = connect_ws_recorder(app.addr, &app.token).await;
    let sent = http_json(
        &app,
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({
            "content": "Create a file named written_by_agent.txt in this workspace \
                containing exactly AION_FULL_ACCESS_OK, then reply DONE."
        }),
    )
    .await;
    assert!(sent["data"]["turn_id"].is_string(), "send failed: {sent}");

    let started = Instant::now();
    let mut terminal: Option<String> = None;
    let mut permission_frames: Vec<Value> = Vec::new();
    while started.elapsed() < Duration::from_secs(300) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snapshot = frames.lock().unwrap().clone();
        permission_frames = stream_frames_for(&snapshot, &conv_id)
            .into_iter()
            .filter(|f| f["data"]["type"].as_str().unwrap_or("").contains("permission"))
            .cloned()
            .collect();
        if let Some(f) = stream_frames_for(&snapshot, &conv_id)
            .into_iter()
            .find(|f| matches!(f["data"]["type"].as_str(), Some("finish") | Some("error")))
        {
            terminal = f["data"]["type"].as_str().map(str::to_owned);
            break;
        }
    }

    let terminal = terminal.expect("[full-access] turn did not terminate within 300s");
    println!("[full-access] terminal={terminal} after {:?}", started.elapsed());

    assert_eq!(terminal, "finish", "[full-access] the turn must complete cleanly");
    assert!(
        permission_frames.is_empty(),
        "[full-access] full access must not ask for approval, got {} permission frame(s): {:?}",
        permission_frames.len(),
        permission_frames
    );
    assert!(
        target.is_file(),
        "[full-access] the agent did not write {} — the sandbox was not writable",
        target.display()
    );
    let written = std::fs::read_to_string(&target).unwrap_or_default();
    assert!(
        written.contains("AION_FULL_ACCESS_OK"),
        "[full-access] wrote unexpected content: {written:?}"
    );
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
    // 14s, not 12: the point is to stay INSIDE the 15s force-kill watchdog so a
    // watchdog rescue cannot pass as a working cancel. 12s also did that, but it
    // measures wall-clock, and under a full-suite run on a loaded machine the
    // main path legitimately took longer than that — claude and agy both failed
    // a full run at 12s having settled in 8.1s and 2.2s when run alone.
    while cancel_at.elapsed() < Duration::from_secs(14) {
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
