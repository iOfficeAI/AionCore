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
            if ftype.contains("permission") {
                let call_id = f["data"]["data"]["call_id"]
                    .as_str()
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
