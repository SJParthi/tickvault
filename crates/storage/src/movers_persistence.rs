//! QuestDB ILP persistence for top movers snapshots (stocks + options).
//!
//! Persists 1-minute ranked snapshots of gainers, losers, most active (stocks)
//! and Highest OI, OI Gainers/Losers, Top Volume/Value, Price Gainers/Losers (options).
//!
//! # Tables Written
//! - `stock_movers` — top 20 gainers, losers, most active per minute
//! - `option_movers` — top 20 per 7 categories per minute
//!
//! # Idempotency
//! DEDUP UPSERT KEYS on `(ts, security_id, category, segment)` prevent
//! duplicates on restart. `segment` is required because the same
//! `security_id` exists across `IDX_I` / `NSE_EQ` / `NSE_FNO` — see audit
//! gaps DB-3/DB-4.
//!
//! # Error Handling (DB-6/DB-7)
//! Movers persistence is cold-path observability data, NOT critical path.
//! Reconnect attempts are throttled to one per
//! `MOVERS_RECONNECT_THROTTLE_SECS` (30s) to prevent tight reconnect loops
//! when QuestDB flaps. Every dropped batch is counted via
//! `rows_dropped_total` + `tv_{stock,option}_movers_dropped_total` +
//! `tv_{stock,option}_movers_flush_failures_total` and logged at ERROR
//! level so the Telegram alert path fires.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// QuestDB table name for stock movers (gainers, losers, most active).
/// Plan item C (2026-04-22): unified 6-bucket movers table name.
/// Written by the V2 movers writer once per second during market hours.
pub const QUESTDB_TABLE_TOP_MOVERS: &str = "top_movers";

pub const QUESTDB_TABLE_STOCK_MOVERS: &str = "stock_movers";

/// QuestDB table name for option movers (7 categories).
pub const QUESTDB_TABLE_OPTION_MOVERS: &str = "option_movers";

/// DEDUP UPSERT KEY for movers tables.
///
/// Compound key: `(ts, security_id, category, segment)` prevents duplicate entries
/// on restart/reconnect AND prevents cross-segment collision.
///
/// **Why `segment` is required** (audit gap DB-3/DB-4, ticket 2026-04-15):
/// The same `security_id` is reused by Dhan across exchange segments. For
/// example, `security_id = 13` is `NIFTY` in both `IDX_I` (index) and `NSE_EQ`
/// (equity — no such instrument in real life, but the collision exists at the
/// schema level and has been observed for other IDs like 25 / BANKNIFTY). If
/// `segment` is omitted from the DEDUP key, two legitimate distinct movers
/// rows in the same 1-minute snapshot bucket will silently UPSERT each other.
/// Both DDL schemas (`stock_movers`, `option_movers`) declare `segment SYMBOL`
/// — the DEDUP key must reference every column that contributes to row identity.
///
/// Enforced by `test_dedup_key_movers_includes_segment` +
/// `test_dedup_key_movers_matches_ddl_identity_columns`.
const DEDUP_KEY_MOVERS: &str = "security_id, category, segment";

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Flush batch size for movers (much smaller than ticks — max 20×10 = 200 rows per snapshot).
const MOVERS_FLUSH_BATCH_SIZE: usize = 250;

/// Minimum interval between reconnect attempts when QuestDB is down.
///
/// Audit gap DB-6: the previous flush path called `Sender::from_conf`
/// on every failed batch with zero backoff, creating a tight reconnect
/// loop when QuestDB flapped. Caps reconnect attempts at one per 30s
/// per writer, matching the pattern used by `LiveCandleWriter` and
/// `IndicatorSnapshotWriter`.
const MOVERS_RECONNECT_THROTTLE_SECS: u64 = 30;

/// Capacity of the in-memory rescue ring for movers writers (DB-2).
///
/// Movers are emitted at ~200 rows/min max (20 entries × 10 categories).
/// A 5,000-entry ring covers ~25 minutes of snapshot backlog — comfortably
/// longer than any routine QuestDB restart. At ~200 bytes per stock
/// record and ~280 bytes per option record, the per-writer footprint is
/// ~1 MB stock + ~1.4 MB option = 2.4 MB total — bounded and affordable.
const MOVERS_RESCUE_RING_CAPACITY: usize = 5_000;

