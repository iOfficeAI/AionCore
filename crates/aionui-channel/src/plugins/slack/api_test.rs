use super::*;
use reqwest::header::HeaderValue;

// -- parse_retry_after -------------------------------------------------------

fn headers_with_retry_after(value: &'static str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(RETRY_AFTER, HeaderValue::from_static(value));
    h
}

#[test]
fn retry_after_parses_seconds() {
    assert_eq!(
        parse_retry_after(&headers_with_retry_after("3")),
        Some(Duration::from_secs(3))
    );
}

#[test]
fn retry_after_caps_at_max() {
    // 99s is clamped to the 30s cap.
    assert_eq!(
        parse_retry_after(&headers_with_retry_after("99")),
        Some(Duration::from_secs(30))
    );
}

#[test]
fn retry_after_missing_is_none() {
    assert_eq!(parse_retry_after(&HeaderMap::new()), None);
}

#[test]
fn retry_after_non_numeric_is_none() {
    assert_eq!(parse_retry_after(&headers_with_retry_after("soon")), None);
}

#[test]
fn api_constructs_with_both_tokens() {
    let client = Client::new();
    let api = SlackApi::new(client, "xoxb-abc", "xapp-def");
    assert_eq!(api.bot_token, "xoxb-abc");
    assert_eq!(api.app_token, "xapp-def");
}

#[test]
fn slack_identity_holds_user_id() {
    let id = SlackIdentity {
        user_id: "U0BOT".into(),
        user: Some("aion".into()),
    };
    assert_eq!(id.user_id, "U0BOT");
    assert_eq!(id.user.as_deref(), Some("aion"));
}
