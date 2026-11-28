//! Integration coverage for three `tickvault-app` modules, aimed at paths
//! the in-crate unit tests do not reach:
//!
//! * `network_rtt` — a SUCCESSFUL TCP + TLS handshake against a real rustls
//!   server bound on 127.0.0.1 port 0 (self-signed certificate embedded
//!   below, valid to 2126), an untrusted certificate, an unresolvable name,
//!   the sample-index saturation, the dhan-off spawn arm, and odd URLs.
//!   (Closed port, missing TLS config and a silent server are already
//!   covered in-crate.)
//! * `order_observability` — exact pacer boundaries, the RateLimitExhausted
//!   fold, reason redaction and the saturating reconcile arithmetic.
//! * `orphan_position_watchdog_boot` — the IST date helper's extremes, the
//!   spawn once-guard and the supervised loop's non-trading-day arm, driven
//!   on a paused clock over a calendar with no trading day around today.
//!
//! No AWS, no DNS, no real broker host; network use is loopback only.

use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use proptest::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tickvault_app::network_rtt::{
    HandshakeTiming, NETWORK_RTT_LATE_BOOT_DELAY_SECS, NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST,
    build_network_rtt_row, full_handshake_config, handshake_hosts, host_from_url,
    measure_handshake, secs_until_network_rtt_run, spawn_network_rtt_session,
};
use tickvault_app::order_observability::{
    AlertPacer, ORDER_ALERT_PAGE_COOLDOWN_SECS, PaceAction, PacedAlertKind, ReconcileVerdict,
    classify_order_side_reconcile, fold_suppressed, map_oms_alert, plain_risk_reason,
};
use tickvault_app::orphan_position_watchdog_boot::{
    WatchdogAuth, ist_date_from_utc, spawn_supervised_orphan_position_watchdog,
};
use tickvault_common::config::{ApplicationConfig, DhanConfig, NseHolidayEntry};
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_trading::oms::engine::OmsAlert;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Self-signed P-256 certificate, CN and IP SAN 127.0.0.1, CA:FALSE,
/// serverAuth, valid 2026-11-28 to 2126-11-04 (DER).
const TEST_CERT_DER: &[u8] = &[
    0x30, 0x82, 0x01, 0xb5, 0x30, 0x82, 0x01, 0x5a, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x14, 0x7e,
    0x35, 0x7b, 0x7b, 0x99, 0x7b, 0x0d, 0xb2, 0x9a, 0x28, 0x78, 0x4b, 0x38, 0x6d, 0xc2, 0x5e, 0x82,
    0xa3, 0x6c, 0xa6, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02, 0x30,
    0x14, 0x31, 0x12, 0x30, 0x10, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x09, 0x31, 0x32, 0x37, 0x2e,
    0x30, 0x2e, 0x30, 0x2e, 0x31, 0x30, 0x20, 0x17, 0x0d, 0x32, 0x36, 0x31, 0x31, 0x32, 0x38, 0x30,
    0x36, 0x30, 0x32, 0x34, 0x32, 0x5a, 0x18, 0x0f, 0x32, 0x31, 0x32, 0x36, 0x31, 0x31, 0x30, 0x34,
    0x30, 0x36, 0x30, 0x32, 0x34, 0x32, 0x5a, 0x30, 0x14, 0x31, 0x12, 0x30, 0x10, 0x06, 0x03, 0x55,
    0x04, 0x03, 0x0c, 0x09, 0x31, 0x32, 0x37, 0x2e, 0x30, 0x2e, 0x30, 0x2e, 0x31, 0x30, 0x59, 0x30,
    0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
    0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04, 0x93, 0x9d, 0x6f, 0x3b, 0x8c, 0xd2, 0xd6, 0x3c,
    0x35, 0x9a, 0x6b, 0x3d, 0x51, 0xb5, 0xd7, 0x80, 0x03, 0xa5, 0x78, 0x45, 0xce, 0xc7, 0xba, 0x69,
    0x82, 0xc5, 0x7b, 0xa7, 0x87, 0x9a, 0xa9, 0x31, 0xa2, 0xb6, 0x5b, 0x5c, 0x57, 0x65, 0x54, 0xc9,
    0xa5, 0x02, 0xc8, 0xf6, 0x1f, 0x2b, 0x07, 0xe7, 0xcb, 0xf9, 0xc7, 0x5c, 0x0c, 0x8d, 0x39, 0x9b,
    0x2a, 0x14, 0x52, 0xed, 0x88, 0xd2, 0xde, 0x65, 0xa3, 0x81, 0x87, 0x30, 0x81, 0x84, 0x30, 0x1d,
    0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04, 0x14, 0x9e, 0x0c, 0x54, 0x24, 0xf2, 0xcc, 0xbd,
    0x41, 0xe9, 0x94, 0xc1, 0xe2, 0xac, 0xcb, 0xae, 0x82, 0x3e, 0x91, 0x0a, 0x55, 0x30, 0x1f, 0x06,
    0x03, 0x55, 0x1d, 0x23, 0x04, 0x18, 0x30, 0x16, 0x80, 0x14, 0x9e, 0x0c, 0x54, 0x24, 0xf2, 0xcc,
    0xbd, 0x41, 0xe9, 0x94, 0xc1, 0xe2, 0xac, 0xcb, 0xae, 0x82, 0x3e, 0x91, 0x0a, 0x55, 0x30, 0x0f,
    0x06, 0x03, 0x55, 0x1d, 0x11, 0x04, 0x08, 0x30, 0x06, 0x87, 0x04, 0x7f, 0x00, 0x00, 0x01, 0x30,
    0x0c, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x02, 0x30, 0x00, 0x30, 0x0e, 0x06,
    0x03, 0x55, 0x1d, 0x0f, 0x01, 0x01, 0xff, 0x04, 0x04, 0x03, 0x02, 0x07, 0x80, 0x30, 0x13, 0x06,
    0x03, 0x55, 0x1d, 0x25, 0x04, 0x0c, 0x30, 0x0a, 0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07,
    0x03, 0x01, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02, 0x03, 0x49,
    0x00, 0x30, 0x46, 0x02, 0x21, 0x00, 0xa1, 0x6e, 0x8a, 0x6c, 0xe1, 0x44, 0xae, 0xa2, 0x7a, 0x07,
    0x44, 0xc3, 0x81, 0x2b, 0x31, 0x14, 0xfa, 0xd9, 0xee, 0x31, 0xa9, 0xd3, 0xa7, 0x4e, 0xfc, 0x08,
    0x9e, 0xb9, 0x77, 0x62, 0x7a, 0x65, 0x02, 0x21, 0x00, 0xc7, 0x94, 0x56, 0x21, 0x53, 0x6d, 0xe5,
    0x04, 0x01, 0x1e, 0xc5, 0xfd, 0x9d, 0xcd, 0x37, 0x7d, 0x76, 0xb0, 0xc4, 0x8f, 0xdd, 0xf2, 0x67,
    0x80, 0x6d, 0xa3, 0xad, 0x65, 0xd7, 0x30, 0xf7, 0x11,
];