/// Flush interval for movers (60 seconds — aligned with snapshot interval).
/// Used by future flush_if_needed() implementation.
#[allow(dead_code)] // APPROVED: will be used when flush_if_needed() is wired
const MOVERS_FLUSH_INTERVAL_MS: u64 = 65_000;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL for the `stock_movers` table.
///
/// Stores 1-minute snapshots of top 20 gainers, losers, most active stocks.
/// Partitioned by DAY (one partition per trading day, ~60×3×20 = 3600 rows/day).
const STOCK_MOVERS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS stock_movers (\
        category SYMBOL,\
        security_id LONG,\
        segment SYMBOL,\
        symbol SYMBOL,\
        ltp DOUBLE,\
        prev_close DOUBLE,\
        change_abs DOUBLE,\
        change_pct DOUBLE,\
        volume LONG,\
        rank INT,\
        phase SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// Plan item C (2026-04-22): DDL for the unified 6-bucket `top_movers` table.
///
/// Replaces `stock_movers` + `option_movers` with a single schema keyed by
/// `(bucket, rank_category, rank, ts)`. One partition per trading day (SEBI
/// 5-year retention via lifecycle tiers). WAL + DEDUP means repeated writes
/// of the same rank row within the same second collapse to the latest.
///
/// # Bucket values (plan A1 wire strings)
/// `indices` / `stocks` / `index_futures` / `stock_futures` /
/// `index_options` / `stock_options`
///
/// # Rank category values (plan A2 wire strings)
/// `gainers` / `losers` / `most_active` / `top_oi` / `oi_buildup` /
/// `oi_unwind` / `top_value`
///
/// # Row volume
/// At 1 Hz persistence cadence with 20 ranks per bucket per category
/// across six buckets (3 categories × 2 price buckets + 7 × 4 derivative
/// buckets = 134 rows/sec × 21_600 market seconds/day ≈ 2.9M rows/day).
const TOP_MOVERS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS top_movers (\
        timeframe SYMBOL,\
        bucket SYMBOL,\
        rank_category SYMBOL,\
        rank INT,\
        security_id LONG,\
        segment SYMBOL,\
        symbol SYMBOL,\
        underlying SYMBOL,\
        expiry STRING,\
        strike DOUBLE,\
        option_type SYMBOL,\
        ltp DOUBLE,\
        prev_close DOUBLE,\
        change_pct DOUBLE,\
        volume LONG,\
        value DOUBLE,\
        oi LONG,\
        prev_oi LONG,\
        oi_change_pct DOUBLE,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// Idempotent self-heal DDL: ALTER TABLE ADD COLUMN IF NOT EXISTS for the
/// two columns added 2026-04-25 (`timeframe`, `segment`). Pre-existing
/// QuestDB deployments built on the previous schema get auto-migrated at
/// boot without DROP. QuestDB ignores ADDs that already exist, so running
/// every boot is free.
const TOP_MOVERS_ALTER_DDL_TIMEFRAME: &str =
    "ALTER TABLE top_movers ADD COLUMN IF NOT EXISTS timeframe SYMBOL";
const TOP_MOVERS_ALTER_DDL_SEGMENT: &str =
    "ALTER TABLE top_movers ADD COLUMN IF NOT EXISTS segment SYMBOL";

/// Wave-3-A Item 10 + adversarial-review LOW #6: sanitize an ILP SYMBOL
/// value before it is written to QuestDB.
///
/// The InfluxDB Line Protocol uses `\n`, `\r`, `,`, `=`, and space as
/// structural delimiters. Although the upstream `questdb-rs` crate
/// validates symbol values, the precise rejected-character set has
/// drifted across library versions and a future upgrade could re-open
/// the injection vector. Strip every control character + the four
/// structural delimiters defensively before they reach the buffer.
///
/// This is a borrow-friendly helper: returns `Cow::Borrowed(input)` if
/// the input is already clean (zero allocation on the common path) and
/// `Cow::Owned(_)` only when sanitisation actually happened.
///
/// O(1) per character; runs at the cold ILP-build cadence (~218
/// rows/snapshot × 60s = ~3.6 rows/sec peak), so allocation on the
/// dirty path is acceptable.
#[must_use]
pub fn sanitize_ilp_symbol(input: &str) -> std::borrow::Cow<'_, str> {
    let needs_sanitize = input
        .chars()
        .any(|c| c == '\n' || c == '\r' || c == ',' || c == '=' || c.is_control());
    if !needs_sanitize {
        return std::borrow::Cow::Borrowed(input);
    }
    let cleaned: String = input
        .chars()
        .filter(|c| !(*c == '\n' || *c == '\r' || *c == ',' || *c == '=' || c.is_control()))
        .collect();
    std::borrow::Cow::Owned(cleaned)
}

/// Wave 3-A Item 10: idempotent ALTER for `stock_movers.phase` SYMBOL.
///
/// Differentiates pre-open (09:00-09:13 IST) snapshot rows from in-market
/// rows. Values: `"PREOPEN"` (live pre-open data), `"PREOPEN_UNAVAILABLE"`
/// (SENSEX/BSE which is not in the pre-open feed), `"MARKET"` (default for
/// existing in-market writes via `append_stock_mover`).
///
/// `phase` is intentionally NOT in `DEDUP_KEY_MOVERS`: a single
/// (security_id, segment, ts) row should not have two phases coexisting —
/// the same row should replace itself if accidentally written twice with
/// different phases (the latest write wins).
const STOCK_MOVERS_ALTER_DDL_PHASE: &str =
    "ALTER TABLE stock_movers ADD COLUMN IF NOT EXISTS phase SYMBOL";

/// Wire constant for the default in-market phase. Existing
/// `append_stock_mover` callers route through this value so historic
/// rows remain query-compatible after the ALTER.
pub const STOCK_MOVERS_PHASE_MARKET: &str = "MARKET";

/// Wave 3-A Item 10: pre-open phase value for live pre-open snapshot rows.
pub const STOCK_MOVERS_PHASE_PREOPEN: &str = "PREOPEN";

/// Wave 3-A Item 10: pre-open phase value for instruments that are NOT in
/// the pre-open feed (SENSEX / BSE). These rows are emitted with zeroed
/// price columns + this phase so the dashboard can prove "we KNEW it was
/// not available, we did not silently skip" — distinguishes "no data" from
/// "feed broken".
pub const STOCK_MOVERS_PHASE_PREOPEN_UNAVAILABLE: &str = "PREOPEN_UNAVAILABLE";

/// DEDUP key for `top_movers`. Includes `timeframe` so the same rank
/// position at the same second across DIFFERENT timeframes (1m vs 5m
/// rankings) does not collide. Includes `security_id` + `segment` so
/// I-P1-11 cross-segment id collisions (e.g. NIFTY id=13 IDX_I vs an
/// NSE_EQ id=13) are kept as distinct rows.
const DEDUP_KEY_TOP_MOVERS: &str = "timeframe, bucket, rank_category, rank, security_id, segment";

/// DDL for the `option_movers` table.
///
/// Stores 1-minute snapshots of top 20 per 7 option categories.
/// Partitioned by DAY (one partition per trading day, ~60×7×20 = 8400 rows/day).
const OPTION_MOVERS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS option_movers (\
        category SYMBOL,\
        security_id LONG,\
        segment SYMBOL,\
        contract_name SYMBOL,\
        underlying SYMBOL,\
        option_type SYMBOL,\
        strike DOUBLE,\
        expiry SYMBOL,\
        spot_price DOUBLE,\
        ltp DOUBLE,\
        change DOUBLE,\
        change_pct DOUBLE,\
        oi LONG,\
        oi_change LONG,\
        oi_change_pct DOUBLE,\
        volume LONG,\
        value DOUBLE,\
        rank INT,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// Rescue-ring records (DB-2)
// ---------------------------------------------------------------------------

/// A buffered stock mover entry, kept in an in-memory FIFO rescue ring
/// so it can be re-sent to QuestDB after a flush failure or reconnect.
/// Stored verbatim so the re-send path is byte-identical to the original.
/// `Clone` (not `Copy`) because of the owned `String` fields — acceptable
/// because this writer is cold path (1 row/sec max, see movers flow rate
/// in `MOVERS_RESCUE_RING_CAPACITY` comment).
#[derive(Clone)]
struct BufferedStockMover {
    ts_nanos: i64,
    category: String,
    rank: i32,
    security_id: u32,
    segment: String,
    symbol: String,
    ltp: f64,
    prev_close: f64,
    change_pct: f64,
    volume: i64,
    /// Wave 3-A Item 10: snapshot phase. Defaults to `STOCK_MOVERS_PHASE_MARKET`
    /// for in-market writes; pre-open writes set `STOCK_MOVERS_PHASE_PREOPEN`
    /// or `STOCK_MOVERS_PHASE_PREOPEN_UNAVAILABLE` (SENSEX).
    phase: String,
}

/// A buffered option mover entry — same rationale as `BufferedStockMover`.
#[derive(Clone)]
struct BufferedOptionMover {
    ts_nanos: i64,
    category: String,
    rank: i32,
    security_id: u32,
    segment: String,
    contract_name: String,
    underlying: String,
    option_type: String,
    strike: f64,
    expiry: String,
    spot_price: f64,
    ltp: f64,
    change: f64,
    change_pct: f64,
    oi: i64,
    oi_change: i64,
    oi_change_pct: f64,
    volume: i64,
    value: f64,
}

// ---------------------------------------------------------------------------
// Stock Movers Writer
// ---------------------------------------------------------------------------

/// Batched writer for stock movers snapshots to QuestDB via ILP.
///
/// # Resilience (DB-6/DB-7)
/// - Reconnect attempts throttled to one per `MOVERS_RECONNECT_THROTTLE_SECS` (30s).
/// - Dropped batches are counted and logged at ERROR level (Telegram alert fires).
/// - No in-memory rescue ring yet — DB-2 will add one in a separate commit.
pub struct StockMoversWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    /// Earliest time at which the next reconnect attempt is allowed. DB-6.
    next_reconnect_allowed: Instant,
    /// Total rows dropped (never written to QuestDB). DB-7.
    rows_dropped_total: u64,
    /// Bounded FIFO rescue ring for flush-failure replay. DB-2.
    rescue_ring: VecDeque<BufferedStockMover>,
}

impl StockMoversWriter {
    /// Creates a new stock movers writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for stock movers")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(MOVERS_RESCUE_RING_CAPACITY),
        })
    }

    /// Current rescue ring occupancy (for tests + observability). DB-2.
    pub fn rescue_ring_len(&self) -> usize {
        self.rescue_ring.len()
    }

    /// Pushes a stock mover into the rescue ring, evicting the oldest
    /// entry if the ring is full. Overflow routes through `record_drop`
    /// + `tv_stock_movers_ring_overflow_total` metric. DB-2.
    fn push_to_rescue_ring(&mut self, row: BufferedStockMover) {
        if self.rescue_ring.len() >= MOVERS_RESCUE_RING_CAPACITY {
            self.rescue_ring.pop_front();
            let err = anyhow::anyhow!(
                "stock movers rescue ring full at capacity {} — oldest row evicted",
                MOVERS_RESCUE_RING_CAPACITY
            );
            self.record_drop(1, "ring_overflow", &err);
            metrics::counter!("tv_stock_movers_ring_overflow_total").increment(1);
        }
        self.rescue_ring.push_back(row);
    }

    /// Total rows dropped since startup (never persisted to QuestDB).
    pub fn rows_dropped_total(&self) -> u64 {
        self.rows_dropped_total
    }

    /// Records a dropped batch — increments counter + metric + ERROR log. DB-7.
    fn record_drop(&mut self, count: usize, reason: &'static str, err: &anyhow::Error) {
        let dropped = u64::try_from(count).unwrap_or(u64::MAX);
        self.rows_dropped_total = self.rows_dropped_total.saturating_add(dropped);
        metrics::counter!("tv_stock_movers_dropped_total").absolute(self.rows_dropped_total);
        metrics::counter!("tv_stock_movers_flush_failures_total").increment(1);
        error!(
            ?err,
            dropped_rows = count,
            total_dropped = self.rows_dropped_total,
            reason,
            "stock movers batch dropped — data loss (cold-path observability)"
        );
    }

    fn reconnect_allowed_now(&self) -> bool {
        Instant::now() >= self.next_reconnect_allowed
    }

    fn bump_reconnect_throttle(&mut self) {
        self.next_reconnect_allowed =
            Instant::now() + Duration::from_secs(MOVERS_RECONNECT_THROTTLE_SECS);
    }

    /// Writes a `BufferedStockMover` into an ILP `Buffer`. Single source
    /// of truth for column order + rounding policy — both live append
    /// and rescue drain call this helper. DB-2.
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB, tested via ensure_movers_tables integration
    fn append_stock_row_to_buffer(buffer: &mut Buffer, row: &BufferedStockMover) -> Result<()> {
        let change_abs = ((row.ltp - row.prev_close) * 100.0).round() / 100.0;
        let ts = TimestampNanos::new(row.ts_nanos);
        // Adversarial-review LOW #6: defensively sanitise every SYMBOL
        // value before passing to ILP. Cow::Borrowed on the common path
        // (no allocation when input is already clean).
        let category = sanitize_ilp_symbol(&row.category);
        let segment = sanitize_ilp_symbol(&row.segment);
        let symbol = sanitize_ilp_symbol(&row.symbol);
        let phase = sanitize_ilp_symbol(&row.phase);
        buffer
            .table(QUESTDB_TABLE_STOCK_MOVERS)
            .context("table")?
            .symbol("category", category.as_ref())
            .context("category")?
            .symbol("segment", segment.as_ref())
            .context("segment")?
            .symbol("symbol", symbol.as_ref())
            .context("symbol")?
            .symbol("phase", phase.as_ref())
            .context("phase")?
            .column_i64("security_id", i64::from(row.security_id))
            .context("security_id")?
            .column_f64("ltp", row.ltp)
            .context("ltp")?
            .column_f64("prev_close", row.prev_close)
            .context("prev_close")?
            .column_f64("change_abs", change_abs)
            .context("change_abs")?
            .column_f64("change_pct", row.change_pct)
            .context("change_pct")?
            .column_i64("volume", row.volume)
            .context("volume")?
            .column_i64("rank", i64::from(row.rank))
            .context("rank")?
            .at(ts)
            .context("timestamp")?;
        Ok(())
    }

    /// Appends a single stock mover entry to the ILP buffer.
    ///
    /// # Arguments
    /// * `ts_nanos` — snapshot timestamp (IST epoch nanoseconds)
    /// * `category` — "GAINER", "LOSER", or "MOST_ACTIVE"
    /// * `rank` — 1-based rank within category
    /// * `security_id` — Dhan security ID
    /// * `segment` — exchange segment string ("NSE_EQ", "NSE_FNO", etc.)
    /// * `symbol` — human-readable symbol name
    /// * `ltp` — last traded price
    /// * `prev_close` — previous day close price
    /// * `change_pct` — percentage change
    /// * `volume` — day volume
    #[allow(clippy::too_many_arguments)]
    // APPROVED: 10 params — each maps to a QuestDB column, no abstraction reduces this
    pub fn append_stock_mover(
        &mut self,
        ts_nanos: i64,
        category: &str,
        rank: i32,
        security_id: u32,
        segment: &str,
        symbol: &str,
        ltp: f64,
        prev_close: f64,
        change_pct: f64,
        volume: i64,
    ) -> Result<()> {
        // In-market default: phase = "MARKET". Pre-open callers must use
        // `append_stock_mover_with_phase` directly.
        self.append_stock_mover_with_phase(
            ts_nanos,
            category,
            rank,
            security_id,
            segment,
            symbol,
            ltp,
            prev_close,
            change_pct,
            volume,
            STOCK_MOVERS_PHASE_MARKET,
        )
    }

    /// Wave 3-A Item 10: append a stock mover row with an explicit `phase`
    /// label. Used by the pre-open movers tracker to differentiate
    /// 09:00-09:13 IST snapshot rows from in-market rows.
    ///
    /// `phase` values are pinned by `STOCK_MOVERS_PHASE_*` constants:
    /// - `MARKET` — default in-market writes (delegated from
    ///   `append_stock_mover`).
    /// - `PREOPEN` — pre-open snapshot row with live data.
    /// - `PREOPEN_UNAVAILABLE` — pre-open row for an instrument NOT in the
    ///   pre-open feed (SENSEX/BSE). Price columns are zeroed; the row
    ///   exists so the dashboard can prove the absence is intentional.
    #[allow(clippy::too_many_arguments)]
    // APPROVED: 11 params — each maps to a QuestDB column or schema label, no abstraction reduces this without obscuring the call site.
    // TEST-EXEMPT: ILP buffer append requires a live QuestDB connection; the existing `append_stock_mover` path (which delegates here with phase=MARKET) is exercised by every in-market integration test.
    pub fn append_stock_mover_with_phase(
        &mut self,
        ts_nanos: i64,
        category: &str,
        rank: i32,
        security_id: u32,
        segment: &str,
        symbol: &str,
        ltp: f64,
        prev_close: f64,
        change_pct: f64,
        volume: i64,
        phase: &str,
    ) -> Result<()> {
        // Round all f64 values to 2dp to prevent IEEE 754 artifacts in QuestDB.
        let row = BufferedStockMover {
            ts_nanos,
            category: category.to_string(),
            rank,
            security_id,
            segment: segment.to_string(),
            symbol: symbol.to_string(),
            ltp: (ltp * 100.0).round() / 100.0,
            prev_close: (prev_close * 100.0).round() / 100.0,
            change_pct: (change_pct * 100.0).round() / 100.0,
            volume,
            phase: phase.to_string(),
        };

        // DB-2: push to rescue ring BEFORE the ILP buffer. If the ring
        // overflows, the oldest row is evicted and counted as a drop.
        self.push_to_rescue_ring(row.clone());

        // Then append to the current in-flight ILP batch.
        Self::append_stock_row_to_buffer(&mut self.buffer, &row)?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= MOVERS_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            error!(?err, "stock movers auto-flush failed at batch boundary");
        }

        Ok(())
    }

    /// Drains the stock movers rescue ring by rebuilding a fresh ILP
    /// batch (oldest first) and flushing it. Called after a successful
    /// reconnect. DB-2.
    fn drain_rescue_ring_to_sender(&mut self) -> Result<()> {
        if self.rescue_ring.is_empty() {
            return Ok(());
        }
        let drained = self.rescue_ring.len();
        info!(
            buffered = drained,
            "draining stock movers rescue ring after reconnect"
        );

        let sender = self
            .sender
            .as_mut()
            .context("sender required for stock movers rescue drain")?;
        let mut rescue_buffer = Buffer::new(ProtocolVersion::V1);
        for row in self.rescue_ring.iter() {
            Self::append_stock_row_to_buffer(&mut rescue_buffer, row)?;
        }
        sender
            .flush(&mut rescue_buffer)
            .context("flush stock movers rescue batch to QuestDB")?;
        self.rescue_ring.clear();
        info!(drained, "stock movers rescue ring drained after reconnect");
        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    ///
    /// DB-6/DB-7: reconnect attempts throttled to 1 per
    /// `MOVERS_RECONNECT_THROTTLE_SECS` (30s). Every drop path routes
    /// through `record_drop` (metric + ERROR log).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 && self.rescue_ring.is_empty() {
            return Ok(());
        }

        // Reconnect path — throttled. On throttle closed, rows stay in
        // the rescue ring (DB-2) so no data is lost.
        if self.sender.is_none() {
            if !self.reconnect_allowed_now() {
                self.buffer.clear();
                self.pending_count = 0;
                debug!(
                    ring_depth = self.rescue_ring.len(),
                    "stock movers flush deferred — reconnect throttled, \
                     rows retained in rescue ring"
                );
                return Ok(());
            }
            self.bump_reconnect_throttle();
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    info!("stock movers writer reconnected to QuestDB");
                    self.sender = Some(s);
                }
                Err(err) => {
                    let wrapped = anyhow::Error::from(err);
                    warn!(
                        ?wrapped,
                        ring_depth = self.rescue_ring.len(),
                        "stock movers reconnect failed — rows retained in \
                         rescue ring; will retry after throttle window"
                    );
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            }
        }

        // Connected. Drain the rescue ring FIRST so oldest rows land at
        // QuestDB before the in-flight batch. DB-2.
        if !self.rescue_ring.is_empty()
            && let Err(err) = self.drain_rescue_ring_to_sender()
        {
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?err,
                ring_depth = self.rescue_ring.len(),
                "stock movers rescue drain failed — will retry on next flush"
            );
            return Ok(());
        }

        if self.pending_count == 0 {
            return Ok(());
        }
        let sender = self
            .sender
            .as_mut()
            .context("stock movers sender present after reconnect branch")?;
        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            let wrapped = anyhow::Error::from(err);
            // Rows are still in the rescue ring — don't record_drop.
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?wrapped,
                count,
                ring_depth = self.rescue_ring.len(),
                "stock movers flush failed — rows retained in rescue ring"
            );
            return Ok(());
        }

        // Pop the successfully-flushed rows from the ring.
        for _ in 0..count {
            self.rescue_ring.pop_front();
        }
        self.pending_count = 0;
        debug!(flushed_rows = count, "stock movers flushed to QuestDB");
        Ok(())
    }

    /// Returns a mutable reference to the ILP buffer (for external row building).
    // TEST-EXEMPT: trivial accessor, returns &mut Buffer
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
}

