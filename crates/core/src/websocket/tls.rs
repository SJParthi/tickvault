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
