//! `ticks` table — live-tick QuestDB ILP persistence (REBUILD, 2026-08-09).
//!
//! ## Why this file exists again
//!
//! The original `tick_persistence.rs` was deleted on 2026-07-17 in the stage-2
//! dead-WS sweep (PR #1631) together with the rest of the dead Dhan tick chain:
//! by then both live feeds had been retired (Dhan live WS 2026-07-13, Groww
//! live WS 2026-07-15) so the writer had zero production callers. The `ticks`
//! TABLE was never dropped (SEBI retention) and its DDL is still owned by
//! `scripts/questdb-init.sh`.
//!
//! The Dhan live main-feed WS was re-authorized on 2026-08-09
//! (`.claude/rules/project/websocket-connection-scope-lock.md`, dated operator
//! quote), and that revival explicitly names `tick_persistence.rs` + a
//! `DEDUP_KEY_TICKS` **const** as things that must come back. This module is
//! that rebuild.
//!
//! **The schema is NOT changed here.** `ticks` already exists with the final
//! column set and the final 5-key DEDUP; [`ticks_create_ddl`] and
//! [`ensure_ticks_table`] reproduce that exact shape idempotently so a fresh
//! box (or a box whose init script never ran) self-heals to it.
//!
//! ## The load-bearing detail: `capture_seq`
//!
//! The live DEDUP key is
//! `UPSERT KEYS(ts, security_id, segment, capture_seq, feed)` — see
//! [`DEDUP_KEY_TICKS`], which is a byte-for-byte copy of the `ALTER TABLE`
//! clause in `scripts/questdb-init.sh` (pinned by
//! `tick_dedup_key_matches_questdb_init_script`).
//!
//! Dhan's `exchange_timestamp` (LTT) is **second-granular**, so `ts` is
//! IDENTICAL for every tick an instrument produces inside one wall-clock
//! second. Without a per-arrival tiebreaker in the key, every tick but the
//! LAST in each second is silently UPSERTed away — invisible, catastrophic
//! data loss. `capture_seq` is that tiebreaker, and it must be:
//!
//! * **strictly monotonic** — two distinct arrivals must never share a value
//!   (they would collapse). [`next_capture_seq`] guarantees this with a
//!   lock-free `max(prev + 1, wall_clock_nanos)` CAS loop, which is monotonic
//!   even when two rows are built inside one nanosecond (`+1` wins) and even
//!   when NTP steps the clock backwards (`+1` wins again).
//! * **restart-safe** — the wall-clock seed means a process restart resumes
//!   ABOVE every value the previous process wrote, so a post-restart tick can
//!   never collide with a pre-restart row in the same second.
//! * **replay-stable** — a WAL replay / reconnect re-send of the SAME frame
//!   must reproduce the SAME `capture_seq` so it collapses (idempotent
//!   replay). That is why [`TickWriter::append_tick_with_seq`] exists: the
//!   replay path threads the sequence recovered from the WAL frame
//!   (`ws_frame_spill`) instead of minting a fresh one.
//!
//! A constant, absent, or non-monotonic `capture_seq` is the single most
//! dangerous defect this module can carry. It is pinned by
//! `n_ticks_in_one_second_with_distinct_capture_seq_produce_distinct_rows`
//! and `capture_seq_is_strictly_monotonic_per_connection`.
//!
//! ## Width discipline
//!
//! * `ParsedTick::security_id` is **`u64`** and the column is **`LONG`
//!   (`i64`)** — a genuine NARROWING. `as i64` would wrap an id with bit 63
//!   set into a NEGATIVE number, aliasing two instruments onto one DEDUP key.
//!   Saturating would alias too. So this conversion is **fail-closed**:
//!   [`TickRow::from_parsed_tick`] returns [`TickRowError::SecurityIdNotRepresentable`],
//!   counted by `tv_tick_rows_refused_total` and logged at `error!`.
//!   All live namespace bands stay below `2^63` by construction
//!   (`truedata-feed-scope-2026-07-24.md` §9.5), so a hit means corruption.
//! * `ParsedTick::volume` is **`u32`** and the column is `LONG` — a WIDENING,
//!   so nothing is lost today. The boundary still routes through
//!   [`saturate_volume_to_i64`] so that a future `u64`-wide cumulative volume
//!   (TrueData `TotVolume`, Groww day volume) saturates LOUDLY
//!   (`tv_tick_volume_saturated_total` + a coded `warn!`) instead of silently
//!   truncating. Honest note: with a `u32` source that arm is unreachable —
//!   it is the guard for the wider feed, not dead-code theatre.
//! * Prices arrive as `f32` and the columns are `DOUBLE`. Every widening goes
//!   through `f32_to_f64_clean` (STORAGE-GAP-02) — a bare `as f64` /
//!   `f64::from` produces `23925.650390625` for `23925.65_f32`.
//!
//! ## NULL-not-0
//!
//! Every `Option` column that is `None` has its ILP token OMITTED, so QuestDB
//! stores NULL. An LTP-only feed must not claim `open = 0`.
//!
//! ## Path class
//!
//! Cold path relative to the socket read loop: the WS reader's job is
//! drain → WAL → channel, and this writer runs behind that. Allocation here is
//! acceptable; it is still kept modest (`&'static str` symbols, one reusable
//! ILP buffer, no per-row `String`).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_TICKS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::price_precision::{f32_to_f64_clean, round_to_2dp};
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tickvault_common::segment::segment_code_to_str;
use tickvault_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Table + key contract
// ---------------------------------------------------------------------------

/// QuestDB table name — the shared live-tick table (both feeds, `feed` in-key).
pub const TICKS_TABLE: &str = QUESTDB_TABLE_TICKS;

/// The COMPLETE `ticks` DEDUP UPSERT key — byte-for-byte the clause in
/// `scripts/questdb-init.sh`:
///
/// ```sql
/// ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, capture_seq, feed)
/// ```
///
/// Declared as a real `const` (never an inline literal in a `format!`) for two
/// reasons that are both mechanical, not stylistic:
///
/// 1. `crates/storage/tests/dedup_segment_meta_guard.rs` discovers keys by
///    scanning `crates/storage/src` for a `DEDUP_KEY_*` string constant. An
///    inline literal is INVISIBLE to that scan, which would put the single
///    most dangerous DEDUP key in the codebase OUTSIDE the guard that proves
///    `segment` (I-P1-11 / STORAGE-GAP-01) and `feed` (operator override
///    2026-06-28) are present.
/// 2. `ts` is FIRST because QuestDB requires the designated timestamp column
///    in every `DEDUP UPSERT KEYS(...)` clause (the 2026-05-18 HTTP-400
///    production regression).
///
/// Why each column is in the key:
///
/// * `ts` — designated timestamp, mandatory.
/// * `security_id` + `segment` — Dhan reuses one numeric id across segments
///   (`13` is NIFTY on `IDX_I` and a different instrument on `NSE_EQ`), so the
///   pair is the only unique instrument identity (I-P1-11).
/// * `capture_seq` — the intra-second tiebreaker; see the module docs. Without
///   it, second-granular `ts` collapses every tick but the last in a second.
/// * `feed` — a Dhan observation and a Groww observation of the same
///   instrument-second are DISTINCT observations, never duplicates.
pub const DEDUP_KEY_TICKS: &str = "ts, security_id, segment, capture_seq, feed";

