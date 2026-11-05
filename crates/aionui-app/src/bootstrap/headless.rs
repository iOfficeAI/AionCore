//! E14 headless Host startup policy (CSBU WorkMate).
//!
//! Defaults: loopback bind only; existing workspace auth required (no `--local`).
//! SSH is transport-only — clients tunnel to this loopback listener with system
//! OpenSSH. No AGPL dependencies.

use super::error::{BootstrapError, BootstrapErrorCode};

/// Inputs checked before a headless Host binds a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadlessBindRequest<'a> {
    pub headless: bool,
    pub host: &'a str,
    pub local: bool,
    pub allow_non_loopback_bind: bool,
}

/// Return whether `host` is a loopback / unspecified-local bind target.
///
/// `0.0.0.0` / `::` are *not* loopback — they accept remote clients and are
/// rejected in headless mode unless `--allow-non-loopback-bind` is set (D14-1).
pub(crate) fn is_loopback_bind_host(host: &str) -> bool {
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// Validate headless Host bind + auth policy. No-op when `headless` is false.
pub(crate) fn validate_headless_bind(req: HeadlessBindRequest<'_>) -> Result<(), BootstrapError> {
    if !req.headless {
        return Ok(());
    }

    if req.local {
        return Err(BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "headless.auth",
            "headless Host requires existing workspace auth; refuse --local / embedded skip-auth",
        )
        .with_field("flag", "--local"));
    }

    if !is_loopback_bind_host(req.host) && !req.allow_non_loopback_bind {
        return Err(BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "headless.bind",
            "headless Host binds loopback only; pass --allow-non-loopback-bind to opt in",
        )
        .with_field("host", req.host));
    }

    Ok(())
}

/// Stable ready line for smoke scripts / SSH tunnel clients (stdout).
pub(crate) fn headless_ready_line(host: &str, port: u16) -> String {
    let display_host = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    format!("WORKMATE_HOST_READY mode=headless url=http://{display_host}:{port}/health")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_headless_always_ok() {
        assert!(
            validate_headless_bind(HeadlessBindRequest {
                headless: false,
                host: "0.0.0.0",
                local: true,
                allow_non_loopback_bind: false,
            })
            .is_ok()
        );
    }

    #[test]
    fn headless_rejects_local_auth_skip() {
        let err = validate_headless_bind(HeadlessBindRequest {
            headless: true,
            host: "127.0.0.1",
            local: true,
            allow_non_loopback_bind: false,
        })
        .expect_err("local skip-auth must fail");
        assert_eq!(err.code(), BootstrapErrorCode::ConfigInvalid);
        assert_eq!(err.stage(), "headless.auth");
    }

    #[test]
    fn headless_rejects_wildcard_without_opt_in() {
        let err = validate_headless_bind(HeadlessBindRequest {
            headless: true,
            host: "0.0.0.0",
            local: false,
            allow_non_loopback_bind: false,
        })
        .expect_err("wildcard bind must fail");
        assert_eq!(err.stage(), "headless.bind");
    }

    #[test]
    fn headless_allows_wildcard_with_opt_in() {
        assert!(
            validate_headless_bind(HeadlessBindRequest {
                headless: true,
                host: "0.0.0.0",
                local: false,
                allow_non_loopback_bind: true,
            })
            .is_ok()
        );
    }

    #[test]
    fn headless_allows_loopback() {
        for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            assert!(
                validate_headless_bind(HeadlessBindRequest {
                    headless: true,
                    host,
                    local: false,
                    allow_non_loopback_bind: false,
                })
                .is_ok(),
                "host={host}"
            );
        }
    }

    #[test]
    fn ready_line_uses_loopback_for_wildcard() {
        let line = headless_ready_line("0.0.0.0", 25808);
        assert!(line.contains("http://127.0.0.1:25808/health"));
        assert!(line.contains("mode=headless"));
    }
}
