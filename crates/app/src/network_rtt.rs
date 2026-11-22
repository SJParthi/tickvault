//! Once-per-session TCP + TLS handshake timing to the Dhan hosts (plan item
//! LF-6a, Quote 57, owner 2026-11-12: "Handshake only (Recommended)" — TCP +
//! TLS connection timing only, no request sent, no REST-rule change).
//!
//! ## What it does
//!
//! At 09:25 IST (or [`NETWORK_RTT_LATE_BOOT_DELAY_SECS`] after a later boot,
//! so never inside the market's first five minutes), one task makes
//! [`NETWORK_RTT_SAMPLES_PER_HOST`] attempts to each Dhan host named in
//! `[dhan]` config (the market feed socket, the order-update socket and the
//! REST API), [`NETWORK_RTT_SPACING_SECS`] apart: resolve the name, open a
//! TCP connection to port 443, finish a TLS handshake, send `close_notify`
//! and close. **No HTTP byte is sent**, so nothing reaches a Dhan API.
//! Each attempt is one row in `network_rtt_session`; the session's rows are
//! written in one flush at the end.
//!
//! The TLS config is the live sockets' own (same roots, same `http/1.1`
//! ALPN), CLONED with session resumption turned off: every sample is a full
//! handshake, and the live sockets' resumption store is never touched.
//!
//! ## Never slows the feed
//!
//! Its own tokio task; every step is async and bounded by a
//! [`NETWORK_RTT_ATTEMPT_TIMEOUT_SECS`] deadline per attempt; the QuestDB
//! flush runs on the blocking pool. A failed attempt is a row (`ok = false`)
//! plus a coded `warn!` (NET-RTT-01); a failed write, or a table that could
//! not be ensured after one retry ([`NETWORK_RTT_ENSURE_RETRY_DELAY_SECS`]),
//! is a coded `error!` (log only, no page). Rows are written only after the
//! table is ensured, so an ILP write never creates it.
//!
//! **Not free, and not measured:** the handshake's CPU (key exchange and
//! certificate verification, about 15 handshakes once per session from
//! 09:25 IST, about 1 s apart) runs on a MAIN-RUNTIME worker, the runtime
//! that also hosts the frame drain. It is bounded and small (a few
//! milliseconds per handshake, Assumed) but unmeasured. To see whether it
//! delays the feed, watch `tv_hot_path_stage_max_ns{stage="ring_dwell"}` and
//! the `ring_dwell` p99 in `hot_path_latency_minute` around 09:25 IST.
//!
//! ## Honest limits
//!
//! * Once per PROCESS: a restart in the day runs it again (more rows, new
//!   `ts`, no duplicate key).
//! * The times include the runtime's scheduling delay (see "Not free, and
//!   not measured" above for the CPU side).
//! * The first resolved address is used; a host with several addresses is
//!   measured on one of them.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use tickvault_common::config::{ApplicationConfig, DhanConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_storage::network_rtt_session_persistence::{
    NetworkRttRow, NetworkRttSessionWriter, ensure_network_rtt_session_table,
};

/// Attempts per host in one session.
pub const NETWORK_RTT_SAMPLES_PER_HOST: usize = 5;

/// Budget of one attempt (resolve + connect + handshake), seconds.
pub const NETWORK_RTT_ATTEMPT_TIMEOUT_SECS: u64 = 2;

/// Pause between two attempts, seconds.
pub const NETWORK_RTT_SPACING_SECS: u64 = 1;

/// The run time, IST seconds of day: 09:25, ten minutes after the open.
pub const NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 25 * 60;

/// Delay after a boot at or after 09:25 IST, so the feed has connected first.
pub const NETWORK_RTT_LATE_BOOT_DELAY_SECS: u64 = 60;

/// Pause before the one retry of a failed table ensure, seconds.
pub const NETWORK_RTT_ENSURE_RETRY_DELAY_SECS: u64 = 5;

/// The TLS port of every Dhan host.
pub const NETWORK_RTT_PORT: u16 = 443;

