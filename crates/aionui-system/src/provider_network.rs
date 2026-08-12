use aionui_common::validate_public_http_url;

use crate::error::SystemError;
use crate::model_fetcher::OutboundNetworkPolicy;
use crate::model_fetcher::network_guard;

/// Exact official provider hosts available to WebUI members. Aionrs v0.2.10
/// owns its HTTP client, so its resolver and redirect behavior cannot be
/// replaced by AionCore. This list removes user-owned origins from that
/// transport; never add wildcards or user-controlled subdomains.
const MEMBER_PROVIDER_HOSTS: &[&str] = &[
    "api-inference.modelscope.cn",
    "api.anthropic.com",
    "api.deepseek.com",
    "api.hunyuan.cloud.tencent.com",
    "api.lingyiwanwu.com",
    "api.minimaxi.com",
    "api.moonshot.ai",
    "api.moonshot.cn",
    "api.novita.ai",
    "api.openai.com",
    "api.poe.com",
    "api.ppinfra.com",
    "api.siliconflow.cn",
    "api.siliconflow.com",
    "api.stepfun.com",
    "api.x.ai",
    "ark.cn-beijing.volces.com",
    "cloud.infini-ai.com",
    "coding.dashscope.aliyuncs.com",
    "dashscope.aliyuncs.com",
    "generativelanguage.googleapis.com",
    "open.bigmodel.cn",
    "openrouter.ai",
    "qianfan.baidubce.com",
    "wishub-x1.ctyun.cn",
];

const MEMBER_HTTP_PROVIDER_PLATFORMS: &[&str] = &[
    "anthropic",
    "claude",
    "custom",
    "dashscope-coding",
    "gemini",
    "minimax",
    "new-api",
    "openai",
];

/// Validate the effective provider destination immediately before a WebUI
/// member's Aionrs runtime is built.
///
/// The exact-host policy compensates for Aionrs owning its HTTP client, while
/// the fresh DNS lookup rejects non-public and mixed public/private answers at
/// the closest available point to the live request.
pub async fn validate_member_provider_runtime(platform: &str, base_url: &str) -> Result<(), SystemError> {
    validate_member_provider_endpoint(platform, base_url)?;
    network_guard::validate_url(OutboundNetworkPolicy::PublicOnly, base_url).await
}

pub(crate) fn validate_member_provider_endpoint(platform: &str, base_url: &str) -> Result<(), SystemError> {
    let platform = platform.trim().to_ascii_lowercase();
    if !MEMBER_HTTP_PROVIDER_PLATFORMS.contains(&platform.as_str()) {
        return Err(SystemError::BadRequest(
            "Provider platform is not available to WebUI members".into(),
        ));
    }

    let url =
        validate_public_http_url(base_url).map_err(|error| SystemError::BadRequest(format!("Provider {error}")))?;
    if url.scheme() != "https" {
        return Err(SystemError::BadRequest("Provider URL must use https".into()));
    }
    if url.port().is_some_and(|port| port != 443) {
        return Err(SystemError::BadRequest(
            "Provider URL must use the default https port".into(),
        ));
    }

    let host = url
        .host_str()
        .ok_or_else(|| SystemError::BadRequest("Provider URL must include a host".into()))?;
    let allowed_for_platform = match platform.as_str() {
        "anthropic" | "claude" => host == "api.anthropic.com",
        "dashscope-coding" => host == "coding.dashscope.aliyuncs.com",
        "gemini" => host == "generativelanguage.googleapis.com",
        "minimax" => host == "api.minimaxi.com",
        "openai" => host == "api.openai.com",
        "custom" | "new-api" => MEMBER_PROVIDER_HOSTS.contains(&host),
        _ => false,
    };
    if !allowed_for_platform {
        return Err(SystemError::BadRequest(
            "Provider host is not an approved WebUI member endpoint".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_provider_presets_are_allowed_on_exact_https_hosts() {
        for host in MEMBER_PROVIDER_HOSTS {
            let url = format!("https://{host}/v1");
            assert!(
                validate_member_provider_endpoint("custom", &url).is_ok(),
                "{url} must be allowed"
            );
        }

        assert!(validate_member_provider_endpoint("anthropic", "https://api.anthropic.com").is_ok());
        assert!(validate_member_provider_endpoint("gemini", "https://generativelanguage.googleapis.com").is_ok());
        assert!(validate_member_provider_endpoint("openai", "https://api.openai.com/v1").is_ok());
    }

    #[test]
    fn member_provider_endpoint_rejects_unapproved_hosts_and_http() {
        for (platform, base_url) in [
            ("custom", "https://api.example.com/v1"),
            ("custom", "https://evil.api.openai.com/v1"),
            ("custom", "https://api.openai.com.attacker.test/v1"),
            ("custom", "http://api.openai.com/v1"),
            ("custom", "https://api.openai.com:8443/v1"),
            ("anthropic", "https://api.openai.com/v1"),
            ("gemini", "https://api.anthropic.com"),
        ] {
            assert!(
                validate_member_provider_endpoint(platform, base_url).is_err(),
                "{platform} endpoint {base_url} must be blocked"
            );
        }
    }

    #[test]
    fn member_provider_endpoint_rejects_sdk_and_unknown_platforms() {
        for platform in ["bedrock", "gemini-vertex-ai", "vertex-ai", "unknown", ""] {
            assert!(
                validate_member_provider_endpoint(platform, "https://api.openai.com/v1").is_err(),
                "{platform} must be blocked"
            );
        }
    }
}
