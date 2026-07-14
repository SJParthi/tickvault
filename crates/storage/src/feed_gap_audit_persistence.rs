//! Feed gap-episode audit table (`feed_gap_audit`, FEED-GAP-01 family —
//! operator directive 2026-07-14: *"irrespective of any situation the Groww
//! feed must never break"* — every gap becomes a NAMED, DURABLE episode).
//!
//! One row per gap-episode EDGE: an `open` row at drop DETECTION (the
//! feed-level last-tick age crosses `FEED_GAP_EPISODE_THRESHOLD_SECS` during
//! market hours), a `closed` row at the recovery edge carrying the measured
//! `gap_secs` + `kill_count` + the NAMED partial 1-minute buckets, and a
//! `dangling_closed` row written by the 15:45 IST scoreboard sweep for
//! episodes whose recovery edge never fired (session-tail EOF / box death —
//! `-1` sentinels, never fabricated measurements).
//!
//! ANNOTATION, NEVER REPAIR — `candles_*` is untouched (live-feed purity);
//! the writer is best-effort and never sits on the feed recovery path.
//!
//! ## Audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_gap_audit (
//!     ts                TIMESTAMP,  -- row emit instant (IST nanos, designated)
//!     trading_date_ist  TIMESTAMP,  -- the trading day (IST midnight)
//!     feed              SYMBOL,     -- groww (feed-agnostic by construction)
//!     start_ts          TIMESTAMP,  -- gap detection instant
//!     end_ts            TIMESTAMP,  -- recovery instant (absent on open rows)
//!     gap_secs          LONG,       -- measured gap; -1 sentinel = unknown
//!     kill_count        INT,        -- stall restarts inside; -1 = unknown
//!     partial_minutes   STRING,     -- comma list of overlapped 1m buckets, bounded
//!     outcome           SYMBOL      -- open / closed / dangling_closed
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, start_ts, outcome);
//! ```
//!
//! `outcome` is IN-KEY per the phase-0 audit-template rule 3 (lifecycle-chain
//! audits keep `outcome` in the DEDUP key so transition rows BOTH survive):
//! the `open` row and its later `closed` / `dangling_closed` row share the
//! same `start_ts` but never overwrite each other. `feed` is in-key per the
//! 2026-06-28 operator override (feed-in-key EVERYWHERE); the designated
//! `ts` is first per the 2026-04-28 regression rule. The table is
//! per-feed-episode, not per-instrument, so I-P1-11's
//! `(security_id, exchange_segment)` pair is N/A.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per gap-episode edge.
pub const FEED_GAP_AUDIT_TABLE: &str = "feed_gap_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression
/// rule); `feed` in-key (operator override 2026-06-28); `start_ts`
/// identifies the episode; `outcome` in-key (phase-0 template rule 3) so
/// the OPEN row and the CLOSE row of one episode BOTH survive.
pub const DEDUP_KEY_FEED_GAP_AUDIT: &str = "ts, trading_date_ist, feed, start_ts, outcome";

/// Outcome wire label: gap detected, episode opened.
pub const GAP_OUTCOME_OPEN: &str = "open";
/// Outcome wire label: recovery edge observed, gap measured.
pub const GAP_OUTCOME_CLOSED: &str = "closed";
/// Outcome wire label: the 15:45 IST scoreboard sweep closed a dangling
/// OPEN episode with `-1` sentinels (session-tail EOF / box death).
pub const GAP_OUTCOME_DANGLING_CLOSED: &str = "dangling_closed";

/// `-1` sentinel: the measurement is unknown (open / dangling rows).
pub const GAP_SENTINEL_UNKNOWN: i64 = -1;

/// Bound on the persisted `partial_minutes` string (defense-in-depth on top
/// of the producer-side bucket-list bound).
pub const GAP_PARTIAL_MINUTES_MAX_CHARS: usize = 200;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One gap-episode edge row, ready for ILP write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedGapAuditRow {
    /// Row emit instant — IST nanoseconds (designated timestamp).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"groww"`; feed-agnostic by construction).
    pub feed: String,
    /// Gap detection instant — IST nanoseconds (the episode identity).
    pub start_ts_nanos: i64,
    /// Recovery instant — IST nanoseconds; `None` on OPEN rows.
    pub end_ts_nanos: Option<i64>,
    /// Measured gap in seconds; [`GAP_SENTINEL_UNKNOWN`] when unknown.
    pub gap_secs: i64,
    /// Stall-watchdog restarts inside the episode;
    /// [`GAP_SENTINEL_UNKNOWN`] when not cheaply measurable.
    pub kill_count: i64,
    /// Comma list of 1m bucket labels overlapped by [start, end], bounded
    /// by the producer (≤10 entries + ellipsis marker).
    pub partial_minutes: String,
    /// Outcome wire label (one of the `GAP_OUTCOME_*` constants).
    pub outcome: &'static str,
}

