//! Dual-feed daily scoreboard tables (`feed_scoreboard_daily` +
//! `feed_coverage_daily`, SCOREBOARD-01 family — operator directive
//! 2026-07-10: run Dhan + Groww live for a month, *"all tracked, captured,
//! visualized, logged, monitored, 100% automated"*).
//!
//! `feed_scoreboard_daily` — ONE row per `(trading_date_ist, feed)` written
//! by the 15:45 IST daily aggregation. The designated `ts` is the
//! **deterministic trading-date 15:45:00 IST stamp** (NOT run wall-clock),
//! so a re-run / catch-up / `TICKVAULT_SCOREBOARD_NOW` backfill UPSERTs the
//! same row instead of duplicating (DEDUP idempotency by construction).
//!
//! `feed_coverage_daily` — per-instrument coverage detail (~1.5K rows/day),
//! config-gated (`[scoreboard] coverage_detail_rows`); the table + writer
//! ship in PR-A so the schema is stable, POPULATION lands with the presence
//! registry in PR-4.
//!
//! ## Honesty columns (Rule 11 — no false OK, no fabricated zeros)
//!
//! - Lag columns carry the `-1` "not measured" sentinel until PR-3 lands the
//!   day histograms; `lag_floor_ms` records each feed's measurement floor
//!   (Dhan LTT is whole IST seconds ⇒ 1000ms floor; Groww is true-ms ⇒ 1ms).
//! - `partial_coverage` / `coverage_source` / `outcome` say WHERE the
//!   numbers came from and whether the day can be vouched for.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_scoreboard_daily (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, feed SYMBOL,
//!     ticks_captured LONG, instruments_seen LONG, mapped_instruments LONG,
//!     unmapped_instruments LONG, covered_instrument_minutes LONG,
//!     unique_win_minutes LONG, both_minutes LONG,
//!     lag_p50_ms LONG, lag_p99_ms LONG, lag_max_ms LONG, lag_samples LONG,
//!     lag_floor_ms LONG,
//!     disconnects_market LONG, disconnects_off_hours LONG, reconnects LONG,
//!     stalls LONG, blame_broker LONG, blame_ours LONG,
//!     blame_indeterminate LONG, restarts_detected LONG,
//!     streaming_minutes LONG, session_minutes LONG, uptime_pct DOUBLE,
//!     partial_coverage BOOLEAN, coverage_source SYMBOL, outcome SYMBOL
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed);
//! ```
//!
//! `feed_scoreboard_daily` is per-RUN-per-feed (no instrument key) so
//! I-P1-11's `(security_id, exchange_segment)` pair is N/A there — it IS in
//! `feed_coverage_daily`'s key (the per-instrument table).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name — one row per `(trading_date_ist, feed)` per day.
pub const FEED_SCOREBOARD_DAILY_TABLE: &str = "feed_scoreboard_daily";

/// QuestDB table name — per-instrument coverage detail (populated by PR-4).
pub const FEED_COVERAGE_DAILY_TABLE: &str = "feed_coverage_daily";

/// DEDUP key for the daily table. Designated timestamp first (2026-04-28
/// regression rule); `feed` in-key (operator override 2026-06-28). The `ts`
/// is DETERMINISTIC (trading-date 15:45:00 IST) so re-runs UPSERT in place.
/// Per-run table ⇒ I-P1-11 instrument pair N/A (no instrument key).
pub const DEDUP_KEY_FEED_SCOREBOARD_DAILY: &str = "ts, trading_date_ist, feed";

/// DEDUP key for the per-instrument coverage table: the FULL I-P1-11
/// composite `(security_id, exchange_segment)` pair + `feed` (2026-06-28
/// override) + the deterministic daily `ts`.
pub const DEDUP_KEY_FEED_COVERAGE_DAILY: &str =
    "ts, trading_date_ist, security_id, exchange_segment, feed";

/// `-1` sentinel for "not measured / source unavailable" LONG columns —
/// never a fabricated zero (audit Rule 11).
pub const SCOREBOARD_UNAVAILABLE_SENTINEL: i64 = -1;

/// Dhan lag measurement floor: LTT is whole IST seconds (live-market-feed
/// rule 6) ⇒ sub-second lag is unmeasurable; healthy p99 reads ~1-2s.
pub const LAG_FLOOR_MS_DHAN: i64 = 1000;

