//! Manual smoke test for the STT streaming upstream adapters against REAL APIs.
//! Not committed / not part of CI. Run:
//!   STT_SMOKE_API_KEY=sk-... STT_SMOKE_MODEL=gpt-4o-mini-transcribe \
//!   cargo run -p aionui-shell --example stt_stream_smoke -- /path/to/pcm16-24k-mono.wav

use aionui_api_types::{
    DeepgramSpeechToTextConfig, OpenAISpeechToTextConfig, SpeechToTextConfig, SpeechToTextProvider,
};
use aionui_shell::{ProviderUpstreamFactory, UpstreamEvent, UpstreamFactory};
use std::time::Duration;

/// Locate the `data` chunk payload inside a RIFF/WAVE file.
fn wav_pcm_payload(bytes: &[u8]) -> &[u8] {
    assert_eq!(&bytes[0..4], b"RIFF", "not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "not a WAVE file");
    let mut off = 12;
    while off + 8 <= bytes.len() {
        let id = &bytes[off..off + 4];
        let size = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return &bytes[off + 8..off + 8 + size.min(bytes.len() - off - 8)];
        }
        off += 8 + size + (size & 1);
    }
    panic!("no data chunk found");
}

#[tokio::main]
async fn main() {
    let provider = std::env::var("STT_SMOKE_PROVIDER").unwrap_or_else(|_| "openai".into());
    let api_key = std::env::var("STT_SMOKE_API_KEY").expect("STT_SMOKE_API_KEY required");
    let default_model = if provider == "deepgram" {
        "nova-3"
    } else {
        "gpt-4o-mini-transcribe"
    };
    let model = std::env::var("STT_SMOKE_MODEL").unwrap_or_else(|_| default_model.into());
    let wav_path = std::env::args().nth(1).expect("usage: stt_stream_smoke <wav>");

    let wav = std::fs::read(&wav_path).expect("read wav");
    let pcm = wav_pcm_payload(&wav);
    println!(
        "[smoke] pcm payload: {} bytes (~{:.1}s @24kHz mono)",
        pcm.len(),
        pcm.len() as f64 / 48000.0
    );

    let config = match provider.as_str() {
        "deepgram" => SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Deepgram,
            auto_send: None,
            openai: None,
            deepgram: Some(DeepgramSpeechToTextConfig {
                api_key,
                base_url: None,
                model: model.clone(),
                language: None,
                detect_language: None,
                punctuate: Some(true),
                smart_format: Some(true),
            }),
        },
        _ => SpeechToTextConfig {
            enabled: true,
            provider: SpeechToTextProvider::Openai,
            auto_send: None,
            openai: Some(OpenAISpeechToTextConfig {
                api_key,
                base_url: None,
                model: model.clone(),
                language: None,
                prompt: None,
                temperature: None,
            }),
            deepgram: None,
        },
    };

    println!("[smoke] connecting upstream (provider={provider}, model={model})...");
    let t0 = std::time::Instant::now();
    let mut upstream = ProviderUpstreamFactory
        .connect(
            &config,
            24000,
            std::env::var("STT_SMOKE_LANG")
                .ok()
                .filter(|s| !s.is_empty())
                .as_deref(),
        )
        .await
        .expect("upstream connect failed");
    println!("[smoke] connected in {:?}", t0.elapsed());

    // Feed 100ms chunks (4800 bytes) at real-time pace, polling events between sends.
    let chunk = 4800usize;
    let mut sent = 0usize;
    let mut events = 0usize;
    while sent < pcm.len() {
        let end = (sent + chunk).min(pcm.len());
        upstream.send_audio(&pcm[sent..end]).await.expect("send_audio failed");
        sent = end;
        // Poll any already-available events without blocking the pace too long.
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(5), upstream.next_event()).await {
            events += 1;
            println!("[smoke] +{:?} event: {:?}", t0.elapsed(), ev.map(describe));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("[smoke] all audio sent ({sent} bytes), committing...");
    upstream.finish().await.expect("finish failed");

    // Drain until Closed or error, max 30s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let next = tokio::time::timeout_at(deadline, upstream.next_event()).await;
        match next {
            Ok(Some(Ok(UpstreamEvent::Closed))) | Ok(None) => {
                println!("[smoke] upstream closed cleanly after {:?}", t0.elapsed());
                break;
            }
            Ok(Some(Ok(ev))) => {
                events += 1;
                println!("[smoke] +{:?} event: {}", t0.elapsed(), describe(ev));
            }
            Ok(Some(Err(e))) => {
                println!("[smoke] ERROR: code={} msg={e}", e.error_code());
                std::process::exit(1);
            }
            Err(_) => {
                println!("[smoke] TIMEOUT waiting for events after commit");
                std::process::exit(2);
            }
        }
    }
    println!("[smoke] DONE: {events} transcript events received");
}

fn describe(ev: UpstreamEvent) -> String {
    match ev {
        UpstreamEvent::Partial(t) => format!("PARTIAL: {t}"),
        UpstreamEvent::Final(t) => format!("FINAL: {t}"),
        UpstreamEvent::Closed => "CLOSED".into(),
    }
}
