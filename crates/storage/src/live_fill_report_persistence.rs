//! `live_fill_report_daily` — one row per trading day for the daily live
//! paper fill calibration report (plan item LF-7, 2026-11-12; owner Quote 57
//! "save into db / dashboard").
//!
//! The report itself (five files per day, copied to
//! `s3://tv-<env>-cold/live-fill-logs/<date>/`) is built in the app crate
//! (`live_fill_report`); this module only keeps its one-line summary queryable
//! from the QuestDB console:
//!
//! ```sql
//! SELECT * FROM live_fill_report_daily ORDER BY ts DESC LIMIT 20;
//! ```
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS live_fill_report_daily (
//!     ts                       TIMESTAMP,  -- IST midnight of the reported day
//!     trading_date             TIMESTAMP,  -- IST midnight of the reported day
//!     reported_at              TIMESTAMP,  -- the run's instant (IST nanos)
//!     feed                     SYMBOL,     -- dhan
//!     status                   SYMBOL,     -- complete / partial / no_orders
//!     light                    SYMBOL,     -- green / amber / red
//!     orders                   LONG,
//!     filled                   LONG,
//!     partial                  LONG,
//!     unfilled                 LONG,
//!     skipped                  LONG,
//!     rejected                 LONG,
//!     worse_than_worst         LONG,
//!     better_than_worst        LONG,
//!     recheck_skipped          LONG,
//!     p99_decision_to_sent_us  LONG,       -- -1 when no sample
//!     p99_feed_lag_us          LONG,       -- -1 when no sample
//!     max_api_rtt_us           LONG,       -- -1 when no sample
//!     s3_uploaded              BOOLEAN
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date, feed);
//! ```
//!
//! DEDUP carries the designated `ts`, the day and `feed` (feed in every
//! persisted key, 2026-06-28). The designated `ts` IS the day's IST midnight
//! (LF-7 review finding 5; it was the run instant, so every attempt added a
//! row and a per-day sum double counted), so a later run of the same day
//! UPSERTS its row: one row per feed and day, the latest run's values, with
//! the run instant kept in `reported_at`. No instrument, so
//! I-P1-11's `(security_id, exchange_segment)` pair is N/A. `PARTITION BY
//! DAY` (the plan said MONTH): the table goes on the DAY retention list beside
//! `live_order_events`, which sweeps day partitions only. `s3_uploaded` is a
//! BOOLEAN because it is always known: the row is written once the files are
//! written (`false`) and again after a verified copy (`true`, the upsert).
//!
//! Cold: at most a few rows a run (files written, then after a verified
//! copy), upserted onto the one row of the day. O(1).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name.
pub const LIVE_FILL_REPORT_DAILY_TABLE: &str = "live_fill_report_daily";

/// DEDUP UPSERT key — `ts` first, then the day and the feed.
pub const DEDUP_KEY_LIVE_FILL_REPORT_DAILY: &str = "ts, trading_date, feed";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const LIVE_FILL_REPORT_DAILY_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date", "TIMESTAMP"),
    ("reported_at", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("status", "SYMBOL"),
    ("light", "SYMBOL"),
    ("orders", "LONG"),
    ("filled", "LONG"),
    ("partial", "LONG"),
    ("unfilled", "LONG"),
    ("skipped", "LONG"),
    ("rejected", "LONG"),
    ("worse_than_worst", "LONG"),
    ("better_than_worst", "LONG"),
    ("recheck_skipped", "LONG"),
    ("p99_decision_to_sent_us", "LONG"),
    ("p99_feed_lag_us", "LONG"),
    ("max_api_rtt_us", "LONG"),
    ("s3_uploaded", "BOOLEAN"),
];

/// One daily report summary, ready for ILP write. `Copy`; building one
/// allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveFillReportDailyRow {
    /// The run's instant (IST epoch nanos), column `reported_at`. The
    /// designated timestamp is the day's midnight, so a re-run upserts.
    pub reported_at_ist_nanos: i64,
    /// Any instant of the reported IST day (IST epoch nanos).
    pub day_ist_nanos: i64,
    /// `dhan`.
    pub feed: &'static str,
    /// `complete` / `partial` / `no_orders`.
    pub status: &'static str,
    /// `green` / `amber` / `red`.
    pub light: &'static str,
    pub orders: i64,
    pub filled: i64,
    pub partial: i64,
    pub unfilled: i64,
    pub skipped: i64,
    pub rejected: i64,
    pub worse_than_worst: i64,
    pub better_than_worst: i64,
    pub recheck_skipped: i64,
    /// `-1` when no sample.
    pub p99_decision_to_sent_us: i64,
    /// `-1` when no sample.
    pub p99_feed_lag_us: i64,
    /// `-1` when no sample.
    pub max_api_rtt_us: i64,
    pub s3_uploaded: bool,
}

