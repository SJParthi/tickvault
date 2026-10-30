//! `hot_path_latency_minute` — one row per hot-path stage per minute: the
//! live proof that every in-memory step a tick passes through stays under
//! 1 µs (owner, 2026-10-30).
//!
//! ## What it records
//!
//! The `tv-telemetry` thread already reads every hot-path stage's histogram
//! once a second ([`crate::hot_path_telemetry::publish_once`]). Each of those
//! one-second windows is added into a fixed per-minute accumulator
//! ([`MinuteAccumulator`]); when the wall-clock minute changes, the minute's
//! rows are built (p50, p99, p99.9, max and the count of samples over
//! 1,024 ns, per stage) and handed to a dedicated writer thread through a
//! bounded queue. The recording path (`record_stage_nanos`) is untouched.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS hot_path_latency_minute (
//!     ts                  TIMESTAMP,  -- minute start (IST nanos)
//!     stage               SYMBOL,     -- hot-path stage label
//!     feed                SYMBOL,     -- dhan / ...
//!     histogram           SYMBOL,     -- log2 / quarter_octave (the bound's resolution)
//!     samples             LONG,       -- samples recorded in the minute
//!     p50_ns              LONG,       -- upper bound of the p50 bucket; null above the top bound
//!     p99_ns              LONG,       -- upper bound of the p99 bucket; null above the top bound
//!     p999_ns             LONG,       -- upper bound of the p99.9 bucket; null above the top bound
//!     max_ns              LONG,       -- the minute's largest sample (exact)
//!     samples_over_1024ns LONG,       -- samples above 1,024 ns (exact count)
//!     publishes           LONG        -- one-second windows folded into the row
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, stage, feed);
//! ```
//!
//! DEDUP carries the designated `ts`, the stage and `feed` (operator
//! override 2026-06-28: feed in every persisted key). Per stage, not per
//! instrument, so I-P1-11's `(security_id, exchange_segment)` pair is N/A.
//!
//! ## Reading it honestly
//!
//! * **The percentiles are bucket UPPER bounds, never low.** The stage
//!   histograms are log2 (`histogram = 'log2'`: the true value is in
//!   `(bound / 2, bound]`, bucket 0 is `[0, 1,024]`); `receipt_to_fold` uses
//!   its quarter-octave histogram (`'quarter_octave'`: at most 25% above).
//!   So "p99_ns = 1024" proves the minute's p99 was at most 1,024 ns.
//! * **"Over 1 µs" is over 1,024 ns.** Both histograms' first bucket ends at
//!   1,024 ns, and counting exactly 1,000 would need one more compare on the
//!   recording path, which the owner ruled must stay untouched. The column is
//!   named for the bound it really counts.
//! * **Minute alignment is ±1 s.** A one-second window is credited to the
//!   minute in which the publisher READ it, so the last second of a minute
//!   can land in the next row. A stage's per-second maximum is swapped
//!   separately from its buckets, so at a minute boundary the max can be
//!   credited to the other minute. Both are measurement-boundary effects; no
//!   sample is lost or counted twice.
//! * **The wall clock going backwards** (a time step) never reopens an
//!   earlier minute: the accumulator keeps adding into its current minute
//!   until the clock passes it again (`publishes` then exceeds 60). A forward
//!   jump closes the current minute; the skipped minutes have no row.
//! * **Absent vs empty.** A stage that recorded nothing in a minute still
//!   gets a row, with `samples = 0` and null percentiles: measured, nothing
//!   happened. A minute with no row was not measured (process down, queue
//!   full, database unreachable past the retention bound). The process's
//!   last, partial minute is not written at exit.
//! * **The first row of a process** also holds every sample recorded before
//!   the publisher's first read (boot).
//!
//! ## Cost
//!
//! 7 rows a minute, ~3,400 rows a day at the box's ~8 hours. QuestDB only;
//! nothing goes to `/metrics` except one drop counter.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

use crate::hot_path_telemetry::{
    ALL_STAGES, FINE_BUCKETS, QuantileBound, STAGE_BUCKETS, SecondWindow, Stage, fine_quantiles,
    stage_quantiles,
};

/// QuestDB table name.
pub const HOT_PATH_LATENCY_MINUTE_TABLE: &str = "hot_path_latency_minute";

/// DEDUP UPSERT key — `ts` first, then the stage and the feed.
pub const DEDUP_KEY_HOT_PATH_LATENCY_MINUTE: &str = "ts, stage, feed";

/// Rows per minute: one per hot-path stage.
pub const STAGES_PER_MINUTE: usize = ALL_STAGES.len();

