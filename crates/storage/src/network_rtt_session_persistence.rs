//! `network_rtt_session` — TCP connect and TLS handshake times to the Dhan
//! hosts, a few attempts once per session (plan item LF-6a, Quote 57, owner
//! 2026-11-12: "Handshake only (Recommended)" — TCP + TLS timing only, no
//! request sent).
//!
//! ## What a row is
//!
//! One handshake attempt: resolve the host, open a TCP connection to port
//! 443, finish a TLS handshake, close. No HTTP byte is sent. A failed attempt
//! is still a row (`ok = false`, `error` names the step) because a failure is
//! itself a measurement. The app writes 5 attempts to each of 3 hosts.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS network_rtt_session (
//!     ts                TIMESTAMP,  -- the attempt's start (IST nanos)
//!     trading_date_ist  TIMESTAMP,  -- IST midnight of that day
//!     feed              SYMBOL,     -- dhan / ...
//!     host              SYMBOL,     -- the DNS name connected to
//!     sample_idx        LONG,       -- 0..4 within the session, per host
//!     tcp_connect_us    LONG,       -- TCP connect time; NULL when it did not finish
//!     tls_handshake_us  LONG,       -- TLS handshake time; NULL when it did not finish
//!     total_us          LONG,       -- resolve + connect + handshake, or to the failure
//!     ok                BOOLEAN,    -- the handshake finished
//!     error             SYMBOL      -- the failed step (fixed set); NULL when ok
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, host, sample_idx, feed);
//! ```
//!
//! DEDUP carries `feed` (operator override 2026-06-28). Per host, not per
//! instrument, so I-P1-11's `(security_id, exchange_segment)` pair is N/A.
//!
//! ## Honest limits
//!
//! * The rows of one session are written in one flush at the end. A flush
//!   that fails loses them (coded `error!`, NET-RTT-01, not spilled): they
//!   are a measurement, and the next session writes new ones.
//! * Times are wall-clock microseconds from a monotonic clock, measured on a
//!   tokio task; a busy runtime adds its scheduling delay to them.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::error;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

/// QuestDB table name.
pub const NETWORK_RTT_SESSION_TABLE: &str = "network_rtt_session";

/// DEDUP UPSERT key — `ts` first, then the host, the attempt and the feed.
pub const DEDUP_KEY_NETWORK_RTT_SESSION: &str = "ts, host, sample_idx, feed";

/// Timeout of one DDL statement of the ensure.
pub const NETWORK_RTT_SESSION_DDL_TIMEOUT_SECS: u64 = 10;

/// ILP-HTTP request timeout of the writer's sender, in seconds. Built with
/// `retry_timeout=0` and `protocol_version=1`, so one flush is one request.
pub const NETWORK_RTT_SESSION_ILP_REQUEST_TIMEOUT_SECS: u64 = 5;

/// Longest host name stored, in bytes (a DNS name is at most 253).
pub const NETWORK_RTT_HOST_MAX_BYTES: usize = 253;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const NETWORK_RTT_SESSION_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("host", "SYMBOL"),
    ("sample_idx", "LONG"),
    ("tcp_connect_us", "LONG"),
    ("tls_handshake_us", "LONG"),
    ("total_us", "LONG"),
    ("ok", "BOOLEAN"),
    ("error", "SYMBOL"),
];

/// The writer's ILP conf string for `host:port`. Pure.
#[must_use]
pub fn network_rtt_session_ilp_conf(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={host}:{http_port};protocol_version=1;retry_timeout=0;request_timeout={};",
        NETWORK_RTT_SESSION_ILP_REQUEST_TIMEOUT_SECS * 1000
    )
}

/// One handshake attempt, ready for ILP write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkRttRow {
    /// The attempt's start (IST epoch nanos). The designated timestamp.
    pub attempt_at_ist_nanos: i64,
    /// Which feed's hosts.
    pub feed: Feed,
    /// The DNS name connected to.
    pub host: String,
    /// The attempt's index for this host within the session.
    pub sample_idx: i64,
    /// TCP connect time in µs; `None` when the connect did not finish.
    pub tcp_connect_us: Option<i64>,
    /// TLS handshake time in µs; `None` when the handshake did not finish.
    pub tls_handshake_us: Option<i64>,
    /// Resolve + connect + handshake, or to the failure, in µs.
    pub total_us: i64,
    /// The handshake finished.
    pub ok: bool,
    /// The failed step (fixed set); `None` when `ok`.
    pub error: Option<&'static str>,
}

impl NetworkRttRow {
    /// IST midnight of the attempt's day.
    #[must_use]
    pub const fn trading_date_ist_nanos(&self) -> i64 {
        self.attempt_at_ist_nanos - self.attempt_at_ist_nanos.rem_euclid(NANOS_PER_DAY)
    }