/// Bytes read per socket read during the handshake.
const HANDSHAKE_READ_BUF_BYTES: usize = 16_384;

/// The host part of a `wss://` / `https://` URL: after the scheme, before
/// the first `/`, `?`, `#` or `:`. `None` when empty. Pure.
#[must_use]
pub fn host_from_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let end = rest.find(['/', '?', '#', ':']).unwrap_or(rest.len());
    let host = rest[..end].trim();
    (!host.is_empty()).then_some(host)
}

/// The distinct hosts to measure, in config order: market feed, order
/// update, REST API. Pure.
#[must_use]
pub fn handshake_hosts(dhan: &DhanConfig) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::with_capacity(3);
    for url in [
        dhan.websocket_url.as_str(),
        dhan.order_update_websocket_url.as_str(),
        dhan.rest_api_base_url.as_str(),
    ] {
        if let Some(h) = host_from_url(url)
            && !hosts.iter().any(|x| x == h)
        {
            hosts.push(h.to_string());
        }
    }
    hosts
}

/// Seconds to wait from `ist_secs_of_day` until the run: until 09:25 IST, or
/// [`NETWORK_RTT_LATE_BOOT_DELAY_SECS`] when already at or past it. Pure.
#[must_use]
pub fn secs_until_network_rtt_run(ist_secs_of_day: u32) -> u64 {
    if ist_secs_of_day < NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST {
        u64::from(NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST - ist_secs_of_day)
    } else {
        NETWORK_RTT_LATE_BOOT_DELAY_SECS
    }
}

/// The live sockets' TLS config, cloned with session resumption off so every
/// handshake is a full one. Pure apart from the clone.
#[must_use]
pub fn full_handshake_config(shared: &rustls::ClientConfig) -> Arc<rustls::ClientConfig> {
    let mut config = shared.clone();
    config.resumption = rustls::client::Resumption::disabled();
    Arc::new(config)
}

/// One attempt's measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeTiming {
    /// TCP connect, µs; `None` when it did not finish.
    pub tcp_connect_us: Option<i64>,
    /// TLS handshake, µs; `None` when it did not finish.
    pub tls_handshake_us: Option<i64>,
    /// Whole attempt, µs.
    pub total_us: i64,
    /// The failed step; `None` when the handshake finished.
    pub error: Option<&'static str>,
}

fn micros_since(t: Instant) -> i64 {
    i64::try_from(t.elapsed().as_micros()).unwrap_or(i64::MAX)
}

/// Drives a rustls client handshake over `stream` until it finishes (or the
/// socket fails). Sends nothing but TLS handshake records. Also used by the
/// order host pre-warm (`order_prewarm`, no-rest rule §12.16).
pub(crate) async fn drive_tls_handshake(
    stream: &mut TcpStream,
    conn: &mut rustls::ClientConnection,
) -> Result<(), ()> {
    let mut out: Vec<u8> = Vec::with_capacity(4_096);
    let mut buf = vec![0u8; HANDSHAKE_READ_BUF_BYTES];
    loop {
        while conn.wants_write() {
            out.clear();
            conn.write_tls(&mut out).map_err(|_| ())?;
            stream.write_all(&out).await.map_err(|_| ())?;
        }
        if !conn.is_handshaking() {
            return Ok(());
        }
        let n = stream.read(&mut buf).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        let mut slice = &buf[..n];
        while !slice.is_empty() {
            conn.read_tls(&mut slice).map_err(|_| ())?;
            conn.process_new_packets().map_err(|_| ())?;
        }
    }
}

