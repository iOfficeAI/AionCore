//! Integration tests for the OpenAI realtime transcription upstream.
//!
//! wiremock does not support WebSocket, so these tests run a small local
//! tokio-tungstenite server: the handshake callback captures the request URI
//! and headers, and each test drives a scripted frame exchange.
//!
//! Hang-safety: every potentially blocking await is wrapped in a 5s timeout
//! (`within`). Unlike the Deepgram tests, several handlers DO wait for the
//! client's `Close` frame: the OpenAI adapter initiates the close handshake
//! itself after commit + final transcript, and that behavior is pinned here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::{OpenAISpeechToTextConfig, SpeechToTextConfig, SpeechToTextProvider};
use aionui_shell::stt_stream_openai::{self, OpenAIRealtimeStream};
use aionui_shell::{OpenAIRealtimeUpstreamFactory, SttError, UpstreamEvent, UpstreamFactory, UpstreamStream};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Await with a deadline so a regression fails fast instead of hanging the suite.
async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, fut)
        .await
        .expect("timed out after 5s")
}

// -- Mock server ---------------------------------------------------------------

#[derive(Debug, Clone)]
struct Handshake {
    uri: String,
    authorization: Option<String>,
}

type ServerWs = WebSocketStream<TcpStream>;

/// Start a one-connection mock WS server. Returns the HTTP base URL to put in
/// the config, the captured handshake, and the server task handle (await it
/// to propagate in-handler assertion panics).
async fn spawn_server<F, Fut>(handler: F) -> (String, Arc<Mutex<Option<Handshake>>>, tokio::task::JoinHandle<()>)
where
    F: FnOnce(ServerWs) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let captured_in_task = captured.clone();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // The Err variant size is fixed by tungstenite's Callback signature.
        #[allow(clippy::result_large_err)]
        let callback = move |req: &Request, resp: Response| {
            *captured_in_task.lock().unwrap() = Some(Handshake {
                uri: req.uri().to_string(),
                authorization: req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            });
            Ok(resp)
        };
        let ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
        handler(ws).await;
    });

    (format!("http://{addr}"), captured, handle)
}

// -- Helpers ---------------------------------------------------------------------

fn make_config(base_url: &str) -> OpenAISpeechToTextConfig {
    OpenAISpeechToTextConfig {
        api_key: "sk-test".into(),
        base_url: Some(base_url.to_owned()),
        model: "gpt-4o-transcribe".into(),
        language: None,
        prompt: None,
        temperature: None,
    }
}

fn committed_frame(item_id: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "type": "input_audio_buffer.committed",
            "event_id": "event_0",
            "previous_item_id": null,
            "item_id": item_id,
        })
        .to_string()
        .into(),
    )
}

fn delta_frame(item_id: &str, delta: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "type": "conversation.item.input_audio_transcription.delta",
            "event_id": "event_1",
            "item_id": item_id,
            "content_index": 0,
            "delta": delta,
        })
        .to_string()
        .into(),
    )
}

fn completed_frame(item_id: &str, transcript: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "type": "conversation.item.input_audio_transcription.completed",
            "event_id": "event_2",
            "item_id": item_id,
            "content_index": 0,
            "transcript": transcript,
        })
        .to_string()
        .into(),
    )
}

fn error_frame(code: &str, message: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "type": "error",
            "event_id": "event_err",
            "error": { "type": "invalid_request_error", "code": code, "message": message },
        })
        .to_string()
        .into(),
    )
}

