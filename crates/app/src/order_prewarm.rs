//! Connect-only TLS pre-warm of the Dhan order REST host (owner card
//! 2026-11-13, "Yes, connect-only (Recommended)"; rule
//! `no-rest-except-live-feed-2026-06-27.md` §12.16).
//!
//! ## What it does
//!
//! With `[order_runtime] prewarm_order_connection = true` (default false),
//! once per trading day per process at 09:14 IST (or at once after a later
//! boot before 15:30 IST; a process restarted the same day runs it once
//! more, on purpose, because its session store starts empty; nothing is
//! persisted across processes) one task resolves the REST API host, opens a TCP connection to
//! port 443, finishes a TLS handshake, waits up to
//! [`ORDER_PREWARM_TICKET_WAIT_MILLIS`] for the server's session ticket,
//! sends `close_notify` and closes. **No HTTP byte and no application data
//! inside the encryption is sent**: the code never touches the TLS
//! connection's plaintext writer. With TLS 1.3 the client's encrypted
//! Finished and its `close_notify` travel in records whose OUTER type is 23
//! (application data); that is the record layer, not data. Pinned by
//! `test_prewarm_once_full_handshake_sends_no_application_data` (a local
//! server completes the handshake and counts the decrypted bytes).
//!
//! The order HTTP client is then built over the SAME base TLS config (one
//! [`OrderTls`] per process: cloned for both, so both share one session
//! store and the same certificate verifier `Arc`, which rustls requires
//! before it reuses a ticket). The first order of the day finds the ticket
//! and does a resumed handshake.
//!
//! ## Honest limits (see the rule section)
//!
//! * reqwest 0.13 cannot open a pooled connection without a request, so the
//!   first order still opens its own TCP connection and does a (resumed)
//!   TLS handshake. Expected saving: low single-digit ms (Assumed, not
//!   measured), not the 13–22 ms of a full handshake.
//! * DNS is warm only if the resolver caches; tickets expire on Dhan's
//!   schedule; a proxy in the environment routes orders through it while the
//!   pre-warm connects direct; Muhurat sessions are not covered.
//! * With the switch on, the order client verifies certificates with the
//!   live sockets' native root store instead of reqwest's platform verifier
//!   (Assumed equivalent on Linux).
//!
//! Cold path: once a day on its own tokio task, never on the frame drain.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::NamedGroup;
use rustls::client::{
    ClientSessionMemoryCache, ClientSessionStore, Tls12ClientSessionValue, Tls13ClientSessionValue,
};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::http_client::HttpClientBuildError;

use crate::network_rtt::{NETWORK_RTT_PORT, drive_tls_handshake, host_from_url};

/// The pre-warm time, IST seconds of day: 09:14.
pub const ORDER_PREWARM_AT_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 14 * 60;

/// No pre-warm at or after this IST second of day: 15:30, the market close.
pub const ORDER_PREWARM_LAST_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 30 * 60;

/// Budget of DNS + TCP connect + TLS handshake, seconds.
pub const ORDER_PREWARM_TIMEOUT_SECS: u64 = 5;

/// How long to wait after the handshake for the server's session ticket
/// (TLS 1.3 sends it after the handshake), milliseconds.
pub const ORDER_PREWARM_TICKET_WAIT_MILLIS: u64 = 1_000;

/// Sessions the shared store keeps (rustls' own default).
pub const ORDER_TLS_SESSION_CACHE_ENTRIES: usize = 256;

/// Seconds past IST midnight before the next day's schedule is computed.
const DAY_ROLL_MARGIN_SECS: u64 = 5;

const SECS_PER_DAY: u32 = 86_400;

/// Bytes read per socket read while waiting for the ticket.
const TICKET_READ_BUF_BYTES: usize = 16_384;

/// A rustls session store that counts lookups and stored sessions, so the
/// pre-warm can tell when the ticket arrived and the log can say how many
/// were stored. Delegates to rustls' in-memory cache.
#[derive(Debug)]
pub struct OrderTlsSessionStore {
    inner: ClientSessionMemoryCache,
    lookups: AtomicU64,
    stored: AtomicU64,
}

