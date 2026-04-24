//! Production runtime variant rotation for 200-level depth WebSocket.
//!
//! **Problem:** For 2+ weeks (2026-04-10 through 2026-04-24) our Rust client
//! has been TCP-reset by Dhan's `full-depth-api.dhan.co` endpoint with
//! `Protocol(ResetWithoutClosingHandshake)` on subscribe. Dhan's own Python
//! SDK (`dhanhq==2.2.0rc1`) works on the same account, same token, same
//! SecurityId. The root cause is some subtle protocol-handshake mismatch
//! (URL path, User-Agent, ALPN) between our `tokio-tungstenite` client and
//! Dhan's server-side filter.
//!
//! **Approach:** Instead of waiting for a human operator to run the
//! `depth_200_variants` diagnostic example and report the winning
//! combination, this module ROTATES through the 8 known variants
//! automatically. If the current variant has thrown
//! `ResetWithoutClosingHandshake` more than [`RESET_ROTATE_THRESHOLD`]
//! consecutive times, the next reconnect attempt uses the next variant in
//! the catalog. On first successful frame received, the winning variant is
//! locked in via an atomic counter and exported as a Prometheus metric.
//!
//! **Surface:** one struct [`DepthVariant`], one constant [`VARIANTS`] of
//! length 8, one rotator [`VariantRotator`] with lock-free atomic
//! progression. No heap allocation after boot. No blocking I/O.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::websocket::types::WebSocketError;

/// Consecutive `ResetWithoutClosingHandshake` count on a single variant
/// before we rotate to the next. Chosen to be small enough that all 8
/// variants can be exhausted well inside the overall 60-attempt reconnect
/// cap (8 × 3 = 24 attempts, leaves 36 attempts of margin on the winning
/// variant) AND fast enough that the operator sees convergence within
/// ~3 minutes of boot rather than ~9 minutes.
pub const RESET_ROTATE_THRESHOLD: u64 = 3;

/// User-Agent string emitted by Dhan's Python SDK's underlying
/// `websockets` library at the time of the 2026-04-23 verification.
const PYTHON_WEBSOCKETS_UA: &str = "Python/3.12 websockets/16.0";

/// Chrome-like User-Agent plus Origin header.
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
// APPROVED: browser-mimicry Origin header; not an endpoint URL, not configurable
const BROWSER_ORIGIN: &str = "https://web.dhan.co";

/// User-Agent flavour to send on the WebSocket upgrade request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UaFlavor {
    /// tungstenite's compiled-in default (e.g. `tungstenite-rs/0.24.0`).
    Default,
    /// Mimics Dhan's official Python SDK underlying `websockets` library.
    Python,
    /// Chrome-like UA + Origin header (simulates a browser client).
    Browser,
}

/// ALPN advertisement mode on the TLS ClientHello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlpnMode {
    /// Force `http/1.1` ALPN — matches existing `build_websocket_tls_connector`.
    Http11,
    /// No ALPN — TLS handshake advertises no protocol preference.
    None,
}

/// One variant of URL + UA + ALPN to try against Dhan's 200-depth endpoint.
#[derive(Debug, Clone, Copy)]
pub struct DepthVariant {
    /// Short label for metrics/logs (e.g. `A_root_default_alpn11`).
    pub label: &'static str,
    /// URL path component (e.g. `/` or `/twohundreddepth`).
    pub url_path: &'static str,
    /// User-Agent flavour.
    pub ua: UaFlavor,
    /// ALPN mode.
    pub alpn: AlpnMode,
}

/// The full catalog of 8 variants, in the order the rotator tries them.
///
/// Variant A is the default current production config (root path, default
/// UA, `http/1.1` ALPN) — matches what `depth_connection.rs` used before
/// this module existed. Any successful state therefore starts at variant
/// A, as long as Dhan accepts it.
pub const VARIANTS: [DepthVariant; 8] = [
    DepthVariant {
        label: "A_root_default_alpn11",
        url_path: "/",
        ua: UaFlavor::Default,
        alpn: AlpnMode::Http11,
    },
    DepthVariant {
        label: "C_root_python_alpn11",
        url_path: "/",
        ua: UaFlavor::Python,
        alpn: AlpnMode::Http11,
    },
    DepthVariant {
        label: "E_root_default_noalpn",
        url_path: "/",
        ua: UaFlavor::Default,
        alpn: AlpnMode::None,
    },
    DepthVariant {
        label: "H_root_python_noalpn",
        url_path: "/",
        ua: UaFlavor::Python,
        alpn: AlpnMode::None,
    },
    DepthVariant {
        label: "F_root_browser_alpn11",
        url_path: "/",
        ua: UaFlavor::Browser,
        alpn: AlpnMode::Http11,
    },
    DepthVariant {
        label: "B_twohundred_default_alpn11",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Default,
        alpn: AlpnMode::Http11,
    },
    DepthVariant {
        label: "D_twohundred_python_alpn11",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Python,
        alpn: AlpnMode::Http11,
    },
    DepthVariant {
        label: "G_twohundred_default_noalpn",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Default,
        alpn: AlpnMode::None,
    },
];

