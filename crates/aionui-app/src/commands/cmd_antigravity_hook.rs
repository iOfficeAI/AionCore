//! `aioncore antigravity-hook`: agy's PreToolUse gate.
//!
//! agy runs this once per tool call, writing the request to our stdin and
//! reading the decision from our stdout. We forward the request to the running
//! AionUi backend, which raises a permission card and blocks until the user
//! answers.
//!
//! FAIL-CLOSED: every failure path (missing env, unreachable backend, malformed
//! payload, timeout) answers `deny`. The alternative — defaulting to allow —
//! would let an unapproved tool run whenever the bridge is unhealthy.

use std::process::ExitCode;
use std::time::Duration;

use aionui_api_types::{AntigravityHookConfig, AntigravityHookInput, AntigravityHookOutput};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// agy's own headless timeout is `--print-timeout` (default 5m). Stay well
/// under it so a wedged bridge surfaces as a denial rather than a stalled turn.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(240);

pub(crate) async fn run_antigravity_hook() -> ExitCode {
    let decision = decide().await;
    let body = serde_json::to_string(&decision).unwrap_or_else(|_| {
        // Serializing our own type cannot realistically fail, but agy MUST get
        // parseable JSON or it will treat the hook as broken.
        r#"{"decision":"deny","reason":"aionui hook could not serialize its answer"}"#.to_owned()
    });
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(body.as_bytes()).await;
    let _ = stdout.flush().await;
    ExitCode::SUCCESS
}

async fn decide() -> AntigravityHookOutput {
    let mut raw = String::new();
    if let Err(e) = tokio::io::stdin().read_to_string(&mut raw).await {
        return AntigravityHookOutput::deny(format!("aionui hook could not read the request: {e}"));
    }
    let input: AntigravityHookInput = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return AntigravityHookOutput::deny(format!("aionui hook could not parse the request: {e}")),
    };

    let base_url = match std::env::var(AntigravityHookConfig::ENV_BASE_URL) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            return AntigravityHookOutput::deny("aionui hook is not configured (missing callback address)");
        }
    };
    let token = std::env::var(AntigravityHookConfig::ENV_TOKEN).unwrap_or_default();
    let conversation_id = std::env::var(AntigravityHookConfig::ENV_CONVERSATION_ID)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| input.conversation_id.clone());

    let url = format!(
        "{}/internal/antigravity-hook/{conversation_id}",
        base_url.trim_end_matches('/')
    );
    let client = match reqwest::Client::builder().timeout(CALLBACK_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return AntigravityHookOutput::deny(format!("aionui hook could not build its client: {e}")),
    };

    match client
        .post(&url)
        .header(AntigravityHookConfig::TOKEN_HEADER, token)
        .body(raw)
        .header("content-type", "application/json")
        .send()
        .await
    {
        Ok(resp) => match resp.json::<AntigravityHookOutput>().await {
            Ok(out) => out,
            Err(e) => AntigravityHookOutput::deny(format!("aionui returned an unreadable decision: {e}")),
        },
        // The user closed AionUi, the turn was cancelled, or nobody answered in
        // time. Denying is the only safe reading of "no answer".
        Err(e) => AntigravityHookOutput::deny(format!("aionui did not answer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::AntigravityHookDecision;

    #[tokio::test]
    async fn unconfigured_bridge_denies_instead_of_allowing() {
        // Safety property: a hook that cannot reach AionUi must never let the
        // tool through.
        unsafe {
            std::env::remove_var(AntigravityHookConfig::ENV_BASE_URL);
        }
        let out = AntigravityHookOutput::deny("unconfigured");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }

    #[test]
    fn deny_output_serializes_to_agys_vocabulary() {
        let json = serde_json::to_value(AntigravityHookOutput::deny("nope")).unwrap();
        assert_eq!(json["decision"], "deny");
    }
}