/// `feed` SYMBOL value for Dhan-sourced rows. Sourced from the canonical
/// [`Feed`] enum so the wire label can never drift from a duplicated literal.
pub const TICK_FEED_DHAN: &str = Feed::Dhan.as_str();

/// `feed` SYMBOL value for Groww-sourced rows.
pub const TICK_FEED_GROWW: &str = Feed::Groww.as_str();

/// `feed` SYMBOL value for TrueData-sourced rows.
pub const TICK_FEED_TRUEDATA: &str = Feed::Truedata.as_str();

/// Timeout for the idempotent QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Returns the complete `ticks` DEDUP UPSERT key.
///
/// Exposed so gap-enforcement / integration tests can assert STORAGE-GAP-01
/// without reaching into the constant directly.
#[must_use]
pub fn tick_dedup_key() -> &'static str {
    DEDUP_KEY_TICKS
}

// ---------------------------------------------------------------------------
// capture_seq — the intra-second dedup tiebreaker
// ---------------------------------------------------------------------------

/// Process-global capture sequence. Seeded lazily from the wall clock on the
/// first call, so a restart resumes ABOVE anything the previous process wrote.
static TICK_CAPTURE_SEQ: AtomicI64 = AtomicI64::new(0);

/// Wall-clock nanoseconds since the Unix epoch, saturating (never panics).
fn wall_clock_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Returns a strictly-monotonic capture sequence: `max(prev + 1, wall_nanos)`.
///
/// Guarantees:
/// * **never repeats, never decreases** — even for two rows built inside one
///   nanosecond (the `+1` wins) and even if NTP steps the clock backwards
///   (the `+1` still wins);
/// * **restart-safe** — the wall-clock floor puts a fresh process above every
///   sequence the previous one emitted;
/// * process-global, so two connections of the SAME feed can never mint the
///   same value (which would collapse two real ticks under the DEDUP key).
///   "Monotonic per connection" follows: each writer observes a strictly
///   increasing subsequence.
///
/// Lock-free CAS loop, O(1), zero heap allocation.
#[must_use]
pub fn next_capture_seq() -> i64 {
    let now = wall_clock_nanos();
    loop {
        let prev = TICK_CAPTURE_SEQ.load(Ordering::Relaxed);
        let next = prev.saturating_add(1).max(now);
        if TICK_CAPTURE_SEQ
            .compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

// ---------------------------------------------------------------------------
// Width-narrowing boundary
// ---------------------------------------------------------------------------

/// Narrows a `u64` cumulative volume onto the `LONG` (`i64`) column, saturating
/// LOUDLY rather than truncating silently.
///
/// The destination is `i64`; today's sources are all narrower or equal
/// (`ParsedTick::volume` is `u32`, Groww/TrueData day volume is `i64`), so the
/// saturating arm is UNREACHABLE from [`TickRow::from_parsed_tick`]. It is the
/// boundary guard for a `u64`-wide feed, and it is what stops a future
/// `raw as i64` from wrapping a huge cumulative volume to a negative number.
fn saturate_volume_to_i64(raw: u64, security_id: i64) -> i64 {
    i64::try_from(raw).unwrap_or_else(|_| {
        metrics::counter!("tv_tick_volume_saturated_total").increment(1);
        warn!(
            code = ErrorCode::StorageGapF32F64Precision.code_str(),
            security_id,
            raw_volume = raw,
            "tick volume exceeds the LONG column range — saturated at i64::MAX \
             (loud, never a silent truncation); the stored value is a FLOOR, \
             not the exchange figure"
        );
        i64::MAX
    })
}

/// Why a [`TickRow`] could not be built from a wire tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TickRowError {
    /// `security_id` does not fit the `LONG` column (bit 63 set).
    ///
    /// Fail-closed on purpose: `as i64` would wrap it negative and
    /// saturating would pin it to `i64::MAX` — BOTH alias two distinct
    /// instruments onto one DEDUP key, which is silent cross-instrument
    /// corruption and strictly worse than refusing one loud row. Every live
    /// namespace band is below `2^63` by construction, so this means the
    /// upstream id is corrupt.
    #[error(
        "security_id {raw} does not fit the ticks.security_id LONG column \
         (bit 63 set) — row refused rather than aliased onto another instrument"
    )]
    SecurityIdNotRepresentable {
        /// The raw wire id that could not be represented.
        raw: u64,
    },
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// One `ticks` row, ready for ILP append. Feed-agnostic and `Copy`.
///
/// Per-feed OPTIONAL columns are `Option<T>`: `None` OMITS the ILP token so
/// the cell is NULL, never a misleading `0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TickRow {
    /// Composite-key part 1 (I-P1-11), already narrowed to the `LONG` column.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11) — `IDX_I` / `NSE_EQ` / `NSE_FNO` / ….
    pub segment: &'static str,
    /// Last traded price (`DOUBLE`).
    pub ltp: f64,
    /// Cumulative day volume (`LONG`).
    pub volume: i64,
    /// Designated timestamp — IST epoch nanoseconds.
    pub ts_ist_nanos: i64,
    /// Replay-stable intra-second dedup tiebreaker. See the module docs.
    pub capture_seq: i64,
    /// Day open. `None` → NULL.
    pub open: Option<f64>,
    /// Day high. `None` → NULL.
    pub high: Option<f64>,
    /// Day low. `None` → NULL.
    pub low: Option<f64>,
    /// Previous-session close. `None` → NULL.
    pub close: Option<f64>,
    /// Open interest. `None` → NULL.
    pub oi: Option<i64>,
    /// Average traded price (VWAP). `None` → NULL.
    pub avg_price: Option<f64>,
    /// Last trade quantity. `None` → NULL.
    pub last_trade_qty: Option<i64>,
    /// Total buy quantity. `None` → NULL.
    pub total_buy_qty: Option<i64>,
    /// Total sell quantity. `None` → NULL.
    pub total_sell_qty: Option<i64>,
    /// Raw exchange timestamp (IST epoch SECONDS), verbatim audit column.
    pub exchange_timestamp: Option<i64>,
    /// Local receive instant as IST nanoseconds. `None` → NULL.
    pub received_at_ist_nanos: Option<i64>,
    /// Deterministic content fingerprint (integrity column, NOT in the key).
    pub payload_hash: Option<i64>,
}