/// One attempt to `host:port`: resolve, TCP connect, TLS handshake (with
/// `tls`, or the step fails as `tls_config_failed` when `None`), then a
/// `close_notify` and close. Every step shares one `timeout` deadline. No
/// HTTP byte is sent.
pub async fn measure_handshake(
    host: &str,
    port: u16,
    tls: Option<Arc<rustls::ClientConfig>>,
    timeout: Duration,
) -> HandshakeTiming {
    let start = Instant::now();
    let deadline = start + timeout;
    let fail = |tcp: Option<i64>, step: &'static str| HandshakeTiming {
        tcp_connect_us: tcp,
        tls_handshake_us: None,
        total_us: micros_since(start),
        error: Some(step),
    };
    let addr = match tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port))).await
    {
        Err(_) => return fail(None, "dns_timeout"),
        Ok(Err(_)) => return fail(None, "dns_failed"),
        Ok(Ok(mut addrs)) => match addrs.next() {
            Some(a) => a,
            None => return fail(None, "dns_no_address"),
        },
    };
    let connect_start = Instant::now();
    let mut stream = match tokio::time::timeout_at(deadline, TcpStream::connect(addr)).await {
        Err(_) => return fail(None, "connect_timeout"),
        Ok(Err(_)) => return fail(None, "connect_failed"),
        Ok(Ok(s)) => s,
    };
    let tcp_us = micros_since(connect_start);
    if let Err(err) = stream.set_nodelay(true) {
        debug!(host, %err, "Dhan host handshake timing: TCP_NODELAY not set");
    }
    let Some(config) = tls else {
        return fail(Some(tcp_us), "tls_config_failed");
    };
    let Ok(server_name) = rustls::pki_types::ServerName::try_from(host.to_string()) else {
        return fail(Some(tcp_us), "tls_failed");
    };
    let Ok(mut conn) = rustls::ClientConnection::new(config, server_name) else {
        return fail(Some(tcp_us), "tls_failed");
    };
    let tls_start = Instant::now();
    match tokio::time::timeout_at(deadline, drive_tls_handshake(&mut stream, &mut conn)).await {
        Err(_) => return fail(Some(tcp_us), "tls_timeout"),
        Ok(Err(())) => return fail(Some(tcp_us), "tls_failed"),
        Ok(Ok(())) => {}
    }
    let tls_us = micros_since(tls_start);
    let total_us = micros_since(start);
    // Close politely: one close_notify alert, best effort, bounded by the
    // same deadline. Nothing else is written.
    conn.send_close_notify();
    let mut out = Vec::with_capacity(64);
    let close_written = conn.write_tls(&mut out).is_ok()
        && matches!(
            tokio::time::timeout_at(deadline, stream.write_all(&out)).await,
            Ok(Ok(()))
        );
    let shut = matches!(
        tokio::time::timeout_at(deadline, stream.shutdown()).await,
        Ok(Ok(()))
    );
    if !(close_written && shut) {
        debug!(
            host,
            "Dhan host handshake timing: the polite close did not finish; the socket is dropped"
        );
    }
    HandshakeTiming {
        tcp_connect_us: Some(tcp_us),
        tls_handshake_us: Some(tls_us),
        total_us,
        error: None,
    }
}

/// The table row for one attempt. Pure.
#[must_use]
pub fn build_network_rtt_row(
    attempt_at_ist_nanos: i64,
    host: &str,
    sample_idx: usize,
    timing: HandshakeTiming,
) -> NetworkRttRow {
    NetworkRttRow {
        attempt_at_ist_nanos,
        feed: Feed::Dhan,
        host: host.to_string(),
        sample_idx: i64::try_from(sample_idx).unwrap_or(i64::MAX),
        tcp_connect_us: timing.tcp_connect_us,
        tls_handshake_us: timing.tls_handshake_us,
        total_us: timing.total_us,
        ok: timing.error.is_none(),
        error: timing.error,
    }
}

fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(
            i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS) * 1_000_000_000,
        )
}

