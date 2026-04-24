//! TLS connector configuration for WebSocket connections.
//!
//! Two builders:
//! - [`build_websocket_tls_connector`]: forces `http/1.1` ALPN. Used by main
//!   feed, 20-level depth, and order-update sockets, which all transit
//!   reverse proxies that would otherwise negotiate HTTP/2 and reject the
//!   WebSocket upgrade.
//! - [`build_websocket_tls_connector_no_alpn`]: advertises NO ALPN protocol.
//!   Used by 200-level depth ONLY, matching Dhan's Python SDK
//!   `dhanhq==2.2.0rc1` exactly (Parthiban verified 2026-04-23 on
//!   SecurityId 72271). Any other combination (tungstenite default UA +
//!   http/1.1 ALPN) produced `Protocol(ResetWithoutClosingHandshake)`
//!   disconnects for 2+ weeks on `full-depth-api.dhan.co`.

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
    build_tls_connector_inner(true)
}

/// Builds a TLS connector WITHOUT any ALPN protocol for the 200-level depth
/// socket.
///
/// Matches the wire behavior of Dhan's Python SDK `dhanhq==2.2.0rc1` which
/// uses the stock `websockets/16.0` library — that library does not set
/// `alpn_protocols` on its `ssl.SSLContext`, so the TLS ClientHello advertises
/// no ALPN. Parthiban ran the Python SDK against our account at SecurityId
/// 72271 on 2026-04-23 and the connection streamed for 30+ minutes with zero
/// disconnects, while our Rust client with `http/1.1` ALPN kept getting
/// `Protocol(ResetWithoutClosingHandshake)`.
///
/// **DO NOT reuse this builder for main feed, 20-level depth, or order-update
/// sockets.** Those go through reverse proxies that require `http/1.1` ALPN.
pub fn build_websocket_tls_connector_no_alpn() -> Result<Connector, WebSocketError> {
    build_tls_connector_inner(false)
}

