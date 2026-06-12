//! Deepgram live (WebSocket) upstream for STT streaming.
//!
//! Connects to Deepgram's realtime `/v1/listen` endpoint over WebSocket and
//! adapts it to the transport-agnostic [`UpstreamStream`] interface driven by
//! `stt_stream::run_stream_session`. Base-URL resolution, auth scheme, and
//! query-parameter behavior mirror the file-endpoint implementation in
//! `stt_deepgram.rs`, with streaming-specific additions (`encoding`,
//! `sample_rate`, `channels`, `interim_results`).

use aionui_api_types::{DeepgramSpeechToTextConfig, SpeechToTextConfig};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::SttError;
use crate::stt_deepgram::resolve_base_url;
use crate::stt_stream::{UpstreamEvent, UpstreamFactory, UpstreamStream};

/// Deepgram leg of the upstream factory.
///
/// Kept as a per-provider unit struct so the route layer (Task 2A-5) can
/// either use it directly or compose it into a provider-dispatch factory that
/// matches on `config.provider` and delegates here for Deepgram.
pub struct DeepgramUpstreamFactory;

#[async_trait::async_trait]
impl UpstreamFactory for DeepgramUpstreamFactory {
    async fn connect(
        &self,
        config: &SpeechToTextConfig,
        sample_rate: u32,
        language_hint: Option<&str>,
    ) -> Result<Box<dyn UpstreamStream>, SttError> {
        // The session validates the config before connecting, but re-check
        // here so the factory is safe standalone (mirrors stt_deepgram::transcribe).
        let deepgram = config.deepgram.as_ref().ok_or(SttError::DeepgramNotConfigured)?;
        if deepgram.api_key.is_empty() {
            return Err(SttError::DeepgramNotConfigured);
        }
        Ok(Box::new(connect(deepgram, sample_rate, language_hint).await?))
    }
}

/// Open a live transcription WebSocket to Deepgram.
///
/// `sample_rate` describes the mono PCM16 audio the client will stream;
/// `language_hint` is the already-resolved language (config language wins
/// over the client hint upstream of this call, but the same precedence is
/// re-applied here to mirror the file path and stay safe standalone).
pub async fn connect(
    config: &DeepgramSpeechToTextConfig,
    sample_rate: u32,
    language_hint: Option<&str>,
) -> Result<DeepgramStream, SttError> {
    let url = build_ws_url(config, sample_rate, language_hint);
    let mut request = url
        .clone()
        .into_client_request()
        .map_err(|e| SttError::RequestFailed(format!("invalid Deepgram WS URL {url}: {e}")))?;
    let auth = HeaderValue::from_str(&format!("Token {}", config.api_key))
        .map_err(|e| SttError::RequestFailed(format!("invalid Deepgram API key for Authorization header: {e}")))?;
    request.headers_mut().insert("Authorization", auth);

    let connector = crate::stt_stream_tls::build_ws_connector()?;
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
        .await
        .map_err(connect_error)?;
    Ok(DeepgramStream { ws })
}

/// Map a handshake/connect failure, surfacing the HTTP status (e.g. 401 for
/// a bad API key) when Deepgram rejected the upgrade.
fn connect_error(e: tungstenite::Error) -> SttError {
    match e {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            let body = response
                .body()
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            SttError::RequestFailed(format!("Deepgram WS handshake returned {status}: {body}"))
        }
        other => SttError::RequestFailed(format!("Deepgram WS connect error: {other}")),
    }
}

