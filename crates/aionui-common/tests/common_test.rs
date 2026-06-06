use aionui_common::*;

// --- ID generation ---

#[test]
fn test_generate_id_returns_uuid() {
    let id = generate_id();
    assert_eq!(id.len(), 36); // UUID string length
    assert!(id.contains('-'));
}

#[test]
fn test_generate_prefixed_id_has_prefix() {
    let id = generate_prefixed_id("cron");
    assert!(id.starts_with("cron_"));
}

#[test]
fn test_id_uniqueness_across_calls() {
    let ids: std::collections::HashSet<String> = (0..1000).map(|_| generate_id()).collect();
    assert_eq!(ids.len(), 1000);
}

#[test]
fn test_id_time_ordering() {
    let id1 = generate_id();
    let id2 = generate_id();
    assert!(id2 >= id1, "UUID v7 should be time-ordered");
}

// --- Timestamp ---

#[test]
fn test_now_ms_returns_positive() {
    assert!(now_ms() > 0);
}

#[test]
fn test_now_ms_monotonic() {
    let t1 = now_ms();
    let t2 = now_ms();
    assert!(t2 >= t1);
}

// --- Crypto ---

#[test]
fn test_encrypt_decrypt_roundtrip() {
    let key = [0xAB_u8; 32];
    let encrypted = encrypt_string("hello world", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "hello world");
}

#[test]
fn test_encrypt_decrypt_empty_string() {
    let key = [0xCD_u8; 32];
    let encrypted = encrypt_string("", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "");
}

#[test]
fn test_encrypt_decrypt_unicode() {
    let key = [0xEF_u8; 32];
    let encrypted = encrypt_string("你好世界🌍", &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, "你好世界🌍");
}

#[test]
fn test_decrypt_wrong_key_fails() {
    let key = [0x11_u8; 32];
    let encrypted = encrypt_string("secret", &key).unwrap();
    let wrong_key = [0x22_u8; 32];
    assert!(decrypt_string(&encrypted, &wrong_key).is_err());
}

#[test]
fn test_encrypt_same_plaintext_different_ciphertext() {
    let key = [0x33_u8; 32];
    let e1 = encrypt_string("test", &key).unwrap();
    let e2 = encrypt_string("test", &key).unwrap();
    assert_ne!(e1, e2, "random nonce should produce different ciphertexts");
}

#[test]
fn test_encrypt_large_text() {
    let key = [0x44_u8; 32];
    let large = "x".repeat(1_000_000);
    let encrypted = encrypt_string(&large, &key).unwrap();
    let decrypted = decrypt_string(&encrypted, &key).unwrap();
    assert_eq!(decrypted, large);
}

// --- ApiError ---

#[test]
fn test_api_error_status_codes() {
    use axum::http::StatusCode;

    assert_eq!(ApiError::NotFound("x".into()).status_code(), StatusCode::NOT_FOUND);
    assert_eq!(ApiError::BadRequest("x".into()).status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(
        ApiError::Unauthorized("x".into()).status_code(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(ApiError::Forbidden("x".into()).status_code(), StatusCode::FORBIDDEN);
    assert_eq!(ApiError::Conflict("x".into()).status_code(), StatusCode::CONFLICT);
    assert_eq!(ApiError::RateLimited.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        ApiError::Internal("x".into()).status_code(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(ApiError::BadGateway("x".into()).status_code(), StatusCode::BAD_GATEWAY);
    assert_eq!(ApiError::Timeout("x".into()).status_code(), StatusCode::GATEWAY_TIMEOUT);
}

#[test]
fn test_api_error_json_format() {
    use axum::response::IntoResponse;
    let resp = ApiError::NotFound("user 123".into()).into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

// --- PaginatedResult ---

#[test]
fn test_paginated_result_serialize() {
    let result = PaginatedResult {
        items: vec!["a", "b"],
        total: 100,
        has_more: true,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["has_more"], true);
    assert_eq!(json["total"], 100);
}

#[test]
fn test_paginated_result_empty() {
    let result: PaginatedResult<i32> = PaginatedResult {
        items: vec![],
        total: 0,
        has_more: false,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["items"], serde_json::json!([]));
}

// --- Enums ---

#[test]
fn test_enum_serde_roundtrip() {
    let roundtrip_cases: Vec<(&str, AgentType)> = vec![
        (r#""acp""#, AgentType::Acp),
        (r#""nanobot""#, AgentType::Nanobot),
        (r#""openclaw-gateway""#, AgentType::OpenclawGateway),
    ];
    for (json_str, expected) in roundtrip_cases {
        let parsed: AgentType = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed, expected);
        let serialized = serde_json::to_string(&expected).unwrap();
        assert_eq!(serialized, json_str);
    }
}

// --- Business structs ---

#[test]
fn test_version_info_update_detection() {
    let v = VersionInfo {
        current: "1.0.0".into(),
        latest: "1.1.0".into(),
        minimum_required: None,
        release_notes: None,
    };
    assert!(v.is_update_available());
    assert!(!v.is_forced());
}

#[test]
fn test_version_info_forced_update() {
    let v = VersionInfo {
        current: "1.0.0".into(),
        latest: "2.0.0".into(),
        minimum_required: Some("1.5.0".into()),
        release_notes: Some("Critical security fix".into()),
    };
    assert!(v.is_update_available());
    assert!(v.is_forced());
}

// --- Constants ---

#[test]
fn test_constants_values() {
    assert_eq!(constants::DEFAULT_PORT, 25808);
    assert_eq!(constants::HEARTBEAT_INTERVAL_MS, 30_000);
    assert_eq!(constants::BODY_LIMIT, 10 * 1024 * 1024);
    assert_eq!(constants::COOKIE_NAME, "aionui-session");
    assert!(constants::SUPPORTED_IMAGE_EXTENSIONS.contains(&".png"));
}
