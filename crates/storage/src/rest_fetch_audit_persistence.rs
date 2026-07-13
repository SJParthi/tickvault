//! `rest_fetch_audit` table — per-fetch forensics for the per-minute REST
//! pipelines (operator scope addition 2026-07-13, folded into PR-2 of the
//! Groww per-minute REST plan; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! ONE row PER `(target minute, symbol, feed, leg)` fetch — success AND
//! failure — so every "how did that minute's pull actually go?" question is
//! answerable from QuestDB: how many ladder attempts ran, how many 429s were
//! hit, the HTTP round-trip of the final attempt, the close→data latency,
//! the final HTTP status, and a typed outcome. Designed for BOTH feeds and
//! all REST legs: `feed` ∈ {`dhan`,`groww`}, `leg` ∈ {`spot_1m`} today
//! (`chain_1m` / `contract_1m` follow in PR-3/PR-4; the Dhan spot leg's
//! emit sites are a fast FOLLOW-UP after #1499 merges — no Dhan module is
//! touched by this PR).
//!
//! BEST-EFFORT ONLY: a forensics write failure must NEVER affect the fetch
//! loop — the emit sites log (coded, stage `audit_*`) + count and continue;
//! the fetch verdict and the failure edge are computed independently.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS rest_fetch_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL, leg SYMBOL,
//!     security_id LONG, exchange_segment SYMBOL, symbol SYMBOL,
//!     attempts INT, final_http_status INT, fetch_latency_ms LONG,
//!     close_to_data_ms LONG, rate_limited_count INT, outcome SYMBOL,
//!     error_class SYMBOL
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, leg, security_id, exchange_segment, outcome);
//! ```
//!
//! DEDUP discipline: designated `ts` FIRST (2026-04-28 rule);
//! `exchange_segment` alongside `security_id` (I-P1-11); `feed` in-key
//! (feed-in-key EVERYWHERE, 2026-06-28); `leg` in-key so a future chain-leg
//! row for the same minute/underlying never collides with the spot row;
//! `outcome` in-key (phase-0-architecture DEDUP rule 3 — TRANSITION rows
//! must BOTH survive: a sweep `named_gap`/`no_token` row for a minute must
//! never UPSERT-overwrite that minute's original ladder forensics row, and
//! a later repair's `ok` row lands ALONGSIDE the earlier failure row). A
//! re-run with the SAME outcome UPSERTs its row in place (idempotent).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name — one row per `(target minute, symbol, feed, leg)`.
pub const REST_FETCH_AUDIT_TABLE: &str = "rest_fetch_audit";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `exchange_segment` alongside `security_id` (I-P1-11); `feed` in-key
/// (2026-06-28 override); `leg` in-key (spot vs future chain/contract rows);
/// `outcome` in-key (phase-0 DEDUP rule 3 — transition rows BOTH survive:
/// a sweep gap row never overwrites the same minute's ladder row).
pub const DEDUP_KEY_REST_FETCH_AUDIT: &str =
    "ts, trading_date_ist, feed, leg, security_id, exchange_segment, outcome";

/// `leg` SYMBOL value — the per-minute SPOT 1m fetch (the only live leg
/// today; `chain_1m` / `contract_1m` land with PR-3/PR-4).
pub const REST_FETCH_LEG_SPOT_1M: &str = "spot_1m";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Typed per-fetch outcome — the SYMBOL wire strings are stable (forensic
/// queries group on them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestFetchOutcome {
    /// The target minute's candle was retrieved AND parsed.
    Ok,
    /// Every attempt got a parseable 2xx but the target minute never
    /// appeared within the ladder (vendor sealing lateness / stale body).
    Empty,
    /// The final attempt failed (transport / non-2xx / parse / budget).
    Error,
    /// The final attempt was an HTTP 429 (rate-limited).
    RateLimited,
    /// No access token was available at fire time — nothing was sent.
    NoToken,
    /// The minute boundary elapsed unfetched (fire overrun / suspend /
    /// clock step) — no request was ever made for it.
    Skipped,
    /// A finally-unrecovered NAMED GAP (post-sweep target-absent minute,
    /// pre-boot unverified minute, or a flush-lost swept minute).
    /// `final_http_status` carries the ACTUAL last HTTP status when a fetch
    /// happened for it (e.g. the sweep's 200 that lacked the minute), or
    /// the 0 sentinel when none did (hostile round 1 item 6 — never a
    /// fabricated 200 + `error` pair).
    NamedGap,
}

