use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use aion_types::message::ImageInputCapability;
use http::Uri;
use serde::Deserialize;
use tracing::error;

const IMAGE_INPUT_CATALOG_SCHEMA_VERSION: u32 = 1;
const IMAGE_INPUT_CATALOG_JSON: &str = include_str!("../../assets/model-capabilities/image_input_models.json");
const BEDROCK_INFERENCE_PROFILE_PREFIXES: [&str; 6] = ["us.", "eu.", "apac.", "au.", "jp.", "global."];

static IMAGE_INPUT_CATALOG: OnceLock<Option<ImageInputCatalog>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct ImageInputCatalog {
    schema_version: u32,
    providers: HashMap<String, ImageInputProvider>,
}

#[derive(Debug, Deserialize)]
struct ImageInputProvider {
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: HashSet<String>,
}

pub(crate) fn resolve_image_input_capability(
    provider: &str,
    base_url: Option<&str>,
    model: &str,
) -> ImageInputCapability {
    embedded_catalog()
        .map(|catalog| resolve_from_catalog(catalog, provider, base_url, model))
        .unwrap_or(ImageInputCapability::Unknown)
}

fn embedded_catalog() -> Option<&'static ImageInputCatalog> {
    IMAGE_INPUT_CATALOG
        .get_or_init(|| match parse_catalog(IMAGE_INPUT_CATALOG_JSON) {
            Ok(catalog) => Some(catalog),
            Err(parse_error) => {
                error!(error = %parse_error, "Failed to parse embedded image input model catalog");
                None
            }
        })
        .as_ref()
}

fn parse_catalog(json: &str) -> Result<ImageInputCatalog, String> {
    let catalog = serde_json::from_str::<ImageInputCatalog>(json)
        .map_err(|parse_error| format!("invalid catalog JSON: {parse_error}"))?;
    if catalog.schema_version != IMAGE_INPUT_CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "unsupported catalog schema version {}; expected {IMAGE_INPUT_CATALOG_SCHEMA_VERSION}",
            catalog.schema_version
        ));
    }
    if catalog.providers.is_empty() {
        return Err("catalog contains no providers".to_owned());
    }
    Ok(catalog)
}

fn resolve_from_catalog(
    catalog: &ImageInputCatalog,
    provider: &str,
    base_url: Option<&str>,
    model: &str,
) -> ImageInputCapability {
    for provider_id in resolve_provider_ids(catalog, provider, base_url) {
        let Some(candidate) = catalog.providers.get(provider_id) else {
            continue;
        };
        if model_supports_image(candidate, model) {
            return ImageInputCapability::Supported;
        }
    }

    ImageInputCapability::Unknown
}

fn resolve_provider_ids<'a>(catalog: &'a ImageInputCatalog, provider: &str, base_url: Option<&str>) -> Vec<&'a str> {
    if let Some(raw_base_url) = base_url {
        let Some(base_url) = normalize_api_root(raw_base_url) else {
            return Vec::new();
        };
        let matches = catalog
            .providers
            .iter()
            .filter_map(|(provider_id, candidate)| {
                let candidate_api = candidate.api.as_deref().and_then(normalize_api_root)?;
                api_roots_match(&base_url, &candidate_api).then_some(provider_id.as_str())
            })
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return matches;
        }

        if let Some(provider_id) = official_provider_id(provider, &base_url) {
            return vec![provider_id];
        }
        return Vec::new();
    }

    builtin_provider_id(provider).into_iter().collect()
}

fn official_provider_id(provider: &str, api_root: &str) -> Option<&'static str> {
    let host = api_root.split('/').next()?;
    match (provider, host) {
        ("openai", "api.openai.com") => Some("openai"),
        ("openai", "generativelanguage.googleapis.com") => Some("google"),
        ("anthropic", "api.anthropic.com") => Some("anthropic"),
        ("vertex", _) => Some("google-vertex"),
        ("bedrock", _) => Some("amazon-bedrock"),
        _ => None,
    }
}

fn builtin_provider_id(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "vertex" => Some("google-vertex"),
        "bedrock" => Some("amazon-bedrock"),
        _ => None,
    }
}

fn model_supports_image(provider: &ImageInputProvider, model: &str) -> bool {
    provider.models.contains(normalize_model_id(model))
}

fn normalize_model_id(model: &str) -> &str {
    let model = model.strip_prefix("models/").unwrap_or(model);
    BEDROCK_INFERENCE_PROFILE_PREFIXES
        .iter()
        .find_map(|prefix| {
            model
                .strip_prefix(prefix)
                .filter(|model| model.starts_with("anthropic."))
        })
        .unwrap_or(model)
}

fn normalize_api_root(raw_url: &str) -> Option<String> {
    let uri = raw_url.trim().parse::<Uri>().ok()?;
    match uri.scheme_str()? {
        "http" | "https" => {}
        _ => return None,
    }
    let authority = uri.authority()?.as_str().to_ascii_lowercase();
    let mut path = uri.path().trim_end_matches('/').to_owned();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if let Some(prefix) = path.strip_suffix(suffix) {
            path = prefix.trim_end_matches('/').to_owned();
            break;
        }
    }
    Some(format!("{authority}{path}"))
}

fn api_roots_match(left: &str, right: &str) -> bool {
    left == right || strip_version_suffix(left) == Some(right) || strip_version_suffix(right) == Some(left)
}

fn strip_version_suffix(api_root: &str) -> Option<&str> {
    api_root.strip_suffix("/v1")
}

#[cfg(test)]
#[path = "image_input_test.rs"]
mod image_input_test;