impl TickRow {
    /// Builds a row from a wire [`ParsedTick`] and a `capture_seq`.
    ///
    /// * `security_id` `u64` → `i64` is checked (see [`TickRowError`]).
    /// * `volume` `u32` → `i64` widens losslessly through the saturating
    ///   boundary helper.
    /// * every `f32` price widens via `f32_to_f64_clean` then `round_to_2dp`
    ///   (STORAGE-GAP-02 + the 2-dp price rule) — never a bare `as f64`.
    /// * a zero-valued Ticker-packet field stays `Some(0.0)` only where the
    ///   exchange really sent it; fields a Ticker packet does not carry
    ///   (OHLC, OI, VWAP, quantities) map to `None` → NULL, never `0`.
    ///
    /// # Errors
    /// [`TickRowError::SecurityIdNotRepresentable`] when the id has bit 63 set.
    pub fn from_parsed_tick(tick: &ParsedTick, capture_seq: i64) -> Result<Self, TickRowError> {
        let security_id = i64::try_from(tick.security_id).map_err(|_| {
            metrics::counter!("tv_tick_rows_refused_total", "reason" => "security_id_width")
                .increment(1);
            error!(
                code = ErrorCode::StorageGapTickDedupSegment.code_str(),
                raw_security_id = tick.security_id,
                segment = segment_code_to_str(tick.exchange_segment_code),
                "tick row refused — security_id does not fit the LONG column; \
                 narrowing it would alias two instruments onto one DEDUP key \
                 (silent cross-instrument corruption). Row NOT written."
            );
            TickRowError::SecurityIdNotRepresentable {
                raw: tick.security_id,
            }
        })?;

        // A Ticker (16-byte) packet carries only LTP + LTT; Quote/Full add the
        // rest. Zero means "not carried" for those, so it becomes NULL.
        let opt_price = |v: f32| (v != 0.0).then(|| round_to_2dp(f32_to_f64_clean(v)));
        let opt_qty = |v: u32| (v != 0).then(|| i64::from(v));

        Ok(Self {
            security_id,
            segment: segment_code_to_str(tick.exchange_segment_code),
            ltp: round_to_2dp(f32_to_f64_clean(tick.last_traded_price)),
            volume: saturate_volume_to_i64(u64::from(tick.volume), security_id),
            // Dhan sends exchange_timestamp already in IST epoch SECONDS — no
            // +19800 offset (`.claude/rules/dhan/live-market-feed.md`).
            ts_ist_nanos: i64::from(tick.exchange_timestamp).saturating_mul(1_000_000_000),
            capture_seq,
            open: opt_price(tick.day_open),
            high: opt_price(tick.day_high),
            low: opt_price(tick.day_low),
            close: opt_price(tick.day_close),
            oi: opt_qty(tick.open_interest),
            avg_price: opt_price(tick.average_traded_price),
            last_trade_qty: (tick.last_trade_quantity != 0)
                .then(|| i64::from(tick.last_trade_quantity)),
            total_buy_qty: opt_qty(tick.total_buy_quantity),
            total_sell_qty: opt_qty(tick.total_sell_quantity),
            exchange_timestamp: Some(i64::from(tick.exchange_timestamp)),
            received_at_ist_nanos: (tick.received_at_nanos != 0).then_some(
                tick.received_at_nanos
                    .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
            ),
            payload_hash: None,
        })
    }
}

// ---------------------------------------------------------------------------
// DDL (idempotent self-heal — the schema is NOT changed, only reproduced)
// ---------------------------------------------------------------------------

/// The idempotent `CREATE TABLE` DDL for `ticks`.
///
/// Byte-compatible with `scripts/questdb-init.sh` (column set + order +
/// `TIMESTAMP(ts) PARTITION BY HOUR WAL`). Pure — no I/O.
#[must_use]
pub fn ticks_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {TICKS_TABLE} (\
            feed SYMBOL, \
            segment SYMBOL, \
            security_id LONG, \
            ltp DOUBLE, \
            open DOUBLE, \
            high DOUBLE, \
            low DOUBLE, \
            close DOUBLE, \
            volume LONG, \
            oi LONG, \
            avg_price DOUBLE, \
            last_trade_qty LONG, \
            total_buy_qty LONG, \
            total_sell_qty LONG, \
            exchange_timestamp LONG, \
            received_at TIMESTAMP, \
            payload_hash LONG, \
            capture_seq LONG, \
            ts TIMESTAMP\
        ) TIMESTAMP(ts) PARTITION BY HOUR WAL"
    )
}

/// Every `ticks` column with its type, for the per-column self-heal ALTERs.
const TICKS_COLUMNS: &[(&str, &str)] = &[
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("security_id", "LONG"),
    ("ltp", "DOUBLE"),
    ("open", "DOUBLE"),
    ("high", "DOUBLE"),
    ("low", "DOUBLE"),
    ("close", "DOUBLE"),
    ("volume", "LONG"),
    ("oi", "LONG"),
    ("avg_price", "DOUBLE"),
    ("last_trade_qty", "LONG"),
    ("total_buy_qty", "LONG"),
    ("total_sell_qty", "LONG"),
    ("exchange_timestamp", "LONG"),
    ("received_at", "TIMESTAMP"),
    ("payload_hash", "LONG"),
    ("capture_seq", "LONG"),
];

/// The ordered DDL statements [`ensure_ticks_table`] issues: CREATE → per-column
/// `ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`. Never a DROP (SEBI retention).
/// Pure, so the statement set is unit-testable without a live QuestDB.
#[must_use]
pub fn ticks_ensure_statements() -> Vec<String> {
    let mut out = vec![ticks_create_ddl()];
    for (col, ty) in TICKS_COLUMNS {
        out.push(format!(
            "ALTER TABLE {TICKS_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty}"
        ));
    }
    out.push(format!(
        "ALTER TABLE {TICKS_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICKS})"
    ));
    out
}