impl OrderTlsSessionStore {
    /// An empty store of [`ORDER_TLS_SESSION_CACHE_ENTRIES`] sessions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: ClientSessionMemoryCache::new(ORDER_TLS_SESSION_CACHE_ENTRIES),
            lookups: AtomicU64::new(0),
            stored: AtomicU64::new(0),
        }
    }

    /// Times a handshake asked the store for a key-group hint or a session.
    #[must_use]
    pub fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    /// Sessions (TLS 1.3 tickets or TLS 1.2 sessions) stored so far.
    #[must_use]
    pub fn tickets_stored(&self) -> u64 {
        self.stored.load(Ordering::Relaxed)
    }
}

impl Default for OrderTlsSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientSessionStore for OrderTlsSessionStore {
    fn set_kx_hint(&self, server_name: ServerName<'static>, group: NamedGroup) {
        self.inner.set_kx_hint(server_name, group);
    }

    fn kx_hint(&self, server_name: &ServerName<'_>) -> Option<NamedGroup> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        self.inner.kx_hint(server_name)
    }

    fn set_tls12_session(&self, server_name: ServerName<'static>, value: Tls12ClientSessionValue) {
        self.stored.fetch_add(1, Ordering::Relaxed);
        self.inner.set_tls12_session(server_name, value);
    }

    fn tls12_session(&self, server_name: &ServerName<'_>) -> Option<Tls12ClientSessionValue> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        self.inner.tls12_session(server_name)
    }

    fn remove_tls12_session(&self, server_name: &ServerName<'static>) {
        self.inner.remove_tls12_session(server_name);
    }

    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        value: Tls13ClientSessionValue,
    ) {
        self.stored.fetch_add(1, Ordering::Relaxed);
        self.inner.insert_tls13_ticket(server_name, value);
    }

    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<Tls13ClientSessionValue> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        self.inner.take_tls13_ticket(server_name)
    }
}

/// The base TLS config the order client and the pre-warm both clone, and the
/// session store inside it.
#[derive(Debug, Clone)]
pub struct OrderTls {
    /// Base config: `shared`'s roots and verifier, ALPN `h2`, `http/1.1`
    /// (reqwest's own default), no early data, resumption in `store`.
    pub config: Arc<rustls::ClientConfig>,
    /// The session store both share.
    pub store: Arc<OrderTlsSessionStore>,
}

/// Builds an [`OrderTls`] from `shared` (normally the live sockets' cached
/// config): a clone with ALPN `h2`, `http/1.1`, early data off and a fresh
/// counting session store. Pure apart from the clone.
#[must_use]
pub fn order_tls_from(shared: &rustls::ClientConfig) -> OrderTls {
    let store = Arc::new(OrderTlsSessionStore::new());
    let mut config = shared.clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config.enable_early_data = false;
    config.resumption = rustls::client::Resumption::store(Arc::clone(&store) as _);
    OrderTls {
        config: Arc::new(config),
        store,
    }
}

static ORDER_TLS: OnceLock<OrderTls> = OnceLock::new();

/// The process-wide [`OrderTls`], built at most once from the live sockets'
/// TLS config, so an order runtime respawn keeps the same store and
/// verifier. A failure is not cached.
///
/// # Errors
/// When no native root certificate loads.
pub fn process_order_tls() -> Result<OrderTls, tickvault_core::websocket::types::WebSocketError> {
    if let Some(t) = ORDER_TLS.get() {
        return Ok(t.clone());
    }
    let shared = tickvault_core::websocket::tls::shared_http11_client_config()?;
    let built = order_tls_from(&shared);
    Ok(ORDER_TLS.get_or_init(|| built).clone())
}

/// Seconds to wait from `ist_secs_of_day` until the pre-warm: until 09:14
/// IST, `0` from 09:14 until 15:30, `None` at or after 15:30 or on a
/// non-trading day. Pure.
#[must_use]
pub fn secs_until_order_prewarm(ist_secs_of_day: u32, is_trading_day: bool) -> Option<u64> {
    if !is_trading_day || ist_secs_of_day >= ORDER_PREWARM_LAST_SECS_OF_DAY_IST {
        return None;
    }
    Some(u64::from(
        ORDER_PREWARM_AT_SECS_OF_DAY_IST.saturating_sub(ist_secs_of_day),
    ))
}

/// Seconds from `ist_secs_of_day` until just past the next IST midnight.
/// Pure.
#[must_use]
pub fn secs_until_next_ist_day(ist_secs_of_day: u32) -> u64 {
    u64::from(SECS_PER_DAY - ist_secs_of_day.min(SECS_PER_DAY - 1)) + DAY_ROLL_MARGIN_SECS
}