/// Runs the session's attempts and returns one row per attempt, rounds of
/// one attempt per host, [`NETWORK_RTT_SPACING_SECS`] between attempts.
async fn measure_session(
    hosts: &[String],
    tls: Option<Arc<rustls::ClientConfig>>,
) -> Vec<NetworkRttRow> {
    let mut rows = Vec::with_capacity(hosts.len() * NETWORK_RTT_SAMPLES_PER_HOST);
    for sample_idx in 0..NETWORK_RTT_SAMPLES_PER_HOST {
        for host in hosts {
            let at = now_ist_nanos();
            let timing = measure_handshake(
                host,
                NETWORK_RTT_PORT,
                tls.clone(),
                Duration::from_secs(NETWORK_RTT_ATTEMPT_TIMEOUT_SECS),
            )
            .await;
            if let Some(step) = timing.error {
                warn!(
                    code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                    host = host.as_str(),
                    sample_idx,
                    error = step,
                    total_us = timing.total_us,
                    "Dhan host handshake timing: attempt did not finish (kept as a row with ok = false)"
                );
            }
            rows.push(build_network_rtt_row(at, host, sample_idx, timing));
            tokio::time::sleep(Duration::from_secs(NETWORK_RTT_SPACING_SECS)).await;
        }
    }
    rows
}

/// Writes the session's rows in one flush, on the blocking pool. Returns the
/// rows written (0 on a failure, which is logged with a code).
async fn write_rows(questdb: QuestDbConfig, rows: Vec<NetworkRttRow>) -> usize {
    let count = rows.len();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut writer = NetworkRttSessionWriter::new(&questdb);
        for row in &rows {
            writer.append_row(row)?;
        }
        writer.flush()
    })
    .await;
    match result {
        Ok(Ok(())) => count,
        Ok(Err(err)) => {
            error!(
                code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                stage = "write",
                rows = count,
                error = %err,
                "network_rtt_session: the session's handshake rows could not be written — \
                 they are lost (a measurement; the next session writes new ones)"
            );
            0
        }
        Err(join) => {
            error!(
                code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                stage = "write",
                rows = count,
                error = %join,
                "network_rtt_session: the write task failed — the session's handshake rows are lost"
            );
            0
        }
    }
}

/// Ensures `network_rtt_session` (one retry after `retry_delay` when the
/// first ensure fails), then writes `rows` ONLY when the table was ensured.
/// An ILP write must never be what creates this table: QuestDB would make it
/// itself, with the wrong designated timestamp and no DEDUP key. When both
/// ensures fail the rows are not written and one coded NET-RTT-01 `error!`
/// (`stage = "ensure_failed"`) names how many. Returns `(ensured, written)`.
async fn ensure_then_write<E, EF, W, WF>(
    mut ensure: E,
    write: W,
    rows: Vec<NetworkRttRow>,
    retry_delay: Duration,
) -> (bool, usize)
where
    E: FnMut() -> EF,
    EF: std::future::Future<Output = bool>,
    W: FnOnce(Vec<NetworkRttRow>) -> WF,
    WF: std::future::Future<Output = usize>,
{
    let mut ensured = ensure().await;
    if !ensured {
        tokio::time::sleep(retry_delay).await;
        ensured = ensure().await;
    }
    if !ensured {
        error!(
            code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
            stage = "ensure_failed",
            rows = rows.len(),
            "network_rtt_session: the table could not be created or checked after one \
             retry — the session's handshake rows are not written (a measurement; the \
             next session tries again)"
        );
        return (false, 0);
    }
    (true, write(rows).await)
}

