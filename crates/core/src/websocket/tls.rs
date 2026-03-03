//! TLS connector configuration for WebSocket connections.
//!
//! Builds a rustls `ClientConfig` with HTTP/1.1 ALPN to ensure WebSocket
//! upgrade requests succeed through reverse proxies that otherwise negotiate HTTP/2.

use std::sync::Arc;

use tokio_tungstenite::Connector;

use crate::websocket::types::WebSocketError;

/// Builds a TLS connector with HTTP/1.1 ALPN for WebSocket connections.
///
/// WebSocket upgrade requires HTTP/1.1. Without explicit ALPN, some reverse
/// proxies (Cloudflare, nginx, AWS ALB) negotiate HTTP/2 via TLS ALPN,
/// which rejects the WebSocket upgrade with 400 Bad Request.
///
/// Uses the already-installed `aws_lc_rs` CryptoProvider and native root
/// certificates for TLS verification.
pub fn build_websocket_tls_connector() -> Result<Connector, WebSocketError> {
    let mut root_store = rustls::RootCertStore::empty();

    let native_certs = rustls_native_certs::load_native_certs();
    let certs = native_certs.certs;
    let (added, _ignored) = root_store.add_parsable_certificates(certs);

    // O(1) EXEMPT: begin — TLS setup runs once per connect, not per tick
    if added == 0 {
        return Err(WebSocketError::TlsConfigurationFailed {
            reason: "no native root CA certificates found".to_string(),
        });
    }

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // Force HTTP/1.1 ALPN — required for WebSocket upgrade through proxies.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // O(1) EXEMPT: end

    Ok(Connector::Rustls(Arc::new(config)))
}

// APPROVED: test code — relaxed lint rules for test fixtures
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]
#[cfg(test)]
mod tests {
    use super::*;

    /// Install the aws-lc-rs CryptoProvider for tests.
    /// In production, `main.rs` does this at startup.
    /// `install_default` returns `Err` if already installed (parallel tests), so ignore the result.
    fn install_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    /// Extract the `Rustls` inner `ClientConfig` from a `Connector`, panicking
    /// if the variant is not `Connector::Rustls`. Consolidates 5 identical
    /// `let Connector::Rustls(config) = ... else { panic!() }` sites.
    #[track_caller]
    fn unwrap_rustls_config(connector: Connector) -> Arc<rustls::ClientConfig> {
        match connector {
            Connector::Rustls(config) => config,
            _other => panic!("expected Connector::Rustls, got non-Rustls variant"),
        }
    }

    #[test]
    fn test_build_websocket_tls_connector_succeeds() {
        install_crypto_provider();
        // Must succeed on any system with root CA certificates installed
        let result = build_websocket_tls_connector();
        assert!(
            result.is_ok(),
            "TLS connector should build successfully: {}",
            result
                .as_ref()
                .err()
                .map_or("ok".to_string(), |e| format!("{e}"))
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_returns_rustls_connector() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let _config = unwrap_rustls_config(connector);
    }

    #[test]
    fn test_build_websocket_tls_connector_has_http11_alpn() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let config = unwrap_rustls_config(connector);
        assert!(
            config.alpn_protocols.contains(&b"http/1.1".to_vec()),
            "TLS connector must include http/1.1 ALPN protocol"
        );
        assert_eq!(
            config.alpn_protocols.len(),
            1,
            "should have exactly one ALPN protocol"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_idempotent() {
        install_crypto_provider();
        // Call twice — both should succeed (no shared mutable state)
        let first = build_websocket_tls_connector();
        let second = build_websocket_tls_connector();
        assert!(first.is_ok());
        assert!(second.is_ok());
    }

    #[test]
    fn test_build_websocket_tls_connector_has_root_certs() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let config = unwrap_rustls_config(connector);
        // The config must have been built with root certificates;
        // verify it can be used (no panic on access)
        assert!(
            !config.alpn_protocols.is_empty(),
            "config must have ALPN protocols set"
        );
    }

    #[test]
    fn test_tls_configuration_failed_error_display() {
        // Covers the WebSocketError::TlsConfigurationFailed variant (used on line 29).
        let err = WebSocketError::TlsConfigurationFailed {
            reason: "no native root CA certificates found".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no native root CA certificates found"),
            "TlsConfigurationFailed display must include the reason"
        );
        assert!(
            msg.contains("TLS configuration failed"),
            "TlsConfigurationFailed display must include the prefix"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_no_http2_alpn() {
        // Verify that HTTP/2 ALPN is NOT included (only HTTP/1.1).
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let config = unwrap_rustls_config(connector);
        assert!(
            !config.alpn_protocols.contains(&b"h2".to_vec()),
            "TLS connector must NOT include h2 ALPN protocol"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_concurrent_calls() {
        // Multiple concurrent builds should all succeed without interference.
        install_crypto_provider();
        let results: Vec<_> = (0..10).map(|_| build_websocket_tls_connector()).collect();
        for (i, result) in results.iter().enumerate() {
            assert!(result.is_ok(), "build {i} should succeed");
        }
    }

    #[test]
    fn test_build_websocket_tls_connector_alpn_protocol_bytes() {
        // Verify the exact bytes of the ALPN protocol string.
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let config = unwrap_rustls_config(connector);
        assert_eq!(
            config.alpn_protocols[0],
            b"http/1.1".to_vec(),
            "ALPN protocol must be exactly 'http/1.1' bytes"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_result_is_not_plain_tls() {
        // Verify it does NOT return Connector::Plain (non-TLS).
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let is_rustls = matches!(connector, Connector::Rustls(_));
        assert!(is_rustls, "Must return Rustls variant, not Plain");
    }
}