/// The PKCS#8 private key of [`TEST_CERT_DER`] (DER). Test-only, loopback only.
const TEST_KEY_DER: &[u8] = &[
    0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02,
    0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30, 0x6b, 0x02,
    0x01, 0x01, 0x04, 0x20, 0xfe, 0x37, 0xcc, 0xbc, 0x5a, 0x01, 0xef, 0x97, 0xd2, 0xb3, 0x7a, 0x96,
    0x42, 0xba, 0xab, 0xa0, 0x35, 0x4a, 0x6c, 0x42, 0x08, 0x88, 0xa0, 0x6c, 0x5b, 0xd5, 0x73, 0xaf,
    0x39, 0xd0, 0x99, 0xf4, 0xa1, 0x44, 0x03, 0x42, 0x00, 0x04, 0x93, 0x9d, 0x6f, 0x3b, 0x8c, 0xd2,
    0xd6, 0x3c, 0x35, 0x9a, 0x6b, 0x3d, 0x51, 0xb5, 0xd7, 0x80, 0x03, 0xa5, 0x78, 0x45, 0xce, 0xc7,
    0xba, 0x69, 0x82, 0xc5, 0x7b, 0xa7, 0x87, 0x9a, 0xa9, 0x31, 0xa2, 0xb6, 0x5b, 0x5c, 0x57, 0x65,
    0x54, 0xc9, 0xa5, 0x02, 0xc8, 0xf6, 0x1f, 0x2b, 0x07, 0xe7, 0xcb, 0xf9, 0xc7, 0x5c, 0x0c, 0x8d,
    0x39, 0x9b, 0x2a, 0x14, 0x52, 0xed, 0x88, 0xd2, 0xde, 0x65,
];