/// One pre-warm's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrewarmOutcome {
    /// TCP connect, µs; `None` when it did not finish.
    pub tcp_connect_us: Option<i64>,
    /// TLS handshake, µs; `None` when it did not finish.
    pub tls_handshake_us: Option<i64>,
    /// Sessions the store gained during this pre-warm.
    pub tickets_stored: u64,
    /// The failed step; `None` when the handshake finished.
    pub error: Option<&'static str>,
}

fn micros_since(t: Instant) -> i64 {
    i64::try_from(t.elapsed().as_micros()).unwrap_or(i64::MAX)
}

/// Reads TLS records after the handshake until the store gains a session or
/// `deadline` passes. Received plaintext is read and dropped; nothing is
/// written.
async fn await_session_ticket(
    stream: &mut TcpStream,
    conn: &mut rustls::ClientConnection,
    store: &OrderTlsSessionStore,
    before: u64,
    deadline: Instant,
) {
    let mut buf = vec![0u8; TICKET_READ_BUF_BYTES];
    while store.tickets_stored() <= before {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        let mut slice = &buf[..n];
        while !slice.is_empty() {
            if conn.read_tls(&mut slice).is_err() || conn.process_new_packets().is_err() {
                return;
            }
            discard_plaintext(conn);
        }
    }
}

fn discard_plaintext(conn: &mut rustls::ClientConnection) {
    use std::io::Read;
    let mut sink = [0u8; 1_024];
    while matches!(conn.reader().read(&mut sink), Ok(n) if n > 0) {}
}

/// One pre-warm of `host:port` with `tls` (a clone of the [`OrderTls`]
/// base): resolve, TCP connect, TLS handshake, wait for a session ticket,
/// `close_notify`, close. Sends the TLS handshake and `close_notify` only,
/// never application data inside the encryption.
pub async fn prewarm_once(host: &str, port: u16, tls: &OrderTls) -> PrewarmOutcome {
    let before = tls.store.tickets_stored();
    let deadline = Instant::now() + Duration::from_secs(ORDER_PREWARM_TIMEOUT_SECS);
    let fail = |tcp: Option<i64>, step: &'static str| PrewarmOutcome {
        tcp_connect_us: tcp,
        tls_handshake_us: None,
        tickets_stored: tls.store.tickets_stored().saturating_sub(before),
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
        debug!(host, %err, "order host pre-warm: TCP_NODELAY not set");
    }
    let Ok(server_name) = ServerName::try_from(host.to_string()) else {
        return fail(Some(tcp_us), "tls_failed");
    };
    let Ok(mut conn) = rustls::ClientConnection::new(Arc::clone(&tls.config), server_name) else {
        return fail(Some(tcp_us), "tls_failed");
    };
    let tls_start = Instant::now();
    match tokio::time::timeout_at(deadline, drive_tls_handshake(&mut stream, &mut conn)).await {
        Err(_) => return fail(Some(tcp_us), "tls_timeout"),
        Ok(Err(())) => return fail(Some(tcp_us), "tls_failed"),
        Ok(Ok(())) => {}
    }
    let tls_us = micros_since(tls_start);
    let ticket_deadline = Instant::now() + Duration::from_millis(ORDER_PREWARM_TICKET_WAIT_MILLIS);
    await_session_ticket(&mut stream, &mut conn, &tls.store, before, ticket_deadline).await;
    // Close politely: close_notify (plus any pending TLS record), best effort.
    conn.send_close_notify();
    let close_deadline = Instant::now() + Duration::from_millis(ORDER_PREWARM_TICKET_WAIT_MILLIS);
    let mut out = Vec::with_capacity(64);
    let close_written = conn.write_tls(&mut out).is_ok()
        && matches!(
            tokio::time::timeout_at(close_deadline, stream.write_all(&out)).await,
            Ok(Ok(()))
        );
    let shut = matches!(
        tokio::time::timeout_at(close_deadline, stream.shutdown()).await,
        Ok(Ok(()))
    );
    if !(close_written && shut) {
        debug!(
            host,
            "order host pre-warm: the polite close did not finish; the socket is dropped"
        );
    }
    PrewarmOutcome {
        tcp_connect_us: Some(tcp_us),
        tls_handshake_us: Some(tls_us),
        tickets_stored: tls.store.tickets_stored().saturating_sub(before),
        error: None,
    }
}

