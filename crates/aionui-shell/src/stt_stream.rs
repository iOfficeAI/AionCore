//! Transport-agnostic STT streaming session state machine.
//!
//! Drives one streaming transcription session: validates the client `start`
//! frame and the stored STT config, opens an upstream provider stream via an
//! injected factory, then pumps audio frames upstream and transcript events
//! back to the client until `stop`/`Done` or an error terminates the session.
//!
//! The session talks to the client through mpsc channels so it stays free of
//! any HTTP/WebSocket dependency: the route layer (Task 2A-5) pumps an axum
//! WebSocket into `ClientFrame`s and out of `SttStreamServerMessage`s, and
//! provider tasks (Tasks 3/4) implement [`UpstreamStream`]/[`UpstreamFactory`].

use aionui_api_types::{SpeechToTextConfig, SpeechToTextProvider, SttStreamClientMessage, SttStreamServerMessage};
use tokio::sync::mpsc;

use crate::error::SttError;

/// Inbound frames from the client connection, already transport-decoded.
#[derive(Debug)]
pub enum ClientFrame {
    /// JSON control frame (`start` / `stop`).
    Text(String),
    /// Raw PCM16 audio chunk.
    Binary(Vec<u8>),
    /// The client connection went away.
    Closed,
}

/// Transcript events surfaced by an upstream provider connection.
#[derive(Debug, PartialEq)]
pub enum UpstreamEvent {
    Partial(String),
    Final(String),
    /// Upstream finished cleanly (all final transcripts delivered).
    Closed,
}

/// Minimal interface the session needs from an upstream STT stream.
#[async_trait::async_trait]
pub trait UpstreamStream: Send {
    async fn send_audio(&mut self, pcm: &[u8]) -> Result<(), SttError>;

    /// Signal end of audio (OpenAI: buffer commit; Deepgram: CloseStream).
    async fn finish(&mut self) -> Result<(), SttError>;

    /// Next transcript event.
    ///
    /// Must be cancel-safe: the session polls this inside a `select!` loop and
    /// may drop the in-flight future at any await point. Implementations must
    /// not hold a decoded event across an await or perform multi-await work
    /// between reading and returning; buffer internally if needed.
    /// Returning `None` is treated like [`UpstreamEvent::Closed`].
    async fn next_event(&mut self) -> Option<Result<UpstreamEvent, SttError>>;
}

/// Factory that opens an upstream connection for a validated config.
///
/// Boxed return so tests can inject mocks and Tasks 3/4 can plug in the real
/// OpenAI/Deepgram providers.
#[async_trait::async_trait]
pub trait UpstreamFactory: Send + Sync {
    async fn connect(
        &self,
        config: &SpeechToTextConfig,
        sample_rate: u32,
        language_hint: Option<&str>,
    ) -> Result<Box<dyn UpstreamStream>, SttError>;
}

/// OpenAI models that must NOT use the streaming path (file-endpoint only).
const NON_STREAMING_OPENAI_MODELS: &[&str] = &["whisper-1"];

/// Build the protocol `error` frame for an [`SttError`].
fn error_frame(e: &SttError) -> SttStreamServerMessage {
    SttStreamServerMessage::Error {
        code: e.error_code().to_owned(),
        msg: e.to_string(),
    }
}

/// Validated parameters extracted from the client `start` frame.
struct StartParams {
    sample_rate: u32,
    language_hint: Option<String>,
}

