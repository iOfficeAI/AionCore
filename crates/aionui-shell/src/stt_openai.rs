use aionui_api_types::{OpenAISpeechToTextConfig, SpeechToTextProvider, SpeechToTextResult};
use reqwest::Client;

use crate::error::SttError;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Resolve the effective base URL. Unset or blank values fall back to the
/// default — the settings UI saves unfilled fields as empty strings, which
/// would otherwise produce a relative URL that fails the request builder.
pub(crate) fn resolve_base_url(configured: Option<&str>) -> &str {
    configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .trim_end_matches("/v1")
}

pub async fn transcribe(
    client: &Client,
    config: &OpenAISpeechToTextConfig,
    audio_data: Vec<u8>,
    file_name: &str,
    mime_type: &str,
    language_hint: Option<&str>,
) -> Result<SpeechToTextResult, SttError> {
    if config.api_key.is_empty() {
        return Err(SttError::OpenaiNotConfigured);
    }

    let base_url = resolve_base_url(config.base_url.as_deref());
    let url = format!("{base_url}/v1/audio/transcriptions");

    // Normalize MIME type: strip codec parameters (e.g. "audio/webm;codecs=opus" → "audio/webm")
    let clean_mime_type = mime_type.split(';').next().unwrap_or(mime_type).trim();
    let file_part = reqwest::multipart::Part::bytes(audio_data)
        .file_name(file_name.to_owned())
        .mime_str(clean_mime_type)
        .map_err(|e| SttError::Unknown(format!("invalid MIME type: {e}")))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", config.model.clone());

    // User-configured language takes precedence over browser languageHint.
    // This lets users override the browser's locale (e.g. browser sends "en-US"
    // but user prefers "es" for transcription).
    let language = config.language.as_deref().filter(|s| !s.is_empty()).or(language_hint);
    let normalized_language = language.map(|lang| {
        // Normalize language codes: "en-US" → "en", "es-MX" → "es"
        // Groq/OpenAI Whisper only accepts base language codes (e.g. "en", "es")
        lang.split('-').next().unwrap_or(lang).to_owned()
    });
    if let Some(lang) = normalized_language.as_deref() {
        form = form.text("language", lang.to_owned());
    }

    if let Some(prompt) = config.prompt.as_deref().filter(|s| !s.is_empty()) {
        form = form.text("prompt", prompt.to_owned());
    }

    if let Some(temp) = config.temperature {
        form = form.text("temperature", temp.to_string());
    }

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| SttError::RequestFailed(format!("OpenAI request error: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_else(|_| "<unreadable>".to_owned());
        return Err(SttError::RequestFailed(format!("OpenAI API returned {status}: {body}")));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| SttError::RequestFailed(format!("failed to parse OpenAI response: {e}")))?;

    let text = body["text"].as_str().unwrap_or("").to_owned();

    Ok(SpeechToTextResult {
        text,
        model: config.model.clone(),
        provider: SpeechToTextProvider::Openai,
        language: normalized_language,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url_value() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com");
    }

    #[test]
    fn resolve_base_url_falls_back_on_none() {
        assert_eq!(resolve_base_url(None), DEFAULT_BASE_URL);
    }

    #[test]
    fn resolve_base_url_falls_back_on_blank() {
        // Settings UI saves unfilled base_url as "" — must not build a relative URL
        assert_eq!(resolve_base_url(Some("")), DEFAULT_BASE_URL);
        assert_eq!(resolve_base_url(Some("   ")), DEFAULT_BASE_URL);
    }

    #[test]
    fn resolve_base_url_trims_trailing_slash_and_v1() {
        assert_eq!(
            resolve_base_url(Some("https://api.groq.com/openai/v1")),
            "https://api.groq.com/openai"
        );
        assert_eq!(resolve_base_url(Some("https://example.com/")), "https://example.com");
        assert_eq!(resolve_base_url(Some("https://example.com/v1/")), "https://example.com");
    }

    #[tokio::test]
    async fn empty_api_key_returns_not_configured() {
        let config = OpenAISpeechToTextConfig {
            api_key: String::new(),
            base_url: None,
            model: "whisper-1".into(),
            language: None,
            prompt: None,
            temperature: None,
        };
        let result = transcribe(&Client::new(), &config, vec![0u8; 10], "test.wav", "audio/wav", None).await;
        assert!(matches!(result, Err(SttError::OpenaiNotConfigured)));
    }
}