/// Read the next text frame from the mock side and parse it as JSON.
async fn read_json(ws: &mut ServerWs) -> serde_json::Value {
    match ws.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str(text.as_str()).expect("client sent invalid JSON"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

/// Read and validate the `session.update` event the adapter sends right after
/// the handshake; every handler must consume it before scripted frames.
async fn read_session_update(ws: &mut ServerWs) -> serde_json::Value {
    let value = read_json(ws).await;
    assert_eq!(value["type"], "session.update", "got {value}");
    value
}

/// Complete the close handshake from the server side after the client
/// initiated it, ignoring errors from an already-finished handshake.
async fn expect_client_close(ws: &mut ServerWs) {
    match ws.next().await {
        Some(Ok(Message::Close(_))) | None => {}
        other => panic!("expected client Close, got {other:?}"),
    }
    let _ = ws.close(None).await;
}

async fn connect(config: &OpenAISpeechToTextConfig, sample_rate: u32) -> OpenAIRealtimeStream {
    within(stt_stream_openai::connect(config, sample_rate, None))
        .await
        .unwrap()
}

async fn expect_event(stream: &mut OpenAIRealtimeStream) -> UpstreamEvent {
    within(stream.next_event())
        .await
        .expect("stream ended")
        .expect("stream error")
}

// -- Tests -------------------------------------------------------------------------

#[tokio::test]
async fn handshake_carries_transcription_intent_and_bearer_auth() {
    let (base_url, captured, handle) = spawn_server(|mut ws| async move {
        // Consume the session.update so the client's post-handshake send
        // cannot race a dropped socket.
        let _ = ws.next().await;
    })
    .await;

    let _stream = connect(&make_config(&base_url), 24000).await;
    within(handle).await.unwrap();

    let handshake = captured.lock().unwrap().clone().unwrap();
    assert_eq!(handshake.uri, "/v1/realtime?intent=transcription");
    assert_eq!(handshake.authorization.as_deref(), Some("Bearer sk-test"));
}

#[tokio::test]
async fn session_update_is_sent_first_with_model_and_pcm_format() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        let value = read_session_update(&mut ws).await;
        assert_eq!(value["session"]["type"], "transcription");
        assert_eq!(value["session"]["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(value["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["model"],
            "gpt-4o-transcribe"
        );
        // No language was configured or hinted, and no prompt was set.
        let transcription = &value["session"]["audio"]["input"]["transcription"];
        assert!(transcription.get("language").is_none());
        assert!(transcription.get("prompt").is_none());
    })
    .await;

    let _stream = connect(&make_config(&base_url), 24000).await;
    within(handle).await.unwrap();
}

#[tokio::test]
async fn session_update_carries_configured_prompt() {
    // The prompt steers transcription style (e.g. Simplified vs Traditional
    // script for `zh`), mirroring the file path's prompt forwarding.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        let value = read_session_update(&mut ws).await;
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["prompt"],
            "请使用简体中文输出"
        );
    })
    .await;

    let mut config = make_config(&base_url);
    config.prompt = Some("请使用简体中文输出".into());
    let _stream = connect(&config, 24000).await;
    within(handle).await.unwrap();
}

#[tokio::test]
async fn blank_prompt_is_omitted_from_session_update() {
    // Settings UI may save an unfilled prompt as "" — must not be forwarded.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        let value = read_session_update(&mut ws).await;
        assert!(
            value["session"]["audio"]["input"]["transcription"]
                .get("prompt")
                .is_none()
        );
    })
    .await;

    let mut config = make_config(&base_url);
    config.prompt = Some("   ".into());
    let _stream = connect(&config, 24000).await;
    within(handle).await.unwrap();
}

#[tokio::test]
async fn session_update_carries_normalized_language_hint() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        let value = read_session_update(&mut ws).await;
        // "zh-CN" must be normalized to "zh", mirroring the file path.
        assert_eq!(value["session"]["audio"]["input"]["transcription"]["language"], "zh");
    })
    .await;

    let _stream = within(stt_stream_openai::connect(
        &make_config(&base_url),
        24000,
        Some("zh-CN"),
    ))
    .await
    .unwrap();
    within(handle).await.unwrap();
}

#[tokio::test]
async fn audio_appends_arrive_base64_encoded() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        for expected in [vec![1u8, 2, 3], vec![0u8, 255, 128]] {
            let value = read_json(&mut ws).await;
            assert_eq!(value["type"], "input_audio_buffer.append", "got {value}");
            let audio = value["audio"].as_str().expect("audio field missing");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(audio)
                .expect("audio is not valid base64");
            assert_eq!(decoded, expected);
        }
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    within(stream.send_audio(&[1, 2, 3])).await.unwrap();
    within(stream.send_audio(&[0, 255, 128])).await.unwrap();
    within(handle).await.unwrap();
}

