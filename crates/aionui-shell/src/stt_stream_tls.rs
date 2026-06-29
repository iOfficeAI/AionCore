//! Shared TLS connector for STT streaming WebSocket upstreams.
//!
//! `tokio_tungstenite::connect_async` relies on rustls's process-level default
//! `CryptoProvider`, which is never installed in this workspace — a real
//! `wss://` connect panics with "Could not automatically determine the
//! process-level CryptoProvider". Mirrors the explicit-connector pattern
//! established by the lark/dingtalk channel plugins
//! (`aionui-channel/src/plugins/lark/plugin.rs`).

use std::sync::Arc;

use crate::error::SttError;

/// Build an explicit rustls connector for upstream WebSocket connections.
///
/// The connector is only consulted for `wss://` targets;
/// `connect_async_tls_with_config` ignores it for plain `ws://` URLs, so
/// callers can pass it unconditionally.
///
/// Explicitly sets ALPN to `http/1.1` only — WebSocket requires an HTTP/1.1
/// upgrade handshake and is incompatible with h2. Without this, some servers
/// negotiate h2 via ALPN and the WebSocket upgrade never completes.
pub(crate) fn build_ws_connector() -> Result<tokio_tungstenite::Connector, SttError> {
    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| SttError::RequestFailed(format!("TLS config error: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_builds_with_rustls_tls() {
        match build_ws_connector() {
            Ok(tokio_tungstenite::Connector::Rustls(config)) => {
                assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
            }
            Ok(_) => panic!("expected a Rustls connector"),
            Err(e) => panic!("connector build failed: {e}"),
        }
    }
}