/// Groww lag measurement floor: `tsInMillis` is true milliseconds (the
/// receipt clock is the sidecar callback capture — one hop downstream).
pub const LAG_FLOOR_MS_GROWW: i64 = 1;

/// NSE regular session minutes ([09:15, 15:30) IST).
pub const SCOREBOARD_SESSION_MINUTES: i64 = 375;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Where the coverage numbers came from (`coverage_source` SYMBOL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageSource {
    /// In-memory presence registry (PR-4; exact unique-wins).
    InMemory,
    /// SQL over the day's `ticks` partition (flagged O(day-rows), cold).
    SqlBackfill,
    /// Registry drained mid-day + SQL backfill for the rest.
    Mixed,
}

impl CoverageSource {
    /// Stable wire label for the `coverage_source` SYMBOL column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::SqlBackfill => "sql_backfill",
            Self::Mixed => "mixed",
        }
    }
}

/// The run verdict (`outcome` SYMBOL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoreboardOutcome {
    /// Every source answered; the row vouches for the day.
    Complete,
    /// A source was unavailable — sentinel columns; cannot fully vouch.
    Partial,
    /// The episode source itself under-counted that day
    /// (`tv_ws_event_audit_dropped_total` non-zero) — treat counts as a floor.
    Degraded,
    /// The feed was switched OFF for the day (runtime-disabled / never
    /// enabled this session) — the measured zeros are real but the day is
    /// a one-horse race: EXCLUDED from the win/coverage verdict rungs and
    /// the month sums (round-4 hostile review 2026-07-10; the round-2
    /// finding — a disabled-feed day previously stamped `complete` zeros
    /// and contaminated the month totals).
    FeedOff,
}

impl ScoreboardOutcome {
    /// Stable wire label for the `outcome` SYMBOL column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Degraded => "degraded",
            Self::FeedOff => "feed_off",
        }
    }
}

/// One per-feed daily scoreboard row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedScoreboardDailyRow {
    /// Deterministic designated timestamp — trading-date 15:45:00 IST nanos.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"dhan"` / `"groww"`).
    pub feed: &'static str,
    /// Ticks persisted for this feed today (`-1` = query unavailable).
    pub ticks_captured: i64,
    /// Distinct `(security_id, segment)` pairs seen today (`-1` = unavailable).
    pub instruments_seen: i64,
    /// Cross-feed ISIN-mapped instruments (`-1` until PR-4).
    pub mapped_instruments: i64,
    /// Instruments with no cross-feed mapping (`-1` until PR-4; named in
    /// logs when it lands — never silently dropped, Rule 11).
    pub unmapped_instruments: i64,
    /// Sum of per-instrument covered session minutes (`-1` until PR-4).
    pub covered_instrument_minutes: i64,
    /// Session minutes where ONLY this feed delivered any tick (feed-level;
    /// `-1` = minute sets unavailable).
    pub unique_win_minutes: i64,
    /// Session minutes where BOTH feeds delivered (`-1` = unavailable).
    pub both_minutes: i64,
    /// Day lag percentiles in ms (`-1` until the PR-3 histograms land).
    pub lag_p50_ms: i64,
    pub lag_p99_ms: i64,
    pub lag_max_ms: i64,
    pub lag_samples: i64,
    /// The honesty column: this feed's lag measurement floor in ms.
    pub lag_floor_ms: i64,
    /// In-market disconnect episodes (`-1` = episode source unavailable).
    pub disconnects_market: i64,
    /// Off-hours disconnect rows (expected idle-cleanup noise; separate).
    pub disconnects_off_hours: i64,
    /// `reconnected` audit rows today.
    pub reconnects: i64,
    /// Stall episodes (stall_restart + never_streamed_restart; rows start
    /// the day PR-2 ships — pre-ship days honestly read 0, runbook note).
    pub stalls: i64,
    /// Blame tallies over the day's episodes (off-hours rows excluded).
    pub blame_broker: i64,
    pub blame_ours: i64,
    pub blame_indeterminate: i64,
    /// Boot-reconciled process-death episodes today.
    pub restarts_detected: i64,
    /// Session minutes with ≥1 tick from this feed (`-1` = unavailable).
    pub streaming_minutes: i64,
    /// The session denominator (375 for a regular NSE day).
    pub session_minutes: i64,
    /// `streaming_minutes / session_minutes * 100` (0.0 when unknown —
    /// paired with `partial_coverage=true` so it can't read as a false 0%).
    pub uptime_pct: f64,
    /// `true` when the row cannot vouch for the full day.
    pub partial_coverage: bool,
    /// Where the coverage numbers came from.
    pub coverage_source: CoverageSource,
    /// The run verdict.
    pub outcome: ScoreboardOutcome,
}

