//! Integration tests for the Deepgram live upstream.
//!
//! wiremock does not support WebSocket, so these tests run a small local
//! tokio-tungstenite server: the handshake callback captures the request URI
//! and headers, and each test drives a scripted frame exchange.
//!
//! Hang-safety: every potentially blocking await is wrapped in a 5s timeout
//! (`within`), and mock handlers never wait for the client's close reply —
//! the client stops polling its WebSocket once it has seen `Closed`, so a
//! server-side drain loop would deadlock the test.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::{DeepgramSpeechToTextConfig, SpeechToTextConfig, SpeechToTextProvider};
use aionui_shell::stt_stream_deepgram::{self, DeepgramStream};
use aionui_shell::{DeepgramUpstreamFactory, SttError, UpstreamEvent, UpstreamFactory, UpstreamStream};
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

fn make_config(base_url: &str) -> DeepgramSpeechToTextConfig {
    DeepgramSpeechToTextConfig {
        api_key: "test-key".into(),
        base_url: Some(base_url.to_owned()),
        model: "nova-3".into(),
        language: None,
        detect_language: None,
        punctuate: None,
        smart_format: None,
    }
}

fn results_frame(transcript: &str, is_final: bool) -> Message {
    // Realistic shape of a Deepgram live `Results` frame.
    Message::Text(
        serde_json::json!({
            "type": "Results",
            "channel_index": [0, 1],
            "duration": 1.0,
            "start": 0.0,
            "is_final": is_final,
            "speech_final": is_final,
            "channel": {
                "alternatives": [{ "transcript": transcript, "confidence": 0.98, "words": [] }],
            },
        })
        .to_string()
        .into(),
    )
}

async fn connect(config: &DeepgramSpeechToTextConfig, sample_rate: u32) -> DeepgramStream {
    within(stt_stream_deepgram::connect(config, sample_rate, None))
        .await
        .unwrap()
}

async fn expect_event(stream: &mut DeepgramStream) -> UpstreamEvent {
    within(stream.next_event())
        .await
        .expect("stream ended")
        .expect("stream error")
}

// -- Tests -------------------------------------------------------------------------

#[tokio::test]
async fn handshake_carries_stream_params_and_token_auth() {
    let (base_url, captured, handle) = spawn_server(|_ws| async {}).await;

    let _stream = connect(&make_config(&base_url), 24000).await;
    within(handle).await.unwrap();

    let handshake = captured.lock().unwrap().clone().unwrap();
    assert!(handshake.uri.contains("/v1/listen"), "got {}", handshake.uri);
    assert!(handshake.uri.contains("encoding=linear16"), "got {}", handshake.uri);
    assert!(handshake.uri.contains("sample_rate=24000"), "got {}", handshake.uri);
    assert!(handshake.uri.contains("channels=1"), "got {}", handshake.uri);
    assert!(handshake.uri.contains("model=nova-3"), "got {}", handshake.uri);
    assert!(handshake.uri.contains("interim_results=true"), "got {}", handshake.uri);
    assert_eq!(handshake.authorization.as_deref(), Some("Token test-key"));
}