/// Parse and validate the first client frame, which must be a `start` control
/// frame describing a mono PCM16 stream.
fn parse_start_frame(frame: ClientFrame) -> Result<Option<StartParams>, SttError> {
    let text = match frame {
        ClientFrame::Text(text) => text,
        ClientFrame::Binary(_) => {
            return Err(SttError::StreamProtocol(
                "expected start frame, got binary audio".into(),
            ));
        }
        ClientFrame::Closed => return Ok(None),
    };

    let message: SttStreamClientMessage =
        serde_json::from_str(&text).map_err(|e| SttError::StreamProtocol(format!("invalid control frame: {e}")))?;

    match message {
        SttStreamClientMessage::Start {
            format,
            sample_rate,
            channels,
            language_hint,
        } => {
            if format != "pcm16" {
                return Err(SttError::StreamProtocol(format!("unsupported audio format: {format}")));
            }
            if channels != 1 {
                return Err(SttError::StreamProtocol(format!(
                    "unsupported channel count: {channels} (expected 1)"
                )));
            }
            Ok(Some(StartParams {
                sample_rate,
                language_hint,
            }))
        }
        SttStreamClientMessage::Stop => Err(SttError::StreamProtocol("expected start frame, got stop".into())),
    }
}

/// Validate the stored STT config for streaming use.
///
/// Mirrors the file-endpoint checks (`stt.rs` / `stt_openai.rs` /
/// `stt_deepgram.rs`): disabled config and missing provider config or empty
/// API key are rejected — an empty API key is rejected even when a custom
/// `base_url` is set, matching `stt_openai::transcribe`. Additionally rejects
/// OpenAI models that only support the file endpoint.
fn validate_config(config: &SpeechToTextConfig) -> Result<(), SttError> {
    if !config.enabled {
        return Err(SttError::Disabled);
    }

    match config.provider {
        SpeechToTextProvider::Openai => {
            let openai = config.openai.as_ref().ok_or(SttError::OpenaiNotConfigured)?;
            if openai.api_key.is_empty() {
                return Err(SttError::OpenaiNotConfigured);
            }
            if NON_STREAMING_OPENAI_MODELS.contains(&openai.model.as_str()) {
                return Err(SttError::StreamUnsupported);
            }
        }
        SpeechToTextProvider::Deepgram => {
            let deepgram = config.deepgram.as_ref().ok_or(SttError::DeepgramNotConfigured)?;
            if deepgram.api_key.is_empty() {
                return Err(SttError::DeepgramNotConfigured);
            }
        }
    }

    Ok(())
}

/// Language configured on the active provider, which takes precedence over
/// the client `start` frame's language hint.
fn config_language(config: &SpeechToTextConfig) -> Option<&str> {
    let language = match config.provider {
        SpeechToTextProvider::Openai => config.openai.as_ref().and_then(|c| c.language.as_deref()),
        SpeechToTextProvider::Deepgram => config.deepgram.as_ref().and_then(|c| c.language.as_deref()),
    };
    language.map(str::trim).filter(|s| !s.is_empty())
}

