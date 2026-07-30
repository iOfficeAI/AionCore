//! Black-box integration tests for IUserRepository (test-plan T2.1 – T2.13).
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.
//! Internal details like SQL queries or column names are not referenced.

use std::sync::Arc;

use aionui_db::{
    DbError, ExternalUserProjection, IUserRepository, SqliteUserRepository, UserStatus, UserType, init_database_memory,
};

async fn repo() -> Arc<dyn IUserRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteUserRepository::new(db.pool().clone()))
}

// -- T2.1 Create user --

#[tokio::test]
async fn t2_1_create_user_returns_user_with_populated_fields() {
    let r = repo().await;
    let user = r.create_user("testuser", "$2b$12$fakehash").await.unwrap();

    assert!(!user.id.is_empty(), "id should be non-empty");
    assert_eq!(user.user_type, UserType::Local);
    assert_eq!(user.status, UserStatus::Active);
    assert_eq!(user.session_generation, 0);
    assert_eq!(user.username.as_deref(), Some("testuser"));
    assert_eq!(user.password_hash.as_deref(), Some("$2b$12$fakehash"));
    assert!(user.created_at > 0);
    assert!(user.updated_at > 0);
}

#[tokio::test]
async fn t2_1_create_user_duplicate_username_returns_conflict() {
    let r = repo().await;
    r.create_user("dup", "h1").await.unwrap();

    let err = r.create_user("dup", "h2").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

// -- T2.2 Find by username --

#[tokio::test]
async fn t2_2_find_by_username_existing() {
    let r = repo().await;
    r.create_user("findme", "h").await.unwrap();

    let found = r.find_by_username("findme").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().username.as_deref(), Some("findme"));
}

#[tokio::test]
async fn t2_2_find_by_username_nonexistent_returns_none() {
    let r = repo().await;
    assert!(r.find_by_username("ghost").await.unwrap().is_none());
}

// -- T2.3 Find by ID --

#[tokio::test]
async fn t2_3_find_by_id_existing() {
    let r = repo().await;
    let user = r.create_user("byid", "h").await.unwrap();

    let found = r.find_by_id(&user.id).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, user.id);
}

#[tokio::test]
async fn t2_3_find_by_id_nonexistent_returns_none() {
    let r = repo().await;
    assert!(r.find_by_id("no_such_id").await.unwrap().is_none());
}

// -- T2.4 List all users --

#[tokio::test]
async fn t2_4_list_users_returns_all() {
    let r = repo().await;
    r.create_user("u1", "h").await.unwrap();
    r.create_user("u2", "h").await.unwrap();

    let users = r.list_users().await.unwrap();
    // system_default_user + u1 + u2
    assert_eq!(users.len(), 3);
}

// -- T2.5 Count users --

#[tokio::test]
async fn t2_5_count_users() {
    let r = repo().await;
    r.create_user("counted", "h").await.unwrap();

    // system_default_user + counted
    assert_eq!(r.count_users().await.unwrap(), 2);
}

// -- T2.6 has_users --

#[tokio::test]
async fn t2_6_has_users_false_with_only_empty_password_system_user() {
    let r = repo().await;
    assert!(!r.has_users().await.unwrap());
}

#[tokio::test]
async fn t2_6_has_users_true_with_real_user() {
    let r = repo().await;
    r.create_user("real", "bcrypt_hash").await.unwrap();
    assert!(r.has_users().await.unwrap());
}

// -- T2.7 Get system user --

#[tokio::test]
async fn t2_7_get_system_user_returns_default() {
    let r = repo().await;
    let user = r.get_system_user().await.unwrap();
    assert!(user.is_some());

    let user = user.unwrap();
    assert_eq!(user.id, "system_default_user");
}

// -- T2.8 Get primary WebUI user --

#[tokio::test]
async fn t2_8_primary_webui_user_is_system_user_when_only_system() {
    let r = repo().await;
    let user = r.get_primary_webui_user().await.unwrap().unwrap();
    assert_eq!(user.id, "system_default_user");
}

#[tokio::test]
async fn t2_8_primary_webui_user_prefers_system_over_admin() {
    let r = repo().await;
    // Can't create another user called "admin" now that seed uses it.
    // The priority check still holds: any non-system user must not shadow system.
    r.create_user("other", "h").await.unwrap();

    let user = r.get_primary_webui_user().await.unwrap().unwrap();
    assert_eq!(
        user.id, "system_default_user",
        "system user should take priority over non-system users"
    );
}