/// Build the `wss://.../v1/listen?...` URL.
///
/// Query parameters mirror `stt_deepgram::transcribe` where applicable: an
/// explicit language (hint, falling back to config) wins;
/// `punctuate`/`smart_format` are sent only when `true`. Streaming additions:
/// `encoding`/`sample_rate`/`channels` describe the raw PCM16 input, and
/// `interim_results=true` enables partial transcripts.
///
/// Divergence from the file path: Deepgram language detection
/// (`detect_language`) is batch-only and silently falls back to the English
/// model on live streams. When no explicit language resolves, streaming uses
/// `language=multi` (nova-2/nova-3 multilingual code-switching mode) with
/// Deepgram's recommended `endpointing=100`; the `detect_language` config
/// flag is irrelevant for streaming.
fn build_ws_url(config: &DeepgramSpeechToTextConfig, sample_rate: u32, language_hint: Option<&str>) -> String {
    let base = resolve_base_url(config.base_url.as_deref());
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a ws:// / wss:// custom base: pass through unchanged.
        base.to_owned()
    };

    let mut params = vec![
        ("encoding", "linear16".to_owned()),
        ("sample_rate", sample_rate.to_string()),
        ("channels", "1".to_owned()),
        ("interim_results", "true".to_owned()),
        ("model", config.model.clone()),
    ];

    let language = language_hint.or(config.language.as_deref()).filter(|s| !s.is_empty());
    if let Some(lang) = language {
        params.push(("language", lang.to_owned()));
    } else {
        // Deepgram language detection is batch-only; streaming uses
        // language=multi (code-switching), with endpointing=100 as
        // recommended by Deepgram for multilingual streams.
        params.push(("language", "multi".to_owned()));
        params.push(("endpointing", "100".to_owned()));
    }

    if config.punctuate == Some(true) {
        params.push(("punctuate", "true".to_owned()));
    }

    if config.smart_format == Some(true) {
        params.push(("smart_format", "true".to_owned()));
    }

    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{ws_base}/v1/listen?{query}")
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass through).
///
/// Values are user-controlled (model/language from settings), so they cannot
/// be trusted to be URL-safe; reqwest did this for the file path.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Live Deepgram WebSocket adapted to [`UpstreamStream`].
pub struct DeepgramStream {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

/// Decode one Deepgram text frame into a transcript event, or `None` for
/// frames the session does not care about (metadata, empty transcripts,
/// unknown types). Synchronous on purpose: `next_event` must not hold a
/// decoded event across an await.
fn parse_text_frame(text: &str) -> Option<UpstreamEvent> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(error = %e, "stt deepgram stream: ignoring unparseable text frame");
            return None;
        }
    };

    match value["type"].as_str() {
        Some("Results") => {
            let transcript = value["channel"]["alternatives"]
                .get(0)
                .and_then(|alt| alt["transcript"].as_str())
                .unwrap_or("");
            if transcript.trim().is_empty() {
                return None;
            }
            if value["is_final"].as_bool() == Some(true) {
                Some(UpstreamEvent::Final(transcript.to_owned()))
            } else {
                Some(UpstreamEvent::Partial(transcript.to_owned()))
            }
        }
        // Bookkeeping frames the session has no use for.
        Some("Metadata" | "UtteranceEnd" | "SpeechStarted") => None,
        other => {
            tracing::debug!(frame_type = ?other, "stt deepgram stream: ignoring unknown frame type");
            None
        }
    }
}

#[async_trait::async_trait]
impl UpstreamStream for DeepgramStream {
    async fn send_audio(&mut self, pcm: &[u8]) -> Result<(), SttError> {
        self.ws
            .send(Message::Binary(pcm.to_vec().into()))
            .await
            .map_err(|e| SttError::RequestFailed(format!("Deepgram audio send failed: {e}")))
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        self.ws
            .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
            .await
            .map_err(|e| SttError::RequestFailed(format!("Deepgram CloseStream send failed: {e}")))
    }

    // Cancel-safe: each loop iteration is a single `ws.next().await` followed
    // by synchronous parsing — nothing is held across an await, and
    // `StreamExt::next` on the WebSocket stream is itself cancel-safe.
    async fn next_event(&mut self) -> Option<Result<UpstreamEvent, SttError>> {
        loop {
            match self.ws.next().await {
                // Stream ended after a close handshake: clean shutdown.
                None | Some(Ok(Message::Close(_))) => return Some(Ok(UpstreamEvent::Closed)),
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_text_frame(text.as_str()) {
                        return Some(Ok(event));
                    }
                }
                // Pings are answered by tungstenite automatically; Deepgram
                // sends no binary frames we consume.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    return Some(Err(SttError::RequestFailed(format!("Deepgram stream error: {e}"))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> DeepgramSpeechToTextConfig {
        DeepgramSpeechToTextConfig {
            api_key: "test-key".into(),
            base_url: None,
            model: "nova-3".into(),
            language: None,
            detect_language: None,
            punctuate: None,
            smart_format: None,
        }
    }

    // -- build_ws_url ---------------------------------------------------------

    #[test]
    fn url_uses_default_base_with_wss_scheme_and_stream_params() {
        let url = build_ws_url(&make_config(), 24000, None);
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"), "got {url}");
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("sample_rate=24000"));
        assert!(url.contains("channels=1"));
        assert!(url.contains("interim_results=true"));
        assert!(url.contains("model=nova-3"));
        // No explicit language: multilingual code-switching mode.
        assert!(url.contains("language=multi"), "got {url}");
        assert!(url.contains("endpointing=100"), "got {url}");
        assert!(!url.contains("punctuate="));
        assert!(!url.contains("smart_format="));
    }