/// Runs [`prewarm_once`] and logs the outcome: one `info!`, or a coded
/// NET-RTT-01 `warn!` (`source = "order_prewarm"`, log only).
pub async fn prewarm_and_log(host: &str, port: u16, tls: &OrderTls) -> PrewarmOutcome {
    let outcome = prewarm_once(host, port, tls).await;
    match outcome.error {
        None => info!(
            host,
            tcp_connect_us = outcome.tcp_connect_us,
            tls_handshake_us = outcome.tls_handshake_us,
            tickets_stored = outcome.tickets_stored,
            "order host pre-warm: TLS handshake done, no request sent (no-rest rule §12.16)"
        ),
        Some(step) => warn!(
            code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
            source = "order_prewarm",
            host,
            error = step,
            "order host pre-warm did not finish; the first order does a full handshake"
        ),
    }
    outcome
}

static PREWARM_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawns the daily pre-warm task once per process (a respawned order
/// runtime does not spawn a second one). Returns whether this call spawned
/// it.
// TEST-EXEMPT: tokio::spawn daily loop over the unit-tested secs_until_order_prewarm, secs_until_next_ist_day and prewarm_once; call site in prewarmed_oms_http_client.
pub fn spawn_daily_order_prewarm(
    host: String,
    tls: OrderTls,
    calendar: Arc<TradingCalendar>,
) -> bool {
    if PREWARM_SPAWNED.swap(true, Ordering::AcqRel) {
        return false;
    }
    tokio::spawn(async move {
        loop {
            let now = tickvault_common::market_hours::now_ist_secs_of_day();
            let wait = match secs_until_order_prewarm(now, calendar.is_trading_day_today()) {
                Some(0) => {
                    prewarm_and_log(&host, NETWORK_RTT_PORT, &tls).await;
                    secs_until_next_ist_day(tickvault_common::market_hours::now_ist_secs_of_day())
                }
                Some(w) => w,
                None => secs_until_next_ist_day(now),
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
    true
}

/// The order HTTP client with the pre-warm on: built over the process-wide
/// [`OrderTls`], and the daily pre-warm spawned (once per process) for the
/// host of `rest_api_base_url`. Falls back to the plain client, with a coded
/// `warn!`, when the TLS config or the host cannot be had.
///
/// # Errors
/// As [`crate::oms_wiring::build_oms_http_client`].
pub fn prewarmed_oms_http_client(
    rest_api_base_url: &str,
    calendar: Arc<TradingCalendar>,
) -> Result<reqwest::Client, HttpClientBuildError> {
    let tls = match process_order_tls() {
        Ok(t) => t,
        Err(err) => {
            warn!(
                code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                source = "order_prewarm",
                error = %err,
                "order host pre-warm: no TLS config; the order client is built without it"
            );
            return crate::oms_wiring::build_oms_http_client();
        }
    };
    let client = crate::oms_wiring::build_oms_http_client_with_tls((*tls.config).clone())?;
    match host_from_url(rest_api_base_url) {
        Some(host) => {
            spawn_daily_order_prewarm(host.to_string(), tls, calendar);
        }
        None => warn!(
            code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
            source = "order_prewarm",
            error = "no_host",
            "order host pre-warm: the REST API URL has no host; no pre-warm"
        ),
    }
    Ok(client)
}

/// The order runtime's HTTP client (no-rest rule §12.16): pre-warmed only
/// when `prewarm` (`[order_runtime] prewarm_order_connection`, default false)
/// is set; otherwise the plain client, with no shared TLS store and no daily
/// task. The order runtime's only client builder, so the switch is decided in
/// one tested place.
///
/// # Errors
/// As [`crate::oms_wiring::build_oms_http_client`].
pub fn order_runtime_http_client(
    prewarm: bool,
    rest_api_base_url: &str,
    calendar: Arc<TradingCalendar>,
) -> Result<reqwest::Client, HttpClientBuildError> {
    choose_order_client(
        prewarm,
        || prewarmed_oms_http_client(rest_api_base_url, calendar),
        crate::oms_wiring::build_oms_http_client,
    )
}

/// Calls `prewarmed` when `prewarm` is set, else `plain`; never both. Pure.
fn choose_order_client<T>(
    prewarm: bool,
    prewarmed: impl FnOnce() -> T,
    plain: impl FnOnce() -> T,
) -> T {
    if prewarm { prewarmed() } else { plain() }
}

#[cfg(test)]
#[path = "order_prewarm_tests.rs"]
mod tests;