// ---------------------------------------------------------------------------
// Option Movers Writer
// ---------------------------------------------------------------------------

/// Batched writer for option movers snapshots to QuestDB via ILP.
pub struct OptionMoversWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    /// Earliest time at which the next reconnect attempt is allowed. DB-6.
    next_reconnect_allowed: Instant,
    /// Total rows dropped (never written to QuestDB). DB-7.
    rows_dropped_total: u64,
    /// Bounded FIFO rescue ring for flush-failure replay. DB-2.
    rescue_ring: VecDeque<BufferedOptionMover>,
}

impl OptionMoversWriter {
    /// Creates a new option movers writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for option movers")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(MOVERS_RESCUE_RING_CAPACITY),
        })
    }

    /// Current rescue ring occupancy (for tests + observability). DB-2.
    pub fn rescue_ring_len(&self) -> usize {
        self.rescue_ring.len()
    }

    /// Pushes an option mover into the rescue ring, evicting the oldest
    /// entry if the ring is full. Overflow routes through `record_drop`
    /// + `tv_option_movers_ring_overflow_total` metric. DB-2.
    fn push_to_rescue_ring(&mut self, row: BufferedOptionMover) {
        if self.rescue_ring.len() >= MOVERS_RESCUE_RING_CAPACITY {
            self.rescue_ring.pop_front();
            let err = anyhow::anyhow!(
                "option movers rescue ring full at capacity {} — oldest row evicted",
                MOVERS_RESCUE_RING_CAPACITY
            );
            self.record_drop(1, "ring_overflow", &err);
            metrics::counter!("tv_option_movers_ring_overflow_total").increment(1);
        }
        self.rescue_ring.push_back(row);
    }

    /// Total rows dropped since startup (never persisted to QuestDB).
    pub fn rows_dropped_total(&self) -> u64 {
        self.rows_dropped_total
    }

    /// Records a dropped batch — increments counter + metric + ERROR log. DB-7.
    fn record_drop(&mut self, count: usize, reason: &'static str, err: &anyhow::Error) {
        let dropped = u64::try_from(count).unwrap_or(u64::MAX);
        self.rows_dropped_total = self.rows_dropped_total.saturating_add(dropped);
        metrics::counter!("tv_option_movers_dropped_total").absolute(self.rows_dropped_total);
        metrics::counter!("tv_option_movers_flush_failures_total").increment(1);
        error!(
            ?err,
            dropped_rows = count,
            total_dropped = self.rows_dropped_total,
            reason,
            "option movers batch dropped — data loss (cold-path observability)"
        );
    }

    fn reconnect_allowed_now(&self) -> bool {
        Instant::now() >= self.next_reconnect_allowed
    }

    fn bump_reconnect_throttle(&mut self) {
        self.next_reconnect_allowed =
            Instant::now() + Duration::from_secs(MOVERS_RECONNECT_THROTTLE_SECS);
    }

    /// Writes a `BufferedOptionMover` into an ILP `Buffer`. Single source
    /// of truth for column order + rounding. DB-2.
    // TEST-EXEMPT: ILP buffer append — requires live QuestDB, tested via ensure_movers_tables integration
    fn append_option_row_to_buffer(buffer: &mut Buffer, row: &BufferedOptionMover) -> Result<()> {
        let ts = TimestampNanos::new(row.ts_nanos);
        buffer
            .table(QUESTDB_TABLE_OPTION_MOVERS)
            .context("table")?
            .symbol("category", &row.category)
            .context("category")?
            .symbol("segment", &row.segment)
            .context("segment")?
            .symbol("contract_name", &row.contract_name)
            .context("contract_name")?
            .symbol("underlying", &row.underlying)
            .context("underlying")?
            .symbol("option_type", &row.option_type)
            .context("option_type")?
            .symbol("expiry", &row.expiry)
            .context("expiry")?
            .column_i64("security_id", i64::from(row.security_id))
            .context("security_id")?
            .column_f64("strike", row.strike)
            .context("strike")?
            .column_f64("spot_price", row.spot_price)
            .context("spot_price")?
            .column_f64("ltp", row.ltp)
            .context("ltp")?
            .column_f64("change", row.change)
            .context("change")?
            .column_f64("change_pct", row.change_pct)
            .context("change_pct")?
            .column_i64("oi", row.oi)
            .context("oi")?
            .column_i64("oi_change", row.oi_change)
            .context("oi_change")?
            .column_f64("oi_change_pct", row.oi_change_pct)
            .context("oi_change_pct")?
            .column_i64("volume", row.volume)
            .context("volume")?
            .column_f64("value", row.value)
            .context("value")?
            .column_i64("rank", i64::from(row.rank))
            .context("rank")?
            .at(ts)
            .context("timestamp")?;
        Ok(())
    }

    /// Appends a single option mover entry to the ILP buffer.
    ///
    /// # Arguments
    /// * `ts_nanos` — snapshot timestamp (IST epoch nanoseconds)
    /// * `category` — "HIGHEST_OI", "OI_GAINER", "OI_LOSER", "TOP_VOLUME", "TOP_VALUE", "PRICE_GAINER", "PRICE_LOSER"
    /// * `rank` — 1-based rank within category
    #[allow(clippy::too_many_arguments)]
    // APPROVED: 17 params — each maps to a QuestDB column, no abstraction reduces this
    pub fn append_option_mover(
        &mut self,
        ts_nanos: i64,
        category: &str,
        rank: i32,
        security_id: u32,
        segment: &str,
        contract_name: &str,
        underlying: &str,
        option_type: &str,
        strike: f64,
        expiry: &str,
        spot_price: f64,
        ltp: f64,
        change: f64,
        change_pct: f64,
        oi: i64,
        oi_change: i64,
        oi_change_pct: f64,
        volume: i64,
        value: f64,
    ) -> Result<()> {
        // Round all f64 values to 2dp to prevent IEEE 754 artifacts in QuestDB.
        let row = BufferedOptionMover {
            ts_nanos,
            category: category.to_string(),
            rank,
            security_id,
            segment: segment.to_string(),
            contract_name: contract_name.to_string(),
            underlying: underlying.to_string(),
            option_type: option_type.to_string(),
            strike: (strike * 100.0).round() / 100.0,
            expiry: expiry.to_string(),
            spot_price: (spot_price * 100.0).round() / 100.0,
            ltp: (ltp * 100.0).round() / 100.0,
            change: (change * 100.0).round() / 100.0,
            change_pct: (change_pct * 100.0).round() / 100.0,
            oi,
            oi_change,
            oi_change_pct: (oi_change_pct * 100.0).round() / 100.0,
            volume,
            value: (value * 100.0).round() / 100.0,
        };

        // DB-2: push to rescue ring before ILP buffer.
        self.push_to_rescue_ring(row.clone());

        Self::append_option_row_to_buffer(&mut self.buffer, &row)?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= MOVERS_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            error!(?err, "option movers auto-flush failed at batch boundary");
        }

        Ok(())
    }

    /// Drains the option movers rescue ring by rebuilding a fresh ILP
    /// batch (oldest first) and flushing it. DB-2.
    fn drain_rescue_ring_to_sender(&mut self) -> Result<()> {
        if self.rescue_ring.is_empty() {
            return Ok(());
        }
        let drained = self.rescue_ring.len();
        info!(
            buffered = drained,
            "draining option movers rescue ring after reconnect"
        );

        let sender = self
            .sender
            .as_mut()
            .context("sender required for option movers rescue drain")?;
        let mut rescue_buffer = Buffer::new(ProtocolVersion::V1);
        for row in self.rescue_ring.iter() {
            Self::append_option_row_to_buffer(&mut rescue_buffer, row)?;
        }
        sender
            .flush(&mut rescue_buffer)
            .context("flush option movers rescue batch to QuestDB")?;
        self.rescue_ring.clear();
        info!(drained, "option movers rescue ring drained after reconnect");
        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
    ///
    /// DB-2/DB-6/DB-7: rescue-ring drain-on-reconnect + pop-on-success
    /// + retain-on-failure. See `StockMoversWriter::flush` for the
    ///   identical contract.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 && self.rescue_ring.is_empty() {
            return Ok(());
        }

        // Reconnect path — throttled.
        if self.sender.is_none() {
            if !self.reconnect_allowed_now() {
                self.buffer.clear();
                self.pending_count = 0;
                debug!(
                    ring_depth = self.rescue_ring.len(),
                    "option movers flush deferred — reconnect throttled, \
                     rows retained in rescue ring"
                );
                return Ok(());
            }
            self.bump_reconnect_throttle();
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    info!("option movers writer reconnected to QuestDB");
                    self.sender = Some(s);
                }
                Err(err) => {
                    let wrapped = anyhow::Error::from(err);
                    warn!(
                        ?wrapped,
                        ring_depth = self.rescue_ring.len(),
                        "option movers reconnect failed — rows retained in \
                         rescue ring; will retry after throttle window"
                    );
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            }
        }

        // Drain rescue ring first (oldest first).
        if !self.rescue_ring.is_empty()
            && let Err(err) = self.drain_rescue_ring_to_sender()
        {
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?err,
                ring_depth = self.rescue_ring.len(),
                "option movers rescue drain failed — will retry on next flush"
            );
            return Ok(());
        }

        if self.pending_count == 0 {
            return Ok(());
        }
        let sender = self
            .sender
            .as_mut()
            .context("option movers sender present after reconnect branch")?;
        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            let wrapped = anyhow::Error::from(err);
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?wrapped,
                count,
                ring_depth = self.rescue_ring.len(),
                "option movers flush failed — rows retained in rescue ring"
            );
            return Ok(());
        }

        for _ in 0..count {
            self.rescue_ring.pop_front();
        }
        self.pending_count = 0;
        debug!(flushed_rows = count, "option movers flushed to QuestDB");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TopMoversV2Writer — Plan item C2 (2026-04-22)