    #[test]
    fn url_swaps_http_base_to_ws_and_strips_trailing_slash() {
        let mut config = make_config();
        config.base_url = Some("http://127.0.0.1:9999/".into());
        let url = build_ws_url(&config, 16000, None);
        assert!(url.starts_with("ws://127.0.0.1:9999/v1/listen?"), "got {url}");
    }

    #[test]
    fn url_blank_base_falls_back_to_default() {
        // Settings UI saves unfilled base_url as "" — mirrors the file path.
        let mut config = make_config();
        config.base_url = Some("   ".into());
        let url = build_ws_url(&config, 16000, None);
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"), "got {url}");
    }

    #[test]
    fn language_hint_wins_over_config_language_and_suppresses_multi_mode() {
        let mut config = make_config();
        config.language = Some("es".into());
        config.detect_language = Some(true);
        let url = build_ws_url(&config, 16000, Some("zh-CN"));
        assert!(url.contains("language=zh-CN"), "got {url}");
        assert!(!url.contains("detect_language="));
        assert!(!url.contains("language=multi"));
        assert!(!url.contains("endpointing="));
    }

    #[test]
    fn config_language_is_used_when_no_hint() {
        let mut config = make_config();
        config.language = Some("es".into());
        let url = build_ws_url(&config, 16000, None);
        assert!(url.contains("language=es"), "got {url}");
        assert!(!url.contains("endpointing="), "got {url}");
    }

    #[test]
    fn no_language_uses_multi_mode_and_never_detect_language() {
        // Deepgram language detection is batch-only; streaming must use
        // language=multi even when the config asks for detection.
        let mut config = make_config();
        config.detect_language = Some(true);
        let url = build_ws_url(&config, 16000, None);
        assert!(!url.contains("detect_language="), "got {url}");
        assert!(url.contains("language=multi"), "got {url}");
        assert!(url.contains("endpointing=100"), "got {url}");
    }

    #[test]
    fn punctuate_and_smart_format_sent_only_when_true() {
        let mut config = make_config();
        config.punctuate = Some(true);
        config.smart_format = Some(false);
        let url = build_ws_url(&config, 16000, None);
        assert!(url.contains("punctuate=true"), "got {url}");
        assert!(!url.contains("smart_format="));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        let mut config = make_config();
        config.model = "nova 3/custom".into();
        let url = build_ws_url(&config, 16000, None);
        assert!(url.contains("model=nova%203%2Fcustom"), "got {url}");
    }

    // -- parse_text_frame -----------------------------------------------------

    fn results_frame(transcript: &str, is_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "channel": { "alternatives": [{ "transcript": transcript, "confidence": 0.99 }] },
        })
        .to_string()
    }

    #[test]
    fn results_frame_maps_to_partial_or_final() {
        assert_eq!(
            parse_text_frame(&results_frame("hel", false)),
            Some(UpstreamEvent::Partial("hel".into()))
        );
        assert_eq!(
            parse_text_frame(&results_frame("hello", true)),
            Some(UpstreamEvent::Final("hello".into()))
        );
    }

    #[test]
    fn empty_or_whitespace_transcripts_are_skipped() {
        assert_eq!(parse_text_frame(&results_frame("", false)), None);
        assert_eq!(parse_text_frame(&results_frame("   ", true)), None);
    }

    #[test]
    fn bookkeeping_and_unknown_frames_are_skipped() {
        assert_eq!(parse_text_frame(r#"{"type":"Metadata","request_id":"x"}"#), None);
        assert_eq!(parse_text_frame(r#"{"type":"UtteranceEnd","last_word_end":1.2}"#), None);
        assert_eq!(parse_text_frame(r#"{"type":"SpeechStarted","timestamp":0.5}"#), None);
        assert_eq!(parse_text_frame(r#"{"type":"SomethingNew"}"#), None);
        assert_eq!(parse_text_frame("not json"), None);
    }

    #[test]
    fn results_frame_without_alternatives_is_skipped() {
        assert_eq!(
            parse_text_frame(r#"{"type":"Results","is_final":true,"channel":{}}"#),
            None
        );
    }
}
