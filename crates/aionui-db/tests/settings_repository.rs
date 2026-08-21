//! Black-box integration tests for ISettingsRepository.
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.

use std::sync::Arc;

use aionui_db::{ISettingsRepository, SqliteSettingsRepository, init_database_memory};

const USER_ID: &str = "system_default_user";

async fn repo() -> Arc<dyn ISettingsRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteSettingsRepository::new(db.pool().clone()))
}

// -- Get default state --

#[tokio::test]
async fn get_settings_returns_none_when_no_row_exists() {
    let r = repo().await;
    assert!(r.get_settings(USER_ID).await.unwrap().is_none());
}

// -- Upsert creates a row --

#[tokio::test]
async fn upsert_creates_settings_with_given_values() {
    let r = repo().await;
    let s = r
        .upsert_settings(USER_ID, "zh-CN", false, true, true, false, true)
        .await
        .unwrap();

    assert_eq!(s.language, "zh-CN");
    assert!(!s.notification_enabled);
    assert!(s.cron_notification_enabled);
    assert!(s.command_queue_enabled);
    assert!(!s.save_upload_to_workspace);
    assert!(s.updated_at > 0);
}

// -- Upsert then get round-trip --

#[tokio::test]
async fn upsert_then_get_returns_consistent_data() {
    let r = repo().await;
    r.upsert_settings(USER_ID, "en-US", true, false, false, true, true)
        .await
        .unwrap();

    let s = r.get_settings(USER_ID).await.unwrap().unwrap();
    assert_eq!(s.language, "en-US");
    assert!(s.notification_enabled);
    assert!(!s.cron_notification_enabled);
    assert!(!s.command_queue_enabled);
    assert!(s.save_upload_to_workspace);
}

// -- Upsert overwrites --

#[tokio::test]
async fn upsert_overwrites_previous_settings() {
    let r = repo().await;
    r.upsert_settings(USER_ID, "en-US", true, false, false, false, true)
        .await
        .unwrap();
    r.upsert_settings(USER_ID, "ja-JP", false, true, true, true, true)
        .await
        .unwrap();

    let s = r.get_settings(USER_ID).await.unwrap().unwrap();
    assert_eq!(s.language, "ja-JP");
    assert!(!s.notification_enabled);
    assert!(s.cron_notification_enabled);
    assert!(s.command_queue_enabled);
    assert!(s.save_upload_to_workspace);
}

// -- updated_at advances on each upsert --

#[tokio::test]
async fn upsert_advances_updated_at() {
    let r = repo().await;
    let first = r
        .upsert_settings(USER_ID, "en-US", true, false, false, false, true)
        .await
        .unwrap();
    let second = r
        .upsert_settings(USER_ID, "en-US", true, false, false, false, true)
        .await
        .unwrap();

    assert!(second.updated_at >= first.updated_at);
}