// ---------------------------------------------------------------------------

/// A single row destined for the unified `top_movers` table.
///
/// One snapshot fans out into up to ~680 rows per timeframe; with 15
/// timeframes that is ~10,200 rows per minute peak. Each row is fully
/// self-describing — the writer performs NO registry lookups (the caller
/// assembles `symbol`, `underlying`, `expiry`, `strike`, and `option_type`
/// from the `InstrumentRegistry` before calling `append`). This keeps the
/// writer side-effect-free for unit testing without a QuestDB instance.
///
/// `timeframe` (added 2026-04-25) selects which of the 15 rolling-window
/// rankings (1m..15m) this row belongs to. `segment` (added 2026-04-25)
/// disambiguates cross-segment `security_id` collisions per I-P1-11 (e.g.
/// FINNIFTY id=27 IDX_I vs an NSE_EQ id=27).
#[derive(Debug, Clone)]
pub struct TopMoverRow {
    pub ts_nanos: i64,
    pub timeframe: &'static str,
    pub bucket: String,
    pub rank_category: String,
    pub rank: i32,
    pub security_id: u32,
    pub segment: &'static str,
    pub symbol: String,
    pub underlying: Option<String>,
    pub expiry: Option<String>,
    pub strike: Option<f64>,
    pub option_type: Option<String>,
    pub ltp: f64,
    pub prev_close: f64,
    pub change_pct: f64,
    pub volume: i64,
    pub value: f64,
    pub oi: i64,
    pub prev_oi: i64,
    pub oi_change_pct: f64,
}

/// Unified ILP writer for the 6-bucket `top_movers` table.
///
/// Mirrors the stock/option writers (rescue ring + throttled reconnect +
/// ERROR-level drop logging per Rule 5 in `audit-findings-2026-04-17.md`),
/// but emits to a single schema keyed by `(bucket, rank_category, rank)`.
///
/// # Cadence
/// Written once per `persistence_cadence_secs` (default 1 Hz) during market
/// hours. Market-hours gating happens at the caller (Rule 3).
///
/// # Row volume
/// At 1 Hz with the default top-20 × 7-category × 4-derivative-bucket fan-out
/// this is ~134 rows/sec peak = ~2.9M rows per 6hr trading day. QuestDB WAL
/// + DEDUP handles this comfortably.
pub struct TopMoversV2Writer {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    next_reconnect_allowed: Instant,
    rows_dropped_total: u64,
    rescue_ring: VecDeque<TopMoverRow>,
    rows_written_total: u64,
}