/// How many minutes of rows the hand-off queue holds while the writer is
/// busy or QuestDB is slow. A full queue drops the newest minute and counts
/// it; the publisher thread never waits.
pub const LATENCY_MINUTE_QUEUE_MINUTES: usize = 64;

/// The bound the over-budget column counts against: both histograms' first
/// bucket ends here, so the count is exact with no change to the recording
/// path.
pub const OVER_BOUND_NANOS: u64 = 1_024;

/// `histogram` value for the stage (log2) histograms.
pub const HISTOGRAM_LOG2: &str = "log2";

/// `histogram` value for the finer `receipt_to_fold` histogram.
pub const HISTOGRAM_QUARTER_OCTAVE: &str = "quarter_octave";

const NANOS_PER_MINUTE: i64 = 60 * 1_000_000_000;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const HOT_PATH_LATENCY_MINUTE_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("stage", "SYMBOL"),
    ("feed", "SYMBOL"),
    ("histogram", "SYMBOL"),
    ("samples", "LONG"),
    ("p50_ns", "LONG"),
    ("p99_ns", "LONG"),
    ("p999_ns", "LONG"),
    ("max_ns", "LONG"),
    ("samples_over_1024ns", "LONG"),
    ("publishes", "LONG"),
];

/// One stage's minute. `Copy`; building one allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyMinuteRow {
    /// Minute start, IST epoch nanos. The designated timestamp.
    pub minute_ist_nanos: i64,
    /// The stage.
    pub stage: Stage,
    /// [`HISTOGRAM_LOG2`] or [`HISTOGRAM_QUARTER_OCTAVE`].
    pub histogram: &'static str,
    /// Samples recorded in the minute.
    pub samples: u64,
    /// p50 bucket upper bound, ns; `None` when empty or above the top bound.
    pub p50_ns: Option<u64>,
    /// p99 bucket upper bound, ns.
    pub p99_ns: Option<u64>,
    /// p99.9 bucket upper bound, ns.
    pub p999_ns: Option<u64>,
    /// The minute's largest sample, ns.
    pub max_ns: u64,
    /// Samples above [`OVER_BOUND_NANOS`].
    pub samples_over_1024ns: u64,
    /// One-second windows folded into this minute.
    pub publishes: u64,
}

/// One minute's rows, one per stage in [`ALL_STAGES`] order.
pub type LatencyMinuteRows = [LatencyMinuteRow; STAGES_PER_MINUTE];

/// The minute start (IST nanos) holding `ist_nanos`.
#[must_use]
pub const fn minute_floor_ist_nanos(ist_nanos: i64) -> i64 {
    ist_nanos - ist_nanos.rem_euclid(NANOS_PER_MINUTE)
}

/// The wall clock now, as IST epoch nanos (UTC + 5:30; the Greeks-style
/// `Utc::now()` stamp, never an exchange time).
#[must_use]
pub fn wall_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

fn bound_of(b: QuantileBound) -> Option<u64> {
    match b {
        QuantileBound::AtMost(n) => Some(n),
        QuantileBound::Overflow => None,
    }
}

fn sum(window: &[u64]) -> u64 {
    window.iter().fold(0u64, |acc, n| acc.saturating_add(*n))
}

fn add_into(into: &mut [u64], from: &[u64]) {
    for (a, b) in into.iter_mut().zip(from.iter()) {
        *a = a.saturating_add(*b);
    }
}

/// Sums one-second windows into the current minute. Lives on the
/// `tv-telemetry` thread only. Fixed arrays, built once, never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinuteAccumulator {
    minute: Option<i64>,
    publishes: u64,
    buckets: [[u64; STAGE_BUCKETS]; STAGES_PER_MINUTE],
    max: [u64; STAGES_PER_MINUTE],
    fine: [u64; FINE_BUCKETS],
}