/// One per-instrument coverage detail row (populated by PR-4).
#[derive(Clone, Debug, PartialEq)]
pub struct FeedCoverageDailyRow {
    /// Deterministic designated timestamp — trading-date 15:45:00 IST nanos.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Canonical instrument id (Dhan SID for mapped pairs; the native id
    /// for unmapped singletons).
    pub security_id: i64,
    /// Exchange segment label (I-P1-11 composite pair with security_id).
    pub exchange_segment: String,
    /// `"cross"` for ISIN-mapped pairs; `"dhan"`/`"groww"` for singletons.
    pub feed: String,
    /// Human symbol for the drill-down.
    pub symbol_name: String,
    pub dhan_minutes: i64,
    pub groww_minutes: i64,
    pub dhan_only_minutes: i64,
    pub groww_only_minutes: i64,
    pub both_minutes: i64,
    /// `true` when the instrument is cross-feed ISIN-mapped.
    pub mapped: bool,
    /// `true` when the registry could not vouch for the full day.
    pub partial_coverage: bool,
}

/// The idempotent `CREATE TABLE` DDL for `feed_scoreboard_daily`. Pure.
#[must_use]
pub fn feed_scoreboard_daily_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_SCOREBOARD_DAILY_TABLE} (\
            ts                         TIMESTAMP, \
            trading_date_ist           TIMESTAMP, \
            feed                       SYMBOL, \
            ticks_captured             LONG, \
            instruments_seen           LONG, \
            mapped_instruments         LONG, \
            unmapped_instruments       LONG, \
            covered_instrument_minutes LONG, \
            unique_win_minutes         LONG, \
            both_minutes               LONG, \
            lag_p50_ms                 LONG, \
            lag_p99_ms                 LONG, \
            lag_max_ms                 LONG, \
            lag_samples                LONG, \
            lag_floor_ms               LONG, \
            disconnects_market         LONG, \
            disconnects_off_hours      LONG, \
            reconnects                 LONG, \
            stalls                     LONG, \
            blame_broker               LONG, \
            blame_ours                 LONG, \
            blame_indeterminate        LONG, \
            restarts_detected          LONG, \
            streaming_minutes          LONG, \
            session_minutes            LONG, \
            uptime_pct                 DOUBLE, \
            partial_coverage           BOOLEAN, \
            coverage_source            SYMBOL, \
            outcome                    SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_FEED_SCOREBOARD_DAILY});"
    )
}

/// The idempotent `CREATE TABLE` DDL for `feed_coverage_daily`. Pure.
#[must_use]
pub fn feed_coverage_daily_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_COVERAGE_DAILY_TABLE} (\
            ts                 TIMESTAMP, \
            trading_date_ist   TIMESTAMP, \
            security_id        LONG, \
            exchange_segment   SYMBOL, \
            feed               SYMBOL, \
            symbol_name        SYMBOL, \
            dhan_minutes       LONG, \
            groww_minutes      LONG, \
            dhan_only_minutes  LONG, \
            groww_only_minutes LONG, \
            both_minutes       LONG, \
            mapped             BOOLEAN, \
            partial_coverage   BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_FEED_COVERAGE_DAILY});"
    )
}