fn install_crypto_provider() {
    // Idempotent: a second install returns Err and is ignored on purpose.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// One global Prometheus recorder for this test binary, so counters the
/// production code bumps can be read back.
fn metrics_handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("the only recorder installed in this test binary")
    })
}

fn load_base_config() -> ApplicationConfig {
    use figment::Figment;
    use figment::providers::{Format, Toml};
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    Figment::new()
        .merge(Toml::file(path))
        .extract()
        .expect("config/base.toml parses")
}

fn dhan_config(feed: &str, order: &str, rest: &str) -> DhanConfig {
    DhanConfig {
        websocket_url: feed.to_string(),
        order_update_websocket_url: order.to_string(),
        rest_api_base_url: rest.to_string(),
        sandbox_base_url: String::new(),
        auth_base_url: String::new(),
        instrument_csv_url: String::new(),
        instrument_csv_fallback_url: String::new(),
        max_instruments_per_connection: 0,
        max_websocket_connections: 0,
    }
}

fn server_config() -> Arc<rustls::ServerConfig> {
    install_crypto_provider();
    let cert = rustls::pki_types::CertificateDer::from(TEST_CERT_DER.to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        TEST_KEY_DER.to_vec(),
    ));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("the embedded certificate and key match");
    // No post-handshake tickets: the server then writes nothing after the
    // client's Finished, so the client's close cannot race a server write.
    config.send_tls13_tickets = 0;
    Arc::new(config)
}

fn trusting_client_config() -> rustls::ClientConfig {
    install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            TEST_CERT_DER.to_vec(),
        ))
        .expect("the embedded certificate is a valid trust anchor");
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

fn untrusting_client_config() -> Arc<rustls::ClientConfig> {
    install_crypto_provider();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
    )
}

/// What the loopback TLS server saw from one client.
#[derive(Debug, Default)]
struct ServerSaw {
    handshake_done: bool,
    plaintext: Vec<u8>,
    peer_sent_close_notify: bool,
    tls_error: bool,
}

/// Accepts ONE connection and serves a rustls handshake over it, reading
/// until the client closes. Returns what it saw.
async fn serve_one_tls_client(
    listener: tokio::net::TcpListener,
    config: Arc<rustls::ServerConfig>,
) -> ServerSaw {
    let (mut sock, _) = listener.accept().await.expect("accept");
    let mut conn = rustls::ServerConnection::new(config).expect("server connection");
    let mut saw = ServerSaw::default();
    let mut buf = vec![0u8; 16_384];
    let mut out = Vec::with_capacity(4_096);
    'outer: loop {
        while conn.wants_write() {
            out.clear();
            if conn.write_tls(&mut out).is_err() || sock.write_all(&out).await.is_err() {
                break 'outer;
            }
        }
        if !conn.is_handshaking() {
            saw.handshake_done = true;
        }
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut slice = &buf[..n];
        while !slice.is_empty() {
            if conn.read_tls(&mut slice).is_err() {
                saw.tls_error = true;
                break 'outer;
            }
            match conn.process_new_packets() {
                Ok(state) => {
                    let pending = state.plaintext_bytes_to_read();
                    if pending > 0 {
                        let mut plain = vec![0u8; pending];
                        if conn.reader().read_exact(&mut plain).is_ok() {
                            saw.plaintext.extend_from_slice(&plain);
                        }
                    }
                    if state.peer_has_closed() {
                        saw.peer_sent_close_notify = true;
                    }
                }
                Err(_) => {
                    saw.tls_error = true;
                    // Flush the alert the server owes the client, then stop.
                    out.clear();
                    let _ = conn.write_tls(&mut out);
                    let _ = sock.write_all(&out).await;
                    break 'outer;
                }
            }
        }
        if !conn.is_handshaking() {
            saw.handshake_done = true;
        }
    }
    saw
}

async fn bind_loopback() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

// ---------------------------------------------------------------------------
// network_rtt — pure helpers
// ---------------------------------------------------------------------------

