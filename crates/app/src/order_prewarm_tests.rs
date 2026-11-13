//! Tests for the order host pre-warm (no-rest rule §12.16). Local servers
//! only: no test reaches a Dhan host.

use super::*;

use tokio::net::TcpListener;

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn empty_roots_tls() -> OrderTls {
    install_crypto_provider();
    let base = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    order_tls_from(&base)
}

/// A local server that records every byte one client sends: it reads the
/// first TLS record (the ClientHello) in full, closes, and returns the bytes
/// read until the client stops sending.
async fn recording_server() -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind"); // APPROVED: test
    let port = listener.local_addr().expect("addr").port(); // APPROVED: test
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept"); // APPROVED: test
        let mut got = Vec::new();
        let mut buf = [0u8; 4_096];
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            // Close once the first record is complete.
            if got.len() >= 5 {
                let len = usize::from(u16::from_be_bytes([got[3], got[4]]));
                if got.len() >= 5 + len {
                    break;
                }
            }
            match tokio::time::timeout_at(deadline, sock.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        // Keep reading briefly after closing our side, so anything the client
        // sends after the failed handshake is recorded too.
        let _ = sock.shutdown().await;
        let tail_deadline = Instant::now() + Duration::from_millis(300);
        while let Ok(Ok(n)) = tokio::time::timeout_at(tail_deadline, sock.read(&mut buf)).await {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    (port, handle)
}

/// The TLS record content types in `bytes` (a stream of whole records).
fn record_types(bytes: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 5 {
        types.push(rest[0]);
        let len = usize::from(u16::from_be_bytes([rest[3], rest[4]]));
        rest = &rest[(5 + len).min(rest.len())..];
    }
    types
}

/// The tracked `config/base.toml`.
fn base_config() -> tickvault_common::config::ApplicationConfig {
    use figment::Figment;
    use figment::providers::{Format, Toml};
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    Figment::new()
        .merge(Toml::file(path))
        .extract()
        .expect("config/base.toml must parse") // APPROVED: test
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The order client built over the pre-warm's TLS config asks the SHARED
/// store for a session at its handshake and offers reqwest's default ALPN.
/// Built with the plain builder, the store sees nothing (fails without the
/// wiring).
#[tokio::test]
async fn test_build_oms_http_client_with_tls_shares_the_session_store_lookups() {
    let tls = empty_roots_tls();
    let (port, server) = recording_server().await;
    let client = crate::oms_wiring::build_oms_http_client_with_tls((*tls.config).clone())
        .expect("client builds"); // APPROVED: test
    // The handshake fails at the local server (it closes after the
    // ClientHello), so no HTTP byte is ever sent.
    let result = client
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await;
    assert!(
        result.is_err(),
        "the local server never completes a handshake"
    );
    let hello = server.await.expect("server task"); // APPROVED: test
    assert_eq!(
        hello.first(),
        Some(&0x16),
        "the first record is a handshake"
    );
    assert!(
        tls.store.lookups() >= 1,
        "the order client must use the pre-warm's session store"
    );
    assert!(contains(&hello, b"\x02h2"), "ALPN must offer h2");
    assert!(
        contains(&hello, b"\x08http/1.1"),
        "ALPN must offer http/1.1"
    );
}

/// A handshake that FAILS (the server closes after the ClientHello): the
/// pre-warm sends handshake and alert records only, no HTTP text, and uses
/// the shared store. It covers the failure path only; the code after a
/// completed handshake is covered by
/// `test_prewarm_once_full_handshake_sends_no_application_data`.
#[tokio::test]
async fn test_prewarm_once_drive_tls_handshake_sends_only_tls_records() {
    let tls = empty_roots_tls();
    let (port, server) = recording_server().await;
    let outcome = prewarm_once("127.0.0.1", port, &tls).await;
    let bytes = server.await.expect("server task"); // APPROVED: test
    assert_eq!(outcome.error, Some("tls_failed"));
    assert!(outcome.tcp_connect_us.is_some());
    assert_eq!(outcome.tls_handshake_us, None);
    let types = record_types(&bytes);
    assert!(!types.is_empty(), "the ClientHello was recorded");
    assert!(
        types.iter().all(|t| *t == 0x16 || *t == 0x15),
        "only handshake (0x16) and alert (0x15) records may be sent, got {types:?}"
    );
    for text in [&b"GET "[..], b"POST", b"HTTP/1.1\r\n", b"PRI * HTTP/2"] {
        assert!(!contains(&bytes, text), "no HTTP text may be sent");
    }
    assert!(
        tls.store.lookups() >= 1,
        "the pre-warm uses the shared store"
    );
}

/// A closed port is a logged outcome, never a panic.
#[tokio::test]
async fn test_prewarm_and_log_refused_port_is_logged_not_panicking() {
    let tls = empty_roots_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind"); // APPROVED: test
    let port = listener.local_addr().expect("addr").port(); // APPROVED: test
    drop(listener);
    let outcome = prewarm_and_log("127.0.0.1", port, &tls).await;
    assert_eq!(outcome.error, Some("connect_failed"));
    assert_eq!(outcome.tickets_stored, 0);
}

#[tokio::test]
async fn test_prewarm_once_unresolvable_host_is_dns_failed() {
    let tls = empty_roots_tls();
    let outcome = prewarm_once("no-such-host.invalid", 443, &tls).await;
    assert!(
        matches!(
            outcome.error,
            Some("dns_failed" | "dns_no_address" | "dns_timeout")
        ),
        "got {:?}",
        outcome.error
    );
}

#[test]
fn test_order_tls_from_sets_reqwest_alpn_no_early_data_tickets_stored_zero() {
    install_crypto_provider();
    let mut base = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    base.alpn_protocols = vec![b"http/1.1".to_vec()];
    base.enable_early_data = true;
    let tls = order_tls_from(&base);
    assert_eq!(
        tls.config.alpn_protocols,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert!(
        !tls.config.enable_early_data,
        "no 0-RTT early data (§12.16)"
    );
    assert_eq!(tls.store.tickets_stored(), 0);
    assert_eq!(tls.store.lookups(), 0);
}

#[test]
fn test_secs_until_order_prewarm_before_inside_after_and_holiday() {
    assert_eq!(ORDER_PREWARM_AT_SECS_OF_DAY_IST, 33_240);
    assert_eq!(ORDER_PREWARM_LAST_SECS_OF_DAY_IST, 55_800);
    // Before 09:14: wait until 09:14.
    assert_eq!(secs_until_order_prewarm(0, true), Some(33_240));
    assert_eq!(
        secs_until_order_prewarm(8 * 3_600 + 30 * 60, true),
        Some(2_640)
    );
    assert_eq!(secs_until_order_prewarm(33_239, true), Some(1));
    // From 09:14 until 15:30: at once.
    assert_eq!(secs_until_order_prewarm(33_240, true), Some(0));
    assert_eq!(secs_until_order_prewarm(12 * 3_600, true), Some(0));
    assert_eq!(secs_until_order_prewarm(55_799, true), Some(0));
    // At or after 15:30: not today.
    assert_eq!(secs_until_order_prewarm(55_800, true), None);
    assert_eq!(secs_until_order_prewarm(86_399, true), None);
    // A weekend or holiday: never.
    assert_eq!(secs_until_order_prewarm(0, false), None);
    assert_eq!(secs_until_order_prewarm(33_240, false), None);
}

#[test]
fn test_secs_until_next_ist_day_lands_just_past_midnight() {
    assert_eq!(secs_until_next_ist_day(0), 86_405);
    assert_eq!(secs_until_next_ist_day(55_800), 30_605);
    assert_eq!(secs_until_next_ist_day(86_399), 6);
    // A reading past the day's end never underflows.
    assert_eq!(secs_until_next_ist_day(90_000), 6);
}

/// The production entry builds the order client and spawns the daily
/// pre-warm once per process: a second call (an order runtime respawn) does
/// not spawn another. The host is local, so a run during market hours only
/// meets a closed port.
#[tokio::test]
async fn test_prewarmed_oms_http_client_spawn_daily_order_prewarm_once_process_order_tls() {
    install_crypto_provider();
    let calendar = Arc::new(
        TradingCalendar::from_config(&base_config().trading).expect("calendar builds"), // APPROVED: test
    );
    let first = prewarmed_oms_http_client("https://127.0.0.1/v2", Arc::clone(&calendar));
    assert!(first.is_ok());
    let second = prewarmed_oms_http_client("https://127.0.0.1/v2", Arc::clone(&calendar));
    assert!(second.is_ok());
    if process_order_tls().is_ok() {
        assert!(PREWARM_SPAWNED.load(Ordering::Acquire));
        assert!(
            !spawn_daily_order_prewarm("127.0.0.1".to_string(), empty_roots_tls(), calendar),
            "the daily task is spawned at most once per process"
        );
    }
}

/// Test-only certificate check that accepts any server: the local server's
/// certificate is a placeholder and its key is generated at run time, so
/// nothing secret is committed. Never used outside this test module.
#[derive(Debug)]
struct AcceptAnyServer;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

/// Serves one fixed key for every client hello.
#[derive(Debug)]
struct OneKey(Arc<rustls::sign::CertifiedKey>);

impl rustls::server::ResolvesServerCert for OneKey {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

/// A server config with an Ed25519 key made from fresh random bytes at run
/// time (PKCS#8 v1 wrapper around a 32-byte seed) and a placeholder
/// certificate the test client never checks.
fn runtime_key_server_config() -> Arc<rustls::ServerConfig> {
    install_crypto_provider();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let mut pkcs8 = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut seed = [0u8; 32];
    provider.secure_random.fill(&mut seed).expect("random"); // APPROVED: test
    pkcs8.extend_from_slice(&seed);
    let key = provider
        .key_provider
        .load_private_key(rustls::pki_types::PrivateKeyDer::Pkcs8(pkcs8.into()))
        .expect("runtime key loads"); // APPROVED: test
    let certified = rustls::sign::CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(vec![0x30, 0x00])],
        key,
    );
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(OneKey(Arc::new(certified))));
    Arc::new(config)
}

/// What a TLS server saw from one client.
#[derive(Debug, Default)]
struct ServerSaw {
    raw: Vec<u8>,
    handshake_done: bool,
    plaintext_bytes: usize,
    close_notify: bool,
}

/// A local TLS server that completes the handshake (sending session
/// tickets), then reads until the client closes, counting every byte of
/// application data the client sent INSIDE the encryption.
async fn completing_tls_server() -> (u16, tokio::task::JoinHandle<ServerSaw>) {
    use std::io::Read;
    let config = runtime_key_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind"); // APPROVED: test
    let port = listener.local_addr().expect("addr").port(); // APPROVED: test
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept"); // APPROVED: test
        let mut conn = rustls::ServerConnection::new(config).expect("server conn"); // APPROVED: test
        let mut saw = ServerSaw::default();
        let mut buf = vec![0u8; 16_384];
        let mut plain = [0u8; 4_096];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let n = match tokio::time::timeout_at(deadline, sock.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            saw.raw.extend_from_slice(&buf[..n]);
            let mut slice = &buf[..n];
            while !slice.is_empty() {
                if conn.read_tls(&mut slice).is_err() {
                    return saw;
                }
                if conn.process_new_packets().is_err() {
                    return saw;
                }
            }
            if !conn.is_handshaking() {
                saw.handshake_done = true;
            }
            loop {
                match conn.reader().read(&mut plain) {
                    Ok(0) => {
                        saw.close_notify = true;
                        break;
                    }
                    Ok(k) => saw.plaintext_bytes += k,
                    Err(_) => break,
                }
            }
            while conn.wants_write() {
                let mut out = Vec::new();
                if conn.write_tls(&mut out).is_err() || sock.write_all(&out).await.is_err() {
                    break;
                }
            }
        }
        saw
    });
    (port, handle)
}