impl LiveFillReportDailyRow {
    /// IST midnight of the reported day.
    #[must_use]
    pub const fn trading_date_ist_nanos(&self) -> i64 {
        self.day_ist_nanos - self.day_ist_nanos.rem_euclid(NANOS_PER_DAY)
    }
}

/// The idempotent `CREATE TABLE` DDL. Pure.
#[must_use]
pub fn live_fill_report_daily_create_ddl() -> String {
    // O(1) EXEMPT: begin — DDL build over a fixed 19-column list, once a day
    let cols = LIVE_FILL_REPORT_DAILY_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    // O(1) EXEMPT: end
    format!(
        "CREATE TABLE IF NOT EXISTS {LIVE_FILL_REPORT_DAILY_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_LIVE_FILL_REPORT_DAILY});"
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn live_fill_report_daily_ddl_statements() -> Vec<String> {
    let mut statements = vec![live_fill_report_daily_create_ddl()];
    // O(1) EXEMPT: begin — DDL build over a fixed 19-column list, once a day
    for (col, ty) in LIVE_FILL_REPORT_DAILY_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {LIVE_FILL_REPORT_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {LIVE_FILL_REPORT_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_LIVE_FILL_REPORT_DAILY});"
    ));
    statements
}

/// Ensures the table (idempotent, self-healing). Returns `true` only when
/// every statement answered 2xx. Failures are logged with a code and never
/// block the caller.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_live_fill_report_daily_table(questdb_config: &QuestDbConfig) -> bool {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::LiveFill01ReportFailed.code_str(),
                stage = "db_row_ensure_client",
                ?err,
                "live_fill_report_daily: HTTP client build failed; table not ensured"
            );
            return false;
        }
    };
    let mut all_ok = true;
    for ddl in &live_fill_report_daily_ddl_statements() {
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
                    code = ErrorCode::LiveFill01ReportFailed.code_str(),
                    stage = "db_row_ensure",
                    %status,
                    ddl = ddl.as_str(),
                    "live_fill_report_daily: DDL returned non-2xx"
                );
            }
            Err(err) => {
                all_ok = false;
                error!(
                    code = ErrorCode::LiveFill01ReportFailed.code_str(),
                    stage = "db_row_ensure",
                    ?err,
                    ddl = ddl.as_str(),
                    "live_fill_report_daily: DDL request failed"
                );
            }
        }
    }
    all_ok
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct LiveFillReportDailyWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl LiveFillReportDailyWriter {
    /// Production constructor. `request_timeout` bounds each flush.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        crate::ilp_overflow::register_overflow_baseline(LIVE_FILL_REPORT_DAILY_TABLE);
        let conf = format!(
            "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
            config.host, config.http_port
        );
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
                warn!(
                    ?err,
                    "live_fill_report_daily writer: QuestDB unreachable; buffering locally"
                );
                Self::for_test()
            }
        }
    }

    /// Disconnected writer with an empty buffer.
    #[must_use]
    // TEST-EXEMPT: test helper and the unreachable-QuestDB fallback of `new`; exercised by the append tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one row. Cold path: once a day.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &LiveFillReportDailyRow) -> Result<()> {
        self.buffer
            .table(LIVE_FILL_REPORT_DAILY_TABLE)
            .context("table")?
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("status", r.status)
            .context("status")?
            .symbol("light", r.light)
            .context("light")?
            .column_ts(
                "trading_date",
                TimestampNanos::new(r.trading_date_ist_nanos()),
            )
            .context("trading_date")?
            .column_ts("reported_at", TimestampNanos::new(r.reported_at_ist_nanos))
            .context("reported_at")?;
        let longs: [(&'static str, i64); 12] = [
            ("orders", r.orders),
            ("filled", r.filled),
            ("partial", r.partial),
            ("unfilled", r.unfilled),
            ("skipped", r.skipped),
            ("rejected", r.rejected),
            ("worse_than_worst", r.worse_than_worst),
            ("better_than_worst", r.better_than_worst),
            ("recheck_skipped", r.recheck_skipped),
            ("p99_decision_to_sent_us", r.p99_decision_to_sent_us),
            ("p99_feed_lag_us", r.p99_feed_lag_us),
            ("max_api_rtt_us", r.max_api_rtt_us),
        ];
        for (name, v) in longs {
            self.buffer.column_i64(name, v).context(name)?;
        }
        self.buffer
            .column_bool("s3_uploaded", r.s3_uploaded)
            .context("s3_uploaded")?
            .at(TimestampNanos::new(r.trading_date_ist_nanos()))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP.
    ///
    /// # Errors
    /// `Err` when disconnected or the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("live_fill_report_daily: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                LIVE_FILL_REPORT_DAILY_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "live_fill_report_daily ILP flush",
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

    fn sample() -> LiveFillReportDailyRow {
        LiveFillReportDailyRow {
            reported_at_ist_nanos: DAY_START + 56_820 * 1_000_000_000,
            day_ist_nanos: DAY_START + 56_820 * 1_000_000_000,
            feed: "dhan",
            status: "complete",
            light: "green",
            orders: 4,
            filled: 1,
            partial: 1,
            unfilled: 0,
            skipped: 1,
            rejected: 1,
            worse_than_worst: 0,
            better_than_worst: 0,
            recheck_skipped: 0,
            p99_decision_to_sent_us: 1_200,
            p99_feed_lag_us: -1,
            max_api_rtt_us: 3_500,
            s3_uploaded: true,
        }
    }

    #[test]
    fn test_live_fill_report_daily_create_ddl_and_dedup() {
        assert_eq!(DEDUP_KEY_LIVE_FILL_REPORT_DAILY, "ts, trading_date, feed");
        let ddl = live_fill_report_daily_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS live_fill_report_daily ("));
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, trading_date, feed)"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        for (col, ty) in LIVE_FILL_REPORT_DAILY_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "{col} missing");
        }
    }

    #[test]
    fn test_live_fill_report_daily_ddl_statements_self_heal_and_never_drop() {
        let stmts = live_fill_report_daily_ddl_statements();
        assert_eq!(
            stmts.len(),
            1 + (LIVE_FILL_REPORT_DAILY_COLUMNS.len() - 1) + 1
        );
        for (col, ty) in LIVE_FILL_REPORT_DAILY_COLUMNS.iter().skip(1) {
            let alter =
                format!("ALTER TABLE live_fill_report_daily ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(stmts.contains(&alter), "missing self-heal for {col}");
        }
        assert!(
            stmts
                .last()
                .is_some_and(|s| s.contains("DEDUP ENABLE UPSERT KEYS(ts, trading_date, feed)"))
        );
        assert!(
            stmts
                .iter()
                .all(|s| !s.to_ascii_uppercase().contains("DROP"))
        );
    }

    #[test]
    fn test_live_fill_report_daily_row_trading_date_ist_nanos_is_ist_midnight() {
        assert_eq!(sample().trading_date_ist_nanos(), DAY_START);
    }

    #[test]
    fn test_live_fill_report_daily_append_row_writes_every_column_and_pending_counts_rows() {
        let mut w = LiveFillReportDailyWriter::for_test();
        assert_eq!(w.pending(), 0);
        w.append_row(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        for needle in [
            "live_fill_report_daily,feed=dhan,status=complete,light=green",
            "orders=4i",
            "filled=1i",
            "partial=1i",
            "skipped=1i",
            "rejected=1i",
            "worse_than_worst=0i",
            "p99_decision_to_sent_us=1200i",
            "p99_feed_lag_us=-1i",
            "max_api_rtt_us=3500i",
            "s3_uploaded=t",
        ] {
            assert!(line.contains(needle), "missing {needle} in {line}");
        }
        for (col, _) in LIVE_FILL_REPORT_DAILY_COLUMNS.iter().skip(1) {
            assert!(line.contains(col), "{col} not written: {line}");
        }
    }

    /// Review finding 5 (LF-7): the designated `ts` was the run instant, so
    /// every attempt of a day added a row. It is now the day's IST midnight
    /// for every run of that day (DEDUP then upserts), and the run instant
    /// goes to `reported_at`.
    #[test]
    fn test_live_fill_report_daily_append_row_designated_ts_is_the_day_so_reruns_upsert() {
        let first = sample();
        let later = LiveFillReportDailyRow {
            reported_at_ist_nanos: DAY_START + 57_660 * 1_000_000_000,
            s3_uploaded: false,
            ..sample()
        };
        let line_of = |r: &LiveFillReportDailyRow| {
            let mut w = LiveFillReportDailyWriter::for_test();
            w.append_row(r).expect("append");
            String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default()
        };
        for (r, at) in [(first, 56_820_i64), (later, 57_660)] {
            let line = line_of(&r);
            assert!(
                line.trim_end().ends_with(&format!(" {DAY_START}")),
                "designated ts must be the day: {line}"
            );
            let reported = DAY_START + at * 1_000_000_000;
            assert!(
                line.contains(&format!("reported_at={}t", reported / 1_000)),
                "reported_at missing: {line}"
            );
        }
    }

    #[test]
    fn test_live_fill_report_daily_flush_without_sender_errs_and_empty_flush_is_ok() {
        let mut w = LiveFillReportDailyWriter::for_test();
        assert!(w.flush().is_ok());
        w.append_row(&sample()).expect("append");
        assert!(w.flush().is_err());
    }

    #[tokio::test]
    async fn test_ensure_live_fill_report_daily_table_unreachable_returns_false() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        assert!(!ensure_live_fill_report_daily_table(&cfg).await);
    }
}