#[test]
fn test_host_from_url_table_of_odd_inputs() {
    let cases: &[(&str, Option<&str>)] = &[
        ("wss://api-feed.dhan.co", Some("api-feed.dhan.co")),
        ("https://api.dhan.co/v2/orders", Some("api.dhan.co")),
        ("https://api.dhan.co?x=1", Some("api.dhan.co")),
        ("https://api.dhan.co#frag", Some("api.dhan.co")),
        ("https://api.dhan.co:443/v2", Some("api.dhan.co")),
        ("wss://  spaced.example  /x", Some("spaced.example")),
        ("no-scheme.example/path", Some("no-scheme.example")),
        ("bare-host", Some("bare-host")),
        // Only the FIRST "://" splits; the rest stays in the path.
        ("a://b://c", Some("b")),
        ("https://", None),
        ("https:///path", None),
        ("https://:443", None),
        ("https://   /x", None),
        ("", None),
        ("/only/a/path", None),
        ("?q", None),
    ];
    for (url, want) in cases {
        assert_eq!(host_from_url(url), *want, "url {url:?}");
    }
}

#[test]
fn test_handshake_hosts_keeps_config_order_and_drops_duplicates_and_blanks() {
    let all_distinct = dhan_config(
        "wss://api-feed.dhan.co",
        "wss://api-order-update.dhan.co",
        "https://api.dhan.co/v2",
    );
    assert_eq!(
        handshake_hosts(&all_distinct),
        vec![
            "api-feed.dhan.co".to_string(),
            "api-order-update.dhan.co".to_string(),
            "api.dhan.co".to_string()
        ]
    );
    // Same host in all three, different ports and paths: one entry.
    let one = dhan_config(
        "wss://h.example:1",
        "wss://h.example/x",
        "https://h.example?y",
    );
    assert_eq!(handshake_hosts(&one), vec!["h.example".to_string()]);
    // Blank URLs are skipped, the order of the rest is kept.
    let blanks = dhan_config("", "https://", "https://rest.example/v2");
    assert_eq!(handshake_hosts(&blanks), vec!["rest.example".to_string()]);
    let none = dhan_config("", "", "");
    assert!(handshake_hosts(&none).is_empty());
    // Duplicate first and third, distinct middle: two, in first-seen order.
    let ab = dhan_config("wss://a.example", "wss://b.example", "https://a.example");
    assert_eq!(
        handshake_hosts(&ab),
        vec!["a.example".to_string(), "b.example".to_string()]
    );
}

#[test]
fn test_secs_until_network_rtt_run_boundaries() {
    let run = NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST;
    assert_eq!(run, 33_900, "09:25 IST");
    assert_eq!(secs_until_network_rtt_run(0), u64::from(run));
    assert_eq!(secs_until_network_rtt_run(run - 1), 1);
    assert_eq!(
        secs_until_network_rtt_run(run),
        NETWORK_RTT_LATE_BOOT_DELAY_SECS
    );
    assert_eq!(
        secs_until_network_rtt_run(run + 1),
        NETWORK_RTT_LATE_BOOT_DELAY_SECS
    );
    assert_eq!(secs_until_network_rtt_run(86_399), 60);
    // Out-of-range input (past one day) still counts as "late": never
    // underflows, never waits a day.
    assert_eq!(secs_until_network_rtt_run(u32::MAX), 60);
}

#[test]
fn test_build_network_rtt_row_copies_every_field_and_saturates_the_index() {
    let failed = HandshakeTiming {
        tcp_connect_us: None,
        tls_handshake_us: None,
        total_us: 2_000_123,
        error: Some("connect_timeout"),
    };
    let row = build_network_rtt_row(-5, "h.example", usize::MAX, failed);
    assert_eq!(row.attempt_at_ist_nanos, -5);
    assert_eq!(row.feed, Feed::Dhan);
    assert_eq!(row.host, "h.example");
    assert_eq!(row.sample_idx, i64::MAX, "usize::MAX saturates to i64::MAX");
    assert_eq!(row.tcp_connect_us, None);
    assert_eq!(row.tls_handshake_us, None);
    assert_eq!(row.total_us, 2_000_123);
    assert!(!row.ok);
    assert_eq!(row.error, Some("connect_timeout"));

    let ok = HandshakeTiming {
        tcp_connect_us: Some(0),
        tls_handshake_us: Some(i64::MAX),
        total_us: i64::MAX,
        error: None,
    };
    let row = build_network_rtt_row(i64::MAX, "", 0, ok);
    assert!(row.ok);
    assert_eq!(row.error, None);
    assert_eq!(row.sample_idx, 0);
    assert_eq!(row.host, "");
    assert_eq!(row.tcp_connect_us, Some(0));
    assert_eq!(row.tls_handshake_us, Some(i64::MAX));
}