#[tokio::test]
async fn happy_flow_streams_audio_and_yields_partial_final_closed() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        // Audio arrives as raw binary frames with the exact PCM bytes.
        for expected in [vec![1u8, 2, 3], vec![4u8, 5, 6]] {
            match ws.next().await {
                Some(Ok(Message::Binary(bytes))) => assert_eq!(bytes.as_ref(), &expected[..]),
                other => panic!("expected binary audio frame, got {other:?}"),
            }
        }
        ws.send(results_frame("hel", false)).await.unwrap();
        ws.send(results_frame("hello", true)).await.unwrap();
        // finish() must arrive as Deepgram's CloseStream control message.
        match ws.next().await {
            Some(Ok(Message::Text(text))) => assert_eq!(text.as_str(), r#"{"type":"CloseStream"}"#),
            other => panic!("expected CloseStream text frame, got {other:?}"),
        }
        // Send Close and return; do NOT drain for the client's close reply —
        // the client stops polling after `Closed`, so draining would hang.
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 16000).await;
    within(stream.send_audio(&[1, 2, 3])).await.unwrap();
    within(stream.send_audio(&[4, 5, 6])).await.unwrap();

    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Partial("hel".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("hello".into()));

    within(stream.finish()).await.unwrap();
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn empty_transcript_results_are_skipped() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        ws.send(results_frame("", false)).await.unwrap();
        ws.send(results_frame("   ", false)).await.unwrap();
        ws.send(results_frame("hi", true)).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 16000).await;
    // The two empty frames must be swallowed: the first event is the real one.
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("hi".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn metadata_and_unknown_frames_are_skipped() {
    let (base_url, _captured, handle) = spawn_server(|mut ws| async move {
        ws.send(Message::Text(
            r#"{"type":"Metadata","request_id":"abc","model_info":{}}"#.into(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(r#"{"type":"SpeechStarted","timestamp":0.1}"#.into()))
            .await
            .unwrap();
        ws.send(Message::Text(r#"{"type":"FutureFrameKind"}"#.into()))
            .await
            .unwrap();
        ws.send(results_frame("done", true)).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let mut stream = connect(&make_config(&base_url), 16000).await;
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Final("done".into()));
    assert_eq!(expect_event(&mut stream).await, UpstreamEvent::Closed);
    within(handle).await.unwrap();
}

#[tokio::test]
async fn abrupt_server_close_surfaces_request_failed() {
    // Handler returns immediately: the socket is dropped without a Close
    // frame, which tungstenite reports as a protocol error.
    let (base_url, _captured, handle) = spawn_server(|_ws| async {}).await;

    let mut stream = connect(&make_config(&base_url), 16000).await;
    within(handle).await.unwrap();

    let err = within(stream.next_event())
        .await
        .expect("expected an event")
        .expect_err("expected transport error");
    assert_eq!(err.error_code(), "STT_REQUEST_FAILED");
}

#[tokio::test]
async fn no_language_streams_in_multi_mode_instead_of_detect_language() {
    // Deepgram language detection is batch-only; on live streams it silently
    // falls back to the English model. Without an explicit language the
    // adapter must request multilingual code-switching mode instead — even
    // when the config asks for detection.
    let (base_url, captured, handle) = spawn_server(|_ws| async {}).await;

    let mut config = make_config(&base_url);
    config.detect_language = Some(true);
    let _stream = connect(&config, 16000).await;
    within(handle).await.unwrap();

    let uri = captured.lock().unwrap().clone().unwrap().uri;
    assert!(!uri.contains("detect_language="), "got {uri}");
    assert!(uri.contains("language=multi"), "got {uri}");
    assert!(uri.contains("endpointing=100"), "got {uri}");
}

#[tokio::test]
async fn explicit_language_suppresses_multi_mode() {
    let (base_url, captured, handle) = spawn_server(|_ws| async {}).await;

    let mut config = make_config(&base_url);
    config.detect_language = Some(true);
    config.language = Some("zh-CN".into());
    let _stream = connect(&config, 16000).await;
    within(handle).await.unwrap();

    let uri = captured.lock().unwrap().clone().unwrap().uri;
    assert!(uri.contains("language=zh-CN"), "got {uri}");
    assert!(!uri.contains("language=multi"), "got {uri}");
    assert!(!uri.contains("endpointing="), "got {uri}");
    assert!(!uri.contains("detect_language="), "got {uri}");
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

    let result = within(stt_stream_deepgram::connect(
        &make_config(&format!("http://{addr}")),
        16000,
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
async fn factory_rejects_missing_deepgram_config() {
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        auto_send: None,
        openai: None,
        deepgram: None,
    };
    let err = within(DeepgramUpstreamFactory.connect(&config, 16000, None))
        .await
        .map(|_| ())
        .expect_err("expected missing-config error");
    assert!(matches!(err, SttError::DeepgramNotConfigured));
}

#[tokio::test]
async fn factory_rejects_empty_api_key() {
    let mut deepgram = make_config("http://127.0.0.1:1");
    deepgram.api_key = String::new();
    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        auto_send: None,
        openai: None,
        deepgram: Some(deepgram),
    };
    let err = within(DeepgramUpstreamFactory.connect(&config, 16000, None))
        .await
        .map(|_| ())
        .expect_err("expected missing-key error");
    assert!(matches!(err, SttError::DeepgramNotConfigured));
}

#[tokio::test]
async fn factory_connects_through_upstream_stream_trait() {
    let (base_url, captured, handle) = spawn_server(|mut ws| async move {
        ws.send(results_frame("via factory", true)).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let config = SpeechToTextConfig {
        enabled: true,
        provider: SpeechToTextProvider::Deepgram,
        auto_send: None,
        openai: None,
        deepgram: Some(make_config(&base_url)),
    };
    let mut stream = within(DeepgramUpstreamFactory.connect(&config, 16000, Some("zh-CN")))
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
    assert!(uri.contains("language=zh-CN"), "got {uri}");
}