/// Run one streaming session to completion.
///
/// The function owns the session lifecycle: it consumes client frames from
/// `incoming`, emits protocol frames on `send`, and returns when the session
/// terminates (after `done`, an `error` frame, or a client disconnect).
/// Send failures on `send` mean the client side is gone, so they end the
/// session silently.
pub async fn run_stream_session(
    mut incoming: mpsc::Receiver<ClientFrame>,
    send: mpsc::Sender<SttStreamServerMessage>,
    config: SpeechToTextConfig,
    factory: &dyn UpstreamFactory,
) {
    // -- 1. Await and validate the start frame -----------------------------
    let Some(first) = incoming.recv().await else {
        return; // channel closed before any frame
    };
    let start = match parse_start_frame(first) {
        Ok(Some(start)) => start,
        Ok(None) => return, // client closed before starting
        Err(e) => {
            tracing::warn!(error = %e, "stt stream: rejected start frame");
            let _ = send.send(error_frame(&e)).await;
            return;
        }
    };

    // -- 2. Validate config and streaming capability -----------------------
    if let Err(e) = validate_config(&config) {
        let _ = send.send(error_frame(&e)).await;
        return;
    }

    // -- 3. Connect upstream ------------------------------------------------
    // User-configured language takes precedence over the client languageHint,
    // mirroring the file-path precedence (stt_openai::transcribe): it lets
    // users override the browser's locale.
    let language_hint =
        config_language(&config).or_else(|| start.language_hint.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let mut upstream = match factory.connect(&config, start.sample_rate, language_hint).await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(error = %e, "stt stream: upstream connect failed");
            let _ = send.send(error_frame(&e)).await;
            return;
        }
    };

    if send.send(SttStreamServerMessage::Ready).await.is_err() {
        return;
    }

    // -- 4. Pump loop --------------------------------------------------------
    // After `stop` we keep draining upstream events but no longer poll the
    // client; the loop ends when upstream closes, errors, or the client
    // disconnects before stopping.
    let mut stopping = false;
    loop {
        tokio::select! {
            frame = incoming.recv(), if !stopping => {
                match frame {
                    Some(ClientFrame::Binary(pcm)) => {
                        if let Err(e) = upstream.send_audio(&pcm).await {
                            let _ = send.send(error_frame(&e)).await;
                            return;
                        }
                    }
                    Some(ClientFrame::Text(text)) => {
                        match serde_json::from_str::<SttStreamClientMessage>(&text) {
                            Ok(SttStreamClientMessage::Stop) => {
                                if let Err(e) = upstream.finish().await {
                                    let _ = send.send(error_frame(&e)).await;
                                    return;
                                }
                                stopping = true;
                            }
                            _ => {
                                let e = SttError::StreamProtocol("unexpected control frame mid-session".into());
                                tracing::warn!(error = %e, "stt stream: protocol violation");
                                let _ = send.send(error_frame(&e)).await;
                                return;
                            }
                        }
                    }
                    // Client went away before stop: abort silently, drop upstream.
                    Some(ClientFrame::Closed) | None => return,
                }
            }
            event = upstream.next_event() => {
                match event {
                    Some(Ok(UpstreamEvent::Partial(text))) => {
                        if send.send(SttStreamServerMessage::Partial { text }).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(UpstreamEvent::Final(text))) => {
                        if send.send(SttStreamServerMessage::Final { text }).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(UpstreamEvent::Closed)) | None => {
                        if stopping {
                            let _ = send.send(SttStreamServerMessage::Done).await;
                        } else {
                            let e = SttError::RequestFailed("upstream closed unexpectedly".into());
                            tracing::warn!(error = %e, "stt stream: upstream closed before stop");
                            let _ = send.send(error_frame(&e)).await;
                        }
                        return;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "stt stream: upstream error");
                        let _ = send.send(error_frame(&e)).await;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // -- Mocks ---------------------------------------------------------------

    /// Arguments captured from `UpstreamFactory::connect`.
    #[derive(Debug, Clone, PartialEq)]
    struct ConnectArgs {
        sample_rate: u32,
        language_hint: Option<String>,
    }

    struct MockUpstream {
        audio: Arc<Mutex<Vec<Vec<u8>>>>,
        finish_called: Arc<AtomicBool>,
        /// Events emitted by `next_event`, in order.
        events: Vec<Result<UpstreamEvent, SttError>>,
        /// When true, `next_event` pends until `finish` has been called,
        /// making the happy-path ordering deterministic.
        emit_only_after_finish: bool,
    }

    #[async_trait::async_trait]
    impl UpstreamStream for MockUpstream {
        async fn send_audio(&mut self, pcm: &[u8]) -> Result<(), SttError> {
            self.audio.lock().unwrap().push(pcm.to_vec());
            Ok(())
        }

        async fn finish(&mut self) -> Result<(), SttError> {
            self.finish_called.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn next_event(&mut self) -> Option<Result<UpstreamEvent, SttError>> {
            if self.emit_only_after_finish && !self.finish_called.load(Ordering::SeqCst) {
                // Pend forever; cancel-safe because it holds no state.
                std::future::pending::<()>().await;
            }
            if self.events.is_empty() {
                None
            } else {
                Some(self.events.remove(0))
            }
        }
    }

    struct MockFactory {
        upstream: Mutex<Option<MockUpstream>>,
        connect_error: Mutex<Option<SttError>>,
        connect_args: Arc<Mutex<Option<ConnectArgs>>>,
    }

    impl MockFactory {
        fn with_upstream(upstream: MockUpstream) -> Self {
            Self {
                upstream: Mutex::new(Some(upstream)),
                connect_error: Mutex::new(None),
                connect_args: Arc::new(Mutex::new(None)),
            }
        }

        fn with_error(error: SttError) -> Self {
            Self {
                upstream: Mutex::new(None),
                connect_error: Mutex::new(Some(error)),
                connect_args: Arc::new(Mutex::new(None)),
            }
        }

        fn connect_called(&self) -> bool {
            self.connect_args.lock().unwrap().is_some()
        }
    }

    #[async_trait::async_trait]
    impl UpstreamFactory for MockFactory {
        async fn connect(
            &self,
            _config: &SpeechToTextConfig,
            sample_rate: u32,
            language_hint: Option<&str>,
        ) -> Result<Box<dyn UpstreamStream>, SttError> {
            *self.connect_args.lock().unwrap() = Some(ConnectArgs {
                sample_rate,
                language_hint: language_hint.map(str::to_owned),
            });
            if let Some(e) = self.connect_error.lock().unwrap().take() {
                return Err(e);
            }
            let upstream = self
                .upstream
                .lock()
                .unwrap()
                .take()
                .expect("mock upstream already consumed");
            Ok(Box::new(upstream))
        }
    }

    // -- Helpers --------------------------------------------------------------

    fn make_openai_config(model: &str) -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Openai,
            auto_send: None,
            openai: Some(OpenAISpeechToTextConfig {
                api_key: "sk-test".into(),
                base_url: None,
                model: model.into(),
                language: None,
                prompt: None,
                temperature: None,
            }),
            deepgram: None,
        }
    }

    fn make_deepgram_config(api_key: &str) -> SpeechToTextConfig {
        SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Deepgram,
            auto_send: None,
            openai: None,
            deepgram: Some(DeepgramSpeechToTextConfig {
                api_key: api_key.to_owned(),
                base_url: None,
                model: "nova-2".into(),
                language: None,
                detect_language: None,
                punctuate: None,
                smart_format: None,
            }),
        }
    }

    fn start_frame(language_hint: Option<&str>) -> ClientFrame {
        let hint = language_hint.map_or(String::new(), |h| format!(r#","languageHint":"{h}""#));
        ClientFrame::Text(format!(
            r#"{{"type":"start","format":"pcm16","sampleRate":16000,"channels":1{hint}}}"#
        ))
    }

    fn stop_frame() -> ClientFrame {
        ClientFrame::Text(r#"{"type":"stop"}"#.into())
    }

    /// Run a session with pre-queued client frames and collect all server
    /// messages. The client sender is kept alive until the session returns so
    /// an idle `incoming` channel pends instead of reading as closed.
    async fn run_with_frames(
        frames: Vec<ClientFrame>,
        config: SpeechToTextConfig,
        factory: &MockFactory,
    ) -> Vec<SttStreamServerMessage> {
        let (client_tx, client_rx) = mpsc::channel(64);
        let (server_tx, mut server_rx) = mpsc::channel(64);
        for frame in frames {
            client_tx.send(frame).await.unwrap();
        }
        run_stream_session(client_rx, server_tx, config, factory).await;
        drop(client_tx);
        let mut messages = Vec::new();
        while let Some(msg) = server_rx.recv().await {
            messages.push(msg);
        }
        messages
    }

    fn assert_error_code(msg: &SttStreamServerMessage, expected: &str) {
        match msg {
            SttStreamServerMessage::Error { code, .. } => assert_eq!(code, expected),
            other => panic!("expected error frame with code {expected}, got {other:?}"),
        }
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn happy_path_streams_audio_and_finishes_with_done() {
        let audio = Arc::new(Mutex::new(Vec::new()));
        let finish_called = Arc::new(AtomicBool::new(false));
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: audio.clone(),
            finish_called: finish_called.clone(),
            events: vec![
                Ok(UpstreamEvent::Partial("hel".into())),
                Ok(UpstreamEvent::Final("hello".into())),
                Ok(UpstreamEvent::Closed),
            ],
            emit_only_after_finish: true,
        });

        let frames = vec![
            start_frame(None),
            ClientFrame::Binary(vec![1, 2]),
            ClientFrame::Binary(vec![3, 4]),
            stop_frame(),
        ];
        let messages = run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        assert!(matches!(messages[0], SttStreamServerMessage::Ready));
        assert!(matches!(&messages[1], SttStreamServerMessage::Partial { text } if text == "hel"));
        assert!(matches!(&messages[2], SttStreamServerMessage::Final { text } if text == "hello"));
        assert!(matches!(messages[3], SttStreamServerMessage::Done));
        assert_eq!(messages.len(), 4);
        assert_eq!(*audio.lock().unwrap(), vec![vec![1, 2], vec![3, 4]]);
        assert!(finish_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn binary_first_frame_is_protocol_error_without_connect() {
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let frames = vec![ClientFrame::Binary(vec![1, 2, 3])];
        let messages = run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_STREAM_PROTOCOL");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn unsupported_format_is_protocol_error() {
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let frames = vec![ClientFrame::Text(
            r#"{"type":"start","format":"webm","sampleRate":16000,"channels":1}"#.into(),
        )];
        let messages = run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_STREAM_PROTOCOL");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn disabled_config_is_rejected_without_connect() {
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let mut config = make_openai_config("gpt-4o-transcribe");
        config.enabled = false;
        let messages = run_with_frames(vec![start_frame(None)], config, &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_DISABLED");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn empty_api_key_is_rejected_even_with_custom_base_url() {
        // Mirrors stt_openai::transcribe: empty API key is not configured,
        // regardless of base_url.
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let mut config = make_openai_config("gpt-4o-transcribe");
        config.openai.as_mut().unwrap().api_key = String::new();
        config.openai.as_mut().unwrap().base_url = Some("https://api.groq.com/openai".into());
        let messages = run_with_frames(vec![start_frame(None)], config, &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_OPENAI_NOT_CONFIGURED");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn deepgram_empty_api_key_is_rejected_without_connect() {
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let messages = run_with_frames(vec![start_frame(None)], make_deepgram_config(""), &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_DEEPGRAM_NOT_CONFIGURED");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn non_streaming_model_is_rejected_without_connect() {
        let factory = MockFactory::with_error(SttError::Unknown("must not connect".into()));
        let messages = run_with_frames(vec![start_frame(None)], make_openai_config("whisper-1"), &factory).await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_STREAM_UNSUPPORTED");
        assert!(!factory.connect_called());
    }

    #[tokio::test]
    async fn connect_failure_maps_to_error_frame() {
        let factory = MockFactory::with_error(SttError::RequestFailed("dial failed".into()));
        let messages = run_with_frames(
            vec![start_frame(None)],
            make_openai_config("gpt-4o-transcribe"),
            &factory,
        )
        .await;

        assert_eq!(messages.len(), 1);
        assert_error_code(&messages[0], "STT_REQUEST_FAILED");
    }

    #[tokio::test]
    async fn upstream_error_mid_stream_ends_session_with_error_frame() {
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Err(SttError::RequestFailed("boom".into()))],
            emit_only_after_finish: false,
        });

        // Keep the client sender alive so the session ends via the upstream
        // error, not via a closed client channel.
        let (client_tx, client_rx) = mpsc::channel(8);
        let (server_tx, mut server_rx) = mpsc::channel(8);
        client_tx.send(start_frame(None)).await.unwrap();
        run_stream_session(client_rx, server_tx, make_openai_config("gpt-4o-transcribe"), &factory).await;
        drop(client_tx);

        assert!(matches!(server_rx.recv().await, Some(SttStreamServerMessage::Ready)));
        assert_error_code(&server_rx.recv().await.unwrap(), "STT_REQUEST_FAILED");
        assert!(server_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn upstream_closed_before_stop_is_request_failed() {
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Ok(UpstreamEvent::Closed)],
            emit_only_after_finish: false,
        });

        let (client_tx, client_rx) = mpsc::channel(8);
        let (server_tx, mut server_rx) = mpsc::channel(8);
        client_tx.send(start_frame(None)).await.unwrap();
        run_stream_session(client_rx, server_tx, make_openai_config("gpt-4o-transcribe"), &factory).await;
        drop(client_tx);

        assert!(matches!(server_rx.recv().await, Some(SttStreamServerMessage::Ready)));
        assert_error_code(&server_rx.recv().await.unwrap(), "STT_REQUEST_FAILED");
        assert!(server_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn client_closed_mid_stream_aborts_silently() {
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Ok(UpstreamEvent::Partial("ignored".into()))],
            emit_only_after_finish: true,
        });

        let frames = vec![start_frame(None), ClientFrame::Binary(vec![9]), ClientFrame::Closed];
        let messages = run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        // Only Ready: no Done and no Error after the client disconnect.
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], SttStreamServerMessage::Ready));
    }

    #[tokio::test]
    async fn mid_session_start_frame_is_protocol_error() {
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![],
            emit_only_after_finish: true,
        });

        let frames = vec![start_frame(None), start_frame(None)];
        let messages = run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], SttStreamServerMessage::Ready));
        assert_error_code(&messages[1], "STT_STREAM_PROTOCOL");
    }

    #[tokio::test]
    async fn language_hint_and_sample_rate_are_passed_to_factory() {
        // No config language: the start-frame hint is forwarded as-is.
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Ok(UpstreamEvent::Closed)],
            emit_only_after_finish: true,
        });

        let frames = vec![start_frame(Some("zh-CN")), stop_frame()];
        run_with_frames(frames, make_openai_config("gpt-4o-transcribe"), &factory).await;

        let args = factory.connect_args.lock().unwrap().clone().unwrap();
        assert_eq!(
            args,
            ConnectArgs {
                sample_rate: 16000,
                language_hint: Some("zh-CN".into()),
            }
        );
    }

    #[tokio::test]
    async fn config_language_takes_precedence_over_start_frame_hint() {
        // Mirrors the file-path precedence: user-configured language wins
        // over the client's languageHint.
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Ok(UpstreamEvent::Closed)],
            emit_only_after_finish: true,
        });

        let mut config = make_openai_config("gpt-4o-transcribe");
        config.openai.as_mut().unwrap().language = Some("es".into());
        let frames = vec![start_frame(Some("zh-CN")), stop_frame()];
        run_with_frames(frames, config, &factory).await;

        let args = factory.connect_args.lock().unwrap().clone().unwrap();
        assert_eq!(args.language_hint, Some("es".into()));
    }

    #[tokio::test]
    async fn empty_config_language_falls_back_to_start_frame_hint() {
        // Settings UI may save unfilled language as "" — must not shadow the hint.
        let factory = MockFactory::with_upstream(MockUpstream {
            audio: Arc::new(Mutex::new(Vec::new())),
            finish_called: Arc::new(AtomicBool::new(false)),
            events: vec![Ok(UpstreamEvent::Closed)],
            emit_only_after_finish: true,
        });

        let mut config = make_openai_config("gpt-4o-transcribe");
        config.openai.as_mut().unwrap().language = Some(String::new());
        let frames = vec![start_frame(Some("zh-CN")), stop_frame()];
        run_with_frames(frames, config, &factory).await;

        let args = factory.connect_args.lock().unwrap().clone().unwrap();
        assert_eq!(args.language_hint, Some("zh-CN".into()));
    }
}