#[test]
fn test_spawn_network_rtt_session_is_none_when_the_dhan_lane_is_off() {
    let mut config = load_base_config();
    config.feeds.dhan_enabled = false;
    assert!(spawn_network_rtt_session(&config).is_none());
}

proptest! {
    #[test]
    fn test_host_from_url_never_returns_a_delimiter_or_blank(url in ".{0,64}") {
        if let Some(host) = host_from_url(&url) {
            prop_assert!(!host.is_empty());
            prop_assert_eq!(host, host.trim());
            prop_assert!(!host.contains(['/', '?', '#', ':']), "{:?}", host);
            prop_assert!(url.contains(host));
        }
    }

    #[test]
    fn test_secs_until_network_rtt_run_lands_at_or_after_0925(boot in 0u32..86_400) {
        let wait = secs_until_network_rtt_run(boot);
        let at = u64::from(boot) + wait;
        if boot < NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST {
            prop_assert_eq!(at, u64::from(NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST));
        } else {
            prop_assert_eq!(wait, NETWORK_RTT_LATE_BOOT_DELAY_SECS);
        }
    }
}

// ---------------------------------------------------------------------------
// network_rtt — measure_handshake against loopback servers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_measure_handshake_full_tls_success_sends_no_application_data() {
    let (listener, port) = bind_loopback().await;
    let server = tokio::spawn(serve_one_tls_client(listener, server_config()));
    let tls = full_handshake_config(&trusting_client_config());
    let timing = measure_handshake("127.0.0.1", port, Some(tls), Duration::from_secs(10)).await;
    assert_eq!(timing.error, None, "{timing:?}");
    let tcp = timing.tcp_connect_us.expect("tcp time recorded");
    let tls_us = timing.tls_handshake_us.expect("tls time recorded");
    assert!(tcp >= 0 && tls_us >= 0, "{timing:?}");
    assert!(timing.total_us >= tls_us, "{timing:?}");
    assert!(timing.total_us >= tcp, "{timing:?}");
    assert!(timing.total_us < 10_000_000, "{timing:?}");

    let saw = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert!(saw.handshake_done, "{saw:?}");
    assert!(!saw.tls_error, "{saw:?}");
    assert!(
        saw.plaintext.is_empty(),
        "no HTTP byte may be sent: {:?}",
        saw.plaintext
    );
    assert!(saw.peer_sent_close_notify, "the polite close arrived");

    // The same timing becomes an ok row.
    let row = build_network_rtt_row(1, "127.0.0.1", 3, timing);
    assert!(row.ok);
    assert_eq!(row.tls_handshake_us, Some(tls_us));
}