/// Create both scoreboard tables if absent (idempotent, schema-self-heal
/// pattern: CREATE → feed ADD COLUMN → DEDUP ENABLE, in that order, per the
/// 2026-06-28 feed-in-key override; never a table drop — SEBI retention).
///
/// Deliberately NO `SET feed = 'dhan' WHERE feed IS NULL` backfill (hostile
/// review 2026-07-10): these are GREENFIELD tables whose writers always
/// stamp `feed`, and `feed_coverage_daily` legitimately carries
/// `'cross'`/`'groww'` rows — a 'dhan' backfill would misattribute the very
/// per-feed verdict this feature exists to produce.
///
/// Failures log at `error!` (code SCOREBOARD-01) but never block the run —
/// the aggregation still reports via the log/Telegram path. NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later boot's ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests)
pub async fn ensure_feed_scoreboard_tables(questdb_config: &QuestDbConfig) {
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
            error!(
                code = "SCOREBOARD-01",
                stage = "ensure_client_build",
                ?err,
                "SCOREBOARD-01: HTTP client build failed — scoreboard tables not \
                 ensured (first ILP write may auto-create them WITHOUT dedup — \
                 duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let statements = [
        feed_scoreboard_daily_create_ddl(),
        format!("ALTER TABLE {FEED_SCOREBOARD_DAILY_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL;"),
        format!(
            "ALTER TABLE {FEED_SCOREBOARD_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_SCOREBOARD_DAILY});"
        ),
        feed_coverage_daily_create_ddl(),
        format!("ALTER TABLE {FEED_COVERAGE_DAILY_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL;"),
        format!(
            "ALTER TABLE {FEED_COVERAGE_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_COVERAGE_DAILY});"
        ),
    ];
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
                error!(code = "SCOREBOARD-01", stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SCOREBOARD-01: scoreboard DDL returned non-2xx (dedup may be \
                     missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => error!(
                code = "SCOREBOARD-01",
                stage = "ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "SCOREBOARD-01: scoreboard DDL request failed"
            ),
        }
    }
}