    /// `host` cut to [`NETWORK_RTT_HOST_MAX_BYTES`] on a char boundary. Pure.
    #[must_use]
    pub fn bounded_host(&self) -> &str {
        let s = self.host.as_str();
        if s.len() <= NETWORK_RTT_HOST_MAX_BYTES {
            return s;
        }
        let mut cut = NETWORK_RTT_HOST_MAX_BYTES;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        &s[..cut]
    }
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn network_rtt_session_ddl_statements() -> Vec<String> {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 10-column list
    let cols = NETWORK_RTT_SESSION_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statements = vec![format!(
        "CREATE TABLE IF NOT EXISTS {NETWORK_RTT_SESSION_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_NETWORK_RTT_SESSION});"
    )];
    for (col, ty) in NETWORK_RTT_SESSION_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {NETWORK_RTT_SESSION_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {NETWORK_RTT_SESSION_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_NETWORK_RTT_SESSION});"
    ));
    statements
}

/// Ensures the table (idempotent, self-healing). Failures are logged with a
/// code (NET-RTT-01, log-only) and never block the caller. Returns `true`
/// when every statement was accepted.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_network_rtt_session_table(questdb_config: &QuestDbConfig) -> bool {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            NETWORK_RTT_SESSION_DDL_TIMEOUT_SECS,
        ))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "network_rtt_session: HTTP client build failed — table not ensured"
            );
            return false;
        }
    };
    let mut all_ok = true;
    for ddl in &network_rtt_session_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                all_ok = false;
                let status = resp.status();
                error!(
                    code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "network_rtt_session: DDL returned non-2xx"
                );
            }
            Err(err) => {
                all_ok = false;
                error!(
                    code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "network_rtt_session: DDL request failed"
                );
            }
        }
    }
    all_ok
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct NetworkRttSessionWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl NetworkRttSessionWriter {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        crate::ilp_overflow::register_overflow_baseline(NETWORK_RTT_SESSION_TABLE);
        let conf = network_rtt_session_ilp_conf(&config.host, config.http_port);
        match Sender::from_conf(conf) {
            Ok(s) => {
                let buffer = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer,
                    pending: 0,
                }
            }
            Err(err) => {
                // An address config error, not an unreachable QuestDB (the HTTP
                // sender connects lazily). The flush that follows returns `Err`
                // and the caller logs the rows as lost.
                error!(
                    code = ErrorCode::NetRtt01HandshakeCheckFailed.code_str(),
                    stage = "sender_build",
                    ?err,
                    "network_rtt_session: no ILP sender (address config refused)"
                );
                Self::for_test()
            }
        }
    }

    /// Disconnected writer with an empty buffer.
    #[must_use]
    // TEST-EXEMPT: test helper and the address-refused fallback of `new`; exercised by the append tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one row. Cold path: a few rows once a session.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &NetworkRttRow) -> Result<()> {
        let mut row = self
            .buffer
            .table(NETWORK_RTT_SESSION_TABLE)
            .context("table")?
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("host", r.bounded_host())
            .context("host")?;
        if let Some(step) = r.error {
            row = row.symbol("error", step).context("error")?;
        }
        row = row
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos()),
            )
            .context("trading_date_ist")?
            .column_i64("sample_idx", r.sample_idx)
            .context("sample_idx")?
            .column_i64("total_us", r.total_us)
            .context("total_us")?
            .column_bool("ok", r.ok)
            .context("ok")?;
        if let Some(us) = r.tcp_connect_us {
            row = row
                .column_i64("tcp_connect_us", us)
                .context("tcp_connect_us")?;
        }
        if let Some(us) = r.tls_handshake_us {
            row = row
                .column_i64("tls_handshake_us", us)
                .context("tls_handshake_us")?;
        }
        row.at(TimestampNanos::new(r.attempt_at_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP. Blocking: call off the runtime
    /// workers (`spawn_blocking`).
    ///
    /// # Errors
    /// `Err` when disconnected or the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("network_rtt_session: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                NETWORK_RTT_SESSION_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "network_rtt_session ILP flush",
                    dropped,
                ),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_START: i64 = 1_794_441_600_000_000_000; // 2026-11-12 00:00 IST-epoch

    fn sample(ok: bool) -> NetworkRttRow {
        NetworkRttRow {
            attempt_at_ist_nanos: DAY_START + 33_900 * 1_000_000_000,
            feed: Feed::Dhan,
            host: "api-feed.dhan.co".to_string(),
            sample_idx: 3,
            tcp_connect_us: Some(812),
            tls_handshake_us: ok.then_some(4_210),
            total_us: if ok { 5_100 } else { 2_000_000 },
            ok,
            error: (!ok).then_some("tls_timeout"),
        }
    }

    #[test]
    fn test_dedup_key_network_rtt_session_includes_ts_host_sample_and_feed() {
        assert_eq!(DEDUP_KEY_NETWORK_RTT_SESSION, "ts, host, sample_idx, feed");
        assert!(DEDUP_KEY_NETWORK_RTT_SESSION.starts_with("ts"));
        assert!(DEDUP_KEY_NETWORK_RTT_SESSION.contains("feed"));
    }

    #[test]
    fn test_network_rtt_session_ddl_statements_create_self_heal_and_dedup() {
        let stmts = network_rtt_session_ddl_statements();
        assert_eq!(stmts.len(), NETWORK_RTT_SESSION_COLUMNS.len() + 1);
        assert!(stmts[0].starts_with("CREATE TABLE IF NOT EXISTS network_rtt_session ("));
        assert!(stmts[0].contains("timestamp(ts) PARTITION BY DAY"));
        assert!(stmts[0].contains("DEDUP UPSERT KEYS(ts, host, sample_idx, feed)"));
        for (col, ty) in NETWORK_RTT_SESSION_COLUMNS {
            assert!(stmts[0].contains(&format!("{col} {ty}")), "{col}");
            if *col != "ts" {
                assert!(
                    stmts.contains(&format!(
                        "ALTER TABLE network_rtt_session ADD COLUMN IF NOT EXISTS {col} {ty};"
                    )),
                    "{col} self-heals"
                );
            }
        }
        assert!(
            stmts
                .iter()
                .all(|s| !s.to_ascii_uppercase().contains("DROP"))
        );
        assert_eq!(
            stmts.last().map(String::as_str),
            Some(
                "ALTER TABLE network_rtt_session DEDUP ENABLE UPSERT KEYS(ts, host, sample_idx, feed);"
            )
        );
    }

    #[test]
    fn test_network_rtt_session_columns_match_the_spec() {
        let names: Vec<&str> = NETWORK_RTT_SESSION_COLUMNS
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(
            names,
            [
                "ts",
                "trading_date_ist",
                "feed",
                "host",
                "sample_idx",
                "tcp_connect_us",
                "tls_handshake_us",
                "total_us",
                "ok",
                "error",
            ]
        );
    }

    #[test]
    fn test_network_rtt_session_ilp_conf_is_one_bounded_request() {
        let conf = network_rtt_session_ilp_conf("tv-questdb", 9000);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(Sender::from_conf(conf).is_ok());
    }

    #[test]
    fn test_network_rtt_row_trading_date_ist_nanos_is_ist_midnight() {
        assert_eq!(sample(true).trading_date_ist_nanos(), DAY_START);
    }

    #[test]
    fn test_bounded_host_cuts_on_a_char_boundary() {
        let mut r = sample(true);
        assert_eq!(r.bounded_host(), "api-feed.dhan.co");
        r.host = "é".repeat(200);
        let cut = r.bounded_host();
        assert!(cut.len() <= NETWORK_RTT_HOST_MAX_BYTES);
        assert!(r.host.starts_with(cut));
    }

    #[test]
    fn test_append_row_writes_an_ok_attempt() {
        let mut w = NetworkRttSessionWriter::for_test();
        w.append_row(&sample(true)).expect("append");
        assert_eq!(w.pending(), 1);
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        for needle in [
            "network_rtt_session,feed=dhan,host=api-feed.dhan.co ",
            "sample_idx=3i",
            "total_us=5100i",
            "ok=t",
            "tcp_connect_us=812i",
            "tls_handshake_us=4210i",
        ] {
            assert!(line.contains(needle), "{needle} missing in {line}");
        }
        assert!(!line.contains("error="), "no error on an ok row: {line}");
    }

    #[test]
    fn test_append_row_writes_a_failed_attempt_with_nulls_omitted() {
        let mut w = NetworkRttSessionWriter::for_test();
        w.append_row(&sample(false)).expect("append");
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        assert!(line.contains("error=tls_timeout"), "{line}");
        assert!(line.contains("ok=f"), "{line}");
        assert!(line.contains("tcp_connect_us=812i"), "{line}");
        assert!(!line.contains("tls_handshake_us"), "NULL omitted: {line}");
    }

    #[test]
    fn test_append_row_refuses_a_negative_timestamp() {
        let mut w = NetworkRttSessionWriter::for_test();
        let mut r = sample(true);
        r.attempt_at_ist_nanos = -1;
        assert!(w.append_row(&r).is_err());
    }

    #[test]
    fn test_flush_without_sender_keeps_rows() {
        let mut w = NetworkRttSessionWriter::for_test();
        assert!(w.flush().is_ok(), "nothing pending is a no-op");
        w.append_row(&sample(true)).expect("append");
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 1);
    }

    #[tokio::test]
    async fn test_ensure_network_rtt_session_table_returns_false_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            ensure_network_rtt_session_table(&cfg),
        )
        .await;
        assert_eq!(done.ok(), Some(false), "returns, and says not ensured");
    }
}