impl RestFetchOutcome {
    /// Stable SYMBOL wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Empty => "empty",
            Self::Error => "error",
            Self::RateLimited => "rate_limited",
            Self::NoToken => "no_token",
            Self::Skipped => "skipped",
            Self::NamedGap => "named_gap",
        }
    }
}

/// One per-fetch forensics row, ready for ILP write. Every string field is
/// a BOUNDED static slug — never raw response text (`error_class` carries
/// a fixed classification like `transport` / `http_5xx` / `named_gap`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestFetchAuditRow {
    /// Designated timestamp — the TARGET minute's open, IST nanoseconds
    /// (the same representation `spot_1m_rest.ts` uses, so the two tables
    /// join per minute).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Source broker (`dhan` / `groww`).
    pub feed: &'static str,
    /// REST leg (`spot_1m` today; `chain_1m` / `contract_1m` later).
    pub leg: &'static str,
    /// The instrument id in the FEED's id space (Dhan SID / Groww stable
    /// index id).
    pub security_id: i64,
    /// Segment classification (`IDX_I` for the spot indices).
    pub exchange_segment: &'static str,
    /// Human symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`).
    pub symbol: &'static str,
    /// Ladder attempts actually made (0 for `no_token`/`skipped`).
    pub attempts: i64,
    /// HTTP status of the FINAL attempt (0 = no HTTP response — transport
    /// failure, no token, or skipped).
    pub final_http_status: i64,
    /// HTTP round-trip (ms) of the FINAL attempt (-1 when no request ran).
    pub fetch_latency_ms: i64,
    /// Minute close → successful retrieval (ms); -1 sentinel for every
    /// non-`ok` outcome (never a fabricated latency — the scoreboard −1
    /// honesty precedent).
    pub close_to_data_ms: i64,
    /// How many attempts of this fetch were HTTP 429.
    pub rate_limited_count: i64,
    /// Typed outcome.
    pub outcome: RestFetchOutcome,
    /// Bounded failure slug (`none` for ok) — NEVER raw body text.
    pub error_class: &'static str,
}

/// The idempotent `CREATE TABLE` DDL for `rest_fetch_audit`. Pure.
#[must_use]
pub fn rest_fetch_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {REST_FETCH_AUDIT_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            leg               SYMBOL, \
            security_id       LONG, \
            exchange_segment  SYMBOL, \
            symbol            SYMBOL, \
            attempts          INT, \
            final_http_status INT, \
            fetch_latency_ms  LONG, \
            close_to_data_ms  LONG, \
            rate_limited_count INT, \
            outcome           SYMBOL, \
            error_class       SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_REST_FETCH_AUDIT});"
    )
}

