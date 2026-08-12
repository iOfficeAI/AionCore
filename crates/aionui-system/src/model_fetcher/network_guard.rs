use std::io;
use std::sync::Arc;
use std::time::Duration;

use aionui_common::{
    PublicHttpUrlError, validate_public_http_url, validate_public_http_url_value, validate_public_resolved_addresses,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;

use crate::error::SystemError;

const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REDIRECTS: usize = 10;

/// Controls whether a provider probe may intentionally connect to services on
/// the host or private network. WebUI members use `PublicOnly`; local mode and
/// live site administrators may use `Unrestricted` for local model servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundNetworkPolicy {
    PublicOnly,
    Unrestricted,
}

/// Select an HTTP client for one provider operation.
///
/// The public-only client disables environment proxies, filters DNS results at
/// connection time, and applies the same literal-address checks to each
/// redirect. The separate up-front lookup produces a useful client error while
/// the resolver remains the enforcement point if DNS changes between checks.
pub(crate) async fn client_for_url(
    unrestricted_client: &reqwest::Client,
    policy: OutboundNetworkPolicy,
    raw_url: &str,
) -> Result<reqwest::Client, SystemError> {
    validate_url(policy, raw_url).await?;
    if policy == OutboundNetworkPolicy::Unrestricted {
        return Ok(unrestricted_client.clone());
    }

    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(PublicDnsResolver))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            if let Some(previous) = attempt.previous().last()
                && !redirect_target_allowed(previous, attempt.url())
            {
                return attempt.error("cross-host or insecure provider redirect blocked");
            }
            match validate_public_http_url_value(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(error) => attempt.error(error),
            }
        }))
        .build()
        .map_err(|_| SystemError::Internal("Failed to configure secure provider HTTP client".into()))
}

fn redirect_target_allowed(previous: &reqwest::Url, next: &reqwest::Url) -> bool {
    if previous.host_str() != next.host_str() {
        return false;
    }

    match (previous.scheme(), next.scheme()) {
        ("http", "https") => previous.port_or_known_default() == Some(80) && next.port_or_known_default() == Some(443),
        (previous_scheme, next_scheme) if previous_scheme == next_scheme => {
            previous.port_or_known_default() == next.port_or_known_default()
        }
        _ => false,
    }
}

pub(crate) async fn validate_url(policy: OutboundNetworkPolicy, raw_url: &str) -> Result<(), SystemError> {
    if policy == OutboundNetworkPolicy::Unrestricted {
        let url = reqwest::Url::parse(raw_url.trim())
            .map_err(|_| SystemError::BadRequest("baseUrl must be a valid http or https URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(SystemError::BadRequest(
                "baseUrl must be a valid http or https URL".into(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(SystemError::BadRequest("baseUrl must not include credentials".into()));
        }
        return Ok(());
    }
    validate_public_destination(raw_url).await
}

async fn validate_public_destination(raw_url: &str) -> Result<(), SystemError> {
    let url = validate_public_http_url(raw_url).map_err(public_url_error)?;

    let host = url
        .host_str()
        .ok_or_else(|| SystemError::BadRequest("baseUrl must include a host".into()))?;
    if host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| SystemError::BadRequest("baseUrl must include a valid port".into()))?;
    let host = host.to_owned();
    let resolved = tokio::time::timeout(DNS_LOOKUP_TIMEOUT, tokio::net::lookup_host((host.as_str(), port)))
        .await
        .map_err(|_| SystemError::Timeout("Provider hostname lookup timed out".into()))?
        .map_err(|_| SystemError::BadGateway("Provider hostname could not be resolved".into()))?;

    validate_public_resolved_addresses(resolved.map(|address| address.ip())).map_err(public_url_error)
}

fn public_url_error(error: PublicHttpUrlError) -> SystemError {
    match error {
        PublicHttpUrlError::NoResolvedAddresses => {
            SystemError::BadGateway("Provider hostname did not resolve to an address".into())
        }
        _ => SystemError::BadRequest(format!("Provider {error}")),
    }
}

#[derive(Debug)]
struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let addresses: Vec<_> = resolved.collect();
            validate_public_resolved_addresses(addresses.iter().map(|address| address.ip()))
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn localhost_dns_resolution_is_blocked() {
        let error = validate_public_destination("http://localhost:8080").await.unwrap_err();
        assert!(matches!(error, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn connection_time_resolver_rejects_private_dns_answers() {
        let result = PublicDnsResolver.resolve("localhost".parse().unwrap()).await;
        assert!(result.is_err());
    }

    #[test]
    fn redirects_stay_on_the_same_host_and_cannot_downgrade_tls() {
        let https = reqwest::Url::parse("https://api.example.com/v1/models").unwrap();
        let same_host = reqwest::Url::parse("https://api.example.com/v2/models").unwrap();
        let upgrade = reqwest::Url::parse("https://api.example.com/v1/models").unwrap();
        let http = reqwest::Url::parse("http://api.example.com/v1/models").unwrap();
        let other_port = reqwest::Url::parse("https://api.example.com:8443/v1/models").unwrap();
        let other_host = reqwest::Url::parse("https://redirect.example.net/v1/models").unwrap();

        assert!(redirect_target_allowed(&https, &same_host));
        assert!(redirect_target_allowed(&http, &upgrade));
        assert!(!redirect_target_allowed(&https, &http));
        assert!(!redirect_target_allowed(&https, &other_port));
        assert!(!redirect_target_allowed(&https, &other_host));
    }
}
