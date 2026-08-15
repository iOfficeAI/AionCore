use super::*;
use serde_json::json;

// -- gateway_ws_url ----------------------------------------------------------

#[test]
fn ws_url_appends_query() {
    assert_eq!(
        gateway_ws_url("wss://gateway.discord.gg"),
        "wss://gateway.discord.gg/?v=10&encoding=json"
    );
}

#[test]
fn ws_url_trims_trailing_slash() {
    assert_eq!(
        gateway_ws_url("wss://gateway.discord.gg/"),
        "wss://gateway.discord.gg/?v=10&encoding=json"
    );
}

// -- backoff -----------------------------------------------------------------

#[test]
fn backoff_exponential_and_capped() {
    assert_eq!(backoff_delay(1), Duration::from_secs(2));
    assert_eq!(backoff_delay(3), Duration::from_secs(8));
    assert_eq!(backoff_delay(20), Duration::from_secs(30));
}

// -- dedup -------------------------------------------------------------------

fn cache() -> DedupCache {
    Arc::new(Mutex::new(HashMap::new()))
}

#[tokio::test]
async fn dedup_first_then_duplicate() {
    let c = cache();
    assert!(!is_duplicate(&c, "m1").await);
    assert!(is_duplicate(&c, "m1").await);
    assert!(!is_duplicate(&c, "m2").await);
}

#[tokio::test]
async fn cleanup_removes_expired_only() {
    let c = cache();
    {
        let mut map = c.lock().await;
        map.insert(
            "old".into(),
            Instant::now() - DISCORD_EVENT_DEDUP_TTL - Duration::from_secs(1),
        );
        map.insert("new".into(), Instant::now());
    }
    cleanup_expired_events(&c).await;
    let map = c.lock().await;
    assert!(!map.contains_key("old"));
    assert!(map.contains_key("new"));
}

// -- handle_dispatch ---------------------------------------------------------

#[tokio::test]
async fn dispatch_ready_sets_session() {
    let (tx, _rx) = mpsc::channel(4);
    let c = cache();
    let mut session = Session::default();
    let frame: GatewayFrame = serde_json::from_value(json!({
        "op": 0, "t": "READY", "s": 1,
        "d": { "session_id": "sess1", "resume_gateway_url": "wss://resume.example", "user": { "id": "BOT1" } }
    }))
    .unwrap();
    handle_dispatch(&frame, "BOT1", &mut session, &tx, &c).await;
    assert_eq!(session.id.as_deref(), Some("sess1"));
    // READY stores the RAW resume URL (the connect site adds the query once).
    assert_eq!(session.resume_url.as_deref(), Some("wss://resume.example"));
}

/// Regression (H1): the resume connect URL must carry exactly one
/// `?v=10&encoding=json`. READY stores the raw URL, so wrapping it once at the
/// connect site (as gateway_loop does) yields a valid single-query URL — not
/// the `.../?v=10&encoding=json/?v=10&encoding=json` a double-wrap produced.
#[tokio::test]
async fn resume_connect_url_has_single_query() {
    let (tx, _rx) = mpsc::channel(4);
    let c = cache();
    let mut session = Session::default();
    let frame: GatewayFrame = serde_json::from_value(json!({
        "op": 0, "t": "READY", "s": 1,
        "d": { "session_id": "sess1", "resume_gateway_url": "wss://resume.example", "user": { "id": "BOT1" } }
    }))
    .unwrap();
    handle_dispatch(&frame, "BOT1", &mut session, &tx, &c).await;

    // Reproduce the reconnect composition: connect site wraps the stored URL.
    let connect_url = gateway_ws_url(session.resume_url.as_deref().unwrap());
    assert_eq!(connect_url, "wss://resume.example/?v=10&encoding=json");
    assert_eq!(connect_url.matches("?v=10&encoding=json").count(), 1);
}

#[tokio::test]
async fn dispatch_message_create_dm_forwards_once() {
    let (tx, mut rx) = mpsc::channel(4);
    let c = cache();
    let mut session = Session::default();
    let frame: GatewayFrame = serde_json::from_value(json!({
        "op": 0, "t": "MESSAGE_CREATE", "s": 2,
        "d": { "id": "175928847299117063", "channel_id": "D1", "author": { "id": "U1", "username": "a" }, "content": "hi" }
    }))
    .unwrap();

    handle_dispatch(&frame, "BOT1", &mut session, &tx, &c).await;
    let msg = rx.try_recv().expect("forwarded");
    assert_eq!(msg.chat_id, "D1");
    assert_eq!(msg.content.text, "hi");

    // Same message id again → deduped.
    handle_dispatch(&frame, "BOT1", &mut session, &tx, &c).await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn dispatch_guild_without_mention_dropped() {
    let (tx, mut rx) = mpsc::channel(4);
    let c = cache();
    let mut session = Session::default();
    let frame: GatewayFrame = serde_json::from_value(json!({
        "op": 0, "t": "MESSAGE_CREATE", "s": 3,
        "d": { "id": "175928847299117064", "channel_id": "C1", "guild_id": "G1", "author": { "id": "U1" }, "content": "hello", "mentions": [] }
    }))
    .unwrap();
    handle_dispatch(&frame, "BOT1", &mut session, &tx, &c).await;
    assert!(rx.try_recv().is_err());
}