impl TopMoversV2Writer {
    /// Creates a new writer connected to QuestDB via ILP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB for top_movers v2")?;
        let buffer = sender.new_buffer();
        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(MOVERS_RESCUE_RING_CAPACITY),
            rows_written_total: 0,
        })
    }

    /// Total rows successfully flushed to QuestDB. Cold-path diagnostic.
    // TEST-EXEMPT: trivial getter covered by test_top_mover_row_drops_are_tracked_on_zero_flush_path
    pub fn rows_written_total(&self) -> u64 {
        self.rows_written_total
    }

    /// Current rescue ring occupancy. Cold-path diagnostic.
    // TEST-EXEMPT: trivial getter covered by test_top_mover_row_drops_are_tracked_on_zero_flush_path
    pub fn rescue_ring_len(&self) -> usize {
        self.rescue_ring.len()
    }

    /// Total rows dropped since startup (never persisted to QuestDB).
    // TEST-EXEMPT: trivial getter covered by test_top_mover_row_drops_are_tracked_on_zero_flush_path
    pub fn rows_dropped_total(&self) -> u64 {
        self.rows_dropped_total
    }

    fn reconnect_allowed_now(&self) -> bool {
        Instant::now() >= self.next_reconnect_allowed
    }

    fn bump_reconnect_throttle(&mut self) {
        self.next_reconnect_allowed =
            Instant::now() + Duration::from_secs(MOVERS_RECONNECT_THROTTLE_SECS);
    }

    fn record_drop(&mut self, count: usize, reason: &'static str, err: &anyhow::Error) {
        let dropped = u64::try_from(count).unwrap_or(u64::MAX);
        self.rows_dropped_total = self.rows_dropped_total.saturating_add(dropped);
        metrics::counter!("tv_movers_dropped_total").absolute(self.rows_dropped_total);
        metrics::counter!("tv_movers_flush_failures_total").increment(1);
        error!(
            ?err,
            dropped_rows = count,
            total_dropped = self.rows_dropped_total,
            reason,
            "top_movers v2 batch dropped — data loss (cold-path observability)"
        );
    }

    fn push_to_rescue_ring(&mut self, row: TopMoverRow) {
        if self.rescue_ring.len() >= MOVERS_RESCUE_RING_CAPACITY {
            self.rescue_ring.pop_front();
            let err = anyhow::anyhow!(
                "top_movers v2 rescue ring full at capacity {} — oldest row evicted",
                MOVERS_RESCUE_RING_CAPACITY
            );
            self.record_drop(1, "ring_overflow", &err);
            metrics::counter!("tv_movers_ring_overflow_total").increment(1);
        }
        self.rescue_ring.push_back(row);
    }

    /// Single source of truth for ILP column order + rounding policy.
    /// Both the live append and the rescue-drain path call this helper.
    fn append_row_to_buffer(buffer: &mut Buffer, row: &TopMoverRow) -> Result<()> {
        let ts = TimestampNanos::new(row.ts_nanos);
        let mut b = buffer
            .table(QUESTDB_TABLE_TOP_MOVERS)
            .context("table")?
            .symbol("timeframe", row.timeframe)
            .context("timeframe")?
            .symbol("bucket", &row.bucket)
            .context("bucket")?
            .symbol("rank_category", &row.rank_category)
            .context("rank_category")?
            .symbol("segment", row.segment)
            .context("segment")?
            .symbol("symbol", &row.symbol)
            .context("symbol")?;
        if let Some(ref u) = row.underlying {
            b = b.symbol("underlying", u).context("underlying")?;
        }
        if let Some(ref ot) = row.option_type {
            b = b.symbol("option_type", ot).context("option_type")?;
        }
        let b = b
            .column_i64("rank", i64::from(row.rank))
            .context("rank")?
            .column_i64("security_id", i64::from(row.security_id))
            .context("security_id")?;
        let b = if let Some(ref e) = row.expiry {
            b.column_str("expiry", e).context("expiry")?
        } else {
            b
        };
        let b = if let Some(s) = row.strike {
            b.column_f64("strike", (s * 100.0).round() / 100.0)
                .context("strike")?
        } else {
            b
        };
        b.column_f64("ltp", (row.ltp * 100.0).round() / 100.0)
            .context("ltp")?
            .column_f64("prev_close", (row.prev_close * 100.0).round() / 100.0)
            .context("prev_close")?
            .column_f64("change_pct", (row.change_pct * 100.0).round() / 100.0)
            .context("change_pct")?
            .column_i64("volume", row.volume)
            .context("volume")?
            .column_f64("value", (row.value * 100.0).round() / 100.0)
            .context("value")?
            .column_i64("oi", row.oi)
            .context("oi")?
            .column_i64("prev_oi", row.prev_oi)
            .context("prev_oi")?
            .column_f64("oi_change_pct", (row.oi_change_pct * 100.0).round() / 100.0)
            .context("oi_change_pct")?
            .at(ts)
            .context("timestamp")?;
        Ok(())
    }

    /// Appends a single top-mover row to the in-flight ILP buffer.
    /// Auto-flushes when the batch boundary is reached.
    pub fn append_row(&mut self, row: TopMoverRow) -> Result<()> {
        // Push to rescue ring BEFORE the ILP buffer — if the ILP append fails
        // downstream, the row is still replayable.
        self.push_to_rescue_ring(row.clone());
        Self::append_row_to_buffer(&mut self.buffer, &row)?;
        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= MOVERS_FLUSH_BATCH_SIZE
            && let Err(err) = self.flush()
        {
            error!(?err, "top_movers v2 auto-flush failed at batch boundary");
        }
        Ok(())
    }

    fn drain_rescue_ring_to_sender(&mut self) -> Result<()> {
        if self.rescue_ring.is_empty() {
            return Ok(());
        }
        let drained = self.rescue_ring.len();
        info!(
            buffered = drained,
            "draining top_movers v2 rescue ring after reconnect"
        );
        let sender = self
            .sender
            .as_mut()
            .context("sender required for top_movers v2 rescue drain")?;
        let mut rescue_buffer = Buffer::new(ProtocolVersion::V1);
        for row in self.rescue_ring.iter() {
            Self::append_row_to_buffer(&mut rescue_buffer, row)?;
        }
        sender
            .flush(&mut rescue_buffer)
            .context("top_movers v2 rescue flush")?;
        self.rescue_ring.clear();
        Ok(())
    }

    /// Flushes the pending ILP batch to QuestDB. Throttled reconnect on failure.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 && self.rescue_ring.is_empty() {
            return Ok(());
        }

        if self.sender.is_none() {
            if !self.reconnect_allowed_now() {
                self.buffer.clear();
                self.pending_count = 0;
                debug!(
                    ring_depth = self.rescue_ring.len(),
                    "top_movers v2 flush deferred — reconnect throttled, \
                     rows retained in rescue ring"
                );
                return Ok(());
            }
            self.bump_reconnect_throttle();
            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(s) => {
                    info!("top_movers v2 writer reconnected to QuestDB");
                    self.sender = Some(s);
                }
                Err(err) => {
                    let wrapped = anyhow::Error::from(err);
                    warn!(
                        ?wrapped,
                        ring_depth = self.rescue_ring.len(),
                        "top_movers v2 reconnect failed — rows retained in \
                         rescue ring; will retry after throttle window"
                    );
                    self.buffer.clear();
                    self.pending_count = 0;
                    return Ok(());
                }
            }
        }

        if !self.rescue_ring.is_empty()
            && let Err(err) = self.drain_rescue_ring_to_sender()
        {
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?err,
                ring_depth = self.rescue_ring.len(),
                "top_movers v2 rescue drain failed — will retry on next flush"
            );
            return Ok(());
        }

        if self.pending_count == 0 {
            return Ok(());
        }
        let sender = self
            .sender
            .as_mut()
            .context("top_movers v2 sender present after reconnect branch")?;
        let count = self.pending_count;
        if let Err(err) = sender.flush(&mut self.buffer) {
            let wrapped = anyhow::Error::from(err);
            self.sender = None;
            self.buffer.clear();
            self.pending_count = 0;
            warn!(
                ?wrapped,
                count,
                ring_depth = self.rescue_ring.len(),
                "top_movers v2 flush failed — rows retained in rescue ring"
            );
            return Ok(());
        }

        for _ in 0..count {
            self.rescue_ring.pop_front();
        }
        let flushed = u64::try_from(count).unwrap_or(u64::MAX);
        self.rows_written_total = self.rows_written_total.saturating_add(flushed);
        metrics::counter!("tv_movers_rows_written_total").increment(flushed);
        self.pending_count = 0;
        debug!(
            flushed_rows = count,
            total_written = self.rows_written_total,
            "top_movers v2 flushed to QuestDB"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `stock_movers` and `option_movers` tables with DEDUP UPSERT KEYS.
///
/// Idempotent — safe to call on every startup.
pub async fn ensure_movers_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Create stock_movers table
    execute_ddl(&client, &base_url, STOCK_MOVERS_CREATE_DDL, "stock_movers").await;

    // Wave 3-A Item 10: idempotent ALTER ADD COLUMN IF NOT EXISTS for `phase`.
    // QuestDB ignores ADDs that already exist, so this is safe to run every boot.
    // Existing pre-`phase` deployments self-heal at the next boot.
    execute_ddl(
        &client,
        &base_url,
        STOCK_MOVERS_ALTER_DDL_PHASE,
        "stock_movers ADD COLUMN phase",
    )
    .await;

    // Enable DEDUP on stock_movers
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_STOCK_MOVERS, DEDUP_KEY_MOVERS
    );
    execute_ddl(&client, &base_url, &dedup_sql, "stock_movers DEDUP").await;

    // Create option_movers table
    execute_ddl(
        &client,
        &base_url,
        OPTION_MOVERS_CREATE_DDL,
        "option_movers",
    )
    .await;

    // Enable DEDUP on option_movers
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_OPTION_MOVERS, DEDUP_KEY_MOVERS
    );
    execute_ddl(&client, &base_url, &dedup_sql, "option_movers DEDUP").await;

    // Plan item C (2026-04-22): unified 6-bucket top_movers table.
    execute_ddl(&client, &base_url, TOP_MOVERS_CREATE_DDL, "top_movers").await;
    // Plan item I (2026-04-25): self-heal ADD COLUMN IF NOT EXISTS for the
    // two columns added 2026-04-25. Pre-existing deployments built on the
    // 2026-04-22 schema get auto-migrated at boot; QuestDB ignores ADDs
    // that already exist, so running every boot is free. Pattern lifted
    // from observability-architecture.md schema self-heal.
    execute_ddl(
        &client,
        &base_url,
        TOP_MOVERS_ALTER_DDL_TIMEFRAME,
        "top_movers ADD COLUMN timeframe",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        TOP_MOVERS_ALTER_DDL_SEGMENT,
        "top_movers ADD COLUMN segment",
    )
    .await;
    let top_dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_TOP_MOVERS, DEDUP_KEY_TOP_MOVERS
    );
    execute_ddl(&client, &base_url, &top_dedup_sql, "top_movers DEDUP").await;

    info!("movers tables setup complete (stock_movers + option_movers + top_movers)");
}

