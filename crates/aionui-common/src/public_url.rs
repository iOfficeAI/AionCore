use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Validation errors for user-controlled HTTP endpoints that must stay on the
/// public internet in a multi-user server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublicHttpUrlError {
    #[error("URL must be a valid http or https URL")]
    InvalidUrl,
    #[error("URL must use http or https")]
    UnsupportedScheme,
    #[error("URL must include a host")]
    MissingHost,
    #[error("URL must not include credentials")]
    EmbeddedCredentials,
    #[error("URL must not target a private or local network address")]
    BlockedAddress,
    #[error("hostname did not resolve to an address")]
    NoResolvedAddresses,
}

/// Parse a user-controlled provider URL and reject non-HTTP schemes,
/// credentials, localhost names, and non-public literal IP addresses.
///
/// DNS names require a second check at the transport boundary using
/// [`validate_public_resolved_addresses`].
pub fn validate_public_http_url(raw_url: &str) -> Result<url::Url, PublicHttpUrlError> {
    let url = url::Url::parse(raw_url.trim()).map_err(|_| PublicHttpUrlError::InvalidUrl)?;
    validate_public_http_url_value(&url)?;
    Ok(url)
}

/// Apply public-network URL checks to an already parsed URL. This is useful for
/// validating every redirect target before it is followed.
pub fn validate_public_http_url_value(url: &url::Url) -> Result<(), PublicHttpUrlError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PublicHttpUrlError::UnsupportedScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PublicHttpUrlError::EmbeddedCredentials);
    }

    match url.host().ok_or(PublicHttpUrlError::MissingHost)? {
        url::Host::Domain(host) => {
            let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
            if normalized_host == "localhost" || normalized_host.ends_with(".localhost") {
                return Err(PublicHttpUrlError::BlockedAddress);
            }
        }
        url::Host::Ipv4(address) => {
            if !is_public_ipv4(address) {
                return Err(PublicHttpUrlError::BlockedAddress);
            }
        }
        url::Host::Ipv6(address) => {
            if !is_public_ipv6(address) {
                return Err(PublicHttpUrlError::BlockedAddress);
            }
        }
    }
    Ok(())
}

/// Reject a DNS answer if it is empty or contains any non-public address.
/// Rejecting the complete mixed answer prevents fallback from a public record
/// to a private one.
pub fn validate_public_resolved_addresses(
    addresses: impl IntoIterator<Item = IpAddr>,
) -> Result<(), PublicHttpUrlError> {
    let mut found = false;
    for address in addresses {
        found = true;
        if !is_public_ip(address) {
            return Err(PublicHttpUrlError::BlockedAddress);
        }
    }
    if !found {
        return Err(PublicHttpUrlError::NoResolvedAddresses);
    }
    Ok(())
}

/// Return whether an address is suitable for a public-only outbound request.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _d] = address.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }

    let segments = address.segments();
    // Globally routable unicast space is currently 2000::/3. This excludes
    // unspecified, loopback, unique-local, link-local, and multicast ranges.
    if segments[0] & 0xe000 != 0x2000 {
        return false;
    }
    // Documentation and Teredo ranges are not valid direct provider targets.
    if segments[0] == 0x2001 && matches!(segments[1], 0x0000 | 0x0db8) {
        return false;
    }
    // 6to4 embeds an IPv4 destination; apply the IPv4 policy to it as well.
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_public_ipv4(embedded);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_are_allowed() {
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(address.parse().unwrap()), "{address} must be allowed");
        }
    }

    #[test]
    fn local_private_link_local_and_metadata_addresses_are_blocked() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address} must be blocked");
        }
    }

    #[test]
    fn numeric_and_localhost_url_forms_are_blocked() {
        for raw_url in [
            "http://localhost:8080",
            "http://service.localhost",
            "http://localhost.",
            "http://127.0.0.1",
            "http://127.1",
            "http://2130706433",
            "http://0x7f000001",
            "http://0177.0.0.1",
            "http://[::1]",
            "http://169.254.169.254/latest/meta-data",
        ] {
            assert!(validate_public_http_url(raw_url).is_err(), "{raw_url} must be blocked");
        }
    }

    #[test]
    fn credentials_and_non_http_schemes_are_blocked() {
        for raw_url in [
            "https://user:password@example.com",
            "file:///etc/passwd",
            "ftp://example.com",
        ] {
            assert!(validate_public_http_url(raw_url).is_err(), "{raw_url} must be blocked");
        }
    }

    #[test]
    fn mixed_public_and_private_dns_answer_is_blocked() {
        let result = validate_public_resolved_addresses(["8.8.8.8".parse().unwrap(), "127.0.0.1".parse().unwrap()]);
        assert_eq!(result, Err(PublicHttpUrlError::BlockedAddress));
    }
}