#[tokio::test]
async fn test_measure_handshake_untrusted_certificate_is_tls_failed() {
    let (listener, port) = bind_loopback().await;
    let server = tokio::spawn(serve_one_tls_client(listener, server_config()));
    let timing = measure_handshake(
        "127.0.0.1",
        port,
        Some(untrusting_client_config()),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(timing.error, Some("tls_failed"), "{timing:?}");
    assert!(timing.tcp_connect_us.is_some(), "TCP finished first");
    assert_eq!(timing.tls_handshake_us, None);
    let saw = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert!(saw.plaintext.is_empty());
    assert!(
        !saw.peer_sent_close_notify,
        "a failed attempt sends no close"
    );
}

#[tokio::test]
async fn test_measure_handshake_name_with_a_nul_byte_is_dns_failed() {
    let timing = measure_handshake(
        "bad\0host",
        443,
        Some(untrusting_client_config()),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(timing.error, Some("dns_failed"), "{timing:?}");
    assert_eq!(timing.tcp_connect_us, None);
    assert_eq!(timing.tls_handshake_us, None);
    assert!(timing.total_us >= 0);
}

// ---------------------------------------------------------------------------
// order_observability — pure mapping, pacing, folding, reconcile
// ---------------------------------------------------------------------------

#[test]
fn test_map_oms_alert_rejected_reason_is_bounded_redacted_and_control_free() {
    let jwt = "eyJhbGciOiJIUzUxMiJ9.eyJzdWIiOiIxMTA2NjU2ODgyIn0.c2lnbmF0dXJlc2lnbmF0dXJl";
    let raw = format!(
        "400 Bad Request\n\t{{\"token\":\"{jwt}\"}} {}",
        "z".repeat(4_000)
    );
    let Some(NotificationEvent::OrderRejected { reason, .. }) =
        map_oms_alert(&OmsAlert::OrderRejected {
            correlation_id: String::new(),
            reason: raw,
        })
    else {
        panic!("OrderRejected must map to OrderRejected");
    };
    assert!(reason.chars().count() <= 300, "{}", reason.chars().count());
    assert!(!reason.contains(jwt), "the token never reaches a page");
    assert!(!reason.chars().any(char::is_control), "{reason:?}");
    assert!(reason.starts_with("400 Bad Request"), "{reason:?}");

    // An empty reason stays empty (no fabricated text).
    let Some(NotificationEvent::OrderRejected { reason, .. }) =
        map_oms_alert(&OmsAlert::OrderRejected {
            correlation_id: "c".to_string(),
            reason: String::new(),
        })
    else {
        panic!("OrderRejected must map to OrderRejected");
    };
    assert_eq!(reason, "");
}

#[test]
fn test_plain_risk_reason_table() {
    let cases = [
        ("MaxDailyLossExceeded", "daily loss limit hit"),
        ("PositionSizeLimitExceeded", "position size limit hit"),
        ("ManualHalt", "manual stop by operator"),
        // Exact match only: case, spacing and unknown slugs fall back.
        ("maxdailylossexceeded", "risk limit hit"),
        (" ManualHalt", "risk limit hit"),
        ("PositionCapacityExhausted", "risk limit hit"),
        ("", "risk limit hit"),
    ];
    for (slug, want) in cases {
        assert_eq!(plain_risk_reason(slug), want, "slug {slug:?}");
    }
}

#[test]
fn test_alert_pacer_cooldown_boundaries_and_independent_kinds() {
    let cooldown = ORDER_ALERT_PAGE_COOLDOWN_SECS;
    assert_eq!(cooldown, 300);
    let mut pacer = AlertPacer::default();
    let k = PacedAlertKind::OrderRejected;
    assert_eq!(pacer.record(k, 1_000), PaceAction::Send { suppressed: 0 });
    assert_eq!(pacer.record(k, 1_000), PaceAction::Suppress);
    assert_eq!(pacer.record(k, 1_000 + cooldown - 1), PaceAction::Suppress);
    // Exactly at the cooldown: pages, folding the two swallowed alerts.
    assert_eq!(
        pacer.record(k, 1_000 + cooldown),
        PaceAction::Send { suppressed: 2 }
    );
    // The fold count resets after a page.
    assert_eq!(
        pacer.record(k, 1_000 + 2 * cooldown),
        PaceAction::Send { suppressed: 0 }
    );
    // A backward clock step reads as "inside the cooldown" (saturating).
    assert_eq!(pacer.record(k, 0), PaceAction::Suppress);

    // Each kind keeps its own clock.
    assert_eq!(
        pacer.record(PacedAlertKind::RateLimitExhausted, 1_000),
        PaceAction::Send { suppressed: 0 }
    );
    assert_eq!(
        pacer.record(PacedAlertKind::CircuitOpen, 1_000),
        PaceAction::Send { suppressed: 0 }
    );
    assert_eq!(
        pacer.record(PacedAlertKind::CircuitOpen, 1_001),
        PaceAction::Suppress
    );
    assert_eq!(
        pacer.record(PacedAlertKind::RateLimitExhausted, 1_001),
        PaceAction::Suppress
    );

    // The very first alert at time 0 and at u64::MAX both page.
    let mut fresh = AlertPacer::default();
    assert_eq!(
        fresh.record(PacedAlertKind::CircuitOpen, 0),
        PaceAction::Send { suppressed: 0 }
    );
    let mut fresh = AlertPacer::default();
    assert_eq!(
        fresh.record(PacedAlertKind::CircuitOpen, u64::MAX),
        PaceAction::Send { suppressed: 0 }
    );
    assert_eq!(
        fresh.record(PacedAlertKind::CircuitOpen, u64::MAX),
        PaceAction::Suppress
    );
}

#[test]
fn test_fold_suppressed_texts_and_passthroughs() {
    let rejected = NotificationEvent::OrderRejected {
        correlation_id: "c7".to_string(),
        reason: "DH-905".to_string(),
    };
    match fold_suppressed(rejected.clone(), 0) {
        NotificationEvent::OrderRejected { reason, .. } => assert_eq!(reason, "DH-905"),
        other => panic!("{other:?}"),
    }
    match fold_suppressed(rejected, 4) {
        NotificationEvent::OrderRejected {
            correlation_id,
            reason,
        } => {
            assert_eq!(correlation_id, "c7");
            assert_eq!(reason, "DH-905 …and 4 more since the last page");
        }
        other => panic!("{other:?}"),
    }
    match fold_suppressed(
        NotificationEvent::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        },
        u64::MAX,
    ) {
        NotificationEvent::RateLimitExhausted { limit_type } => assert_eq!(
            limit_type,
            format!("per_second …and {} more since the last page", u64::MAX)
        ),
        other => panic!("{other:?}"),
    }
    // No free-text field: passes through unchanged.
    assert!(matches!(
        fold_suppressed(
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 3
            },
            9
        ),
        NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 3
        }
    ));
    assert!(matches!(
        fold_suppressed(NotificationEvent::CircuitBreakerClosed, 1),
        NotificationEvent::CircuitBreakerClosed
    ));
}