/// Idempotently self-heals the `ticks` schema (CREATE → ADD COLUMN → DEDUP
/// ENABLE). Best-effort: failures log and continue, they never block boot.
///
/// A failed ensure leaves the table to be auto-created by the first ILP write
/// WITHOUT the DEDUP keys — an intra-second duplicate/collapse window until a
/// later ensure succeeds. That consequence is logged, never swallowed.
// TEST-EXEMPT: live-QuestDB DDL runner; the statement set is unit-tested via
// ticks_ensure_statements(), and the 200 / 500 / unreachable arms are exercised
// by the mock-HTTP tokio tests below.
pub async fn ensure_ticks_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_tick_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                stage = "ensure_client_build",
                ?err,
                "ticks table not ensured — HTTP client build failed; the first \
                 ILP write may auto-create the table WITHOUT the 5-key DEDUP, \
                 which collapses intra-second ticks until a later ensure succeeds"
            );
            return;
        }
    };
    for ddl in &ticks_ensure_statements() {
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
                metrics::counter!("tv_tick_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "ticks DDL returned non-2xx — the 5-key DEDUP may be missing, \
                     which collapses intra-second ticks"
                );
            }
            Err(err) => {
                metrics::counter!("tv_tick_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "ticks DDL request failed"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// ILP-over-HTTP conf: per-flush server ACK (the 2026-07-05 fire-and-forget
/// lesson) with `retry_timeout=0` (the caller owns retry cadence) and a bounded
/// `request_timeout` so a hung flush cannot wedge the pipeline.
fn ticks_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Batched `ticks` ILP writer — one per feed CONNECTION.
///
/// Lazy: an unreachable QuestDB at construction still builds (rows buffer
/// locally). `flush` returns `Err` — including a server-side reject via the
/// HTTP ACK — and the pending buffer is discarded LOUDLY so one poisoned row
/// can never wedge the rest of the session.
pub struct TickWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// The `feed` SYMBOL stamped on every row from this writer.
    feed: Feed,
    /// Highest `capture_seq` this connection has stamped — the per-connection
    /// monotonicity witness (`0` before the first row).
    last_capture_seq: i64,
}

impl TickWriter {
    /// Production constructor — ILP-over-HTTP, lazy on connect failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy-build contract is
    // exercised by tick_writer_new_is_lazy_and_buffers_without_network, and every
    // append/flush path is covered via for_test().
    pub fn new(config: &QuestDbConfig, feed: Feed) -> Self {
        match Sender::from_conf(&ticks_ilp_http_conf(config)) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    feed,
                    last_capture_seq: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    feed = feed.as_str(),
                    "ticks writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    feed,
                    last_capture_seq: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer with an empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test(feed: Feed) -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            feed,
            last_capture_seq: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// The highest `capture_seq` stamped by this connection so far (`0` before
    /// the first row). The monotonicity witness for
    /// `capture_seq_is_strictly_monotonic_per_connection`.
    #[must_use]
    // TEST-EXEMPT: observability accessor, asserted by the monotonicity tests.
    pub fn last_capture_seq(&self) -> i64 {
        self.last_capture_seq
    }

    /// Test-only view of the raw ILP buffer bytes (wire-shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends a live tick, minting a fresh strictly-monotonic `capture_seq`.
    ///
    /// Use this on the LIVE arrival path only. The WAL-replay / reconnect
    /// re-send path MUST use [`Self::append_tick_with_seq`] with the sequence
    /// recovered from the frame, otherwise a replayed frame mints a NEW
    /// sequence and lands as a DUPLICATE row instead of collapsing.
    ///
    /// # Errors
    /// [`TickRow::from_parsed_tick`] refusals and ILP buffer errors.
    pub fn append_tick(&mut self, tick: &ParsedTick) -> Result<()> {
        self.append_tick_with_seq(tick, next_capture_seq())
    }

    /// Appends a live tick with a caller-supplied, replay-stable `capture_seq`
    /// (sourced from the WAL frame sequence).
    ///
    /// # Errors
    /// [`TickRow::from_parsed_tick`] refusals and ILP buffer errors.
    pub fn append_tick_with_seq(&mut self, tick: &ParsedTick, capture_seq: i64) -> Result<()> {
        let row = TickRow::from_parsed_tick(tick, capture_seq)?;
        self.append_row(&row)
    }

    /// Appends one prepared [`TickRow`] to the ILP buffer (no flush).
    ///
    /// ILP requires every SYMBOL before any field column, so `segment` and
    /// `feed` are written first. Both are routed through `sanitize_ilp_symbol`
    /// (defence in depth against line-protocol injection) even though both come
    /// from closed `&'static str` sets today. `None` columns are OMITTED so the
    /// cell is NULL rather than `0`.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, row: &TickRow) -> Result<()> {
        let feed = self.feed.as_str();
        self.buffer
            .table(TICKS_TABLE)
            .context("table")?
            .symbol("segment", sanitize_ilp_symbol(row.segment).as_ref())
            .context("segment")?
            .symbol("feed", sanitize_ilp_symbol(feed).as_ref())
            .context("feed")?
            .column_i64("security_id", row.security_id)
            .context("security_id")?
            .column_f64("ltp", row.ltp)
            .context("ltp")?;

        // Per-feed OPTIONAL columns — omitted (NULL) when None.
        if let Some(v) = row.open {
            self.buffer.column_f64("open", v).context("open")?;
        }
        if let Some(v) = row.high {
            self.buffer.column_f64("high", v).context("high")?;
        }
        if let Some(v) = row.low {
            self.buffer.column_f64("low", v).context("low")?;
        }
        if let Some(v) = row.close {
            self.buffer.column_f64("close", v).context("close")?;
        }
        self.buffer
            .column_i64("volume", row.volume)
            .context("volume")?;
        if let Some(v) = row.oi {
            self.buffer.column_i64("oi", v).context("oi")?;
        }
        if let Some(v) = row.avg_price {
            self.buffer
                .column_f64("avg_price", v)
                .context("avg_price")?;
        }
        if let Some(v) = row.last_trade_qty {
            self.buffer
                .column_i64("last_trade_qty", v)
                .context("last_trade_qty")?;
        }
        if let Some(v) = row.total_buy_qty {
            self.buffer
                .column_i64("total_buy_qty", v)
                .context("total_buy_qty")?;
        }
        if let Some(v) = row.total_sell_qty {
            self.buffer
                .column_i64("total_sell_qty", v)
                .context("total_sell_qty")?;
        }
        if let Some(v) = row.exchange_timestamp {
            self.buffer
                .column_i64("exchange_timestamp", v)
                .context("exchange_timestamp")?;
        }
        if let Some(v) = row.received_at_ist_nanos {
            self.buffer
                .column_ts("received_at", TimestampNanos::new(v))
                .context("received_at")?;
        }
        if let Some(v) = row.payload_hash {
            self.buffer
                .column_i64("payload_hash", v)
                .context("payload_hash")?;
        }

        // capture_seq is the LAST column before the designated timestamp.
        self.buffer
            .column_i64("capture_seq", row.capture_seq)
            .context("capture_seq")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .context("designated timestamp")?;

        self.pending = self.pending.saturating_add(1);
        self.last_capture_seq = self.last_capture_seq.max(row.capture_seq);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP with a per-flush server ACK.
    ///
    /// On ANY failed flush the pending buffer is DISCARDED (the shadow-writer
    /// precedent): a server-REJECTED row retained across flushes would be
    /// re-sent forever and block every later row. The discard is LOUD —
    /// counter + `error!` — never silent.
    ///
    /// # Errors
    /// `Err` when disconnected or when the HTTP flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "ticks: no ILP sender (QuestDB unreachable) — {dropped} pending \
                 row(s) discarded"
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
                    "ticks ILP flush failed — {dropped} pending row(s) discarded \
                     (poisoned-buffer defence)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("ticks: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drops every buffered-but-unflushed row (poisoned-buffer defence).
    ///
    /// Returns the discarded count and reports it LOUDLY —
    /// `tv_ticks_dropped_total` plus a coded `error!` — so tick loss is never
    /// silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_ticks_dropped_total", "feed" => self.feed.as_str())
                .increment(dropped as u64);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = self.feed.as_str(),
                dropped,
                "tick auto-flush failed — buffered tick rows discarded from the \
                 ILP buffer; these ticks are NOT in QuestDB"
            );
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
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root above crates/storage")
            .to_path_buf()
    }

    fn sample_tick() -> ParsedTick {
        ParsedTick {
            security_id: 13,
            exchange_segment_code: 0, // IDX_I
            last_traded_price: 23_146.45,
            last_trade_quantity: 50,
            exchange_timestamp: 1_780_000_000,
            received_at_nanos: 1_780_000_000_111_000_000,
            average_traded_price: 23_145.10,
            volume: 1_234_567,
            total_sell_quantity: 9_000,
            total_buy_quantity: 10_000,
            day_open: 23_100.0,
            day_close: 23_090.0,
            day_high: 23_200.0,
            day_low: 23_050.0,
            open_interest: 987_654,
            oi_day_high: 0,
            oi_day_low: 0,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        }
    }

    fn sample_row() -> TickRow {
        TickRow::from_parsed_tick(&sample_tick(), 42).expect("sample tick must build")
    }

    // ======================================================================
    // The DEDUP key — the single most dangerous detail in this module
    // ======================================================================

    /// The const MUST equal the live `ALTER TABLE ... DEDUP ENABLE UPSERT
    /// KEYS(...)` clause in `scripts/questdb-init.sh`, character for
    /// character. A drift here means the writer believes in a key the
    /// database does not enforce.
    #[test]
    fn tick_dedup_key_matches_questdb_init_script() {
        let script = std::fs::read_to_string(workspace_root().join("scripts/questdb-init.sh"))
            .expect("scripts/questdb-init.sh must exist");
        let expected = format!("ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICKS})");
        assert!(
            script.contains(&expected),
            "DEDUP_KEY_TICKS drifted from the live DDL.\n  const  : {DEDUP_KEY_TICKS}\n  \
             expected clause not found in scripts/questdb-init.sh: {expected}"
        );
        // Belt and braces: the exact 5-key literal, so a future edit that
        // changes BOTH sides in lockstep still trips review.
        assert_eq!(
            DEDUP_KEY_TICKS,
            "ts, security_id, segment, capture_seq, feed"
        );
    }

    /// I-P1-11 / STORAGE-GAP-01 / feed-in-key / ts-first, asserted on whole
    /// tokens (this is what `dedup_segment_meta_guard.rs` scans for).
    #[test]
    fn tick_dedup_key_has_ts_first_segment_capture_seq_and_feed() {
        let tokens: Vec<&str> = DEDUP_KEY_TICKS.split(',').map(str::trim).collect();
        assert_eq!(
            tokens.first().copied(),
            Some("ts"),
            "designated ts must lead"
        );
        for required in ["ts", "security_id", "segment", "capture_seq", "feed"] {
            assert!(
                tokens.contains(&required),
                "DEDUP key missing `{required}`: {DEDUP_KEY_TICKS}"
            );
        }
        assert_eq!(tokens.len(), 5, "exactly 5 keys: {DEDUP_KEY_TICKS}");
        assert_eq!(tick_dedup_key(), DEDUP_KEY_TICKS);
    }

    /// The DEDUP key is declared as a real `const` (not an inline literal), so
    /// `dedup_segment_meta_guard.rs`'s `DEDUP_KEY_*` string-constant scan
    /// can see it. An inline literal would silently leave the `ticks` key
    /// outside the meta-guard.
    #[test]
    fn tick_dedup_key_is_a_scannable_const_declaration() {
        let src = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tick_persistence.rs"),
        )
        .expect("own source must be readable");
        // The needle is assembled PIECEWISE across lines on purpose: the
        // meta-guard's scanner treats any single line carrying `const `,
        // `DEDUP_KEY_` and the `&str` type together as a declaration, so a
        // verbatim literal here would register as a second (bogus, bodyless)
        // DEDUP key and fail the guard on this test file's own text.
        let decl_prefix = ["pub ", "const "].concat();
        let needle = format!("{decl_prefix}DEDUP_KEY_TICKS: &str = ");
        assert!(
            src.contains(&needle),
            "DEDUP_KEY_TICKS must stay a plain string-constant declaration so \
             the dedup_segment_meta_guard scanner discovers it; an inline \
             literal in the DDL would leave the ticks key outside the guard"
        );
        // And the DDL builder must interpolate the CONST, never a literal.
        assert!(
            src.contains("DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICKS})"),
            "the ensure DDL must interpolate DEDUP_KEY_TICKS, not an inline key"
        );
    }

    // ======================================================================
    // capture_seq — intra-second survival + monotonicity
    // ======================================================================

    /// THE regression this module exists to prevent: Dhan `ts` is
    /// second-granular, so N ticks for ONE instrument inside ONE second share
    /// `ts`. Only a distinct `capture_seq` keeps them distinct rows. This
    /// asserts the full 5-tuple key differs for all N, including the
    /// value-identical `45 → 75 → 45` index sequence (volume 0, no trades)
    /// that a content-hash tiebreaker used to collapse.
    #[test]
    fn n_ticks_in_one_second_with_distinct_capture_seq_produce_distinct_rows() {
        const N: usize = 8;
        let ltps = [23_146.45_f32, 23_146.75, 23_146.45, 23_146.75];
        let mut writer = TickWriter::for_test(Feed::Dhan);
        let mut keys: Vec<(i64, i64, &'static str, i64, &'static str)> = Vec::new();

        for i in 0..N {
            let mut tick = sample_tick();
            // Same instrument, same wall-clock SECOND for every tick.
            tick.exchange_timestamp = 1_780_000_000;
            tick.last_traded_price = ltps[i % ltps.len()];
            tick.volume = 0; // index shape: value-identical rows
            let seq = next_capture_seq();
            writer
                .append_tick_with_seq(&tick, seq)
                .expect("append must succeed");
            let row = TickRow::from_parsed_tick(&tick, seq).expect("row");
            keys.push((
                row.ts_ist_nanos,
                row.security_id,
                row.segment,
                row.capture_seq,
                Feed::Dhan.as_str(),
            ));
        }

        // Every row shares ts + security_id + segment + feed …
        assert!(
            keys.iter().all(|k| k.0 == keys[0].0),
            "all {N} ticks must share the same second-granular ts"
        );
        // … so capture_seq is the ONLY thing keeping them apart.
        let mut seqs: Vec<i64> = keys.iter().map(|k| k.3).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), N, "every capture_seq must be distinct");

        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            N,
            "all {N} same-second rows must have DISTINCT 5-key DEDUP tuples — \
             otherwise QuestDB upserts every row but the last one away"
        );
        assert_eq!(writer.pending(), N, "all {N} rows buffered");
    }

    /// The counter-example: a CONSTANT `capture_seq` collapses the whole
    /// second into one row. Proves the test above is non-vacuous.
    #[test]
    fn constant_capture_seq_collapses_a_whole_second_into_one_row() {
        let mut keys = Vec::new();
        for i in 0..8 {
            let mut tick = sample_tick();
            tick.exchange_timestamp = 1_780_000_000;
            tick.last_traded_price = 23_100.0 + i as f32;
            let row = TickRow::from_parsed_tick(&tick, 7).expect("row");
            keys.push((
                row.ts_ist_nanos,
                row.security_id,
                row.segment,
                row.capture_seq,
            ));
        }
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            1,
            "a constant capture_seq must collapse 8 distinct ticks to 1 key — \
             this is the catastrophic loss the module guards against"
        );
    }

    /// `capture_seq` is strictly monotonic per connection (and process-wide),
    /// so two arrivals can never share a value.
    #[test]
    fn capture_seq_is_strictly_monotonic_per_connection() {
        let mut writer = TickWriter::for_test(Feed::Dhan);
        assert_eq!(writer.last_capture_seq(), 0, "no rows yet");
        let mut prev = writer.last_capture_seq();
        for _ in 0..500 {
            writer
                .append_tick(&sample_tick())
                .expect("append must succeed");
            let cur = writer.last_capture_seq();
            assert!(
                cur > prev,
                "capture_seq must strictly increase per connection: {cur} !> {prev}"
            );
            prev = cur;
        }
    }

    /// [`next_capture_seq`] never repeats and never decreases, even for calls
    /// inside the same nanosecond, and it is seeded from the wall clock so a
    /// restart resumes above the previous process's values.
    #[test]
    fn test_next_capture_seq_is_monotonic_and_wall_clock_seeded() {
        let before = wall_clock_nanos();
        let first = next_capture_seq();
        assert!(
            first >= before,
            "seq must be floored at the wall clock (restart-safety): {first} < {before}"
        );
        let mut prev = first;
        for _ in 0..10_000 {
            let cur = next_capture_seq();
            assert!(cur > prev, "strictly monotonic: {cur} !> {prev}");
            prev = cur;
        }
    }

    /// The replay contract: re-appending the SAME frame with its ORIGINAL
    /// sequence reproduces the SAME DEDUP tuple (collapses = idempotent),
    /// while minting a fresh sequence would create a duplicate row.
    #[test]
    fn test_append_tick_with_seq_is_replay_stable() {
        let tick = sample_tick();
        let wal_seq = 123_456_789;
        let a = TickRow::from_parsed_tick(&tick, wal_seq).expect("row a");
        let b = TickRow::from_parsed_tick(&tick, wal_seq).expect("row b");
        assert_eq!(a, b, "same frame + same seq => byte-identical row");
        let fresh = TickRow::from_parsed_tick(&tick, next_capture_seq()).expect("row c");
        assert_ne!(
            a.capture_seq, fresh.capture_seq,
            "a freshly-minted seq must differ (that is why replay threads the WAL seq)"
        );
    }

    // ======================================================================
    // Width discipline
    // ======================================================================

    /// `security_id` u64 → LONG i64 is fail-closed: an id with bit 63 set is
    /// REFUSED, never wrapped negative (`as i64`) or saturated — both alias two
    /// distinct instruments onto one DEDUP key.
    #[test]
    fn test_from_parsed_tick_refuses_unrepresentable_security_id() {
        let mut tick = sample_tick();
        tick.security_id = u64::MAX;
        let err = TickRow::from_parsed_tick(&tick, 1).expect_err("must refuse");
        assert_eq!(
            err,
            TickRowError::SecurityIdNotRepresentable { raw: u64::MAX }
        );
        assert!(err.to_string().contains("row refused"));

        // The whole live band set stays below 2^63 and must pass unharmed.
        for raw in [
            0_u64,
            13,
            1 << 59,            // TrueData band
            1 << 61,            // GDF token band
            (1_u64 << 62) | 42, // Groww index band
            (1_u64 << 63) - 1,  // the exact boundary
        ] {
            let mut ok = sample_tick();
            ok.security_id = raw;
            let row = TickRow::from_parsed_tick(&ok, 1)
                .unwrap_or_else(|e| panic!("id {raw} must be representable: {e}"));
            assert_eq!(row.security_id as u64, raw, "id must round-trip exactly");
            assert!(row.security_id >= 0, "id must stay positive");
        }

        // And the writer surfaces the refusal instead of writing a bad row.
        let mut writer = TickWriter::for_test(Feed::Dhan);
        let mut bad = sample_tick();
        bad.security_id = 1 << 63;
        assert!(writer.append_tick(&bad).is_err(), "writer must propagate");
        assert_eq!(writer.pending(), 0, "no row buffered for a refused tick");
    }

    /// Volume width: the `ParsedTick` source is `u32` and the column is `LONG`
    /// (`i64`) — a WIDENING, so nothing truncates today. The boundary helper
    /// still saturates LOUDLY for a future `u64`-wide feed instead of wrapping
    /// negative via `as i64`.
    #[test]
    fn test_saturate_volume_to_i64_widens_losslessly_and_saturates_loudly() {
        // Every u32 (the live source width) round-trips exactly.
        for raw in [0_u32, 1, 1_234_567, u32::MAX] {
            assert_eq!(saturate_volume_to_i64(u64::from(raw), 13), i64::from(raw));
        }
        // i64::MAX itself is representable.
        assert_eq!(
            saturate_volume_to_i64(i64::MAX as u64, 13),
            i64::MAX,
            "the boundary value is exact, not saturated"
        );
        // Beyond i64::MAX: saturate (and count/warn), never wrap negative.
        for raw in [(i64::MAX as u64) + 1, u64::MAX] {
            let got = saturate_volume_to_i64(raw, 13);
            assert_eq!(got, i64::MAX, "must saturate, not truncate");
            assert!(got > 0, "a silent `as i64` would have wrapped negative");
        }
        // The end-to-end path: a u32-sourced tick widens exactly.
        let mut tick = sample_tick();
        tick.volume = u32::MAX;
        let row = TickRow::from_parsed_tick(&tick, 1).expect("row");
        assert_eq!(row.volume, i64::from(u32::MAX));
    }

    /// f32 → f64 goes through `f32_to_f64_clean`, never a bare widening —
    /// otherwise `23925.65_f32` lands as `23925.650390625` in QuestDB
    /// (STORAGE-GAP-02, operator-spotted 2026-05-25).
    #[test]
    fn test_prices_widen_via_f32_to_f64_clean_not_raw_widening() {
        for raw in [23_925.65_f32, 23_937.30, 10.20, 25_461.30] {
            let mut tick = sample_tick();
            tick.last_traded_price = raw;
            tick.day_open = raw;
            let row = TickRow::from_parsed_tick(&tick, 1).expect("row");
            let clean = round_to_2dp(f32_to_f64_clean(raw));
            assert_eq!(row.ltp, clean, "ltp must use f32_to_f64_clean");
            assert_eq!(row.open, Some(clean), "open must use f32_to_f64_clean");
            // The naive widening is measurably different for these values.
            let naive = f64::from(raw);
            if (naive - clean).abs() > f64::EPSILON {
                assert_ne!(row.ltp, naive, "must NOT be the raw f64::from widening");
            }
            // No more than 2 decimals reach the wire.
            let rendered = format!("{}", row.ltp);
            if let Some((_, frac)) = rendered.split_once('.') {
                assert!(frac.len() <= 2, "price {rendered} exceeds 2dp");
            }
        }
        // The source scan: this module must never widen f32 with `as f64`.
        let src = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tick_persistence.rs"),
        )
        .expect("own source");
        // The needles are ASSEMBLED, never written literally, so that no line
        // of this detector can match itself.
        //
        // A `// SCANNER-SELF` opt-out comment is not sufficient on its own:
        // it pins the exclusion to a LINE, and rustfmt is free to move code
        // between lines. That is not hypothetical -- a `cargo fmt --all` run
        // reflowed the original single-line predicate and separated it from
        // its marker, so the scanner flagged its own source and the test
        // failed in CI while passing before the format. Assembling the
        // needles makes the detector unmatchable by construction, which no
        // formatter can undo.
        let needle_f32 = concat!("f", "32");
        let needle_widen = concat!(" as ", "f64");
        for (idx, line) in src.lines().enumerate() {
            if line.contains("SCANNER-SELF") || line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(needle_f32) && line.contains(needle_widen) {
                panic!("line {} widens an f32 with `as f64`: {line}", idx + 1); // SCANNER-SELF
            }
        }
    }

    // ======================================================================
    // ILP wire shape + injection
    // ======================================================================

    #[test]
    fn test_append_row_writes_symbols_columns_and_capture_seq() {
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(TICKS_TABLE), "table first: {line}");
        assert!(line.contains(",segment=IDX_I"), "segment tag: {line}");
        assert!(line.contains(",feed=dhan"), "feed tag: {line}");
        assert!(line.contains("security_id=13i"), "sid: {line}");
        assert!(line.contains("capture_seq=42i"), "capture_seq: {line}");
        assert!(line.contains("volume=1234567i"), "volume: {line}");
        assert!(line.contains("oi=987654i"), "oi: {line}");
        assert!(
            line.contains("exchange_timestamp=1780000000i"),
            "ltt: {line}"
        );
        // Symbols must precede all field columns (ILP requirement).
        let first_field = line.find(" security_id=").expect("field section");
        assert!(
            line.find(",feed=").expect("feed tag") < first_field,
            "all SYMBOLs must precede the first column: {line}"
        );
        assert_eq!(w.last_capture_seq(), 42);
    }

    /// NULL-not-0: a Ticker-shape tick (LTP + LTT only) must leave every
    /// column it does not carry ABSENT from the wire, so QuestDB stores NULL
    /// rather than a misleading `0`.
    #[test]
    fn test_ticker_shape_row_omits_absent_columns_as_null() {
        let mut tick = ParsedTick::default();
        tick.security_id = 25;
        tick.exchange_segment_code = 0;
        tick.last_traded_price = 51_234.55;
        tick.exchange_timestamp = 1_780_000_000;
        let row = TickRow::from_parsed_tick(&tick, 9).expect("row");
        assert_eq!(row.open, None);
        assert_eq!(row.high, None);
        assert_eq!(row.low, None);
        assert_eq!(row.close, None);
        assert_eq!(row.oi, None);
        assert_eq!(row.avg_price, None);
        assert_eq!(row.last_trade_qty, None);
        assert_eq!(row.total_buy_qty, None);
        assert_eq!(row.total_sell_qty, None);
        assert_eq!(row.received_at_ist_nanos, None, "no receive clock => NULL");

        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_row(&row).expect("append");
        let line = w.buffer_utf8();
        for absent in [
            "open=",
            "high=",
            "low=",
            "close=",
            "oi=",
            "avg_price=",
            "last_trade_qty=",
            "total_buy_qty=",
            "total_sell_qty=",
            "received_at=",
            "payload_hash=",
        ] {
            assert!(
                !line.contains(absent),
                "`{absent}` must be OMITTED (NULL), never written as 0: {line}"
            );
        }
        // volume IS required-for-both and is written verbatim (indices are 0).
        assert!(
            line.contains("volume=0i"),
            "volume written verbatim: {line}"
        );
    }

    /// Line-protocol injection: a hostile SYMBOL value carrying a space,
    /// comma, equals sign or newline must not be able to terminate the tag
    /// section or start a second ILP line.
    ///
    /// The defence is TWO layered mechanisms, and this test pins BOTH
    /// (measured on the real encoder, not assumed):
    ///   * `sanitize_ilp_symbol` STRIPS `,` `=` `\n` `\r` + control chars;
    ///   * `questdb-rs` BACKSLASH-ESCAPES a space (`IDX I` → `IDX\ I`),
    ///     which is the one delimiter the sanitiser deliberately keeps.
    ///
    /// So after append, the tag section must contain NO raw unescaped
    /// `,` / `=` / ` ` inside a tag VALUE, exactly 2 tags, and exactly one
    /// ILP line.
    #[test]
    fn test_append_row_escapes_hostile_symbol_values_no_ilp_injection() {
        const HOSTILE: &[&str] = &[
            "IDX I",                       // space — escaped by the encoder
            "IDX,I",                       // comma — tag separator, stripped
            "IDX=I",                       // equals — kv separator, stripped
            "IDX\nI",                      // newline — line separator, stripped
            "IDX\r\nI",                    // CRLF
            "A,b=c ticks,segment=X ltp=1", // a fully-formed injected line
            "x\nticks,segment=EVIL ltp=1i 1",
            "\u{0}\u{7}IDX", // control chars
        ];
        for hostile in HOSTILE {
            let mut row = sample_row();
            row.segment = hostile;
            let mut w = TickWriter::for_test(Feed::Dhan);
            w.append_row(&row).expect("hostile append must not panic");
            let line = w.buffer_utf8();

            // Exactly ONE ILP line (the trailing terminator only).
            assert_eq!(
                line.matches('\n').count(),
                1,
                "hostile value {hostile:?} produced >1 ILP line: {line:?}"
            );
            assert!(line.ends_with('\n'), "line must terminate: {line:?}");
            assert!(
                !line.contains('\r'),
                "raw CR must never reach the wire: {line:?}"
            );
            assert_eq!(
                line.matches("ticks,").count(),
                1,
                "hostile value {hostile:?} injected a second row: {line:?}"
            );

            // Split the line into tag section / field section on the FIRST
            // unescaped space — this is exactly how an ILP parser reads it.
            let parts = split_unescaped(line.trim_end_matches('\n'), ' ');
            assert_eq!(
                parts.len(),
                3,
                "an ILP line has exactly 3 unescaped-space-separated parts \
                 (tags, fields, timestamp); hostile value {hostile:?} produced \
                 {}: {line:?}",
                parts.len()
            );
            let tag_section = &parts[0];
            assert!(
                parts[1].starts_with("security_id="),
                "field section must start at security_id, not inside a hostile \
                 tag value: {:?}",
                parts[1]
            );

            // Exactly 2 tags (measurement + segment + feed).
            let tags = split_unescaped(tag_section, ',');
            assert_eq!(
                tags.len(),
                3,
                "hostile value {hostile:?} split the tag section into {} parts: \
                 {tag_section:?}",
                tags.len()
            );
            assert_eq!(tags[0], "ticks", "measurement: {:?}", tags[0]);
            assert!(tags[1].starts_with("segment="), "tag 1: {:?}", tags[1]);
            assert!(tags[2].starts_with("feed=dhan"), "tag 2: {:?}", tags[2]);

            // No raw (unescaped) delimiter survives inside the tag VALUE.
            let value = tags[1].trim_start_matches("segment=");
            for (pos, ch) in value.char_indices() {
                if matches!(ch, ',' | '=' | ' ') {
                    assert!(
                        pos > 0 && value.as_bytes()[pos - 1] == b'\\',
                        "unescaped {ch:?} survived in the tag value {value:?} \
                         (from {hostile:?}) — line-protocol injection"
                    );
                }
            }
            assert!(
                !value.contains('\n') && !value.contains('\r'),
                "line breaks must never survive in a tag value: {value:?}"
            );
            assert!(
                !value.chars().any(char::is_control),
                "control chars must be stripped: {value:?}"
            );
        }

        // Non-vacuity: the sanitiser really does strip, and the encoder really
        // does escape — a clean value passes through untouched.
        assert_eq!(sanitize_ilp_symbol("IDX,I").as_ref(), "IDXI");
        assert_eq!(sanitize_ilp_symbol("IDX=I").as_ref(), "IDXI");
        assert_eq!(sanitize_ilp_symbol("IDX\nI").as_ref(), "IDXI");
        assert_eq!(sanitize_ilp_symbol("IDX_I").as_ref(), "IDX_I");
        let mut w = TickWriter::for_test(Feed::Dhan);
        let mut spaced = sample_row();
        spaced.segment = "IDX I";
        w.append_row(&spaced).expect("append");
        assert!(
            w.buffer_utf8().contains(r"segment=IDX\ I"),
            "the encoder must BACKSLASH-ESCAPE a space (the one delimiter the \
             sanitiser keeps): {}",
            w.buffer_utf8()
        );
    }

    /// Splits on `sep` only where it is NOT backslash-escaped (ILP rules).
    fn split_unescaped(s: &str, sep: char) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut escaped = false;
        for ch in s.chars() {
            if escaped {
                cur.push(ch);
                escaped = false;
            } else if ch == '\\' {
                cur.push(ch);
                escaped = true;
            } else if ch == sep {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(ch);
            }
        }
        out.push(cur);
        out
    }

    /// Per-feed rows never collide: `feed` is a DEDUP key column, so a Dhan and
    /// a Groww observation of the same instrument-second-sequence are BOTH kept.
    #[test]
    fn test_feed_symbol_is_stamped_per_writer() {
        for (feed, label) in [
            (Feed::Dhan, "dhan"),
            (Feed::Groww, "groww"),
            (Feed::Truedata, "truedata"),
        ] {
            let mut w = TickWriter::for_test(feed);
            w.append_row(&sample_row()).expect("append");
            assert!(
                w.buffer_utf8().contains(&format!(",feed={label}")),
                "writer must stamp feed={label}"
            );
        }
        assert_eq!(TICK_FEED_DHAN, "dhan");
        assert_eq!(TICK_FEED_GROWW, "groww");
        assert_eq!(TICK_FEED_TRUEDATA, "truedata");
    }

    // ======================================================================
    // DDL + writer lifecycle
    // ======================================================================

    #[test]
    fn test_ticks_create_ddl_matches_the_live_schema() {
        let ddl = ticks_create_ddl();
        for (col, ty) in TICKS_COLUMNS {
            assert!(
                ddl.contains(&format!("{col} {ty}")),
                "DDL missing {col} {ty}"
            );
        }
        assert!(ddl.contains("ts TIMESTAMP"));
        assert!(ddl.contains("TIMESTAMP(ts) PARTITION BY HOUR WAL"));
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS ticks"));
        assert!(!ddl.contains("DROP"), "never a DROP — SEBI retention");

        // Same column set as the live init script (schema unchanged).
        let script = std::fs::read_to_string(workspace_root().join("scripts/questdb-init.sh"))
            .expect("init script");
        let live = script
            .split("CREATE TABLE IF NOT EXISTS ticks ")
            .nth(1)
            .and_then(|s| s.split('\n').next())
            .expect("live ticks CREATE");
        for (col, ty) in TICKS_COLUMNS {
            assert!(
                live.contains(&format!("{col} {ty}")),
                "live DDL lacks {col} {ty} — schema drift"
            );
        }
    }

    #[test]
    fn test_ticks_ensure_statements_are_create_then_alter_then_dedup() {
        let stmts = ticks_ensure_statements();
        assert_eq!(stmts.len(), 1 + TICKS_COLUMNS.len() + 1);
        assert!(stmts[0].starts_with("CREATE TABLE IF NOT EXISTS ticks"));
        for s in &stmts[1..stmts.len() - 1] {
            assert!(
                s.contains("ADD COLUMN IF NOT EXISTS"),
                "self-heal ALTER: {s}"
            );
        }
        let last = stmts.last().expect("dedup statement");
        assert_eq!(
            last,
            &format!("ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICKS})")
        );
        assert!(
            stmts.iter().all(|s| !s.contains("DROP")),
            "no DROP anywhere — SEBI retention"
        );
    }

    #[test]
    fn test_ticks_ilp_http_conf_is_http_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = ticks_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    #[test]
    fn test_flush_when_disconnected_errors_and_discards_pending() {
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_row(&sample_row()).expect("append");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"), "{err}");
        assert!(err.to_string().contains("discarded"), "{err}");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty(), "buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = TickWriter::for_test(Feed::Dhan);
        assert!(empty.flush().is_ok());
    }

    #[test]
    fn test_discard_pending_clears_buffer_and_count() {
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_row(&sample_row()).expect("append");
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 2);
        assert_eq!(w.discard_pending(), 2, "returns the discarded count");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        assert_eq!(w.discard_pending(), 0, "idempotent");
    }

    // ======================================================================
    // Persistence helpers — mock QuestDB /exec (real code paths)
    // ======================================================================

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

    #[tokio::test]
    async fn test_ensure_ticks_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_ticks_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_ticks_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_ticks_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_ticks_table_unreachable_degrades_without_panic() {
        // Port 1 is reserved and never listening — a real transport failure.
        ensure_ticks_table(&mock_cfg(1)).await;
    }

    #[tokio::test]
    async fn tick_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction, so
        // new() against an unreachable host still buffers locally.
        let mut w = TickWriter::new(&mock_cfg(1), Feed::Dhan);
        assert_eq!(w.pending(), 0);
        w.append_tick(&sample_tick())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1);
    }
}