impl FeedGapAuditRow {
    /// Builds an OPEN row (gap detected; end/gap/kill unknown).
    #[must_use]
    pub fn open(
        ts_ist_nanos: i64,
        trading_date_ist_nanos: i64,
        feed: &str,
        start_ts_nanos: i64,
    ) -> Self {
        Self {
            ts_ist_nanos,
            trading_date_ist_nanos,
            feed: feed.to_string(),
            start_ts_nanos,
            end_ts_nanos: None,
            gap_secs: GAP_SENTINEL_UNKNOWN,
            kill_count: GAP_SENTINEL_UNKNOWN,
            partial_minutes: String::new(),
            outcome: GAP_OUTCOME_OPEN,
        }
    }

    /// Builds a CLOSED row (recovery edge; measurements carried).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn closed(
        ts_ist_nanos: i64,
        trading_date_ist_nanos: i64,
        feed: &str,
        start_ts_nanos: i64,
        end_ts_nanos: i64,
        gap_secs: i64,
        kill_count: i64,
        partial_minutes: String,
    ) -> Self {
        Self {
            ts_ist_nanos,
            trading_date_ist_nanos,
            feed: feed.to_string(),
            start_ts_nanos,
            end_ts_nanos: Some(end_ts_nanos),
            gap_secs,
            kill_count,
            partial_minutes,
            outcome: GAP_OUTCOME_CLOSED,
        }
    }

    /// Builds a DANGLING-CLOSED row (the 15:45 sweep; `-1` sentinels —
    /// never fabricated measurements, Rule 11).
    #[must_use]
    pub fn dangling_closed(
        ts_ist_nanos: i64,
        trading_date_ist_nanos: i64,
        feed: &str,
        start_ts_nanos: i64,
    ) -> Self {
        Self {
            ts_ist_nanos,
            trading_date_ist_nanos,
            feed: feed.to_string(),
            start_ts_nanos,
            end_ts_nanos: None,
            gap_secs: GAP_SENTINEL_UNKNOWN,
            kill_count: GAP_SENTINEL_UNKNOWN,
            partial_minutes: String::new(),
            outcome: GAP_OUTCOME_DANGLING_CLOSED,
        }
    }
}

/// The idempotent `CREATE TABLE` DDL. Pure (testable without QuestDB).
#[must_use]
pub fn feed_gap_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_GAP_AUDIT_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            start_ts          TIMESTAMP, \
            end_ts            TIMESTAMP, \
            gap_secs          LONG, \
            kill_count        INT, \
            partial_minutes   STRING, \
            outcome           SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_FEED_GAP_AUDIT});"
    )
}

/// Create the gap-episode table if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` (code FEED-GAP-01) + counter but never
/// block the caller — the episode Telegram bubbles still fire. NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later boot's ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via feed_gap_audit_create_ddl tests)
pub async fn ensure_feed_gap_audit_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!(
                "tv_feed_gap_audit_write_errors_total",
                "stage" => "ensure_client_build"
            )
            .increment(1);
            error!(
                code = "FEED-GAP-01",
                stage = "ensure_client_build",
                ?err,
                "FEED-GAP-01: HTTP client build failed — gap table not \
                 ensured (first ILP write may auto-create it WITHOUT dedup — \
                 duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let ddl = feed_gap_audit_create_ddl();
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
            metrics::counter!(
                "tv_feed_gap_audit_write_errors_total",
                "stage" => "ensure_ddl"
            )
            .increment(1);
            error!(code = "FEED-GAP-01", stage = "ensure_ddl",
                %status, ddl = ddl.as_str(),
                body = %body.chars().take(200).collect::<String>(),
                "FEED-GAP-01: gap-table DDL returned non-2xx (dedup may be \
                 missing — duplicate-row window until a later ensure succeeds)");
        }
        Err(err) => {
            metrics::counter!(
                "tv_feed_gap_audit_write_errors_total",
                "stage" => "ensure_ddl"
            )
            .increment(1);
            error!(
                code = "FEED-GAP-01",
                stage = "ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "FEED-GAP-01: gap-table DDL request failed"
            );
        }
    }
}