impl Default for MinuteAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl MinuteAccumulator {
    /// Nothing absorbed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            minute: None,
            publishes: 0,
            buckets: [[0; STAGE_BUCKETS]; STAGES_PER_MINUTE],
            max: [0; STAGES_PER_MINUTE],
            fine: [0; FINE_BUCKETS],
        }
    }

    /// The minute currently accumulating (IST nanos), if any.
    #[must_use]
    pub const fn current_minute(&self) -> Option<i64> {
        self.minute
    }

    /// Adds one second's window, read at `wall_ist_nanos`. Returns the
    /// previous minute's rows when this window starts a later minute.
    /// A wall clock that moved backwards keeps adding into the current
    /// minute (never reopens an earlier one, whose row may already be
    /// written). O(stages × buckets), no allocation.
    pub fn absorb(
        &mut self,
        wall_ist_nanos: i64,
        second: &SecondWindow,
    ) -> Option<LatencyMinuteRows> {
        let observed = minute_floor_ist_nanos(wall_ist_nanos);
        let closed = match self.minute {
            Some(current) if observed > current => {
                let rows = self.rows();
                self.reset(observed);
                rows
            }
            Some(_) => None,
            None => {
                self.reset(observed);
                None
            }
        };
        for (into, from) in self.buckets.iter_mut().zip(second.stage_buckets.iter()) {
            add_into(into, from);
        }
        for (into, from) in self.max.iter_mut().zip(second.stage_max_nanos.iter()) {
            *into = (*into).max(*from);
        }
        add_into(&mut self.fine, &second.receipt_to_fold_fine);
        self.publishes = self.publishes.saturating_add(1);
        closed
    }

    fn reset(&mut self, minute: i64) {
        *self = Self::new();
        self.minute = Some(minute);
    }

    /// The current minute's rows so far; `None` before the first absorb.
    #[must_use]
    pub fn rows(&self) -> Option<LatencyMinuteRows> {
        let minute = self.minute?;
        Some(ALL_STAGES.map(|stage| self.row(minute, stage)))
    }

    fn row(&self, minute: i64, stage: Stage) -> LatencyMinuteRow {
        let i = stage as usize;
        let (histogram, samples, first, p50, p99, p999) = if stage == Stage::ReceiptToFold {
            let q = fine_quantiles(&self.fine);
            (
                HISTOGRAM_QUARTER_OCTAVE,
                sum(&self.fine),
                self.fine[0],
                q.and_then(|q| q.p50),
                q.and_then(|q| q.p99),
                q.and_then(|q| q.p999),
            )
        } else {
            let buckets = &self.buckets[i];
            let q = stage_quantiles(buckets);
            (
                HISTOGRAM_LOG2,
                sum(buckets),
                buckets[0],
                q.and_then(|q| bound_of(q.p50)),
                q.and_then(|q| bound_of(q.p99)),
                q.and_then(|q| bound_of(q.p999)),
            )
        };
        LatencyMinuteRow {
            minute_ist_nanos: minute,
            stage,
            histogram,
            samples,
            p50_ns: p50,
            p99_ns: p99,
            p999_ns: p999,
            max_ns: self.max[i],
            samples_over_1024ns: samples.saturating_sub(first),
            publishes: self.publishes,
        }
    }
}

/// What [`offer`] did with one minute's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferOutcome {
    /// Queued for the writer.
    Queued,
    /// The queue was full (the writer is behind); the minute is dropped and
    /// counted.
    QueueFull,
    /// No writer was started in this process (tests, or the spawn failed).
    NoSink,
    /// The writer thread is gone.
    Closed,
}

impl OfferOutcome {
    /// The `reason` label for the drop counter; `None` when queued.
    #[must_use]
    pub const fn drop_reason(self) -> Option<&'static str> {
        match self {
            Self::Queued => None,
            Self::QueueFull => Some("queue_full"),
            Self::NoSink => Some("no_writer"),
            Self::Closed => Some("writer_gone"),
        }
    }
}

static SINK: OnceLock<crossbeam_channel::Sender<LatencyMinuteRows>> = OnceLock::new();

/// Hands one minute's rows to the writer thread without waiting. O(1),
/// no allocation (the queue is a pre-allocated array of `Copy` rows). A
/// minute that cannot be queued is counted on
/// `tv_hot_path_latency_minute_dropped_total{reason}` (local `/metrics`
/// only, no alarm).
pub fn offer(rows: &LatencyMinuteRows) -> OfferOutcome {
    let outcome = match SINK.get() {
        None => OfferOutcome::NoSink,
        Some(tx) => match tx.try_send(*rows) {
            Ok(()) => OfferOutcome::Queued,
            Err(crossbeam_channel::TrySendError::Full(_)) => OfferOutcome::QueueFull,
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => OfferOutcome::Closed,
        },
    };
    // One literal label per arm: a runtime label value makes the macro build
    // a label `Vec` on every call (found by the DHAT test).
    match outcome {
        OfferOutcome::Queued => {}
        OfferOutcome::QueueFull => {
            metrics::counter!("tv_hot_path_latency_minute_dropped_total", "reason" => "queue_full")
                .increment(1);
        }
        OfferOutcome::NoSink => {
            metrics::counter!("tv_hot_path_latency_minute_dropped_total", "reason" => "no_writer")
                .increment(1);
        }
        OfferOutcome::Closed => {
            metrics::counter!("tv_hot_path_latency_minute_dropped_total", "reason" => "writer_gone")
                .increment(1);
        }
    }
    outcome
}

