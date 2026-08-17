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
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            decision_from_response(status, &body)
        }
        // The user closed AionUi, the turn was cancelled, or nobody answered in
        // time. Denying is the only safe reading of "no answer".
        Err(e) => AntigravityHookOutput::deny(format!("aionui did not answer: {e}")),
    }
}

/// Turn a callback response into a decision.
///
/// **The status is read before the body.** A non-2xx carries an `ErrorResponse`,
/// not an `AntigravityHookOutput`, so handing it to the decision parser reports
/// a rejection as a parse failure. That is what users saw when CSRF was blocking
/// this endpoint (fixed in #860): the reply carried
/// `aionui returned an unreadable decision: error decoding response body`,
/// which names the wrong thing to go looking at — the body was perfectly
/// readable, the request had been refused.
///
/// Split out from the caller so the mapping is testable without standing up a
/// server; the reasons above are exactly the cases worth pinning.
///
/// Every branch denies. A hook that cannot obtain a clear approval must never
/// let the tool through.
fn decision_from_response(status: reqwest::StatusCode, body: &str) -> AntigravityHookOutput {
    if !status.is_success() {
        // Name the code when the body is our own error envelope; some rejections
        // (a proxy, a crash page) will not have one.
        let code = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["code"].as_str().map(str::to_owned));
        return match code {
            Some(code) => AntigravityHookOutput::deny(format!("aionui refused the request: HTTP {status} ({code})")),
            None => AntigravityHookOutput::deny(format!("aionui refused the request: HTTP {status}")),
        };
    }
    match serde_json::from_str::<AntigravityHookOutput>(body) {
        Ok(out) => out,
        Err(e) => AntigravityHookOutput::deny(format!("aionui returned an unreadable decision: {e}")),
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

    /// The bug this split exists for: a refusal used to be reported as a parse
    /// failure. Live 2026-08-17, while CSRF was blocking this endpoint, the user
    /// saw `aionui returned an unreadable decision: error decoding response
    /// body` — which points at the body when the problem was the status.
    #[test]
    fn a_refusal_is_named_as_one_not_as_an_unreadable_body() {
        let out = decision_from_response(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"success":false,"error":"CSRF token validation failed","code":"CSRF_INVALID"}"#,
        );
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            reason.contains("403") && reason.contains("CSRF_INVALID"),
            "the reason must name the status and the code, not the parser: {reason}"
        );
        assert!(
            !reason.contains("unreadable"),
            "a refused request is not an unreadable one: {reason}"
        );
    }

    /// Not every rejection carries our error envelope — a proxy or a crash page
    /// will not. The status alone still has to reach the user.
    #[test]
    fn a_refusal_without_our_error_envelope_still_names_the_status() {
        let out = decision_from_response(reqwest::StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(reason.contains("502"), "reason must still name the status: {reason}");
    }

    /// The branch that keeps its old message: a 2xx whose body really is
    /// unparseable. Here "unreadable" is the truth.
    #[test]
    fn an_unparseable_success_body_is_still_reported_as_unreadable() {
        let out = decision_from_response(reqwest::StatusCode::OK, "not json");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(reason.contains("unreadable"), "reason: {reason}");
    }

    /// The happy path must still get through, or this change would deny
    /// everything and look like a fix.
    #[test]
    fn an_approval_is_passed_through_unchanged() {
        let out = decision_from_response(reqwest::StatusCode::OK, r#"{"decision":"allow"}"#);
        assert_eq!(out.decision, AntigravityHookDecision::Allow);
    }

    #[test]
    fn deny_output_serializes_to_agys_vocabulary() {
        let json = serde_json::to_value(AntigravityHookOutput::deny("nope")).unwrap();
        assert_eq!(json["decision"], "deny");
    }
}
