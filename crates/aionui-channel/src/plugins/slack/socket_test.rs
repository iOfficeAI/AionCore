use super::*;

// -- backoff_delay -----------------------------------------------------------

#[test]
fn backoff_exponential() {
    assert_eq!(backoff_delay(1), Duration::from_secs(2));
    assert_eq!(backoff_delay(2), Duration::from_secs(4));
    assert_eq!(backoff_delay(3), Duration::from_secs(8));
}

#[test]
fn backoff_capped() {
    assert_eq!(backoff_delay(5), Duration::from_secs(30));
    assert_eq!(backoff_delay(20), Duration::from_secs(30));
}

// -- is_duplicate / cleanup --------------------------------------------------

fn empty_cache() -> DedupCache {
    Arc::new(Mutex::new(HashMap::new()))
}

#[tokio::test]
async fn dedup_first_seen_not_duplicate() {
    let cache = empty_cache();
    assert!(!is_duplicate(&cache, "Ev1").await);
}

#[tokio::test]
async fn dedup_second_seen_is_duplicate() {
    let cache = empty_cache();
    is_duplicate(&cache, "Ev1").await;
    assert!(is_duplicate(&cache, "Ev1").await);
}

#[tokio::test]
async fn dedup_distinct_ids_not_duplicate() {
    let cache = empty_cache();
    is_duplicate(&cache, "Ev1").await;
    assert!(!is_duplicate(&cache, "Ev2").await);
}

#[tokio::test]
async fn cleanup_removes_expired_only() {
    let cache = empty_cache();
    {
        let mut map = cache.lock().await;
        map.insert(
            "old".into(),
            Instant::now() - SLACK_EVENT_DEDUP_TTL - Duration::from_secs(1),
        );
        map.insert("recent".into(), Instant::now());
    }
    cleanup_expired_events(&cache).await;
    let map = cache.lock().await;
    assert!(!map.contains_key("old"));
    assert!(map.contains_key("recent"));
}

// -- handle_event_callback: dedup + normalize + forward ----------------------

#[tokio::test]
async fn event_callback_forwards_app_mention_once() {
    let (tx, mut rx) = mpsc::channel(8);
    let cache = empty_cache();
    let payload = serde_json::json!({
        "type": "event_callback",
        "event_id": "EvA",
        "event": { "type": "app_mention", "user": "U1", "text": "<@U0BOT> hi", "ts": "1700000000.000100", "channel": "C1" }
    });

    handle_event_callback(payload.clone(), "U0BOT", &tx, &cache).await;
    let msg = rx.try_recv().expect("first delivery");
    assert_eq!(msg.chat_id, "C1");
    assert_eq!(msg.content.text, "hi");

    // Same event_id again → deduped, nothing forwarded.
    handle_event_callback(payload, "U0BOT", &tx, &cache).await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn event_callback_drops_bot_echo() {
    let (tx, mut rx) = mpsc::channel(8);
    let cache = empty_cache();
    let payload = serde_json::json!({
        "type": "event_callback",
        "event_id": "EvB",
        "event": { "type": "message", "user": "U0BOT", "text": "echo", "ts": "1700000000.0", "channel": "C1" }
    });
    handle_event_callback(payload, "U0BOT", &tx, &cache).await;
    assert!(rx.try_recv().is_err());
}