/// The pre-warm against a server that COMPLETES the handshake: the code
/// after the handshake (ticket wait, `close_notify`, close) runs, the
/// session ticket is stored, and the server receives ZERO bytes of
/// application data inside the encryption, then a clean `close_notify`.
/// With TLS 1.3 the client's Finished and its `close_notify` travel in
/// records whose outer type is 23 (application data); that is the record
/// layer, not data, so the check is on the decrypted stream. Fails if the
/// pre-warm ever writes through the plaintext writer (an HTTP request or an
/// HTTP/2 preface after the handshake), or stops closing politely.
#[tokio::test]
async fn test_prewarm_once_full_handshake_sends_no_application_data() {
    install_crypto_provider();
    let base = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_no_client_auth();
    let tls = order_tls_from(&base);
    let (port, server) = completing_tls_server().await;
    let outcome = prewarm_once("127.0.0.1", port, &tls).await;
    let saw = server.await.expect("server task"); // APPROVED: test
    assert_eq!(outcome.error, None, "the handshake completes: {outcome:?}");
    assert!(outcome.tls_handshake_us.is_some());
    assert!(saw.handshake_done, "the server finished the handshake");
    assert!(
        outcome.tickets_stored >= 1,
        "the ticket wait ran and stored the server's ticket: {outcome:?}"
    );
    assert_eq!(
        saw.plaintext_bytes, 0,
        "no application data may be sent inside the encryption"
    );
    assert!(saw.close_notify, "the pre-warm closes with close_notify");
    let types = record_types(&saw.raw);
    assert!(
        types.iter().all(|t| (0x14..=0x17).contains(t)),
        "only TLS record types: {types:?}"
    );
    for text in [&b"GET "[..], b"POST", b"HTTP/1.1\r\n", b"PRI * HTTP/2"] {
        assert!(!contains(&saw.raw, text), "no HTTP text may be sent");
    }
}

/// §12.16: the switch picks exactly one builder; off (the default) never
/// touches the pre-warm builder. Fails if the condition is inverted or both
/// are called.
#[test]
fn test_order_runtime_http_client_choose_order_client_calls_only_the_chosen_builder() {
    let calls = std::cell::Cell::new((0u32, 0u32));
    let off = choose_order_client(
        false,
        || {
            calls.set((calls.get().0 + 1, calls.get().1));
            "prewarmed"
        },
        || {
            calls.set((calls.get().0, calls.get().1 + 1));
            "plain"
        },
    );
    assert_eq!((off, calls.get()), ("plain", (0, 1)));
    let on = choose_order_client(true, || "prewarmed", || "plain");
    assert_eq!(on, "prewarmed");
    // The production entry with the switch off builds the plain client (the
    // no-spawn half is proven in its own binary, `order_prewarm_switch_guard`,
    // because the spawned-once flag is process-wide).
    let calendar = Arc::new(
        TradingCalendar::from_config(&base_config().trading).expect("calendar builds"), // APPROVED: test
    );
    assert!(order_runtime_http_client(false, "https://127.0.0.1/v2", calendar).is_ok());
}
