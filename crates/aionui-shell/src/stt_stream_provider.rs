//! Provider-dispatch upstream factory for STT streaming.
//!
//! The route layer needs a single [`UpstreamFactory`] that works for any
//! stored config; this module composes the per-provider factories by
//! matching on `config.provider` and delegating.

use aionui_api_types::{SpeechToTextConfig, SpeechToTextProvider};

use crate::error::SttError;
use crate::stt_stream::{UpstreamFactory, UpstreamStream};
use crate::stt_stream_deepgram::DeepgramUpstreamFactory;
use crate::stt_stream_openai::OpenAIRealtimeUpstreamFactory;

/// Dispatches to [`OpenAIRealtimeUpstreamFactory`] or
/// [`DeepgramUpstreamFactory`] based on `config.provider`.
pub struct ProviderUpstreamFactory;

#[async_trait::async_trait]
impl UpstreamFactory for ProviderUpstreamFactory {
    async fn connect(
        &self,
        config: &SpeechToTextConfig,
        sample_rate: u32,
        language_hint: Option<&str>,
    ) -> Result<Box<dyn UpstreamStream>, SttError> {
        match config.provider {
            SpeechToTextProvider::Openai => {
                OpenAIRealtimeUpstreamFactory
                    .connect(config, sample_rate, language_hint)
                    .await
            }
            SpeechToTextProvider::Deepgram => {
                DeepgramUpstreamFactory
                    .connect(config, sample_rate, language_hint)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each provider arm must reach its concrete factory: with a missing
    /// provider config, the factory's own validation error proves dispatch.
    #[tokio::test]
    async fn dispatches_openai_to_openai_factory() {
        let config = SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Openai,
            auto_send: None,
            openai: None,
            deepgram: None,
        };
        let err = ProviderUpstreamFactory
            .connect(&config, 16000, None)
            .await
            .map(|_| ())
            .expect_err("expected OpenAI factory error");
        assert!(matches!(err, SttError::OpenaiNotConfigured));
    }

    #[tokio::test]
    async fn dispatches_deepgram_to_deepgram_factory() {
        let config = SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Deepgram,
            auto_send: None,
            openai: None,
            deepgram: None,
        };
        let err = ProviderUpstreamFactory
            .connect(&config, 16000, None)
            .await
            .map(|_| ())
            .expect_err("expected Deepgram factory error");
        assert!(matches!(err, SttError::DeepgramNotConfigured));
    }
}
