#![allow(clippy::disallowed_types)]

//! Black-box tests for API response formats (test-plan T3.1, T3.2, T3.3).

use aionui_api_types::{ApiResponse, ErrorResponse};

// --- T3.1: Success response format ---

#[test]
fn t3_1_success_response_with_data() {
    let resp = ApiResponse::ok("result");
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["data"], "result");
}

#[test]
fn t3_1_success_response_with_message() {
    let resp = ApiResponse::message("Operation completed");
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["message"], "Operation completed");
    // data should be absent (not null)
    assert!(json.get("data").is_none());
}

#[test]
fn t3_1_success_response_with_data_and_message() {
    let resp = ApiResponse::with_message(vec![1, 2, 3], "Found 3 items");
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["data"], serde_json::json!([1, 2, 3]));
    assert_eq!(json["message"], "Found 3 items");
}

#[test]
fn t3_1_success_response_struct_data() {
    #[derive(serde::Serialize)]
    struct UserData {
        id: String,
        name: String,
    }
    let resp = ApiResponse::ok(UserData {
        id: "u1".into(),
        name: "Alice".into(),
    });
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["id"], "u1");
    assert_eq!(json["data"]["name"], "Alice");
}

// --- T3.2: Error response format ---

#[test]
fn t3_2_error_response_format() {
    let resp = ErrorResponse::new("Resource not found", "NOT_FOUND");
    let json = serde_json::to_value(&resp).unwrap();

    assert_eq!(json["success"], false);
    assert_eq!(json["error"], "Resource not found");
    assert_eq!(json["code"], "NOT_FOUND");
}

#[test]
fn t3_2_error_response_has_all_fields() {
    let resp = ErrorResponse::new("err", "CODE");
    let json = serde_json::to_value(&resp).unwrap();

    // Verify all three required fields exist
    assert!(json.get("success").is_some());
    assert!(json.get("error").is_some());
    assert!(json.get("code").is_some());
    assert!(json.get("details").is_none());
}
