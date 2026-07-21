use aion_types::message::ImageInputCapability;
use serde_json::json;

use super::{
    IMAGE_INPUT_CATALOG_JSON, ImageInputCatalog, parse_catalog, resolve_from_catalog, resolve_image_input_capability,
};

fn catalog() -> ImageInputCatalog {
    serde_json::from_value(json!({
        "schema_version": 1,
        "providers": {
            "openai": {
                "models": ["gpt-4o"]
            },
            "google": {
                "models": ["gemini-2.5-flash"]
            },
            "dashscope": {
                "api": "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "models": ["qwen3.7-plus"]
            },
            "moonshot-global": {
                "api": "https://api.moonshot.ai/v1",
                "models": ["kimi-k2.6"]
            },
            "openrouter": {
                "api": "https://openrouter.ai/api/v1",
                "models": []
            },
            "amazon-bedrock": {
                "models": ["anthropic.claude-sonnet-4-20250514-v1:0"]
            },
            "deepseek": {
                "api": "https://api.deepseek.com",
                "models": ["deepseek-vl"]
            }
        }
    }))
    .expect("valid catalog fixture")
}

#[test]
fn embedded_allowlist_is_valid_and_contains_regression_provider() {
    let catalog = parse_catalog(IMAGE_INPUT_CATALOG_JSON).expect("valid embedded catalog");

    assert!(catalog.providers.contains_key("dashscope"));
    assert!(catalog.providers.contains_key("moonshot-global"));
}

#[test]
fn rejects_unknown_catalog_schema_version() {
    let error = parse_catalog(r#"{"schema_version":2,"providers":{"openai":{"models":["gpt-4o"]}}}"#)
        .expect_err("unknown schemas must fail closed");

    assert!(error.contains("unsupported catalog schema version 2"));
}

#[test]
fn embedded_allowlist_resolves_regression_models_without_network() {
    assert_eq!(
        resolve_image_input_capability(
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "qwen3.7-plus",
        ),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_image_input_capability(
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "kimi-k2.6",
        ),
        ImageInputCapability::Unknown
    );
    assert_eq!(
        resolve_image_input_capability("openai", Some("https://api.moonshot.ai/v1"), "kimi-k2.6"),
        ImageInputCapability::Supported
    );
}

#[test]
fn resolves_supported_and_unlisted_models_on_the_same_provider() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "qwen3.7-plus",
        ),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "kimi-k2.6",
        ),
        ImageInputCapability::Unknown
    );
}

#[test]
fn resolves_same_model_id_by_provider_api_not_model_name_alone() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.moonshot.ai/v1"), "kimi-k2.6",),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://openrouter.ai/api/v1"), "kimi-k2.6",),
        ImageInputCapability::Unknown
    );
}

#[test]
fn normalizes_bedrock_inference_profile_prefixes() {
    let catalog = catalog();

    for model in [
        "anthropic.claude-sonnet-4-20250514-v1:0",
        "us.anthropic.claude-sonnet-4-20250514-v1:0",
        "global.anthropic.claude-sonnet-4-20250514-v1:0",
    ] {
        assert_eq!(
            resolve_from_catalog(&catalog, "bedrock", None, model),
            ImageInputCapability::Supported
        );
    }
}

#[test]
fn normalizes_full_endpoint_and_optional_v1_suffix() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://api.deepseek.com/v1/chat/completions?trace=1"),
            "deepseek-vl",
        ),
        ImageInputCapability::Supported
    );
}

#[test]
fn maps_official_provider_hosts_without_catalog_api_urls() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.openai.com/v1"), "gpt-4o",),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            "models/gemini-2.5-flash",
        ),
        ImageInputCapability::Supported
    );
}

#[test]
fn unknown_provider_or_model_fails_closed_as_unknown() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://private.example/v1"), "gpt-4o",),
        ImageInputCapability::Unknown
    );
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.openai.com/v1"), "missing-model"),
        ImageInputCapability::Unknown
    );
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("not-a-url"), "gpt-4o"),
        ImageInputCapability::Unknown
    );
}