#[tokio::test]
async fn finish_sends_commit_event() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        let value = read_json(&mut ws).await;
        assert_eq!(value["type"], "input_audio_buffer.commit", "got {value}");
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    within(stream.finish()).await.unwrap();
    within(handle).await.unwrap();
}

#[tokio::test]
async fn deltas_accumulate_and_completed_resets_partial_buffer() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        ws.send(delta_frame("item_1", "he")).await.unwrap();
        ws.send(delta_frame("item_1", "llo")).await.unwrap();
        ws.send(completed_frame("item_1", "hello")).await.unwrap();
        // A new item after a completed transcript must start from scratch:
        // the first item's partial buffer was dropped by its completed event.
        ws.send(delta_frame("item_2", "wo")).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("he".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("hello".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("hello".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("wo".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn error_event_maps_to_request_failed() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        ws.send(error_frame("invalid_value", "boom")).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    let err = within(stream.next_event())
        .await
        .expect("expected an event")
        .expect_err("expected upstream error");
    assert_eq!(err.error_code(), "STT_REQUEST_FAILED");
    assert!(err.to_string().contains("boom"), "got: {err}");
    within(handle).await.unwrap();
}

#[tokio::test]
async fn lifecycle_events_are_skipped() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        for frame in [
            r#"{"type":"session.created","session":{}}"#,
            r#"{"type":"session.updated","session":{}}"#,
            r#"{"type":"input_audio_buffer.speech_started","audio_start_ms":10}"#,
            r#"{"type":"input_audio_buffer.speech_stopped","audio_end_ms":900}"#,
            r#"{"type":"conversation.item.created","item":{}}"#,
            r#"{"type":"rate_limits.updated","rate_limits":[]}"#,
        ] {
            ws.send(Message::Text(frame.into())).await.unwrap();
        }
        // `committed` is consumed for item tracking, not surfaced either.
        ws.send(committed_frame("item_1")).await.unwrap();
        ws.send(completed_frame("item_1", "hi")).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    // The bookkeeping frames must be swallowed: the first event is the real one.
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("hi".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn adapter_initiates_close_after_commit_and_final_transcript() {
    // OpenAI keeps the socket open after delivering the last `completed`
    // event; the adapter must initiate the close handshake itself so the
    // session can observe `Closed` and emit `Done`. The manual commit is
    // acknowledged with a `committed` event whose item then completes; the
    // mock stays silent after `completed` until the client's Close arrives.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        let value = read_json(&mut ws).await;
        assert_eq!(value["type"], "input_audio_buffer.commit");
        ws.send(committed_frame("item_1")).await.unwrap();
        ws.send(delta_frame("item_1", "done")).await.unwrap();
        ws.send(completed_frame("item_1", "done")).await.unwrap();
        expect_client_close(&mut ws).await;
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    within(stream.finish()).await.unwrap();
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("done".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("done".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn close_waits_for_tail_item_finalized_after_commit() {
    // Regression test for real-API tail loss: server VAD finalized a first
    // item BEFORE the manual commit; the commit then created a second item
    // holding the tail audio. The adapter must not treat the first item's
    // final as "done" — it has to wait until the tail item's transcript
    // arrives before initiating close, or the tail sentence is lost.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        // Server VAD segment: item_a is committed and finalized on its own.
        ws.send(committed_frame("item_a")).await.unwrap();
        ws.send(delta_frame("item_a", "one")).await.unwrap();
        ws.send(completed_frame("item_a", "one")).await.unwrap();
        // The manual commit arrives only now (client called finish()).
        let value = read_json(&mut ws).await;
        assert_eq!(value["type"], "input_audio_buffer.commit");
        // Its acknowledgement creates the tail item, still transcribing.
        ws.send(committed_frame("item_b")).await.unwrap();
        ws.send(delta_frame("item_b", "two")).await.unwrap();
        ws.send(completed_frame("item_b", "two")).await.unwrap();
        // Reaching this point only after sending item_b's completed proves
        // the adapter did not initiate close while item_b was pending; if it
        // had, the sends above would precede the Close in the stream and the
        // client-side event order assertions below would fail.
        expect_client_close(&mut ws).await;
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("one".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("one".into()));
    within(stream.finish()).await.unwrap();
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("two".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("two".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn interleaved_item_deltas_accumulate_per_item() {
    // Deltas from different items must not contaminate each other's
    // accumulated partial text.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        ws.send(delta_frame("item_a", "he")).await.unwrap();
        ws.send(delta_frame("item_b", "x")).await.unwrap();
        ws.send(delta_frame("item_a", "llo")).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("he".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("x".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("hello".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn commit_on_empty_buffer_after_finish_leads_to_closed() {
    // A commit with no (or too little) trailing audio yields a benign
    // `input_audio_buffer_commit_empty` error instead of a transcript; the
    // adapter must treat it as "nothing more coming" and close gracefully
    // rather than fail the session or wait forever.
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        let value = read_json(&mut ws).await;
        assert_eq!(value["type"], "input_audio_buffer.commit");
        ws.send(error_frame(
            "input_audio_buffer_commit_empty",
            "buffer too small. Expected at least 100ms of audio.",
        ))
        .await
        .unwrap();
        expect_client_close(&mut ws).await;
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 24000).await;
    within(stream.finish()).await.unwrap();
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn handshake_rejection_maps_to_request_failed_with_status() {
    // Plain HTTP server answering 401 to the upgrade request (bad API key path).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await;
        stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        stream.shutdown().await.ok();
    });

    let result = within(stt_stream_openai::connect(
        &make_config(&format!("http://{addr}")),
        24000,
        None,
    ))
    .await;
    let err = match result {
        Ok(_) => panic!("expected handshake failure"),
        Err(e) => e,
    };
    within(server).await.unwrap();

    assert_eq!(err.error_code(), "STT_REQUEST_FAILED");
    assert!(err.to_string().contains("401"), "got: {err}");
}

#[tokio::test]
async fn factory_rejects_missing_openai_config() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        auto_send: None,
        openai: None,
        deepgram: None,
    };
    let err = within(OpenAIRealtimeUpstreamFactory.connect(&config, 24000, None))
        .await
        .map(|_| ())
        .expect_err("expected missing-config error");
    assert!(matches!(err, SttError::OpenaiNotConfigured));
}

#[tokio::test]
async fn factory_rejects_empty_api_key() {
    let mut openai = make_config("http://127.0.0.1:1");
    openai.api_key = String::new();
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        auto_send: None,
        openai: Some(openai),
        deepgram: None,
    };
    let err = within(OpenAIRealtimeUpstreamFactory.connect(&config, 24000, None))
        .await
        .map(|_| ())
        .expect_err("expected missing-key error");
    assert!(matches!(err, SttError::OpenaiNotConfigured));
}

#[tokio::test]
async fn factory_connects_through_upstream_stream_trait() {
    let (base_url, captured, handle) = spawn_server(|mut ws| async move {
        read_session_update(&mut ws).await;
        ws.send(completed_frame("item_1", "via factory")).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Openai,
        auto_send: None,
        openai: Some(make_config(&base_url)),
        deepgram: None,
    };
    let mut stream = within(OpenAIRealtimeUpstreamFactory.connect(&config, 24000, None))
        .await
        .unwrap();

    assert_eq!(
        within(stream.next_event()).await.unwrap().unwrap(),
        UpstreamEvent::Final("via factory".into())
    );
    assert_eq!(
        within(stream.next_event()).await.unwrap().unwrap(),
        UpstreamEvent::Closed
    );
    within(handle).await.unwrap();

    let uri = captured.lock().unwrap().clone().unwrap().uri;
    assert_eq!(uri, "/v1/realtime?intent=transcription");
}