// -- T2.9 Set system user credentials --

#[tokio::test]
async fn t2_9_set_system_user_credentials_updates_username_and_hash() {
    let r = repo().await;
    r.set_system_user_credentials("newadmin", "secure_hash").await.unwrap();

    let user = r.get_system_user().await.unwrap().unwrap();
    assert_eq!(user.username.as_deref(), Some("newadmin"));
    assert_eq!(user.password_hash.as_deref(), Some("secure_hash"));
}

#[tokio::test]
async fn t2_9_set_system_user_credentials_conflict_with_existing_username() {
    let r = repo().await;
    r.create_user("existing", "h").await.unwrap();

    let err = r.set_system_user_credentials("existing", "hash").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

// -- T2.10 Update password --

#[tokio::test]
async fn t2_10_update_password_changes_hash_and_updated_at() {
    let r = repo().await;
    let user = r.create_user("pwduser", "old").await.unwrap();

    r.update_password(&user.id, "new_hash").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.password_hash.as_deref(), Some("new_hash"));
    assert!(updated.updated_at >= user.updated_at);
}

// -- T2.11 Update username --

#[tokio::test]
async fn t2_11_update_username_succeeds() {
    let r = repo().await;
    let user = r.create_user("oldname", "h").await.unwrap();

    r.update_username(&user.id, "newname").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.username.as_deref(), Some("newname"));
}

#[tokio::test]
async fn t2_11_update_username_conflict_with_existing() {
    let r = repo().await;
    r.create_user("taken", "h").await.unwrap();
    let other = r.create_user("free", "h").await.unwrap();

    let err = r.update_username(&other.id, "taken").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected Conflict, got: {err:?}");
}

// -- T2.12 Update last login --

#[tokio::test]
async fn t2_12_update_last_login_sets_timestamp() {
    let r = repo().await;
    let user = r.create_user("loginuser", "h").await.unwrap();
    assert!(user.last_login.is_none());

    r.update_last_login(&user.id).await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert!(updated.last_login.is_some());
    assert!(updated.last_login.unwrap() > 0);
}

// -- T2.13 Update JWT secret --

#[tokio::test]
async fn t2_13_update_jwt_secret_sets_value() {
    let r = repo().await;
    let user = r.create_user("jwtuser", "h").await.unwrap();
    assert!(user.jwt_secret.is_none());

    r.update_jwt_secret(&user.id, "my_secret").await.unwrap();

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.jwt_secret.as_deref(), Some("my_secret"));
}

#[tokio::test]
async fn t2_14_external_user_provision_is_idempotent() {
    let r = repo().await;

    let first = r
        .ensure_external_user(
            UserType::Aionpro,
            "pro-user-1",
            ExternalUserProjection {
                username: Some("Pro User".to_string()),
                email: Some("pro@example.com".to_string()),
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let second = r
        .ensure_external_user(UserType::Aionpro, "pro-user-1", ExternalUserProjection::default())
        .await
        .unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(first.user_type, UserType::Aionpro);
    assert_eq!(first.external_user_id.as_deref(), Some("pro-user-1"));
    assert!(first.password_hash.is_none());
    assert!(r.find_by_username("Pro User").await.unwrap().is_none());
}

#[tokio::test]
async fn t2_15_disabled_user_is_not_active() {
    let r = repo().await;
    let user = r
        .ensure_external_user(
            UserType::Aionpro,
            "pro-user-disabled",
            ExternalUserProjection::default(),
        )
        .await
        .unwrap();

    assert!(r.find_active_by_id(&user.id).await.unwrap().is_some());
    r.set_status(&user.id, UserStatus::Disabled).await.unwrap();

    let disabled = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(disabled.session_generation, 1);
    assert!(r.find_active_by_id(&user.id).await.unwrap().is_none());

    r.set_status(&user.id, UserStatus::Disabled).await.unwrap();
    let disabled_again = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(disabled_again.session_generation, 1);
}

#[tokio::test]
async fn t2_16_increment_session_generation_revokes_old_sessions() {
    let r = repo().await;
    let user = r.create_user("generation-user", "h").await.unwrap();

    assert_eq!(r.increment_session_generation(&user.id).await.unwrap(), 1);
    assert_eq!(r.increment_session_generation(&user.id).await.unwrap(), 2);

    let updated = r.find_by_id(&user.id).await.unwrap().unwrap();
    assert_eq!(updated.session_generation, 2);
}