fn build_tls_connector_inner(force_http11_alpn: bool) -> Result<Connector, WebSocketError> {
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

    if force_http11_alpn {
        // Force HTTP/1.1 ALPN — required for WebSocket upgrade through proxies.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }
    // O(1) EXEMPT: end

    Ok(Connector::Rustls(Arc::new(config)))
}

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

    // -----------------------------------------------------------------------
    // TLS error variant coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_tls_configuration_failed_error_debug_format() {
        let err = WebSocketError::TlsConfigurationFailed {
            reason: "no native root CA certificates found".to_string(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("TlsConfigurationFailed"));
        assert!(debug.contains("no native root CA certificates found"));
    }

    #[test]
    fn test_tls_configuration_failed_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WebSocketError>();
    }

    // -----------------------------------------------------------------------
    // Verify ALPN protocol is exactly HTTP/1.1 (not HTTP/2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_websocket_tls_connector_alpn_only_http11() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector().unwrap();
        let config = unwrap_rustls_config(connector);
        // Exactly one ALPN protocol
        assert_eq!(config.alpn_protocols.len(), 1);
        // Must be http/1.1
        assert_eq!(config.alpn_protocols[0], b"http/1.1");
        // Must NOT contain h2
        assert!(!config.alpn_protocols.contains(&b"h2".to_vec()));
    }

    // -----------------------------------------------------------------------
    // Multiple sequential builds produce independent configs
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_websocket_tls_connector_produces_independent_configs() {
        install_crypto_provider();
        let c1 = build_websocket_tls_connector().unwrap();
        let c2 = build_websocket_tls_connector().unwrap();
        let cfg1 = unwrap_rustls_config(c1);
        let cfg2 = unwrap_rustls_config(c2);
        // They should be different Arc instances (not sharing state)
        assert!(!Arc::ptr_eq(&cfg1, &cfg2));
    }

    // -----------------------------------------------------------------------
    // Error path — TlsConfigurationFailed variant exercises
    // -----------------------------------------------------------------------

    #[test]
    fn test_tls_configuration_failed_error_matches_pattern() {
        let err = WebSocketError::TlsConfigurationFailed {
            reason: "no native root CA certificates found".to_string(),
        };
        assert!(matches!(err, WebSocketError::TlsConfigurationFailed { .. }));
    }

    #[test]
    fn test_tls_configuration_failed_error_reason_preserved() {
        let reason = "test reason for TLS failure";
        let err = WebSocketError::TlsConfigurationFailed {
            reason: reason.to_string(),
        };
        if let WebSocketError::TlsConfigurationFailed { reason: r } = err {
            assert_eq!(r, reason);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn test_tls_configuration_failed_empty_reason() {
        let err = WebSocketError::TlsConfigurationFailed {
            reason: String::new(),
        };
        let msg = err.to_string();
        assert!(msg.contains("TLS configuration failed"));
    }

    // -----------------------------------------------------------------------
    // TLS connector — root cert store is populated
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_websocket_tls_connector_root_certs_loaded() {
        install_crypto_provider();
        // If the function succeeds, it means root certs were found and added > 0.
        // If it returns Err, it means 0 root certs were found.
        let result = build_websocket_tls_connector();
        assert!(
            result.is_ok(),
            "TLS connector should succeed on any system with root CAs"
        );
    }

    // -----------------------------------------------------------------------
    // TLS connector — ALPN is http/1.1 not h2 (WebSocket compatibility)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_websocket_tls_connector_no_h2_alpn_even_if_called_twice() {
        install_crypto_provider();
        for _ in 0..3 {
            let connector = build_websocket_tls_connector().unwrap();
            let config = unwrap_rustls_config(connector);
            assert_eq!(config.alpn_protocols.len(), 1);
            assert_eq!(config.alpn_protocols[0], b"http/1.1");
        }
    }

    // -----------------------------------------------------------------------
    // No-ALPN connector (200-level depth only — matches Python SDK wire behavior)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_websocket_tls_connector_no_alpn_succeeds() {
        install_crypto_provider();
        let result = build_websocket_tls_connector_no_alpn();
        assert!(result.is_ok(), "no-ALPN TLS connector must build");
    }

    #[test]
    fn test_build_websocket_tls_connector_no_alpn_has_empty_alpn_list() {
        // Matches Python SDK `dhanhq==2.2.0rc1` which uses `websockets/16.0`
        // with default SSLContext — no ALPN advertised. Verified 2026-04-23.
        install_crypto_provider();
        let connector = build_websocket_tls_connector_no_alpn().unwrap();
        let config = unwrap_rustls_config(connector);
        assert!(
            config.alpn_protocols.is_empty(),
            "200-depth TLS connector MUST NOT advertise any ALPN protocol; \
             anything else regresses to ResetWithoutClosingHandshake on full-depth-api.dhan.co"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_no_alpn_does_not_contain_http11() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector_no_alpn().unwrap();
        let config = unwrap_rustls_config(connector);
        assert!(
            !config.alpn_protocols.contains(&b"http/1.1".to_vec()),
            "no-ALPN connector must not contain http/1.1 — that combo caused \
             2+ weeks of 200-depth disconnects; use build_websocket_tls_connector() for \
             main feed/20-depth/order-update instead"
        );
    }

    #[test]
    fn test_build_websocket_tls_connector_no_alpn_does_not_contain_h2() {
        install_crypto_provider();
        let connector = build_websocket_tls_connector_no_alpn().unwrap();
        let config = unwrap_rustls_config(connector);
        assert!(!config.alpn_protocols.contains(&b"h2".to_vec()));
    }

    #[test]
    fn test_http11_and_no_alpn_connectors_produce_distinct_alpn_lists() {
        install_crypto_provider();
        let with_alpn = build_websocket_tls_connector().unwrap();
        let no_alpn = build_websocket_tls_connector_no_alpn().unwrap();
        let cfg_with = unwrap_rustls_config(with_alpn);
        let cfg_no = unwrap_rustls_config(no_alpn);
        assert_eq!(cfg_with.alpn_protocols.len(), 1);
        assert!(cfg_no.alpn_protocols.is_empty());
    }
}
