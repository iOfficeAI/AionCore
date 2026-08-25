//! JWT service integration tests.
//!
//! Tests the full lifecycle of JWT operations: sign, verify, blacklist, rotate.

use aionui_auth::{AuthError, JwtService, resolve_jwt_secret};

#[test]
fn full_lifecycle_sign_verify_blacklist() {
    let service = JwtService::new("integration_test_secret".into());

    // Sign a token
    let token = service.sign("user_42", "testuser").unwrap();
    assert!(!token.is_empty());

    // Verify the token
    let payload = service.verify(&token).unwrap();
    assert_eq!(payload.user_id, "user_42");
    assert_eq!(payload.username, "testuser");

    // Blacklist the token
    service.blacklist_token(&token);

    // Verification should now fail
    let result = service.verify(&token);
    assert!(matches!(result, Err(AuthError::TokenBlacklisted)));
}

#[test]
fn secret_rotation_invalidates_all_tokens() {
    let service = JwtService::new("original_secret".into());

    let token1 = service.sign("user_1", "alice").unwrap();
    let token2 = service.sign("user_2", "bob").unwrap();

    // Both tokens are valid
    assert!(service.verify(&token1).is_ok());
    assert!(service.verify(&token2).is_ok());

    // Rotate the secret
    service.rotate_secret().unwrap();

    // Both old tokens are now invalid
    assert!(service.verify(&token1).is_err());
    assert!(service.verify(&token2).is_err());

    // New tokens with the new secret work
    let new_token = service.sign("user_3", "charlie").unwrap();
    let payload = service.verify(&new_token).unwrap();
    assert_eq!(payload.user_id, "user_3");
}

#[test]
fn resolve_secret_priority_order() {
    // Environment variable takes precedence
    let (secret, generated) = resolve_jwt_secret(Some("env"), Some("db"));
    assert_eq!(secret, "env");
    assert!(!generated);

    // Database value is fallback
    let (secret, generated) = resolve_jwt_secret(None, Some("db"));
    assert_eq!(secret, "db");
    assert!(!generated);

    // Random generation as last resort
    let (secret, generated) = resolve_jwt_secret(None, None);
    assert!(!secret.is_empty());
    assert!(generated);

    // Generated secrets are unique
    let (secret2, _) = resolve_jwt_secret(None, None);
    assert_ne!(secret, secret2);

    // An empty value from either source is treated as absent — an empty string is
    // never a usable encryption root, so it must not shadow a lower-priority
    // source or be adopted verbatim.
    let (secret, generated) = resolve_jwt_secret(Some(""), Some("db"));
    assert_eq!(secret, "db", "empty env must fall through to the database value");
    assert!(!generated);

    let (secret, generated) = resolve_jwt_secret(Some(""), Some(""));
    assert!(!secret.is_empty(), "both sources empty → generate a real secret");
    assert!(generated);
}