#[test]
fn test_classify_order_side_reconcile_table() {
    use ReconcileVerdict::{Mismatch, Reconciled, Unverified};
    let cases: &[(u64, u64, u64, bool, Option<u64>, ReconcileVerdict)] = &[
        (0, 0, 0, true, Some(0), Reconciled),
        (5, 5, 0, true, Some(5), Reconciled),
        // The DB count is informational: a restart makes it larger.
        (5, 5, 0, true, Some(500), Reconciled),
        (5, 5, 0, true, Some(0), Reconciled),
        (5, 5, 0, true, None, Unverified),
        (0, 0, 0, true, None, Unverified),
        // A leak, a drop or a failed heartbeat is always a mismatch.
        (6, 5, 0, true, Some(5), Mismatch),
        (5, 6, 0, true, Some(6), Mismatch),
        (5, 4, 1, true, Some(4), Mismatch),
        (5, 5, 0, false, Some(5), Mismatch),
        (5, 5, 0, false, None, Mismatch),
        (0, 0, 1, true, None, Mismatch),
        // Saturating sum: never wraps into a false balance.
        (u64::MAX, u64::MAX, 1, true, Some(1), Mismatch),
        (u64::MAX, u64::MAX, 0, true, Some(1), Reconciled),
    ];
    for &(received, appended, dropped, eod, db, want) in cases {
        assert_eq!(
            classify_order_side_reconcile(received, appended, dropped, eod, db),
            want,
            "received={received} appended={appended} dropped={dropped} eod={eod} db={db:?}"
        );
    }
}

proptest! {
    #[test]
    fn test_pacer_pages_once_per_cooldown_and_counts_every_alert(
        gaps in proptest::collection::vec(0u64..700, 1..60),
    ) {
        let mut pacer = AlertPacer::default();
        let mut now = 0u64;
        let mut last_sent: Option<u64> = None;
        let mut swallowed_since_send = 0u64;
        let mut sends = 0u64;
        let mut folded_total = 0u64;
        for gap in &gaps {
            now += gap;
            let action = pacer.record(PacedAlertKind::OrderRejected, now);
            let expect_send = last_sent.is_none_or(|l| now - l >= ORDER_ALERT_PAGE_COOLDOWN_SECS);
            match action {
                PaceAction::Send { suppressed } => {
                    prop_assert!(expect_send);
                    prop_assert_eq!(suppressed, swallowed_since_send);
                    folded_total += suppressed;
                    swallowed_since_send = 0;
                    last_sent = Some(now);
                    sends += 1;
                }
                PaceAction::Suppress => {
                    prop_assert!(!expect_send);
                    swallowed_since_send += 1;
                }
            }
        }
        // Nothing is lost: every alert is a page, folded into one, or still pending.
        prop_assert_eq!(sends + folded_total + swallowed_since_send, gaps.len() as u64);
    }
}

// ---------------------------------------------------------------------------
// orphan_position_watchdog_boot
// ---------------------------------------------------------------------------