/// Builds the ILP-over-HTTP conf string. HTTP (NOT ILP TCP) so every flush
/// gets a per-request server ACK — a server-side reject (schema drift, DEDUP
/// violation) surfaces as `Err` instead of a silently empty table (the
/// 2026-07-05 ws_event_audit lesson). `retry_timeout=0` disables questdb-rs's
/// INTERNAL ~10s sleep-and-resend ladder and `request_timeout=5000` bounds a
/// single flush to 5s (the shadow-writer 2026-07-06 precedent) — so a flush
/// against a down/black-holed QuestDB can never wedge the caller for tens of
/// seconds (review HIGH 2026-07-14: the bridge-loop edge write must stay
/// bounded even inside its `spawn_blocking` offload).
fn gap_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `feed_gap_audit`. Mirrors
/// `FeedEpisodeAuditWriter`: unreachable QuestDB at construction still
/// builds (`sender = None`); `append_row` fills the local buffer and
/// `flush` returns `Err` until QuestDB is reachable.
pub struct FeedGapAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl FeedGapAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure
    /// (`http::` does not dial at construction; failures surface at flush).
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = gap_ilp_http_conf(config);
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
                    "feed_gap_audit writer: QuestDB unreachable — buffering locally"
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
    // TEST-EXEMPT: test-only helper used by append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one gap-episode edge row (cold path — episodes are rare).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &FeedGapAuditRow) -> Result<()> {
        let bounded_minutes: String = r
            .partial_minutes
            .chars()
            .take(GAP_PARTIAL_MINUTES_MAX_CHARS)
            .collect();
        let mut b = self
            .buffer
            .table(FEED_GAP_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("outcome", r.outcome)
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_ts("start_ts", TimestampNanos::new(r.start_ts_nanos))
            .context("start_ts")?;
        if let Some(end_nanos) = r.end_ts_nanos {
            b = b
                .column_ts("end_ts", TimestampNanos::new(end_nanos))
                .context("end_ts")?;
        }
        b.column_i64("gap_secs", r.gap_secs)
            .context("gap_secs")?
            .column_i64("kill_count", r.kill_count)
            .context("kill_count")?
            .column_str("partial_minutes", bounded_minutes.as_str())
            .context("partial_minutes")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK — a
    /// server-side reject surfaces as `Err`, never a silently empty table).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("feed_gap_audit: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("feed_gap_audit ILP flush")?;
        self.pending = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TS: i64 = 1_770_000_000_000_000_000;
    const DAY: i64 = 1_769_990_400_000_000_000;

    #[test]
    fn test_gap_ilp_conf_targets_http_with_bounded_no_retry_shape() {
        // Review HIGH (2026-07-14): the bare `http::addr=host:port;` conf let
        // questdb-rs's internal ~10s retry ladder block the caller. The conf
        // must pin the house shape (shadow-writer 2026-07-06 precedent):
        // retry_timeout=0 (our caller owns retry) + request_timeout=5000.
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        assert_eq!(
            gap_ilp_http_conf(&cfg),
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
    }

    #[test]
    fn test_feed_gap_audit_ddl_contains_expected_columns_and_dedup() {
        let ddl = feed_gap_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "start_ts",
            "end_ts",
            "gap_secs",
            "kill_count",
            "partial_minutes",
            "outcome",
        ] {
            assert!(ddl.contains(col), "DDL missing column: {col}\n{ddl}");
        }
        assert!(
            ddl.contains("DEDUP UPSERT KEYS(ts, trading_date_ist, feed, start_ts, outcome)"),
            "DDL missing DEDUP key clause\n{ddl}"
        );
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
    }

    #[test]
    fn test_dedup_key_carries_outcome_and_feed_in_key() {
        // Phase-0 template rule 3 (outcome in-key so OPEN + CLOSE both
        // survive) + the 2026-06-28 feed-in-key operator override.
        assert!(DEDUP_KEY_FEED_GAP_AUDIT.contains("outcome"));
        assert!(DEDUP_KEY_FEED_GAP_AUDIT.contains("feed"));
        assert!(DEDUP_KEY_FEED_GAP_AUDIT.starts_with("ts,"));
    }

    #[test]
    fn test_open_row_builder_carries_sentinels() {
        let r = FeedGapAuditRow::open(TS, DAY, "groww", TS - 10_000_000_000);
        assert_eq!(r.outcome, GAP_OUTCOME_OPEN);
        assert_eq!(r.gap_secs, GAP_SENTINEL_UNKNOWN);
        assert_eq!(r.kill_count, GAP_SENTINEL_UNKNOWN);
        assert!(r.end_ts_nanos.is_none());
        assert!(r.partial_minutes.is_empty());
    }

    #[test]
    fn test_closed_row_builder_carries_measurements() {
        let start = TS - 35_000_000_000;
        let r = FeedGapAuditRow::closed(TS, DAY, "groww", start, TS, 35, 2, "10:15,10:16".into());
        assert_eq!(r.outcome, GAP_OUTCOME_CLOSED);
        assert_eq!(r.gap_secs, 35);
        assert_eq!(r.kill_count, 2);
        assert_eq!(r.end_ts_nanos, Some(TS));
        assert_eq!(r.partial_minutes, "10:15,10:16");
    }

    #[test]
    fn test_dangling_closed_row_builder_uses_sentinels_never_fabrication() {
        let r = FeedGapAuditRow::dangling_closed(TS, DAY, "groww", TS - 60_000_000_000);
        assert_eq!(r.outcome, GAP_OUTCOME_DANGLING_CLOSED);
        assert_eq!(r.gap_secs, GAP_SENTINEL_UNKNOWN);
        assert_eq!(r.kill_count, GAP_SENTINEL_UNKNOWN);
        assert!(r.end_ts_nanos.is_none());
    }

    #[test]
    fn test_append_open_row_fills_buffer_without_end_ts() {
        let mut w = FeedGapAuditWriter::for_test();
        let r = FeedGapAuditRow::open(TS, DAY, "groww", TS - 10_000_000_000);
        w.append_row(&r).expect("append open");
        assert_eq!(w.pending(), 1);
        let ilp = w.buffer_utf8();
        assert!(ilp.contains(FEED_GAP_AUDIT_TABLE));
        assert!(ilp.contains("feed=groww"));
        assert!(ilp.contains("outcome=open"));
        assert!(!ilp.contains("end_ts"), "open row must omit end_ts:\n{ilp}");
    }

    #[test]
    fn test_append_closed_row_carries_end_ts_and_partial_minutes() {
        let mut w = FeedGapAuditWriter::for_test();
        let r = FeedGapAuditRow::closed(
            TS,
            DAY,
            "groww",
            TS - 35_000_000_000,
            TS,
            35,
            2,
            "10:15,10:16".into(),
        );
        w.append_row(&r).expect("append closed");
        let ilp = w.buffer_utf8();
        assert!(ilp.contains("outcome=closed"));
        assert!(ilp.contains("end_ts"));
        assert!(ilp.contains("10:15,10:16"));
    }

    #[test]
    fn test_partial_minutes_bounded_at_write_boundary() {
        let mut w = FeedGapAuditWriter::for_test();
        let huge = "x".repeat(5_000);
        let r = FeedGapAuditRow::closed(TS, DAY, "groww", TS - 1, TS, 1, 0, huge);
        w.append_row(&r).expect("append");
        let ilp = w.buffer_utf8();
        // The stored string is truncated to the bound (plus ILP framing).
        assert!(!ilp.contains(&"x".repeat(GAP_PARTIAL_MINUTES_MAX_CHARS + 1)));
    }

    #[test]
    fn test_flush_disconnected_errors_and_rows_stay_buffered() {
        let mut w = FeedGapAuditWriter::for_test();
        let r = FeedGapAuditRow::open(TS, DAY, "groww", TS - 1);
        w.append_row(&r).expect("append");
        assert!(w.flush().is_err(), "disconnected flush must Err");
        assert_eq!(w.pending(), 1, "rows stay buffered on failed flush");
    }

    #[test]
    fn test_flush_noop_when_empty() {
        let mut w = FeedGapAuditWriter::for_test();
        assert!(w.flush().is_ok());
    }
}