/// The idempotent `CREATE TABLE` DDL. Pure.
#[must_use]
pub fn hot_path_latency_minute_create_ddl() -> String {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 11-column list
    let cols = HOT_PATH_LATENCY_MINUTE_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    // O(1) EXEMPT: end
    format!(
        "CREATE TABLE IF NOT EXISTS {HOT_PATH_LATENCY_MINUTE_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_HOT_PATH_LATENCY_MINUTE});"
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn hot_path_latency_minute_ddl_statements() -> Vec<String> {
    let mut statements = vec![hot_path_latency_minute_create_ddl()];
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed 11-column list
    for (col, ty) in HOT_PATH_LATENCY_MINUTE_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {HOT_PATH_LATENCY_MINUTE_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {HOT_PATH_LATENCY_MINUTE_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_HOT_PATH_LATENCY_MINUTE});"
    ));
    statements
}

/// Ensures the table (idempotent, self-healing). Failures are logged with a
/// code and never block the caller; a failed ensure leaves the first ILP
/// write to create the table without DEDUP until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_hot_path_latency_minute_table(questdb_config: &QuestDbConfig) {
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
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "hot_path_latency_minute_ensure_client_build",
                ?err,
                "hot_path_latency_minute: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    for ddl in &hot_path_latency_minute_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "hot_path_latency_minute_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "hot_path_latency_minute: DDL returned non-2xx"
                );
            }
            Err(err) => error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "hot_path_latency_minute_ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "hot_path_latency_minute: DDL request failed"
            ),
        }
    }
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct HotPathLatencyMinuteWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl HotPathLatencyMinuteWriter {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        crate::ilp_overflow::register_overflow_baseline(HOT_PATH_LATENCY_MINUTE_TABLE);
        let conf = format!("http::addr={}:{};", config.host, config.http_port);
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
                    "hot_path_latency_minute writer: QuestDB unreachable — buffering locally"
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
    // TEST-EXEMPT: accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one row. Cold path: seven a minute, on the writer thread.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &LatencyMinuteRow) -> Result<()> {
        let row = self
            .buffer
            .table(HOT_PATH_LATENCY_MINUTE_TABLE)
            .context("table")?
            .symbol("stage", r.stage.as_str())
            .context("stage")?
            .symbol("feed", Feed::Dhan.as_str())
            .context("feed")?
            .symbol("histogram", r.histogram)
            .context("histogram")?
            .column_i64("samples", to_i64(r.samples))
            .context("samples")?;
        // A null percentile is the column left out of the row.
        let row = match r.p50_ns {
            Some(n) => row.column_i64("p50_ns", to_i64(n)).context("p50_ns")?,
            None => row,
        };
        let row = match r.p99_ns {
            Some(n) => row.column_i64("p99_ns", to_i64(n)).context("p99_ns")?,
            None => row,
        };
        let row = match r.p999_ns {
            Some(n) => row.column_i64("p999_ns", to_i64(n)).context("p999_ns")?,
            None => row,
        };
        row.column_i64("max_ns", to_i64(r.max_ns))
            .context("max_ns")?
            .column_i64("samples_over_1024ns", to_i64(r.samples_over_1024ns))
            .context("samples_over_1024ns")?
            .column_i64("publishes", to_i64(r.publishes))
            .context("publishes")?
            .at(TimestampNanos::new(r.minute_ist_nanos))
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
            anyhow::bail!("hot_path_latency_minute: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                HOT_PATH_LATENCY_MINUTE_TABLE,
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "hot_path_latency_minute ILP flush",
                    dropped,
                ),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Edge latch for the writer's flush-failure log: one `error!` when a
/// failure episode starts, one `info!` when it ends; the minutes between
/// stay in the buffer (bounded by `ilp_overflow`) and are retried.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushEpisode {
    failing: bool,
}

impl FlushEpisode {
    /// Steps the latch with one flush result. Returns `Some(true)` when an
    /// episode starts (log at error), `Some(false)` when it ends (log at
    /// info), `None` otherwise.
    pub fn step(&mut self, ok: bool) -> Option<bool> {
        match (self.failing, ok) {
            (false, false) => {
                self.failing = true;
                Some(true)
            }
            (true, true) => {
                self.failing = false;
                Some(false)
            }
            _ => None,
        }
    }
}

/// Installs the process-wide hand-off queue [`offer`] sends into, with room
/// for `capacity` minutes, and returns its receiving end. The writer thread
/// calls it once ([`spawn_hot_path_latency_minute_writer`]); the DHAT test
/// calls it to measure [`offer`] against a real, pre-allocated queue.
///
/// # Errors
/// A queue was already installed in this process.
pub fn install_sink(
    capacity: usize,
) -> std::io::Result<crossbeam_channel::Receiver<LatencyMinuteRows>> {
    let (tx, rx) = crossbeam_channel::bounded::<LatencyMinuteRows>(capacity.max(1));
    if SINK.set(tx).is_err() {
        return Err(std::io::Error::other(
            "hot_path_latency_minute queue already installed",
        ));
    }
    Ok(rx)
}