/// Builds the ILP-over-HTTP conf string (per-flush server ACK — the
/// 2026-07-05 fire-and-forget lesson; cold path, once/day).
fn scoreboard_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// Lazy ILP-over-HTTP writer for both scoreboard tables. Mirrors
/// `WsEventAuditWriter`: unreachable QuestDB at construction still builds;
/// `flush` returns `Err` (incl. server-side rejects via the HTTP ACK).
pub struct FeedScoreboardWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl FeedScoreboardWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = scoreboard_ilp_http_conf(config);
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
                    "feed_scoreboard writer: QuestDB unreachable — buffering locally"
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

    /// Appends one per-feed daily scoreboard row (cold path, twice/day).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_daily_row(&mut self, r: &FeedScoreboardDailyRow) -> Result<()> {
        self.buffer
            .table(FEED_SCOREBOARD_DAILY_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("coverage_source", r.coverage_source.as_str())
            .context("coverage_source")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("ticks_captured", r.ticks_captured)
            .context("ticks_captured")?
            .column_i64("instruments_seen", r.instruments_seen)
            .context("instruments_seen")?
            .column_i64("mapped_instruments", r.mapped_instruments)
            .context("mapped_instruments")?
            .column_i64("unmapped_instruments", r.unmapped_instruments)
            .context("unmapped_instruments")?
            .column_i64("covered_instrument_minutes", r.covered_instrument_minutes)
            .context("covered_instrument_minutes")?
            .column_i64("unique_win_minutes", r.unique_win_minutes)
            .context("unique_win_minutes")?
            .column_i64("both_minutes", r.both_minutes)
            .context("both_minutes")?
            .column_i64("lag_p50_ms", r.lag_p50_ms)
            .context("lag_p50_ms")?
            .column_i64("lag_p99_ms", r.lag_p99_ms)
            .context("lag_p99_ms")?
            .column_i64("lag_max_ms", r.lag_max_ms)
            .context("lag_max_ms")?
            .column_i64("lag_samples", r.lag_samples)
            .context("lag_samples")?
            .column_i64("lag_floor_ms", r.lag_floor_ms)
            .context("lag_floor_ms")?
            .column_i64("disconnects_market", r.disconnects_market)
            .context("disconnects_market")?
            .column_i64("disconnects_off_hours", r.disconnects_off_hours)
            .context("disconnects_off_hours")?
            .column_i64("reconnects", r.reconnects)
            .context("reconnects")?
            .column_i64("stalls", r.stalls)
            .context("stalls")?
            .column_i64("blame_broker", r.blame_broker)
            .context("blame_broker")?
            .column_i64("blame_ours", r.blame_ours)
            .context("blame_ours")?
            .column_i64("blame_indeterminate", r.blame_indeterminate)
            .context("blame_indeterminate")?
            .column_i64("restarts_detected", r.restarts_detected)
            .context("restarts_detected")?
            .column_i64("streaming_minutes", r.streaming_minutes)
            .context("streaming_minutes")?
            .column_i64("session_minutes", r.session_minutes)
            .context("session_minutes")?
            .column_f64("uptime_pct", r.uptime_pct)
            .context("uptime_pct")?
            .column_bool("partial_coverage", r.partial_coverage)
            .context("partial_coverage")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Appends one per-instrument coverage detail row (PR-4 population).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_coverage_row(&mut self, r: &FeedCoverageDailyRow) -> Result<()> {
        self.buffer
            .table(FEED_COVERAGE_DAILY_TABLE)
            .context("table")?
            .symbol("exchange_segment", r.exchange_segment.as_str())
            .context("exchange_segment")?
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("symbol_name", r.symbol_name.as_str())
            .context("symbol_name")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("dhan_minutes", r.dhan_minutes)
            .context("dhan_minutes")?
            .column_i64("groww_minutes", r.groww_minutes)
            .context("groww_minutes")?
            .column_i64("dhan_only_minutes", r.dhan_only_minutes)
            .context("dhan_only_minutes")?
            .column_i64("groww_only_minutes", r.groww_only_minutes)
            .context("groww_only_minutes")?
            .column_i64("both_minutes", r.both_minutes)
            .context("both_minutes")?
            .column_bool("mapped", r.mapped)
            .context("mapped")?
            .column_bool("partial_coverage", r.partial_coverage)
            .context("partial_coverage")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("feed_scoreboard: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("feed_scoreboard ILP flush")?;
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

    fn sample_daily_row() -> FeedScoreboardDailyRow {
        FeedScoreboardDailyRow {
            ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "dhan",
            ticks_captured: 1_842_551,
            instruments_seen: 776,
            mapped_instruments: SCOREBOARD_UNAVAILABLE_SENTINEL,
            unmapped_instruments: SCOREBOARD_UNAVAILABLE_SENTINEL,
            covered_instrument_minutes: SCOREBOARD_UNAVAILABLE_SENTINEL,
            unique_win_minutes: 14,
            both_minutes: 350,
            lag_p50_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_p99_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_max_ms: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_samples: SCOREBOARD_UNAVAILABLE_SENTINEL,
            lag_floor_ms: LAG_FLOOR_MS_DHAN,
            disconnects_market: 3,
            disconnects_off_hours: 5,
            reconnects: 3,
            stalls: 0,
            blame_broker: 2,
            blame_ours: 0,
            blame_indeterminate: 1,
            restarts_detected: 0,
            streaming_minutes: 373,
            session_minutes: SCOREBOARD_SESSION_MINUTES,
            uptime_pct: 99.47,
            partial_coverage: false,
            coverage_source: CoverageSource::SqlBackfill,
            outcome: ScoreboardOutcome::Complete,
        }
    }

    fn sample_coverage_row() -> FeedCoverageDailyRow {
        FeedCoverageDailyRow {
            ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            security_id: 13,
            exchange_segment: "IDX_I".to_string(),
            feed: "cross".to_string(),
            symbol_name: "NIFTY".to_string(),
            dhan_minutes: 375,
            groww_minutes: 374,
            dhan_only_minutes: 1,
            groww_only_minutes: 0,
            both_minutes: 374,
            mapped: true,
            partial_coverage: false,
        }
    }

    #[test]
    fn test_feed_scoreboard_daily_create_ddl_contains_expected_columns() {
        let ddl = feed_scoreboard_daily_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "ticks_captured",
            "instruments_seen",
            "mapped_instruments",
            "unmapped_instruments",
            "covered_instrument_minutes",
            "unique_win_minutes",
            "both_minutes",
            "lag_p50_ms",
            "lag_p99_ms",
            "lag_max_ms",
            "lag_samples",
            "lag_floor_ms",
            "disconnects_market",
            "disconnects_off_hours",
            "reconnects",
            "stalls",
            "blame_broker",
            "blame_ours",
            "blame_indeterminate",
            "restarts_detected",
            "streaming_minutes",
            "session_minutes",
            "uptime_pct",
            "partial_coverage",
            "coverage_source",
            "outcome",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_FEED_SCOREBOARD_DAILY})"
        )));
    }

    #[test]
    fn test_feed_scoreboard_dedup_keys_ts_first_and_feed_in_key() {
        // 2026-04-28 regression rule: designated ts leads the key.
        assert!(
            DEDUP_KEY_FEED_SCOREBOARD_DAILY
                .trim_start()
                .starts_with("ts,")
        );
        // Operator override 2026-06-28: feed in-key (whole-token match).
        let has_token =
            |key: &str, t: &str| key.split([',', ' ']).map(str::trim).any(|tok| tok == t);
        assert!(has_token(DEDUP_KEY_FEED_SCOREBOARD_DAILY, "feed"));
        assert!(DEDUP_KEY_FEED_SCOREBOARD_DAILY.contains("trading_date_ist"));
        // Per-run table: exactly (ts, trading_date_ist, feed).
        assert_eq!(DEDUP_KEY_FEED_SCOREBOARD_DAILY.matches(',').count() + 1, 3);
    }

    #[test]
    fn test_feed_coverage_daily_create_ddl_dedup_key_full_instrument_pair_plus_feed() {
        // I-P1-11: the per-instrument table carries the FULL composite
        // (security_id, exchange_segment) pair + feed + the deterministic ts.
        let has_token = |t: &str| {
            DEDUP_KEY_FEED_COVERAGE_DAILY
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(
            DEDUP_KEY_FEED_COVERAGE_DAILY
                .trim_start()
                .starts_with("ts,")
        );
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        assert!(has_token("feed"));
        assert!(has_token("trading_date_ist"));
        assert_eq!(DEDUP_KEY_FEED_COVERAGE_DAILY.matches(',').count() + 1, 5);
        let ddl = feed_coverage_daily_create_ddl();
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_FEED_COVERAGE_DAILY})"
        )));
    }

    #[test]
    fn test_scoreboard_outcome_and_coverage_source_labels_stable() {
        assert_eq!(ScoreboardOutcome::Complete.as_str(), "complete");
        assert_eq!(ScoreboardOutcome::Partial.as_str(), "partial");
        assert_eq!(ScoreboardOutcome::Degraded.as_str(), "degraded");
        // Round 4 (2026-07-10): the feed-was-off one-horse-race day —
        // excluded from the month sums by the runbook SQL.
        assert_eq!(ScoreboardOutcome::FeedOff.as_str(), "feed_off");
        assert_eq!(CoverageSource::InMemory.as_str(), "in_memory");
        assert_eq!(CoverageSource::SqlBackfill.as_str(), "sql_backfill");
        assert_eq!(CoverageSource::Mixed.as_str(), "mixed");
    }

    #[test]
    fn test_lag_floor_and_sentinel_constants_pinned() {
        // The honesty columns' constants: Dhan LTT is whole IST seconds
        // (≥1s floor); Groww tsInMillis is true-ms; -1 = never a
        // fabricated zero (Rule 11).
        assert_eq!(LAG_FLOOR_MS_DHAN, 1000);
        assert_eq!(LAG_FLOOR_MS_GROWW, 1);
        assert_eq!(SCOREBOARD_UNAVAILABLE_SENTINEL, -1);
        assert_eq!(SCOREBOARD_SESSION_MINUTES, 375);
    }

    #[test]
    fn test_append_daily_row_writes_feed_and_outcome_symbols() {
        let mut w = FeedScoreboardWriter::for_test();
        w.append_daily_row(&sample_daily_row())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(FEED_SCOREBOARD_DAILY_TABLE));
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(
            line.contains(",outcome=complete"),
            "outcome tag missing: {line}"
        );
        assert!(
            line.contains(",coverage_source=sql_backfill"),
            "coverage_source tag missing: {line}"
        );
        // The -1 sentinels are written verbatim, never zeroed.
        assert!(
            line.contains("lag_p99_ms=-1i"),
            "sentinel must persist: {line}"
        );
    }

    #[test]
    fn test_append_coverage_row_writes_instrument_pair() {
        let mut w = FeedScoreboardWriter::for_test();
        w.append_coverage_row(&sample_coverage_row())
            .expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(line.starts_with(FEED_COVERAGE_DAILY_TABLE));
        assert!(line.contains(",exchange_segment=IDX_I"));
        assert!(line.contains(",feed=cross"));
        assert!(line.contains("security_id=13i"));
    }

    #[test]
    fn test_scoreboard_flush_when_disconnected_errors() {
        let mut w = FeedScoreboardWriter::for_test();
        w.append_daily_row(&sample_daily_row())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert_eq!(w.pending(), 1, "rows stay pending, never lost");
        // Empty flush is a no-op Ok.
        let mut empty = FeedScoreboardWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    #[test]
    fn test_scoreboard_writers_use_ilp_http_conf() {
        // Transport ratchet (2026-07-05 lesson): HTTP port, per-flush ACK.
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = scoreboard_ilp_http_conf(&cfg);
        assert_eq!(conf, "http::addr=tv-questdb:9000;");
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }
}