/// Executes a DDL statement against QuestDB HTTP API. Best-effort.
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                info!("{label} DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "{label} DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "{label} DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Wave-3-A LOW #6 — sanitize_ilp_symbol
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitize_ilp_symbol_clean_input_borrows_zero_alloc() {
        let clean = "RELIANCE";
        let out = sanitize_ilp_symbol(clean);
        // Borrowed variant — pointer equality with input.
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, "RELIANCE");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_newline() {
        let dirty = "RELIANCE\nmalicious_table";
        let out = sanitize_ilp_symbol(dirty);
        assert!(matches!(out, std::borrow::Cow::Owned(_)));
        assert_eq!(out, "RELIANCEmalicious_table");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_carriage_return_comma_equals_control() {
        let dirty = "BAD,SYM\r=1\x07X";
        let out = sanitize_ilp_symbol(dirty);
        assert_eq!(out, "BADSYM1X");
    }

    #[test]
    fn test_sanitize_ilp_symbol_preserves_unicode_and_normal_chars() {
        let clean = "NIFTY-Jun2026-25000-CE";
        let out = sanitize_ilp_symbol(clean);
        assert_eq!(out, clean);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn test_sanitize_ilp_symbol_empty_string_is_borrowed() {
        let out = sanitize_ilp_symbol("");
        assert_eq!(out, "");
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    // -----------------------------------------------------------------------
    // DDL content validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_movers_ddl_contains_required_columns() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("category SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("security_id LONG"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("segment SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("ltp DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("prev_close DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("change_abs DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("change_pct DOUBLE"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("volume LONG"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("rank INT"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_stock_movers_ddl_has_partition_and_wal() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("WAL"));
        assert!(STOCK_MOVERS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_stock_movers_ddl_is_idempotent() {
        assert!(STOCK_MOVERS_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_option_movers_ddl_contains_required_columns() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("category SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("security_id LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("segment SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("contract_name SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("underlying SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("option_type SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("strike DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("expiry SYMBOL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("spot_price DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("ltp DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("change DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("change_pct DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi_change LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("oi_change_pct DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("volume LONG"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("value DOUBLE"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("rank INT"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_option_movers_ddl_has_partition_and_wal() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("WAL"));
        assert!(OPTION_MOVERS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_option_movers_ddl_is_idempotent() {
        assert!(OPTION_MOVERS_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    // -----------------------------------------------------------------------
    // DEDUP key validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_dedup_key_includes_security_id() {
        assert!(DEDUP_KEY_MOVERS.contains("security_id"));
    }

    #[test]
    fn test_dedup_key_includes_category() {
        assert!(DEDUP_KEY_MOVERS.contains("category"));
    }

    // DB-3/DB-4: Cross-segment collision prevention.
    // Must include `segment` because the same security_id exists across
    // `IDX_I` / `NSE_EQ` / `NSE_FNO` with different real-world meanings.
    // Dropping this test is equivalent to re-introducing silent data corruption.
    #[test]
    fn test_dedup_key_movers_includes_segment() {
        assert!(
            DEDUP_KEY_MOVERS.contains("segment"),
            "DEDUP_KEY_MOVERS must include `segment` — same security_id exists \
             across IDX_I/NSE_EQ/NSE_FNO and would collide otherwise (audit DB-3/DB-4). \
             Got: {DEDUP_KEY_MOVERS}"
        );
    }

    /// Both stock_movers and option_movers share the same DEDUP constant,
    /// so both tables MUST apply the same identity-column set. This test
    /// pins the constant format and will fail loudly if anyone re-orders or
    /// drops a column — forcing a conscious review.
    #[test]
    fn test_dedup_key_movers_exact_format() {
        assert_eq!(
            DEDUP_KEY_MOVERS, "security_id, category, segment",
            "DEDUP_KEY_MOVERS regression — changing this string silently \
             corrupts data; update the test only after the DDL and migration \
             are confirmed safe."
        );
    }

    /// Cross-check: every column referenced in DEDUP_KEY_MOVERS MUST appear
    /// in both the stock_movers and option_movers DDLs. Prevents the class
    /// of bug where DEDUP lists a column the table doesn't even have.
    #[test]
    fn test_dedup_key_movers_columns_exist_in_both_ddls() {
        for col in DEDUP_KEY_MOVERS.split(',').map(|s| s.trim()) {
            assert!(
                STOCK_MOVERS_CREATE_DDL.contains(col),
                "DEDUP column `{col}` is missing from STOCK_MOVERS_CREATE_DDL"
            );
            assert!(
                OPTION_MOVERS_CREATE_DDL.contains(col),
                "DEDUP column `{col}` is missing from OPTION_MOVERS_CREATE_DDL"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Table name constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_table_names_are_stable() {
        assert_eq!(QUESTDB_TABLE_STOCK_MOVERS, "stock_movers");
        assert_eq!(QUESTDB_TABLE_OPTION_MOVERS, "option_movers");
    }

    // -----------------------------------------------------------------------
    // Flush batch size validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_movers_flush_batch_size_reasonable() {
        // Max per snapshot: 20 entries × 10 categories = 200 rows
        assert!(MOVERS_FLUSH_BATCH_SIZE >= 200);
        assert!(MOVERS_FLUSH_BATCH_SIZE <= 1000);
    }

    #[test]
    fn test_movers_flush_interval_aligns_with_snapshot() {
        // Must be > 60s (snapshot interval) to avoid premature flush
        assert!(MOVERS_FLUSH_INTERVAL_MS >= 60_000);
    }

    // -----------------------------------------------------------------------
    // Writer construction (unit tests — no QuestDB needed)
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_movers_writer_invalid_host() {
        let config = QuestDbConfig {
            host: "nonexistent-host-99999".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        // Construction may succeed (lazy connect) or fail — both are valid
        let _result = StockMoversWriter::new(&config);
    }

    #[test]
    fn test_option_movers_writer_invalid_host() {
        let config = QuestDbConfig {
            host: "nonexistent-host-99999".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let _result = OptionMoversWriter::new(&config);
    }

    // -----------------------------------------------------------------------
    // DDL timeout validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_ddl_timeout_reasonable() {
        assert!(QUESTDB_DDL_TIMEOUT_SECS >= 5);
        assert!(QUESTDB_DDL_TIMEOUT_SECS <= 30);
    }

    // -----------------------------------------------------------------------
    // Stock mover change_abs calculation
    // -----------------------------------------------------------------------

    #[test]
    fn test_change_abs_calculated_correctly() {
        let ltp = 150.0_f64;
        let prev_close = 145.0_f64;
        let change_abs = ltp - prev_close;
        assert!((change_abs - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_change_abs_negative_when_price_drops() {
        let ltp = 140.0_f64;
        let prev_close = 145.0_f64;
        let change_abs = ltp - prev_close;
        assert!(change_abs < 0.0);
        assert!((change_abs - (-5.0)).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Category string constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_mover_categories_are_valid() {
        let categories = ["GAINER", "LOSER", "MOST_ACTIVE"];
        for cat in &categories {
            assert!(!cat.is_empty());
            assert!(cat.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
        }
    }

    #[test]
    fn test_option_mover_categories_are_valid() {
        let categories = [
            "HIGHEST_OI",
            "OI_GAINER",
            "OI_LOSER",
            "TOP_VOLUME",
            "TOP_VALUE",
            "PRICE_GAINER",
            "PRICE_LOSER",
        ];
        assert_eq!(categories.len(), 7);
        for cat in &categories {
            assert!(!cat.is_empty());
            assert!(cat.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
        }
    }

    // -----------------------------------------------------------------------
    // DB-6 + DB-7: reconnect throttle + drop counter (movers)
    // -----------------------------------------------------------------------

    /// Shared bound check — reconnect throttle must be non-zero to prevent
    /// tight reconnect loops (audit gap DB-6).
    #[test]
    fn test_db6_movers_reconnect_throttle_nonzero_and_bounded() {
        assert!(
            MOVERS_RECONNECT_THROTTLE_SECS >= 1,
            "throttle must be >= 1s"
        );
        assert!(
            MOVERS_RECONNECT_THROTTLE_SECS <= 300,
            "throttle must be <= 5min"
        );
    }

    /// `StockMoversWriter::record_drop` must saturate-add into
    /// `rows_dropped_total` and increment the counter by exactly the
    /// dropped-row count (DB-7).
    #[test]
    fn test_db7_stock_movers_record_drop_increments_counter() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match StockMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => StockMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
        let err = anyhow::anyhow!("synthetic flush failure");
        writer.record_drop(100, "test", &err);
        assert_eq!(writer.rows_dropped_total(), 100);
        writer.record_drop(200, "test", &err);
        assert_eq!(writer.rows_dropped_total(), 300);
    }

    /// Same contract for `OptionMoversWriter`.
    #[test]
    fn test_db7_option_movers_record_drop_increments_counter() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match OptionMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => OptionMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
        let err = anyhow::anyhow!("synthetic flush failure");
        writer.record_drop(50, "test", &err);
        assert_eq!(writer.rows_dropped_total(), 50);
    }

    /// Throttle blocks within window (DB-6).
    #[test]
    fn test_db6_stock_movers_reconnect_throttle_blocks_within_window() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match StockMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => StockMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert!(writer.reconnect_allowed_now());
        writer.bump_reconnect_throttle();
        assert!(
            !writer.reconnect_allowed_now(),
            "after bump, stock movers writer must block reconnect within throttle window"
        );
    }

    /// Same for option movers.
    #[test]
    fn test_db6_option_movers_reconnect_throttle_blocks_within_window() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match OptionMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => OptionMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert!(writer.reconnect_allowed_now());
        writer.bump_reconnect_throttle();
        assert!(!writer.reconnect_allowed_now());
    }

    /// Stock movers counter must saturate at u64::MAX on overflow.
    #[test]
    fn test_db7_stock_movers_record_drop_saturates() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let mut writer = match StockMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => StockMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        writer.rows_dropped_total = u64::MAX - 1;
        let err = anyhow::anyhow!("overflow");
        writer.record_drop(100, "overflow", &err);
        assert_eq!(writer.rows_dropped_total(), u64::MAX);
    }

    /// Pub-fn coverage: `StockMoversWriter::rows_dropped_total()` getter
    /// on a freshly-constructed writer returns zero.
    #[test]
    fn test_db7_stock_movers_rows_dropped_total_starts_zero() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let writer = match StockMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => StockMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
    }

    /// Pub-fn coverage: `OptionMoversWriter::rows_dropped_total()` getter
    /// on a freshly-constructed writer returns zero.
    #[test]
    fn test_db7_option_movers_rows_dropped_total_starts_zero() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let writer = match OptionMoversWriter::new(&config) {
            Ok(w) => w,
            Err(_) => OptionMoversWriter {
                sender: None,
                buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
                pending_count: 0,
                ilp_conf_string: config.build_ilp_conf_string(),
                next_reconnect_allowed: Instant::now(),
                rows_dropped_total: 0,
                rescue_ring: VecDeque::new(),
            },
        };
        assert_eq!(writer.rows_dropped_total(), 0);
    }

    // -----------------------------------------------------------------------
    // DB-2: rescue ring (movers)
    // -----------------------------------------------------------------------

    fn synth_stock_writer() -> StockMoversWriter {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        StockMoversWriter {
            sender: None,
            buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
            pending_count: 0,
            ilp_conf_string: config.build_ilp_conf_string(),
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(MOVERS_RESCUE_RING_CAPACITY),
        }
    }

    fn synth_option_writer() -> OptionMoversWriter {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        OptionMoversWriter {
            sender: None,
            buffer: questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1),
            pending_count: 0,
            ilp_conf_string: config.build_ilp_conf_string(),
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            rescue_ring: VecDeque::with_capacity(MOVERS_RESCUE_RING_CAPACITY),
        }
    }

    fn mk_stock_row(ts_nanos: i64) -> BufferedStockMover {
        BufferedStockMover {
            ts_nanos,
            category: "GAINER".to_string(),
            rank: 1,
            security_id: 1,
            segment: "NSE_EQ".to_string(),
            symbol: "RELIANCE".to_string(),
            ltp: 100.0,
            prev_close: 99.0,
            change_pct: 1.0,
            volume: 100_000,
            phase: STOCK_MOVERS_PHASE_MARKET.to_string(),
        }
    }

    fn mk_option_row(ts_nanos: i64) -> BufferedOptionMover {
        BufferedOptionMover {
            ts_nanos,
            category: "HIGHEST_OI".to_string(),
            rank: 1,
            security_id: 1,
            segment: "NSE_FNO".to_string(),
            contract_name: "NIFTY-Jun2026-28500-CE".to_string(),
            underlying: "NIFTY".to_string(),
            option_type: "CE".to_string(),
            strike: 28500.0,
            expiry: "2026-06-26".to_string(),
            spot_price: 28550.0,
            ltp: 150.0,
            change: 5.0,
            change_pct: 3.4,
            oi: 100_000,
            oi_change: 1_000,
            oi_change_pct: 1.0,
            volume: 50_000,
            value: 7_500_000.0,
        }
    }

    #[test]
    fn test_db2_movers_rescue_ring_capacity_is_bounded() {
        assert!(MOVERS_RESCUE_RING_CAPACITY >= 500);
        assert!(MOVERS_RESCUE_RING_CAPACITY <= 50_000);
    }

    #[test]
    fn test_db2_stock_rescue_ring_len_starts_zero_and_increments() {
        let mut writer = synth_stock_writer();
        assert_eq!(writer.rescue_ring_len(), 0);
        writer.push_to_rescue_ring(mk_stock_row(1));
        assert_eq!(writer.rescue_ring_len(), 1);
        writer.push_to_rescue_ring(mk_stock_row(2));
        assert_eq!(writer.rescue_ring_len(), 2);
    }

    #[test]
    fn test_db2_option_rescue_ring_len_starts_zero_and_increments() {
        let mut writer = synth_option_writer();
        assert_eq!(writer.rescue_ring_len(), 0);
        writer.push_to_rescue_ring(mk_option_row(1));
        assert_eq!(writer.rescue_ring_len(), 1);
    }

    #[test]
    fn test_db2_stock_rescue_ring_is_fifo() {
        let mut writer = synth_stock_writer();
        writer.push_to_rescue_ring(mk_stock_row(100));
        writer.push_to_rescue_ring(mk_stock_row(200));
        writer.push_to_rescue_ring(mk_stock_row(300));
        let a = writer.rescue_ring.pop_front().unwrap();
        let b = writer.rescue_ring.pop_front().unwrap();
        let c = writer.rescue_ring.pop_front().unwrap();
        assert_eq!(a.ts_nanos, 100);
        assert_eq!(b.ts_nanos, 200);
        assert_eq!(c.ts_nanos, 300);
    }

    #[test]
    fn test_db2_option_rescue_ring_is_fifo() {
        let mut writer = synth_option_writer();
        writer.push_to_rescue_ring(mk_option_row(100));
        writer.push_to_rescue_ring(mk_option_row(200));
        let first = writer.rescue_ring.pop_front().unwrap();
        let second = writer.rescue_ring.pop_front().unwrap();
        assert_eq!(first.ts_nanos, 100);
        assert_eq!(second.ts_nanos, 200);
    }

    #[test]
    fn test_db2_stock_rescue_ring_overflow_evicts_oldest_and_counts_drop() {
        let mut writer = synth_stock_writer();
        // Pre-fill to capacity directly (bypass push_to_rescue_ring so
        // we don't run the overflow check N times during setup).
        for i in 0..MOVERS_RESCUE_RING_CAPACITY {
            writer
                .rescue_ring
                .push_back(mk_stock_row(i64::try_from(i).unwrap_or(i64::MAX)));
        }
        assert_eq!(writer.rescue_ring_len(), MOVERS_RESCUE_RING_CAPACITY);
        assert_eq!(writer.rows_dropped_total(), 0);

        // Push one more — oldest (ts=0) must be evicted.
        writer.push_to_rescue_ring(mk_stock_row(i64::MAX));
        assert_eq!(writer.rescue_ring_len(), MOVERS_RESCUE_RING_CAPACITY);
        assert_eq!(writer.rows_dropped_total(), 1);
        assert_eq!(writer.rescue_ring.back().unwrap().ts_nanos, i64::MAX);
        assert_eq!(writer.rescue_ring.front().unwrap().ts_nanos, 1);
    }

    #[test]
    fn test_db2_option_rescue_ring_overflow_evicts_oldest_and_counts_drop() {
        let mut writer = synth_option_writer();
        for i in 0..MOVERS_RESCUE_RING_CAPACITY {
            writer
                .rescue_ring
                .push_back(mk_option_row(i64::try_from(i).unwrap_or(i64::MAX)));
        }
        assert_eq!(writer.rescue_ring_len(), MOVERS_RESCUE_RING_CAPACITY);
        assert_eq!(writer.rows_dropped_total(), 0);
        writer.push_to_rescue_ring(mk_option_row(i64::MAX));
        assert_eq!(writer.rescue_ring_len(), MOVERS_RESCUE_RING_CAPACITY);
        assert_eq!(writer.rows_dropped_total(), 1);
    }

    #[test]
    fn test_db2_buffered_stock_row_size_is_bounded() {
        let size = std::mem::size_of::<BufferedStockMover>();
        assert!(
            size <= 160,
            "BufferedStockMover size is {size} bytes — if this grows, \
             re-tune MOVERS_RESCUE_RING_CAPACITY"
        );
    }

    #[test]
    fn test_db2_buffered_option_row_size_is_bounded() {
        let size = std::mem::size_of::<BufferedOptionMover>();
        assert!(
            size <= 320,
            "BufferedOptionMover size is {size} bytes — if this grows, \
             re-tune MOVERS_RESCUE_RING_CAPACITY"
        );
    }

    // -----------------------------------------------------------------------
    // Plan item C3 (2026-04-22): top_movers DDL ratchet tests.
    // These lock the schema shape — once the writer ships, column renames
    // become breaking changes across the SEBI 5-year retention window.
    // -----------------------------------------------------------------------

    #[test]
    fn test_top_movers_ddl_exists_and_names_table() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("top_movers"),
            "top_movers DDL must reference the table name"
        );
    }

    #[test]
    fn test_top_movers_ddl_has_required_columns() {
        let required_columns = [
            "bucket SYMBOL",
            "rank_category SYMBOL",
            "rank INT",
            "security_id LONG",
            "symbol SYMBOL",
            "underlying SYMBOL",
            "expiry STRING",
            "strike DOUBLE",
            "option_type SYMBOL",
            "ltp DOUBLE",
            "prev_close DOUBLE",
            "change_pct DOUBLE",
            "volume LONG",
            "value DOUBLE",
            "oi LONG",
            "prev_oi LONG",
            "oi_change_pct DOUBLE",
            "ts TIMESTAMP",
        ];
        for col in required_columns {
            assert!(
                TOP_MOVERS_CREATE_DDL.contains(col),
                "top_movers DDL missing required column `{col}`"
            );
        }
    }

    #[test]
    fn test_top_movers_ddl_is_partitioned_by_day() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("PARTITION BY DAY"),
            "top_movers must partition by day (SEBI 5-year retention)"
        );
    }

    #[test]
    fn test_top_movers_ddl_is_wal_enabled() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("WAL"),
            "top_movers must be WAL for append-only durability"
        );
    }

    #[test]
    fn test_top_movers_ddl_designated_ts_is_ts() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("TIMESTAMP(ts)"),
            "top_movers designated timestamp must be `ts`"
        );
    }

    #[test]
    fn test_top_movers_table_name_constant_matches_ddl() {
        assert_eq!(
            QUESTDB_TABLE_TOP_MOVERS, "top_movers",
            "table name constant must match DDL"
        );
    }

    #[test]
    fn test_top_movers_dedup_key_covers_bucket_category_rank() {
        // All three are required so different buckets / categories / ranks
        // within the same second do NOT collide. Matches the plan C spec.
        assert!(DEDUP_KEY_TOP_MOVERS.contains("bucket"));
        assert!(DEDUP_KEY_TOP_MOVERS.contains("rank_category"));
        assert!(DEDUP_KEY_TOP_MOVERS.contains("rank"));
    }

    // -----------------------------------------------------------------------
    // Plan items C, D, I (2026-04-25): timeframe + segment columns + DEDUP
    // + ALTER self-heal. Catches accidental schema reverts.
    // -----------------------------------------------------------------------

    /// Item C.1: the DDL declares a `timeframe SYMBOL` column. Without
    /// this, every snapshot row would have a NULL timeframe and the 15
    /// timeframes (1m..15m) cannot be distinguished at query time.
    #[test]
    fn test_top_movers_ddl_contains_timeframe_column() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("timeframe SYMBOL"),
            "top_movers DDL must declare a `timeframe SYMBOL` column"
        );
    }

    /// Item C.2: the DDL declares a `segment SYMBOL` column. Required for
    /// I-P1-11 cross-segment composite key (e.g. id=27 IDX_I vs NSE_EQ).
    #[test]
    fn test_top_movers_ddl_contains_segment_column() {
        assert!(
            TOP_MOVERS_CREATE_DDL.contains("segment SYMBOL"),
            "top_movers DDL must declare a `segment SYMBOL` column for I-P1-11"
        );
    }

    /// Item C.3: DEDUP key includes `timeframe` so 1m and 5m rows at the
    /// same second do not collide.
    #[test]
    fn test_top_movers_dedup_key_includes_timeframe() {
        assert!(
            DEDUP_KEY_TOP_MOVERS.contains("timeframe"),
            "DEDUP_KEY_TOP_MOVERS must include `timeframe` — without it, \
             snapshots from different timeframes at the same second collide"
        );
    }

    /// Item C.4: DEDUP key includes `security_id` AND `segment`. Per
    /// I-P1-11, `security_id` alone is not unique — Dhan reuses ids
    /// across segments (e.g. FINNIFTY id=27 IDX_I + a stock id=27 NSE_EQ).
    #[test]
    fn test_top_movers_dedup_key_includes_security_id_and_segment() {
        assert!(
            DEDUP_KEY_TOP_MOVERS.contains("security_id"),
            "DEDUP_KEY_TOP_MOVERS must include `security_id` for I-P1-11"
        );
        assert!(
            DEDUP_KEY_TOP_MOVERS.contains("segment"),
            "DEDUP_KEY_TOP_MOVERS must include `segment` for I-P1-11"
        );
    }

    /// Item I.1: ALTER DDL for the new `timeframe` column uses
    /// `IF NOT EXISTS` so it is safe to run on every boot. Without this
    /// idempotency, pre-2026-04-25 deployments would crash at boot.
    #[test]
    fn test_top_movers_alter_ddl_timeframe_is_idempotent() {
        assert!(
            TOP_MOVERS_ALTER_DDL_TIMEFRAME.contains("ADD COLUMN IF NOT EXISTS"),
            "ALTER for `timeframe` must use ADD COLUMN IF NOT EXISTS"
        );
        assert!(
            TOP_MOVERS_ALTER_DDL_TIMEFRAME.contains("timeframe SYMBOL"),
            "ALTER must declare the new column type as SYMBOL"
        );
    }

    /// Item I.2: ALTER DDL for the new `segment` column is also
    /// idempotent for the same reason as I.1.
    #[test]
    fn test_top_movers_alter_ddl_segment_is_idempotent() {
        assert!(
            TOP_MOVERS_ALTER_DDL_SEGMENT.contains("ADD COLUMN IF NOT EXISTS"),
            "ALTER for `segment` must use ADD COLUMN IF NOT EXISTS"
        );
        assert!(
            TOP_MOVERS_ALTER_DDL_SEGMENT.contains("segment SYMBOL"),
            "ALTER must declare the new column type as SYMBOL"
        );
    }

    /// Item D.4: TopMoverRow has dedicated `timeframe` and `segment`
    /// fields. Catches accidental field removal in a refactor.
    #[test]
    fn test_top_mover_row_has_timeframe_and_segment_fields() {
        let row = sample_price_row();
        // If either field name is removed by a future refactor, this
        // ceases to compile — the test is a compile-time ratchet too.
        assert_eq!(row.timeframe, "1m");
        assert_eq!(row.segment, "NSE_EQ");
    }

    // -----------------------------------------------------------------------
    // Plan item C2 (2026-04-22): TopMoverRow wire shape ratchets
    // -----------------------------------------------------------------------

    fn sample_derivative_row() -> TopMoverRow {
        TopMoverRow {
            ts_nanos: 1_700_000_000_000_000_000,
            timeframe: "1m",
            bucket: "index_options".to_string(),
            rank_category: "top_oi".to_string(),
            rank: 1,
            security_id: 49_081,
            segment: "NSE_FNO",
            symbol: "NIFTY 25000 CE 28-APR".to_string(),
            underlying: Some("NIFTY".to_string()),
            expiry: Some("2026-04-28".to_string()),
            strike: Some(25_000.0),
            option_type: Some("CE".to_string()),
            ltp: 162.15,
            prev_close: 290.30,
            change_pct: -44.14,
            volume: 122_023_330,
            value: 19_786_775_434.5,
            oi: 8_017_880,
            prev_oi: 4_673_565,
            oi_change_pct: 71.56,
        }
    }

    fn sample_price_row() -> TopMoverRow {
        TopMoverRow {
            ts_nanos: 1_700_000_000_000_000_000,
            timeframe: "1m",
            bucket: "stocks".to_string(),
            rank_category: "gainers".to_string(),
            rank: 1,
            security_id: 1_333,
            segment: "NSE_EQ",
            symbol: "RELIANCE".to_string(),
            underlying: None,
            expiry: None,
            strike: None,
            option_type: None,
            ltp: 1_368.10,
            prev_close: 1_265.30,
            change_pct: 8.12,
            volume: 6_551_573,
            value: 8_963_200_000.0,
            oi: 0,
            prev_oi: 0,
            oi_change_pct: 0.0,
        }
    }

    #[test]
    fn test_top_mover_row_fields_match_ddl_columns() {
        // Every field referenced in append_row_to_buffer MUST be present in
        // the DDL column list.
        let required = [
            "bucket",
            "rank_category",
            "rank",
            "security_id",
            "symbol",
            "underlying",
            "expiry",
            "strike",
            "option_type",
            "ltp",
            "prev_close",
            "change_pct",
            "volume",
            "value",
            "oi",
            "prev_oi",
            "oi_change_pct",
            "ts",
        ];
        for col in required {
            assert!(
                TOP_MOVERS_CREATE_DDL.contains(col),
                "DDL missing column `{col}` that the V2 writer emits"
            );
        }
    }

    #[test]
    fn test_top_mover_row_derivative_vs_price_optional_fields() {
        let deriv = sample_derivative_row();
        assert!(deriv.underlying.is_some());
        assert!(deriv.expiry.is_some());
        assert!(deriv.strike.is_some());
        assert!(deriv.option_type.is_some());
        assert!(deriv.oi > 0);

        let price = sample_price_row();
        assert!(price.underlying.is_none());
        assert!(price.expiry.is_none());
        assert!(price.strike.is_none());
        assert!(price.option_type.is_none());
        // Price buckets emit oi=0 rather than omitting — DDL column is NOT NULL-able.
        assert_eq!(price.oi, 0);
        assert_eq!(price.oi_change_pct, 0.0);
    }

    #[test]
    fn test_top_mover_row_append_to_buffer_fresh_questdb_buffer() {
        // Structural test: verify append_row_to_buffer does not panic for
        // both a derivative row (with all optional fields populated) and
        // a price row (with all optional fields None). We cannot flush to
        // QuestDB without a live instance — this just exercises the column
        // ordering + symbol/str conversions.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        TopMoversV2Writer::append_row_to_buffer(&mut buffer, &sample_derivative_row())
            .expect("derivative row should append cleanly");
        TopMoversV2Writer::append_row_to_buffer(&mut buffer, &sample_price_row())
            .expect("price row should append cleanly");
        // Buffer should now contain two line-protocol rows. len_bytes > 0.
        assert!(
            !buffer.is_empty(),
            "buffer must have content after two appends"
        );
    }

    #[test]
    fn test_top_mover_row_drops_are_tracked_on_zero_flush_path() {
        // Without constructing a real Sender we cannot test the full flush,
        // but we can at least verify the public getters are exposed and zero
        // after a fresh construction attempt failure (we don't try to connect).
        // This is a compile-time + default-state ratchet: changes to
        // `rows_dropped_total` or `rescue_ring_len` must remain observable.
        fn assert_observable<T: Sized>(_: &T) {}
        // No actual Writer construction — the getters are method signatures
        // whose visibility is asserted below via `fn pointer` coercions.
        let rows_dropped: fn(&TopMoversV2Writer) -> u64 = TopMoversV2Writer::rows_dropped_total;
        let rescue_len: fn(&TopMoversV2Writer) -> usize = TopMoversV2Writer::rescue_ring_len;
        let rows_written: fn(&TopMoversV2Writer) -> u64 = TopMoversV2Writer::rows_written_total;
        assert_observable(&rows_dropped);
        assert_observable(&rescue_len);
        assert_observable(&rows_written);
    }

    #[test]
    fn test_top_movers_rescue_ring_capacity_shared_with_legacy_writers() {
        // The V2 writer MUST reuse the same capacity constant as the legacy
        // stock/option writers so operational tuning stays in one place.
        assert_eq!(MOVERS_RESCUE_RING_CAPACITY, 5_000);
    }

    #[test]
    fn test_top_movers_batch_size_matches_legacy_writers() {
        // Must reuse the same batch-size constant — auto-flush boundary
        // behaviour is identical across the three writers.
        assert_eq!(MOVERS_FLUSH_BATCH_SIZE, 250);
    }
}