/// Lock-free rotator state shared across reconnect attempts on a single
/// 200-depth connection. Cheap to clone (it's an `Arc` around two atomics).
#[derive(Debug, Clone)]
pub struct VariantRotator {
    /// Current variant index into [`VARIANTS`]. Reads via `Ordering::Relaxed`
    /// — one reader per connection, no ABA concern.
    current_idx: Arc<AtomicUsize>,
    /// Consecutive `ResetWithoutClosingHandshake` count on the CURRENT
    /// variant. Resets to zero on rotation or success.
    consecutive_resets: Arc<AtomicU64>,
    /// Whether we have ever received a successful frame on any variant.
    /// Once true, the rotator stops rotating — the locked-in winner is
    /// preserved for the life of the process.
    locked_in: Arc<std::sync::atomic::AtomicBool>,
}

impl VariantRotator {
    /// Constructs a rotator that starts at variant A. O(1), boot-time only.
    // TEST-EXEMPT: exercised by test_rotator_starts_at_variant_a and every other rotator test
    pub fn new() -> Self {
        Self {
            current_idx: Arc::new(AtomicUsize::new(0)),
            consecutive_resets: Arc::new(AtomicU64::new(0)),
            locked_in: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Returns the variant to use on the NEXT connect attempt.
    ///
    /// Does NOT mutate state — `record_reset` and `record_success` are the
    /// mutators. This is separated so the reconnect loop can log the
    /// variant label before attempting and react to the outcome after.
    // TEST-EXEMPT: exercised by test_rotator_starts_at_variant_a, test_rotator_rotates_on_threshold_reached
    pub fn current(&self) -> DepthVariant {
        let idx = self.current_idx.load(Ordering::Relaxed) % VARIANTS.len();
        VARIANTS[idx]
    }

    /// Records a successful connection (first frame received). Locks the
    /// current variant in — future rotation is suppressed.
    // TEST-EXEMPT: exercised by test_rotator_success_locks_in_current_variant
    pub fn record_success(&self) {
        self.locked_in.store(true, Ordering::Relaxed);
        self.consecutive_resets.store(0, Ordering::Relaxed);
    }

    /// Records a `ResetWithoutClosingHandshake` on the current variant.
    /// Returns `true` if this tripped rotation to the next variant, `false`
    /// otherwise. After rotation, [`Self::current`] returns the new variant.
    // TEST-EXEMPT: exercised by test_rotator_rotates_on_threshold_reached, test_rotator_does_not_rotate_before_threshold
    pub fn record_reset(&self) -> bool {
        if self.locked_in.load(Ordering::Relaxed) {
            return false;
        }
        let count = self.consecutive_resets.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= RESET_ROTATE_THRESHOLD {
            self.consecutive_resets.store(0, Ordering::Relaxed);
            let new_idx = self.current_idx.fetch_add(1, Ordering::Relaxed) + 1;
            let wrapped = new_idx % VARIANTS.len();
            metrics::counter!(
                "tv_depth_200_variant_rotations_total",
                "to" => VARIANTS[wrapped].label
            )
            .increment(1);
            return true;
        }
        false
    }

    /// Whether the rotator has locked in a winning variant.
    /// Primarily used by tests; also useful for observability.
    // TEST-EXEMPT: exercised by test_rotator_starts_at_variant_a, test_rotator_success_locks_in_current_variant
    pub fn is_locked_in(&self) -> bool {
        self.locked_in.load(Ordering::Relaxed)
    }
}

impl Default for VariantRotator {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the `Request` for a given variant. The URL carries the token +
/// client_id query params; the `User-Agent` (and `Origin` for Browser) are
/// set on the request headers.
///
/// `base_host` is e.g. `full-depth-api.dhan.co` (no `wss://`, no path).
// TEST-EXEMPT: exercised by test_build_request_variant_a_has_no_user_agent_override, test_build_request_python_sets_python_ua, test_build_request_browser_sets_ua_and_origin
pub fn build_request_for_variant(
    variant: DepthVariant,
    base_host: &str,
    access_token: &str,
    client_id: &str,
) -> Result<Request, WebSocketError> {
    // O(1) EXEMPT: per-reconnect URL build (cold path, 1 alloc/reconnect, not per-tick)
    let url = format!(
        "wss://{base_host}{path}?token={access_token}&clientId={client_id}&authType=2",
        path = variant.url_path
    );
    let mut request =
        url.as_str()
            .into_client_request()
            .map_err(|err| WebSocketError::ConnectionFailed {
                // O(1) EXEMPT: error path, fires at most once per reconnect attempt
                url: format!("wss://{base_host}{}", variant.url_path),
                source: err,
            })?;

    match variant.ua {
        UaFlavor::Default => {
            // Tungstenite supplies its compiled-in default; leave untouched.
        }
        UaFlavor::Python => {
            let Ok(hv) = HeaderValue::from_str(PYTHON_WEBSOCKETS_UA) else {
                return Err(WebSocketError::TlsConfigurationFailed {
                    reason: "invalid python UA string".into(),
                });
            };
            request.headers_mut().insert("User-Agent", hv);
        }
        UaFlavor::Browser => {
            let Ok(ua_hv) = HeaderValue::from_str(BROWSER_UA) else {
                return Err(WebSocketError::TlsConfigurationFailed {
                    reason: "invalid browser UA string".into(),
                });
            };
            let Ok(origin_hv) = HeaderValue::from_str(BROWSER_ORIGIN) else {
                return Err(WebSocketError::TlsConfigurationFailed {
                    reason: "invalid browser origin string".into(),
                });
            };
            request.headers_mut().insert("User-Agent", ua_hv);
            request.headers_mut().insert("Origin", origin_hv);
        }
    }

    Ok(request)
}

/// Builds the TLS connector for a given variant's ALPN mode. Re-uses the
/// native root cert store the same way `build_websocket_tls_connector`
/// does, but toggles `alpn_protocols` based on the variant.
// TEST-EXEMPT: exercised by test_build_tls_connector_http11_has_http11_alpn, test_build_tls_connector_none_has_no_alpn
pub fn build_tls_connector_for_variant(variant: DepthVariant) -> Result<Connector, WebSocketError> {
    let mut root_store = rustls::RootCertStore::empty();
    let native_certs = rustls_native_certs::load_native_certs();
    let (added, _ignored) = root_store.add_parsable_certificates(native_certs.certs);
    if added == 0 {
        return Err(WebSocketError::TlsConfigurationFailed {
            reason: "no native root CA certificates found".into(),
        });
    }

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    match variant.alpn {
        AlpnMode::Http11 => {
            // O(1) EXEMPT: ALPN list built once per TLS connector (cold path)
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
        }
        AlpnMode::None => {
            // Leave empty — TLS ClientHello advertises no preference.
        }
    }
    Ok(Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn test_variants_catalog_has_exactly_eight() {
        assert_eq!(VARIANTS.len(), 8, "catalog must have exactly 8 variants");
    }

    #[test]
    fn test_variants_catalog_variant_a_is_first() {
        // Variant A (root + default UA + http/1.1) MUST be the first try
        // so production starts with the previous-known config and only
        // rotates if that genuinely fails.
        assert_eq!(VARIANTS[0].label, "A_root_default_alpn11");
        assert_eq!(VARIANTS[0].url_path, "/");
        assert_eq!(VARIANTS[0].ua, UaFlavor::Default);
        assert_eq!(VARIANTS[0].alpn, AlpnMode::Http11);
    }

    #[test]
    fn test_variants_catalog_labels_are_unique() {
        let mut seen = std::collections::HashSet::with_capacity(VARIANTS.len());
        for v in VARIANTS.iter() {
            assert!(seen.insert(v.label), "duplicate variant label: {}", v.label);
        }
    }

    #[test]
    fn test_rotator_starts_at_variant_a() {
        let r = VariantRotator::new();
        assert_eq!(r.current().label, "A_root_default_alpn11");
        assert!(!r.is_locked_in());
    }

    #[test]
    fn test_rotator_does_not_rotate_before_threshold() {
        let r = VariantRotator::new();
        for _ in 0..(RESET_ROTATE_THRESHOLD - 1) {
            assert!(!r.record_reset());
        }
        assert_eq!(r.current().label, "A_root_default_alpn11");
    }

    #[test]
    fn test_rotator_rotates_on_threshold_reached() {
        let r = VariantRotator::new();
        for _ in 0..(RESET_ROTATE_THRESHOLD - 1) {
            assert!(!r.record_reset());
        }
        assert!(r.record_reset(), "N-th reset must trigger rotation");
        assert_ne!(r.current().label, "A_root_default_alpn11");
    }

    #[test]
    fn test_rotator_wraps_after_all_variants_exhausted() {
        let r = VariantRotator::new();
        // Advance fully through the catalog.
        for _ in 0..(VARIANTS.len() * RESET_ROTATE_THRESHOLD as usize) {
            r.record_reset();
        }
        // Should wrap back to A after 8 full rotations.
        assert_eq!(r.current().label, "A_root_default_alpn11");
    }

    #[test]
    fn test_rotator_success_locks_in_current_variant() {
        let r = VariantRotator::new();
        // Bump to a different variant first.
        for _ in 0..RESET_ROTATE_THRESHOLD {
            r.record_reset();
        }
        let locked_label = r.current().label;
        r.record_success();
        // Further resets must NOT rotate away from the locked-in variant.
        for _ in 0..(RESET_ROTATE_THRESHOLD * 3) {
            r.record_reset();
        }
        assert_eq!(r.current().label, locked_label);
        assert!(r.is_locked_in());
    }

    #[test]
    fn test_rotator_is_clone_cheap_shares_state() {
        let r1 = VariantRotator::new();
        let r2 = r1.clone();
        // Advance via r1.
        for _ in 0..RESET_ROTATE_THRESHOLD {
            r1.record_reset();
        }
        // r2 observes the same state — both are Arc handles.
        assert_ne!(r2.current().label, "A_root_default_alpn11");
    }

    #[test]
    fn test_build_request_variant_a_has_no_user_agent_override() {
        let req = build_request_for_variant(
            VARIANTS[0],
            "full-depth-api.dhan.co",
            "token_xyz",
            "1106656882",
        )
        .expect("build_request_for_variant should succeed for variant A");
        assert!(
            !req.headers().contains_key("User-Agent"),
            "variant A (Default UA) must not set an explicit User-Agent header"
        );
        assert_eq!(req.uri().path(), "/");
    }

    #[test]
    fn test_build_request_python_sets_python_ua() {
        let python_variant = VARIANTS
            .iter()
            .find(|v| v.ua == UaFlavor::Python && v.alpn == AlpnMode::Http11)
            .expect("catalog must contain a Python+http/1.1 variant");
        let req = build_request_for_variant(
            *python_variant,
            "full-depth-api.dhan.co",
            "token_xyz",
            "1106656882",
        )
        .expect("build_request_for_variant should succeed for python variant");
        assert_eq!(
            req.headers()
                .get("User-Agent")
                .and_then(|v| v.to_str().ok()),
            Some(PYTHON_WEBSOCKETS_UA)
        );
    }

    #[test]
    fn test_build_request_browser_sets_ua_and_origin() {
        let browser_variant = VARIANTS
            .iter()
            .find(|v| v.ua == UaFlavor::Browser)
            .expect("catalog must contain a Browser variant");
        let req = build_request_for_variant(
            *browser_variant,
            "full-depth-api.dhan.co",
            "token_xyz",
            "1106656882",
        )
        .expect("build_request_for_variant should succeed for browser variant");
        assert!(req.headers().get("User-Agent").is_some());
        assert_eq!(
            req.headers().get("Origin").and_then(|v| v.to_str().ok()),
            Some(BROWSER_ORIGIN)
        );
    }

    #[test]
    fn test_build_tls_connector_http11_has_http11_alpn() {
        install_crypto_provider();
        let variant = VARIANTS
            .iter()
            .find(|v| v.alpn == AlpnMode::Http11)
            .expect("catalog must contain http11 variant");
        let connector = build_tls_connector_for_variant(*variant).expect("connector must build");
        if let Connector::Rustls(config) = connector {
            assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        } else {
            panic!("expected rustls connector");
        }
    }

    #[test]
    fn test_build_tls_connector_none_has_no_alpn() {
        install_crypto_provider();
        let variant = VARIANTS
            .iter()
            .find(|v| v.alpn == AlpnMode::None)
            .expect("catalog must contain no-alpn variant");
        let connector = build_tls_connector_for_variant(*variant).expect("connector must build");
        if let Connector::Rustls(config) = connector {
            assert!(
                config.alpn_protocols.is_empty(),
                "AlpnMode::None must result in empty alpn_protocols"
            );
        } else {
            panic!("expected rustls connector");
        }
    }

    #[test]
    fn test_reset_rotate_threshold_is_three() {
        // Ratchet — 2026-04-24 chose 3 so 8 variants × 3 resets = 24
        // fits well inside the 60-attempt overall reconnect cap AND the
        // operator sees convergence within ~3 minutes of boot. If someone
        // raises this without also raising DEPTH_RECONNECT_MAX_ATTEMPTS,
        // we will exhaust the cap before trying all variants, defeating
        // the rotation.
        assert_eq!(RESET_ROTATE_THRESHOLD, 3);
    }
}
