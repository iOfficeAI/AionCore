use super::*;

#[test]
fn api_uses_bot_auth_header() {
    let api = DiscordApi::new(Client::new(), "abc.def.ghi");
    assert_eq!(api.auth_header(), "Bot abc.def.ghi");
    assert_eq!(api.token, "abc.def.ghi");
}

#[test]
fn retry_after_uses_body_seconds() {
    let body = RateLimitBody {
        retry_after: Some(1.5),
        global: Some(false),
    };
    assert_eq!(retry_after_delay(Some(body)), Duration::from_secs_f64(1.5));
}

#[test]
fn retry_after_defaults_when_absent() {
    assert_eq!(retry_after_delay(None), Duration::from_secs_f64(1.0));
    let body = RateLimitBody {
        retry_after: None,
        global: None,
    };
    assert_eq!(retry_after_delay(Some(body)), Duration::from_secs_f64(1.0));
}

#[test]
fn retry_after_is_capped() {
    let body = RateLimitBody {
        retry_after: Some(999.0),
        global: Some(true),
    };
    assert_eq!(retry_after_delay(Some(body)), Duration::from_secs(30));
}

#[test]
fn discord_identity_holds_id() {
    let id = DiscordIdentity {
        id: "BOT1".into(),
        display_name: Some("aion".into()),
    };
    assert_eq!(id.id, "BOT1");
    assert_eq!(id.display_name.as_deref(), Some("aion"));
}