/// Create the `rest_fetch_audit` table if absent (idempotent self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE — the house template). Failures log at `error!` (code SPOT1M-02,
/// stage `audit_ensure_*`) but never block — the fetch loop is unaffected;
/// NOTE the HTTP-CLIENT-01-class consequence: a failed ensure leaves the
/// table to be auto-created by the first ILP write WITHOUT dedup (a
/// duplicate-row window until a later ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
pub async fn ensure_rest_fetch_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_ensure_client_build")
                .increment(1);
            error!(
                code = "SPOT1M-02",
                stage = "audit_ensure_client_build",
                ?err,
                "SPOT1M-02: HTTP client build failed — rest_fetch_audit table \
                 not ensured (first ILP write may auto-create it WITHOUT dedup \
                 — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![rest_fetch_audit_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("feed", "SYMBOL"),
        ("leg", "SYMBOL"),
        ("security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("symbol", "SYMBOL"),
        ("attempts", "INT"),
        ("final_http_status", "INT"),
        ("fetch_latency_ms", "LONG"),
        ("close_to_data_ms", "LONG"),
        ("rate_limited_count", "INT"),
        ("outcome", "SYMBOL"),
        ("error_class", "SYMBOL"),
    ] {
        statements.push(format!(
            "ALTER TABLE {REST_FETCH_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {REST_FETCH_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_REST_FETCH_AUDIT});"
    ));
    for ddl in &statements {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_ensure_ddl")
                    .increment(1);
                error!(code = "SPOT1M-02", stage = "audit_ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SPOT1M-02: rest_fetch_audit DDL returned non-2xx (dedup may \
                     be missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_ensure_ddl")
                    .increment(1);
                error!(
                    code = "SPOT1M-02",
                    stage = "audit_ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "SPOT1M-02: rest_fetch_audit DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn rest_fetch_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `rest_fetch_audit`. Same contract as
/// `Spot1mRestWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); a failed flush DISCARDS the pending buffer
/// (poisoned-buffer defense) — the rows are forensics, best-effort, and the
/// discard is counted + coded, never silent.
pub struct RestFetchAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl RestFetchAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_rest_fetch_audit_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = rest_fetch_audit_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "rest_fetch_audit writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one per-fetch forensics row (cold path, ≤3 rows/minute/leg).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &RestFetchAuditRow) -> Result<()> {
        self.buffer
            .table(REST_FETCH_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("leg", r.leg)
            .context("leg")?
            .symbol("exchange_segment", r.exchange_segment)
            .context("exchange_segment")?
            .symbol("symbol", r.symbol)
            .context("symbol")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .symbol("error_class", r.error_class)
            .context("error_class")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("attempts", r.attempts)
            .context("attempts")?
            .column_i64("final_http_status", r.final_http_status)
            .context("final_http_status")?
            .column_i64("fetch_latency_ms", r.fetch_latency_ms)
            .context("fetch_latency_ms")?
            .column_i64("close_to_data_ms", r.close_to_data_ms)
            .context("close_to_data_ms")?
            .column_i64("rate_limited_count", r.rate_limited_count)
            .context("rate_limited_count")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer
    /// defense) — the rows are best-effort forensics; the discard is
    /// counted (`tv_rest_fetch_audit_rows_discarded_total`), never silent.
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "rest_fetch_audit: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending forensics row(s) discarded (best-effort)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "rest_fetch_audit ILP flush failed — {dropped} pending \
                     forensics row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("rest_fetch_audit: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_rest_fetch_audit_rows_discarded_total").increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> RestFetchAuditRow {
        RestFetchAuditRow {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "groww",
            leg: REST_FETCH_LEG_SPOT_1M,
            security_id: 4_611_686_018_427_387_905, // an index-band id (bit 62)
            exchange_segment: "IDX_I",
            symbol: "NIFTY",
            attempts: 2,
            final_http_status: 200,
            fetch_latency_ms: 143,
            close_to_data_ms: 1_042,
            rate_limited_count: 0,
            outcome: RestFetchOutcome::Ok,
            error_class: "none",
        }
    }

    #[test]
    fn test_rest_fetch_audit_create_ddl_contains_expected_columns() {
        let ddl = rest_fetch_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "leg",
            "security_id",
            "exchange_segment",
            "symbol",
            "attempts",
            "final_http_status",
            "fetch_latency_ms",
            "close_to_data_ms",
            "rate_limited_count",
            "outcome",
            "error_class",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_REST_FETCH_AUDIT})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `exchange_segment` with `security_id` (I-P1-11); `feed`
    /// in-key (2026-06-28 override); `leg` in-key (spot vs future
    /// chain/contract rows); `outcome` in-key (phase-0 DEDUP rule 3 —
    /// transition rows BOTH survive; hostile round 1 item 5) —
    /// whole-token matches.
    #[test]
    fn test_rest_fetch_audit_dedup_key_ts_first_segment_feed_leg_and_outcome_in_key() {
        assert!(DEDUP_KEY_REST_FETCH_AUDIT.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_REST_FETCH_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("trading_date_ist"));
        assert!(has_token("feed"));
        assert!(has_token("leg"));
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        assert!(has_token("outcome"));
        // Exactly (ts, trading_date_ist, feed, leg, security_id,
        // exchange_segment, outcome).
        assert_eq!(DEDUP_KEY_REST_FETCH_AUDIT.matches(',').count() + 1, 7);
    }

    /// Outcome SYMBOL wire strings are stable (forensic queries group on
    /// them) and cover every enum variant distinctly.
    #[test]
    fn test_rest_fetch_outcome_wire_strings_stable_and_distinct() {
        let all = [
            (RestFetchOutcome::Ok, "ok"),
            (RestFetchOutcome::Empty, "empty"),
            (RestFetchOutcome::Error, "error"),
            (RestFetchOutcome::RateLimited, "rate_limited"),
            (RestFetchOutcome::NoToken, "no_token"),
            (RestFetchOutcome::Skipped, "skipped"),
            (RestFetchOutcome::NamedGap, "named_gap"),
        ];
        for (variant, wire) in all {
            assert_eq!(variant.as_str(), wire);
        }
        let mut wires: Vec<&str> = all.iter().map(|(v, _)| v.as_str()).collect();
        wires.sort_unstable();
        wires.dedup();
        assert_eq!(wires.len(), all.len(), "wire strings must be distinct");
    }

    #[test]
    fn test_append_row_writes_symbols_and_columns() {
        let mut w = RestFetchAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(REST_FETCH_AUDIT_TABLE));
        assert!(line.contains(",feed=groww"), "feed tag missing: {line}");
        assert!(line.contains(",leg=spot_1m"), "leg tag missing: {line}");
        assert!(
            line.contains(",exchange_segment=IDX_I"),
            "segment tag missing: {line}"
        );
        assert!(line.contains(",outcome=ok"), "outcome tag missing: {line}");
        assert!(
            line.contains(",error_class=none"),
            "error_class tag missing: {line}"
        );
        assert!(line.contains("attempts=2i"), "attempts missing: {line}");
        assert!(
            line.contains("final_http_status=200i"),
            "status missing: {line}"
        );
        assert!(
            line.contains("fetch_latency_ms=143i"),
            "latency missing: {line}"
        );
        assert!(
            line.contains("close_to_data_ms=1042i"),
            "close_to_data missing: {line}"
        );
        assert!(
            line.contains("rate_limited_count=0i"),
            "429 count missing: {line}"
        );
    }

    /// A failure row stamps the -1 latency sentinels + a bounded slug —
    /// never a fabricated latency, never raw body text.
    #[test]
    fn test_append_row_failure_sentinels() {
        let mut w = RestFetchAuditWriter::for_test();
        let row = RestFetchAuditRow {
            outcome: RestFetchOutcome::Error,
            error_class: "transport",
            final_http_status: 0,
            fetch_latency_ms: -1,
            close_to_data_ms: -1,
            attempts: 5,
            rate_limited_count: 1,
            ..sample_row()
        };
        w.append_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(line.contains(",outcome=error"), "got: {line}");
        assert!(line.contains(",error_class=transport"), "got: {line}");
        assert!(line.contains("close_to_data_ms=-1i"), "got: {line}");
        assert!(line.contains("fetch_latency_ms=-1i"), "got: {line}");
        assert!(line.contains("final_http_status=0i"), "got: {line}");
    }

    #[test]
    fn test_rest_fetch_audit_flush_when_disconnected_errors_and_discards_pending() {
        let mut w = RestFetchAuditWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = RestFetchAuditWriter::for_test();
        assert!(empty.flush().is_ok());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_rest_fetch_audit_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = rest_fetch_audit_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Mock QuestDB /exec HTTP server + unreachable host (the
    // spot_1m_rest_persistence pattern) — exercises the real ensure /
    // constructor code paths: success (200), non-2xx (500), transport error.
    // ========================================================================

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn mock_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn unreachable_cfg() -> QuestDbConfig {
        // Port 1 is reserved and never listening; guarantees a real HTTP
        // transport failure without touching any live service.
        mock_cfg(1)
    }

    #[tokio::test]
    async fn test_ensure_rest_fetch_audit_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_rest_fetch_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_rest_fetch_audit_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_rest_fetch_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_rest_fetch_audit_table_unreachable_degrades_without_panic() {
        ensure_rest_fetch_audit_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_rest_fetch_audit_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = RestFetchAuditWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}
