use aion_config::compat::OpenAiApiMode;
use aion_types::message::ImageInputCapability;

use crate::capability::image_input::resolve_image_input_capability;

use super::{resolve_aionrs_url_and_compat_with_mode, resolve_model_compat_overrides};

#[test]
fn model_settings_resolve_explicit_vision_and_api_overrides() {
    let overrides = resolve_model_compat_overrides(
        "gpt-5.6-sol",
        r#"{
            "gpt-5.6-sol": {
                "image_input": "supported",
                "openai_api_mode": "chat_completions"
            }
        }"#,
    )
    .unwrap();

    assert_eq!(overrides.image_input, Some(ImageInputCapability::Supported));
    assert_eq!(overrides.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
}

#[test]
fn missing_model_settings_keep_vision_and_api_automatic() {
    let overrides = resolve_model_compat_overrides("gpt-5.6-sol", r#"{"gpt-4o":{"image_input":"supported"}}"#).unwrap();

    assert_eq!(overrides.image_input, None);
    assert_eq!(overrides.openai_api_mode, None);
}

#[test]
fn empty_model_settings_preserve_catalog_vision_support() {
    let overrides = resolve_model_compat_overrides("gpt-4o", "{}").unwrap();
    let capability = overrides
        .image_input
        .unwrap_or_else(|| resolve_image_input_capability("openai", Some("https://api.openai.com/v1"), "gpt-4o"));

    assert_eq!(capability, ImageInputCapability::Supported);
}

#[test]
fn unsupported_image_input_explicitly_disables_vision() {
    let overrides = resolve_model_compat_overrides("gpt-4o", r#"{"gpt-4o":{"image_input":"unsupported"}}"#).unwrap();

    assert_eq!(overrides.image_input, Some(ImageInputCapability::Unsupported));
}

#[test]
fn omitted_image_input_in_a_model_entry_keeps_catalog_automatic() {
    let overrides = resolve_model_compat_overrides("gpt-4o", r#"{"gpt-4o":{"openai_api_mode":"responses"}}"#).unwrap();

    assert_eq!(overrides.image_input, None);
    assert_eq!(overrides.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn invalid_model_settings_are_rejected() {
    let result = resolve_model_compat_overrides("gpt-5.6-sol", "not-json");

    assert!(result.is_err());
}

#[test]
fn explicit_chat_completions_overrides_gpt_5_6_responses_default() {
    let (base_url, compat) = resolve_aionrs_url_and_compat_with_mode(
        "openai",
        "https://api.openai.com/v1",
        "openai",
        "gpt-5.6-sol",
        false,
        Some(OpenAiApiMode::ChatCompletions),
    );

    assert_eq!(base_url.as_deref(), Some("https://api.openai.com/v1"));
    assert_eq!(compat.api_path.as_deref(), Some("/chat/completions"));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
}

#[test]
fn explicit_responses_overrides_non_responses_model_default() {
    let (_, compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1",
        "openai",
        "gpt-4o",
        false,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(compat.api_path.as_deref(), Some("/responses"));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn explicit_api_mode_rewrites_known_complete_endpoints_in_both_directions() {
    let (chat_url, chat_compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1/responses",
        "openai",
        "gpt-5.6-sol",
        true,
        Some(OpenAiApiMode::ChatCompletions),
    );
    let (responses_url, responses_compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/v1/chat/completions",
        "openai",
        "gpt-4o",
        true,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(
        chat_url.as_deref(),
        Some("https://proxy.example.com/v1/chat/completions")
    );
    assert_eq!(chat_compat.openai_api_mode, Some(OpenAiApiMode::ChatCompletions));
    assert_eq!(responses_url.as_deref(), Some("https://proxy.example.com/v1/responses"));
    assert_eq!(responses_compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}

#[test]
fn explicit_api_mode_keeps_unrecognized_complete_url_but_controls_wire_format() {
    let (base_url, compat) = resolve_aionrs_url_and_compat_with_mode(
        "custom",
        "https://proxy.example.com/generate",
        "openai",
        "gpt-4o",
        true,
        Some(OpenAiApiMode::Responses),
    );

    assert_eq!(base_url.as_deref(), Some("https://proxy.example.com/generate"));
    assert_eq!(compat.api_path.as_deref(), Some(""));
    assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses));
}