#[test]
fn test_ist_date_from_utc_boundaries() {
    let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day).expect("valid date");
    let cases = [
        (0i64, d(1970, 1, 1)),
        // 18:29:59 UTC is 23:59:59 IST, same date.
        (18 * 3_600 + 29 * 60 + 59, d(1970, 1, 1)),
        // 18:30:00 UTC is midnight IST, next date.
        (18 * 3_600 + 30 * 60, d(1970, 1, 2)),
        // Just before the epoch: 23:59:59 UTC on 31 Dec is 05:29:59 IST on 1 Jan.
        (-1, d(1970, 1, 1)),
        // 2026-11-27 18:29:59 UTC → 23:59:59 IST same day.
        (1_795_804_199, d(2026, 11, 27)),
        (1_795_804_200, d(2026, 11, 28)),
        // Out-of-range instants fall back to the epoch, never panic.
        (i64::MAX, d(1970, 1, 1)),
        (i64::MIN, d(1970, 1, 1)),
    ];
    for (utc, want) in cases {
        assert_eq!(ist_date_from_utc(utc), want, "utc {utc}");
    }
}

/// A calendar on which no day within 60 days either side of today (IST)
/// trades: every weekday in that span is a holiday and weekends never trade.
/// The span is relative to the wall clock on purpose: the watchdog loop
/// reads the wall clock to pick "today", so a fixed date range would make
/// this test start failing once the real date leaves it.
fn calendar_with_no_trading_day() -> Arc<TradingCalendar> {
    use chrono::Datelike;
    let mut trading = load_base_config().trading;
    trading.nse_holidays.clear();
    trading.muhurat_trading_dates.clear();
    trading.nse_mock_trading_dates.clear();
    let today = ist_date_from_utc(chrono::Utc::now().timestamp());
    let mut day = today - chrono::Days::new(60);
    let end = today + chrono::Days::new(60);
    while day <= end {
        if !matches!(day.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
            trading.nse_holidays.push(NseHolidayEntry {
                date: day.format("%Y-%m-%d").to_string(),
                name: "closed".to_string(),
            });
        }
        day = day.succ_opt().expect("next day");
    }
    let calendar = TradingCalendar::from_config(&trading).expect("calendar builds");
    for offset in 0..=3 {
        let d = today + chrono::Days::new(offset);
        assert!(!calendar.is_trading_day(d), "{d} must not trade");
    }
    Arc::new(calendar)
}

#[tokio::test(start_paused = true)]
async fn test_orphan_watchdog_spawns_once_and_never_checks_on_a_closed_calendar() {
    let handle = metrics_handle();
    let calendar = calendar_with_no_trading_day();
    let notifier = NotificationService::disabled();
    let empty_token: TokenHandle = Arc::new(arc_swap::ArcSwap::from_pointee(None));
    let auth = WatchdogAuth::Static {
        token_handle: empty_token,
        client_id: "1106656882".to_string(),
    };
    // A port nothing listens on: the trading arm (which would call it)
    // must never run on this calendar.
    let first = spawn_supervised_orphan_position_watchdog(
        auth.clone(),
        Arc::clone(&notifier),
        Arc::clone(&calendar),
        "http://127.0.0.1:1".to_string(),
        true,
    );
    // The once-guard: a second spawn is a task that ends at once.
    let second = spawn_supervised_orphan_position_watchdog(
        auth,
        notifier,
        calendar,
        "http://127.0.0.1:1".to_string(),
        false,
    );
    second.await.expect("the duplicate spawn is an empty task");

    // Three virtual days: the loop wakes at each 15:25 IST, finds no
    // trading day, floor-sleeps, and keeps going.
    tokio::time::sleep(Duration::from_secs(3 * 86_400)).await;
    assert!(!first.is_finished(), "the supervised watchdog stays alive");

    let rendered = handle.render();
    assert!(
        !rendered.contains("tv_orphan_position_watchdog_runs_total"),
        "no check runs on a non-trading day: {rendered}"
    );
    assert!(
        !rendered.contains("tv_orphan_position_watchdog_fetch_failures_total"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("tv_orphan_position_watchdog_respawns_total"),
        "the inner loop never died: {rendered}"
    );
    first.abort();
    let joined = first.await;
    assert!(joined.is_err_and(|e| e.is_cancelled()));
}