/// Spawns the once-per-session handshake timing task when the Dhan lane is
/// on (`[feeds] dhan_enabled`); `None` otherwise. Its own task; returns at
/// once.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested parts (host_from_url, handshake_hosts, secs_until_network_rtt_run, full_handshake_config, measure_handshake, build_network_rtt_row); call site pinned by test_main_spawns_spawn_network_rtt_session.
pub fn spawn_network_rtt_session(
    config: &ApplicationConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.feeds.dhan_enabled {
        return None;
    }
    let hosts = handshake_hosts(&config.dhan);
    let questdb = config.questdb.clone();
    Some(tokio::spawn(async move {
        let wait =
            secs_until_network_rtt_run(tickvault_common::market_hours::now_ist_secs_of_day());
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let tls = match tickvault_core::websocket::tls::shared_http11_client_config() {
            Ok(shared) => Some(full_handshake_config(&shared)),
            Err(err) => {
                warn!(
                    code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                    error = %err,
                    "Dhan host handshake timing: no TLS config; attempts record tls_config_failed"
                );
                None
            }
        };
        let rows = measure_session(&hosts, tls).await;
        let ok = rows.iter().filter(|r| r.ok).count();
        let attempts = rows.len();
        let ensure_cfg = questdb.clone();
        let (ensured, written) = ensure_then_write(
            || ensure_network_rtt_session_table(&ensure_cfg),
            move |rows| write_rows(questdb, rows),
            rows,
            Duration::from_secs(NETWORK_RTT_ENSURE_RETRY_DELAY_SECS),
        )
        .await;
        info!(
            attempts,
            ok,
            written,
            ensured,
            "Dhan host handshake timing: session measured (network_rtt_session)"
        );
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    fn empty_roots_config() -> Arc<rustls::ClientConfig> {
        install_crypto_provider();
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        )
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

    #[test]
    fn test_host_from_url_takes_the_name_between_scheme_and_path() {
        assert_eq!(
            host_from_url("wss://api-feed.dhan.co"),
            Some("api-feed.dhan.co")
        );
        assert_eq!(
            host_from_url("wss://api-order-update.dhan.co"),
            Some("api-order-update.dhan.co")
        );
        assert_eq!(host_from_url("https://api.dhan.co/v2"), Some("api.dhan.co"));
        assert_eq!(
            host_from_url("wss://h.example:8443/x?y=1"),
            Some("h.example")
        );
        assert_eq!(host_from_url("api.dhan.co"), Some("api.dhan.co"));
        assert_eq!(host_from_url("wss://"), None);
        assert_eq!(host_from_url(""), None);
    }

    #[test]
    fn test_handshake_hosts_are_the_three_dhan_hosts_deduplicated() {
        let d = dhan_config(
            "wss://api-feed.dhan.co",
            "wss://api-order-update.dhan.co",
            "https://api.dhan.co/v2",
        );
        assert_eq!(
            handshake_hosts(&d),
            [
                "api-feed.dhan.co",
                "api-order-update.dhan.co",
                "api.dhan.co"
            ]
        );
        let d = dhan_config("wss://a.example", "wss://a.example/x", "");
        assert_eq!(handshake_hosts(&d), ["a.example"]);
    }

    #[test]
    fn test_base_toml_hosts_are_dhan_hosts() {
        let toml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml"),
        )
        .expect("config/base.toml");
        for host in [
            "api-feed.dhan.co",
            "api-order-update.dhan.co",
            "api.dhan.co",
        ] {
            assert!(toml.contains(&format!("://{host}")), "{host} in base.toml");
        }
    }

    #[test]
    fn test_secs_until_network_rtt_run_waits_for_09_25_then_runs_after_a_minute() {
        assert_eq!(secs_until_network_rtt_run(8 * 3_600 + 30 * 60), 55 * 60);
        assert_eq!(secs_until_network_rtt_run(9 * 3_600 + 15 * 60), 10 * 60);
        assert_eq!(secs_until_network_rtt_run(9 * 3_600 + 24 * 60 + 59), 1);
        assert_eq!(secs_until_network_rtt_run(9 * 3_600 + 25 * 60), 60);
        assert_eq!(secs_until_network_rtt_run(14 * 3_600), 60);
        // A run never lands inside 09:15-09:20 IST: from any boot second it
        // starts at or after 09:25, or a minute after a boot past 09:25.
        for s in (0..86_400u32).step_by(7) {
            let at = u64::from(s) + secs_until_network_rtt_run(s);
            let at_sod = at % 86_400;
            assert!(
                !(33_300..33_600).contains(&at_sod) || at >= 86_400,
                "boot {s} runs at {at_sod}"
            );
        }
    }

    #[test]
    fn test_full_handshake_config_turns_resumption_off_and_keeps_alpn() {
        install_crypto_provider();
        let mut shared = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        shared.alpn_protocols = vec![b"http/1.1".to_vec()];
        let full = full_handshake_config(&shared);
        assert_eq!(full.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let disabled = format!("{:?}", rustls::client::Resumption::disabled());
        assert_eq!(format!("{:?}", full.resumption), disabled);
        assert_ne!(
            format!("{:?}", shared.resumption),
            disabled,
            "the shared config is untouched"
        );
    }

    #[test]
    fn test_build_network_rtt_row_marks_ok_from_the_error() {
        let ok = HandshakeTiming {
            tcp_connect_us: Some(900),
            tls_handshake_us: Some(4_000),
            total_us: 5_200,
            error: None,
        };
        let row = build_network_rtt_row(1_000, "api.dhan.co", 2, ok);
        assert!(row.ok);
        assert_eq!(row.error, None);
        assert_eq!(row.sample_idx, 2);
        assert_eq!(row.host, "api.dhan.co");
        assert_eq!(row.feed, Feed::Dhan);
        assert_eq!(row.tls_handshake_us, Some(4_000));
        let failed = HandshakeTiming {
            tcp_connect_us: Some(900),
            tls_handshake_us: None,
            total_us: 2_000_000,
            error: Some("tls_timeout"),
        };
        let row = build_network_rtt_row(1_000, "api.dhan.co", 4, failed);
        assert!(!row.ok);
        assert_eq!(row.error, Some("tls_timeout"));
        assert_eq!(row.tls_handshake_us, None);
    }

    #[tokio::test]
    async fn test_measure_handshake_refused_port_is_connect_failed() {
        // Bind then drop: the port is closed.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let t = measure_handshake(
            "127.0.0.1",
            port,
            Some(empty_roots_config()),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(t.error, Some("connect_failed"), "{t:?}");
        assert_eq!(t.tcp_connect_us, None);
        assert_eq!(t.tls_handshake_us, None);
    }

    #[tokio::test]
    async fn test_measure_handshake_silent_server_is_tls_timeout_and_sends_only_tls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut got = vec![0u8; 4_096];
            let n = sock.read(&mut got).await.unwrap_or(0);
            // Hold the socket open, say nothing, until the client gives up.
            tokio::time::sleep(Duration::from_millis(600)).await;
            got.truncate(n);
            got
        });
        let t = measure_handshake(
            "127.0.0.1",
            port,
            Some(empty_roots_config()),
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(t.error, Some("tls_timeout"), "{t:?}");
        assert!(t.tcp_connect_us.is_some(), "the TCP connect finished");
        assert!(t.total_us >= 250_000, "{t:?}");
        let first = server.await.expect("server");
        // The first byte is a TLS handshake record (0x16), never HTTP text.
        assert_eq!(first.first(), Some(&0x16), "{first:?}");
        assert!(!first.starts_with(b"GET"));
    }

    #[tokio::test]
    async fn test_measure_handshake_server_that_closes_is_tls_failed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut got = [0u8; 512];
            let _ = sock.read(&mut got).await;
            // Not TLS: an HTTP reply, then close.
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        });
        let t = measure_handshake(
            "127.0.0.1",
            port,
            Some(empty_roots_config()),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(t.error, Some("tls_failed"), "{t:?}");
        assert!(t.tcp_connect_us.is_some());
    }

    #[tokio::test]
    async fn test_measure_handshake_without_tls_config_records_tls_config_failed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let t = measure_handshake("127.0.0.1", port, None, Duration::from_secs(2)).await;
        assert_eq!(t.error, Some("tls_config_failed"), "{t:?}");
        assert!(t.tcp_connect_us.is_some());
    }

    #[test]
    fn test_main_spawns_spawn_network_rtt_session() {
        let main = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
        )
        .expect("main.rs");
        assert!(main.contains("network_rtt::spawn_network_rtt_session(&config)"));
    }

    #[test]
    fn test_session_constants_match_the_plan() {
        // Plan item LF-6: 5 handshakes per host, 1 s apart, 2 s per attempt,
        // three hosts, so 15 rows.
        assert_eq!(NETWORK_RTT_SAMPLES_PER_HOST, 5);
        assert_eq!(NETWORK_RTT_SPACING_SECS, 1);
        assert_eq!(NETWORK_RTT_ATTEMPT_TIMEOUT_SECS, 2);
        assert_eq!(NETWORK_RTT_PORT, 443);
        assert_eq!(NETWORK_RTT_RUN_AT_SECS_OF_DAY_IST, 33_900);
        assert_eq!(NETWORK_RTT_ENSURE_RETRY_DELAY_SECS, 5);
    }

    fn sample_rows(n: usize) -> Vec<NetworkRttRow> {
        let timing = HandshakeTiming {
            tcp_connect_us: Some(1),
            tls_handshake_us: Some(2),
            total_us: 3,
            error: None,
        };
        (0..n)
            .map(|i| build_network_rtt_row(1, "api.dhan.co", i, timing))
            .collect()
    }

    /// Runs `ensure_then_write` with an ensure that answers `answers` in
    /// order; returns `(ensured, written, ensure calls, write calls)`.
    async fn run_ensure_then_write(answers: &[bool]) -> (bool, usize, usize, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ensure_calls = AtomicUsize::new(0);
        let write_calls = AtomicUsize::new(0);
        let (ensured, written) = ensure_then_write(
            || {
                let i = ensure_calls.fetch_add(1, Ordering::SeqCst);
                let answer = answers.get(i).copied().unwrap_or(false);
                async move { answer }
            },
            |rows: Vec<NetworkRttRow>| {
                write_calls.fetch_add(1, Ordering::SeqCst);
                async move { rows.len() }
            },
            sample_rows(15),
            Duration::ZERO,
        )
        .await;
        (
            ensured,
            written,
            ensure_calls.load(Ordering::SeqCst),
            write_calls.load(Ordering::SeqCst),
        )
    }

    /// Review finding (LF-6a): rows were written over ILP even when the
    /// ensure failed, so QuestDB could create the table itself with the wrong
    /// designated timestamp and no DEDUP key. Two failed ensures write nothing.
    #[tokio::test]
    async fn test_regression_failed_ensure_twice_writes_no_rows() {
        let (ensured, written, ensures, writes) = run_ensure_then_write(&[false, false]).await;
        assert!(!ensured);
        assert_eq!(written, 0);
        assert_eq!(ensures, 2, "one retry after the first failure");
        assert_eq!(writes, 0, "an ILP write must never create the table");
    }

    #[tokio::test]
    async fn test_failed_ensure_then_success_on_retry_writes_the_rows() {
        let (ensured, written, ensures, writes) = run_ensure_then_write(&[false, true]).await;
        assert!(ensured);
        assert_eq!(written, 15);
        assert_eq!((ensures, writes), (2, 1));
    }

    #[tokio::test]
    async fn test_ensured_first_time_writes_without_a_retry() {
        let (ensured, written, ensures, writes) = run_ensure_then_write(&[true]).await;
        assert!(ensured);
        assert_eq!(written, 15);
        assert_eq!((ensures, writes), (1, 1));
    }

    /// No request leaves an attempt (Quote 57: handshake only): every byte
    /// the server receives, up to the client's close, is a TLS handshake or
    /// alert record, never application data or HTTP text.
    #[tokio::test]
    async fn test_measure_handshake_sends_only_tls_handshake_records() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut all = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut all)).await;
            all
        });
        let t = measure_handshake(
            "127.0.0.1",
            port,
            Some(empty_roots_config()),
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(t.error, Some("tls_timeout"), "{t:?}");
        let bytes = server.await.expect("server");
        assert!(!bytes.is_empty(), "the ClientHello reached the server");
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            assert!(rest.len() >= 5, "a whole TLS record header: {rest:?}");
            let content_type = rest[0];
            assert!(
                content_type == 0x16 || content_type == 0x15,
                "only handshake (22) or alert (21) records, got {content_type}"
            );
            let len = usize::from(u16::from_be_bytes([rest[3], rest[4]]));
            assert!(rest.len() >= 5 + len, "a whole TLS record body");
            rest = &rest[5 + len..];
        }
    }
}