/// Starts the writer thread (`tv-latency-minute`) and installs the queue the
/// publisher thread [`offer`]s into. Call once, before
/// `hot_path_telemetry::spawn_publisher` (a minute closed before this runs
/// is counted as `no_writer`).
///
/// # Errors
/// A writer was already installed in this process, or the OS refused to
/// create the thread.
// TEST-EXEMPT: spawns a real ILP writer thread against QuestDB; the queue, the rows and the flush latch are unit-tested below.
pub fn spawn_hot_path_latency_minute_writer(
    config: &QuestDbConfig,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let rx = install_sink(LATENCY_MINUTE_QUEUE_MINUTES)?;
    let mut writer = HotPathLatencyMinuteWriter::new(config);
    std::thread::Builder::new()
        .name("tv-latency-minute".to_owned())
        .spawn(move || {
            let mut episode = FlushEpisode::default();
            // Ends when every sender is gone (never, in production: the
            // sender lives in a static).
            while let Ok(rows) = rx.recv() {
                for row in &rows {
                    if let Err(err) = writer.append_row(row) {
                        error!(
                            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                            stage = "hot_path_latency_minute_append",
                            ?err,
                            "hot_path_latency_minute: a row could not be built — skipped"
                        );
                    }
                }
                let result = writer.flush();
                match (episode.step(result.is_ok()), &result) {
                    (Some(true), Err(err)) => error!(
                        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                        stage = "hot_path_latency_minute_flush",
                        error = %err,
                        pending = writer.pending(),
                        "hot_path_latency_minute: flush failed — rows kept and retried \
                         next minute (logged once per failure episode)"
                    ),
                    (Some(false), _) => {
                        info!("hot_path_latency_minute: flush recovered — buffered minutes written")
                    }
                    _ => {}
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot_path_telemetry::{STAGE_BUCKETS, bucket_index, fine_bucket_index};

    /// Any instant; every test works on whole minutes from its floor.
    const T0: i64 = 1_793_000_000 * 1_000_000_000;

    fn minute(m: i64) -> i64 {
        minute_floor_ist_nanos(T0) + m * NANOS_PER_MINUTE
    }

    /// A window holding `n` samples of `nanos` on `stage`.
    fn window_with(stage: Stage, nanos: u64, n: u64) -> SecondWindow {
        let mut w = SecondWindow::empty();
        let i = stage as usize;
        w.stage_buckets[i][bucket_index(nanos)] += n;
        w.stage_max_nanos[i] = nanos;
        if stage == Stage::ReceiptToFold {
            w.receipt_to_fold_fine[fine_bucket_index(nanos)] += n;
        }
        w
    }

    fn row_of(rows: &LatencyMinuteRows, stage: Stage) -> LatencyMinuteRow {
        rows[stage as usize]
    }

    #[test]
    fn test_minute_floor_and_negative_instants() {
        assert_eq!(minute_floor_ist_nanos(0), 0);
        assert_eq!(minute_floor_ist_nanos(NANOS_PER_MINUTE - 1), 0);
        assert_eq!(minute_floor_ist_nanos(NANOS_PER_MINUTE), NANOS_PER_MINUTE);
        assert_eq!(minute_floor_ist_nanos(-1), -NANOS_PER_MINUTE);
    }

    #[test]
    fn test_rows_follow_all_stages_order() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &SecondWindow::empty());
        let rows = acc.rows().expect("rows");
        for (row, stage) in rows.iter().zip(ALL_STAGES.iter()) {
            assert_eq!(row.stage, *stage);
            assert_eq!(row.minute_ist_nanos, minute(0));
        }
    }

    /// Edge case: an empty minute is a row of zeros with null percentiles,
    /// never a fast-looking 0.
    #[test]
    fn test_empty_minute_has_zero_samples_and_null_percentiles() {
        let mut acc = MinuteAccumulator::new();
        assert!(acc.rows().is_none(), "nothing absorbed yet");
        for s in 0..60 {
            assert!(
                acc.absorb(minute(0) + s * 1_000_000_000, &SecondWindow::empty())
                    .is_none()
            );
        }
        let rows = acc
            .absorb(minute(1), &SecondWindow::empty())
            .expect("closed");
        for row in rows {
            assert_eq!(row.samples, 0);
            assert_eq!((row.p50_ns, row.p99_ns, row.p999_ns), (None, None, None));
            assert_eq!(row.max_ns, 0);
            assert_eq!(row.samples_over_1024ns, 0);
            assert_eq!(row.publishes, 60);
        }
    }

    /// Edge case: one sample — every percentile is its bucket.
    #[test]
    fn test_one_sample_sets_every_percentile_to_its_bucket() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::DrainMainFrame, 700, 1));
        let rows = acc.rows().expect("rows");
        let r = row_of(&rows, Stage::DrainMainFrame);
        assert_eq!(r.samples, 1);
        assert_eq!(r.histogram, HISTOGRAM_LOG2);
        assert_eq!(
            (r.p50_ns, r.p99_ns, r.p999_ns),
            (Some(1_024), Some(1_024), Some(1_024))
        );
        assert_eq!(r.max_ns, 700);
        assert_eq!(r.samples_over_1024ns, 0);
    }

    /// Edge case: every sample in one bucket.
    #[test]
    fn test_all_samples_in_one_bucket() {
        let mut acc = MinuteAccumulator::new();
        for s in 0..10 {
            let _ = acc.absorb(
                minute(0) + s,
                &window_with(Stage::SocketToWal, 3_000, 1_000),
            );
        }
        let r = row_of(&acc.rows().expect("rows"), Stage::SocketToWal);
        assert_eq!(r.samples, 10_000);
        assert_eq!(
            (r.p50_ns, r.p99_ns, r.p999_ns),
            (Some(4_096), Some(4_096), Some(4_096))
        );
        assert_eq!(r.samples_over_1024ns, 10_000);
        assert_eq!(r.publishes, 10);
    }

    /// The percentiles come from the minute's SUM, not the last second: a
    /// slow second inside an otherwise fast minute moves only the tail.
    #[test]
    fn test_percentiles_are_over_the_whole_minute() {
        let mut acc = MinuteAccumulator::new();
        for s in 0..59 {
            let _ = acc.absorb(minute(0) + s, &window_with(Stage::RingDwell, 500, 1_000));
        }
        let _ = acc.absorb(minute(0) + 59, &window_with(Stage::RingDwell, 9_000, 100));
        let r = row_of(&acc.rows().expect("rows"), Stage::RingDwell);
        assert_eq!(r.samples, 59_100);
        assert_eq!(r.p50_ns, Some(1_024));
        assert_eq!(r.p99_ns, Some(1_024), "100 of 59,100 is under 1%");
        assert_eq!(r.p999_ns, Some(16_384), "but over 0.1%");
        assert_eq!(r.max_ns, 9_000);
        assert_eq!(r.samples_over_1024ns, 100);
    }

    /// Edge case: samples in the unbounded top bucket give a NULL
    /// percentile (never a made-up bound), and the exact max still shows it.
    #[test]
    fn test_top_bucket_overflow_is_null_with_exact_max() {
        let huge = u64::MAX / 2;
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::MainRuntimeLag, huge, 5));
        let r = row_of(&acc.rows().expect("rows"), Stage::MainRuntimeLag);
        assert_eq!(r.samples, 5);
        assert_eq!((r.p50_ns, r.p99_ns, r.p999_ns), (None, None, None));
        assert_eq!(r.max_ns, huge);
        assert_eq!(r.samples_over_1024ns, 5);
        // The fine histogram's top bound (2^24 ns) on receipt_to_fold.
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::ReceiptToFold, 50_000_000, 3));
        let r = row_of(&acc.rows().expect("rows"), Stage::ReceiptToFold);
        assert_eq!(r.histogram, HISTOGRAM_QUARTER_OCTAVE);
        assert_eq!(r.p99_ns, None);
        assert_eq!(r.max_ns, 50_000_000);
    }

    /// `receipt_to_fold` reads its finer histogram: 1,100 ns is reported at
    /// most 1,280 ns, where log2 would say 2,048.
    #[test]
    fn test_receipt_to_fold_uses_the_quarter_octave_bound() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::ReceiptToFold, 1_100, 10));
        let r = row_of(&acc.rows().expect("rows"), Stage::ReceiptToFold);
        assert_eq!(r.p99_ns, Some(1_280));
        assert_eq!(r.samples, 10);
        assert_eq!(r.samples_over_1024ns, 10);
    }

    /// Bucket counts saturate rather than wrap.
    #[test]
    fn test_saturating_counts_never_wrap() {
        let mut acc = MinuteAccumulator::new();
        let mut w = SecondWindow::empty();
        w.stage_buckets[0][0] = u64::MAX;
        let _ = acc.absorb(minute(0), &w);
        let _ = acc.absorb(minute(0) + 1, &w);
        let r = row_of(&acc.rows().expect("rows"), Stage::SocketToWal);
        assert_eq!(r.samples, u64::MAX);
        assert_eq!(r.p99_ns, Some(1_024));
    }

    /// Minute rollover: the window read in the new minute belongs to the new
    /// row; the closed row carries only its own minute.
    #[test]
    fn test_minute_rollover_closes_the_previous_minute() {
        let mut acc = MinuteAccumulator::new();
        assert!(
            acc.absorb(
                minute(0) + 59_000_000_000,
                &window_with(Stage::RingDwell, 500, 7)
            )
            .is_none()
        );
        let rows = acc
            .absorb(minute(1), &window_with(Stage::RingDwell, 5_000, 3))
            .expect("minute 0 closed");
        let closed = row_of(&rows, Stage::RingDwell);
        assert_eq!(closed.minute_ist_nanos, minute(0));
        assert_eq!(closed.samples, 7);
        assert_eq!(closed.max_ns, 500);
        assert_eq!(closed.publishes, 1);
        assert_eq!(acc.current_minute(), Some(minute(1)));
        let open = row_of(&acc.rows().expect("rows"), Stage::RingDwell);
        assert_eq!((open.samples, open.max_ns, open.publishes), (3, 5_000, 1));
    }

    /// A forward jump closes the current minute; skipped minutes get no row.
    #[test]
    fn test_forward_jump_closes_once_and_skips_the_gap() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &SecondWindow::empty());
        let rows = acc
            .absorb(minute(10), &SecondWindow::empty())
            .expect("closed");
        assert_eq!(rows[0].minute_ist_nanos, minute(0));
        assert_eq!(acc.current_minute(), Some(minute(10)));
    }

    /// Edge case: the wall clock going backwards never reopens an earlier
    /// minute (whose row may already be written, and DEDUP would overwrite
    /// it with a partial one); it keeps adding into the current minute.
    #[test]
    fn test_clock_going_backwards_keeps_the_current_minute() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(5), &window_with(Stage::RingDwell, 500, 1));
        assert!(
            acc.absorb(minute(2), &window_with(Stage::RingDwell, 500, 1))
                .is_none(),
            "a backward step closes nothing"
        );
        assert_eq!(acc.current_minute(), Some(minute(5)));
        assert!(
            acc.absorb(
                minute(5) + 30_000_000_000,
                &window_with(Stage::RingDwell, 500, 1)
            )
            .is_none()
        );
        let rows = acc
            .absorb(minute(6), &SecondWindow::empty())
            .expect("closed when the clock passes minute 5");
        let r = row_of(&rows, Stage::RingDwell);
        assert_eq!(r.minute_ist_nanos, minute(5));
        assert_eq!((r.samples, r.publishes), (3, 3));
    }

    /// The max is the largest per-second max, per stage.
    #[test]
    fn test_max_is_the_largest_second_max_per_stage() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::SocketToWal, 900, 1));
        let _ = acc.absorb(minute(0) + 1, &window_with(Stage::SocketToWal, 400, 1));
        let _ = acc.absorb(minute(0) + 2, &window_with(Stage::RingDwell, 7_000, 1));
        let rows = acc.rows().expect("rows");
        assert_eq!(row_of(&rows, Stage::SocketToWal).max_ns, 900);
        assert_eq!(row_of(&rows, Stage::RingDwell).max_ns, 7_000);
        assert_eq!(row_of(&rows, Stage::DrainMainFrame).max_ns, 0);
    }

    /// One test owns the process-wide queue, so nothing races it: before
    /// the install a minute is counted as `no_writer`; after it, minutes queue
    /// until the queue is full, then are counted as `queue_full`; a second
    /// install is refused; dropping the receiver reads as `writer_gone`.
    #[test]
    fn test_install_sink_then_offer_queues_fills_and_refuses_a_second_install() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &SecondWindow::empty());
        let built = acc.rows().expect("rows");
        assert_eq!(
            offer(&built),
            OfferOutcome::NoSink,
            "no queue installed yet"
        );
        let rx = install_sink(2).expect("first install");
        assert!(install_sink(2).is_err(), "a second install is refused");
        assert_eq!(offer(&built), OfferOutcome::Queued);
        assert_eq!(offer(&built), OfferOutcome::Queued);
        assert_eq!(offer(&built), OfferOutcome::QueueFull);
        assert_eq!(rx.try_recv().expect("queued"), built);
        drop(rx);
        assert_eq!(offer(&built), OfferOutcome::Closed);
        assert_eq!(OfferOutcome::Queued.drop_reason(), None);
        assert_eq!(OfferOutcome::QueueFull.drop_reason(), Some("queue_full"));
        assert_eq!(OfferOutcome::NoSink.drop_reason(), Some("no_writer"));
        assert_eq!(OfferOutcome::Closed.drop_reason(), Some("writer_gone"));
    }

    #[test]
    fn test_flush_episode_logs_once_per_episode() {
        let mut e = FlushEpisode::default();
        assert_eq!(e.step(true), None);
        assert_eq!(e.step(false), Some(true));
        assert_eq!(e.step(false), None);
        assert_eq!(e.step(false), None);
        assert_eq!(e.step(true), Some(false));
        assert_eq!(e.step(true), None);
    }

    #[test]
    fn test_table_and_dedup_key_constants() {
        assert_eq!(HOT_PATH_LATENCY_MINUTE_TABLE, "hot_path_latency_minute");
        assert_eq!(DEDUP_KEY_HOT_PATH_LATENCY_MINUTE, "ts, stage, feed");
        assert!(DEDUP_KEY_HOT_PATH_LATENCY_MINUTE.starts_with("ts"));
        assert!(DEDUP_KEY_HOT_PATH_LATENCY_MINUTE.contains("feed"));
        assert_eq!(STAGES_PER_MINUTE, ALL_STAGES.len());
        assert_eq!(STAGE_BUCKETS, 28);
        assert_eq!(OVER_BOUND_NANOS, 1_024);
        assert_eq!(bucket_index(OVER_BOUND_NANOS), 0);
        assert_eq!(bucket_index(OVER_BOUND_NANOS + 1), 1);
        assert_eq!(fine_bucket_index(OVER_BOUND_NANOS), 0);
        assert_eq!(fine_bucket_index(OVER_BOUND_NANOS + 1), 1);
    }

    #[test]
    fn test_hot_path_latency_minute_create_ddl_has_every_column_and_the_key() {
        let create = hot_path_latency_minute_create_ddl();
        assert!(create.starts_with("CREATE TABLE IF NOT EXISTS hot_path_latency_minute ("));
        assert!(create.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(create.contains("DEDUP UPSERT KEYS(ts, stage, feed)"));
        for (col, ty) in HOT_PATH_LATENCY_MINUTE_COLUMNS {
            assert!(create.contains(&format!("{col} {ty}")), "{col}");
        }
    }

    #[test]
    fn test_hot_path_latency_minute_ddl_statements_self_heal_and_never_drop() {
        let all = hot_path_latency_minute_ddl_statements();
        assert_eq!(all.len(), HOT_PATH_LATENCY_MINUTE_COLUMNS.len() + 1);
        assert!(all.iter().all(|s| !s.to_ascii_uppercase().contains("DROP")));
        assert!(
            all.last()
                .is_some_and(|s| s.contains("DEDUP ENABLE UPSERT KEYS(ts, stage, feed)"))
        );
    }

    #[test]
    fn test_append_row_writes_every_column_and_omits_null_percentiles() {
        let mut acc = MinuteAccumulator::new();
        let _ = acc.absorb(minute(0), &window_with(Stage::DrainMainFrame, 700, 4));
        let rows = acc.rows().expect("rows");
        let mut w = HotPathLatencyMinuteWriter::for_test();
        for row in &rows {
            w.append_row(row).expect("append");
        }
        assert_eq!(w.pending(), STAGES_PER_MINUTE);
        let text = String::from_utf8_lossy(w.buffer.as_bytes()).into_owned();
        let drain = text
            .lines()
            .find(|l| l.contains("stage=drain_main_frame"))
            .expect("drain row");
        assert!(drain.starts_with(
            "hot_path_latency_minute,stage=drain_main_frame,feed=dhan,histogram=log2 "
        ));
        for col in [
            "samples=4i",
            "p50_ns=1024i",
            "p99_ns=1024i",
            "p999_ns=1024i",
            "max_ns=700i",
            "samples_over_1024ns=0i",
            "publishes=1i",
        ] {
            assert!(drain.contains(col), "{col} in {drain}");
        }
        let empty = text
            .lines()
            .find(|l| l.contains("stage=socket_to_wal"))
            .expect("empty row");
        assert!(empty.contains("samples=0i"));
        assert!(!empty.contains("p99_ns"), "a null percentile is omitted");
        assert!(w.flush().is_err(), "no sender: flush reports the failure");
    }

    #[test]
    fn test_to_i64_saturates() {
        assert_eq!(to_i64(5), 5);
        assert_eq!(to_i64(u64::MAX), i64::MAX);
    }

    #[test]
    fn test_wall_ist_nanos_now_is_utc_plus_ist() {
        let utc = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let ist = wall_ist_nanos_now();
        let diff = ist - utc - IST_UTC_OFFSET_NANOS;
        assert!(diff.abs() < 5_000_000_000, "within 5 s: {diff}");
    }
}
